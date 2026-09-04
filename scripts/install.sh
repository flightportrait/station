#!/bin/sh
# Station installer. Fresh Debian-family machine (Raspberry Pi OS
# included) to a running station. Run as a normal user; sudo is used
# where needed and the script says why before each use.
#
#   sh install.sh                 # the Station radio (rx), no readsb
#   sh install.sh --build-readsb  # also build readsb from source as the
#                                 # fallback radio (slow on a Pi 3)
#
# On a machine that already runs a receiver (readsb, dump1090-fa,
# PiAware, FR24, an adsb.im image, ultrafeeder in docker) the dongle can
# serve one program, so the script stops and offers two doors:
#
#   sh install.sh --add       # install nothing; print the line that makes
#                             # the existing software feed FlightPortrait
#   sh install.sh --replace   # stop it, install the Station, carry its
#                             # feeds and keys over into the wizard
#
#   sh install.sh --print     # install nothing; the manual path, one screen
#   sh install.sh --station-key <uuid>   # a key you kept, not a new one
#                                        # (STATION_KEY= does the same)
#
# Idempotent: everything already present is detected and kept.

HOME_DIR="$HOME/station"
MLATC_RELEASE="https://github.com/flightportrait/mlatc/releases/latest/download"
RX_RELEASE="https://github.com/flightportrait/rx/releases/latest/download"
FP_ADSB="feed.flightportrait.com,30004,beast_reduce_plus_out"

# --- helpers (sourced by scripts/tests with STATION_INSTALL_LIB=1) -------

is_uuid() {
    printf '%s' "$1" | grep -Eq '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$'
}

new_key() {
    if [ -r /proc/sys/kernel/random/uuid ]; then
        cat /proc/sys/kernel/random/uuid
    else
        od -An -N16 -tx1 /dev/urandom | tr -d ' \n' \
            | sed 's/^\(.\{8\}\)\(.\{4\}\)\(.\{4\}\)\(.\{4\}\)\(.\{12\}\)$/\1-\2-\3-\4-\5/'
    fi
}

# What already owns the dongle here. Sets DETECTED_KIND (readsb,
# dump1090-fa, piaware, fr24feed, adsbim, docker), DETECTED_WHAT (a name
# for a sentence) and DETECTED_UNIT or DETECTED_CONTAINER. Empty when
# nothing is found.
detect_receiver() {
    DETECTED_KIND=""; DETECTED_WHAT=""; DETECTED_UNIT=""; DETECTED_CONTAINER=""
    if command -v docker >/dev/null 2>&1; then
        line=$(docker ps --format '{{.Names}} {{.Image}}' 2>/dev/null \
            | grep -iE 'ultrafeeder|readsb|piaware|fr24' | head -1)
        if [ -n "$line" ]; then
            DETECTED_KIND="docker"
            DETECTED_CONTAINER=${line%% *}
            DETECTED_WHAT="the ${line#* } container ($DETECTED_CONTAINER)"
            return 0
        fi
    fi
    if [ -d /opt/adsb ] || systemctl list-unit-files adsb-setup.service 2>/dev/null | grep -q adsb-setup; then
        DETECTED_KIND="adsbim"; DETECTED_WHAT="an adsb.im feeder image"
        return 0
    fi
    for u in readsb dump1090-fa piaware fr24feed; do
        if systemctl is-active --quiet "$u" 2>/dev/null || systemctl is-enabled --quiet "$u" 2>/dev/null; then
            DETECTED_KIND="$u"; DETECTED_UNIT="$u"
            case "$u" in
                readsb) DETECTED_WHAT="readsb" ;;
                dump1090-fa) DETECTED_WHAT="dump1090-fa" ;;
                piaware) DETECTED_WHAT="PiAware" ;;
                fr24feed) DETECTED_WHAT="the FR24 feeder" ;;
            esac
            return 0
        fi
    done
    if pgrep -x readsb >/dev/null 2>&1; then
        DETECTED_KIND="readsb"; DETECTED_WHAT="a readsb started by hand"
        return 0
    fi
    # Installed but not running, or no systemctl to ask: the files say so.
    for u in readsb dump1090-fa; do
        if [ -r "/etc/default/$u" ] && { [ -f "/etc/systemd/system/$u.service" ] || [ -f "/lib/systemd/system/$u.service" ]; }; then
            DETECTED_KIND="$u"; DETECTED_UNIT="$u"; DETECTED_WHAT="$u (installed)"
            return 0
        fi
    done
    return 1
}

# readsb's /etc/default/readsb (or dump1090-fa's): every
# --net-connector host,port,proto[,uuid=...] in it, as lines of
#   adsb HOST PORT PROTO UUID
# for the ADS-B output protocols. Inputs and other protocols are skipped.
parse_readsb_connectors() {
    tr '"\n' '  ' < "$1" | awk '
    {
        for (i = 1; i <= NF; i++) {
            v = ""
            if ($i == "--net-connector") v = $(i+1)
            else if (index($i, "--net-connector=") == 1) v = substr($i, 17)
            if (v == "") continue
            n = split(v, f, ",")
            if (n < 3) continue
            if (f[3] !~ /^beast_(reduce_plus_out|reduce_out|out)$/) continue
            uuid = ""
            for (k = 4; k <= n; k++) if (index(f[k], "uuid=") == 1) uuid = substr(f[k], 6)
            print "adsb", f[1], f[2], f[3] (uuid == "" ? "" : " " uuid)
        }
    }'
}

# ultrafeeder's ULTRAFEEDER_CONFIG (entries separated by ; or newlines):
#   adsb,host,port,proto[,uuid=...]  ->  adsb HOST PORT PROTO UUID
#   mlat,host,port[,uuid=...][,...]  ->  mlat HOST PORT - UUID
parse_ultrafeeder_config() {
    printf '%s' "$1" | tr ';' '\n' | awk '
    {
        gsub(/^[ \t]+|[ \t]+$/, "")
        if ($0 == "") next
        n = split($0, f, ",")
        for (k = 1; k <= n; k++) gsub(/^[ \t]+|[ \t]+$/, "", f[k])
        uuid = ""
        for (k = 4; k <= n; k++) if (index(f[k], "uuid=") == 1) uuid = substr(f[k], 6)
        if (f[1] == "adsb" && n >= 4 && f[4] ~ /^beast_(reduce_plus_out|reduce_out|out)$/)
            print "adsb", f[2], f[3], f[4] (uuid == "" ? "" : " " uuid)
        else if (f[1] == "mlat" && n >= 3)
            print "mlat", f[2], f[3], "-" (uuid == "" ? "" : " " uuid)
    }'
}

# The environment of a docker container, one VAR=value per line.
container_env() {
    docker inspect -f '{{range .Config.Env}}{{println .}}{{end}}' "$1" 2>/dev/null
}

# Feed lines (from the parsers above, on stdin) to [[feed]] blocks, one
# per host: name from the host, adsb and mlat endpoints, the first key
# seen for that host.
feeds_to_toml() {
    awk '
    {
        h = $2
        if (!(h in seen)) { seen[h] = 1; order[++n] = h }
        if ($1 == "adsb" && adsb[h] == "") adsb[h] = h ":" $3
        if ($1 == "mlat" && mlat[h] == "") mlat[h] = h ":" $3
        if ($5 != "" && uuid[h] == "") uuid[h] = $5
    }
    END {
        for (i = 1; i <= n; i++) {
            h = order[i]
            name = h
            sub(/^(in|feed|mlat)\./, "", name)
            if (name == "") name = h
            print ""
            print "[[feed]]"
            print "name = \"" name "\""
            if (adsb[h] != "") print "adsb = \"" adsb[h] "\""
            if (mlat[h] != "") print "mlat = \"" mlat[h] "\""
            if (uuid[h] != "") print "uuid = \"" uuid[h] "\""
        }
    }'
}

# The feed lines of what was detected, from where that software keeps them.
detected_feed_lines() {
    case "$DETECTED_KIND" in
        readsb)      [ -r /etc/default/readsb ] && parse_readsb_connectors /etc/default/readsb ;;
        dump1090-fa|piaware) [ -r /etc/default/dump1090-fa ] && parse_readsb_connectors /etc/default/dump1090-fa ;;
        docker)
            cfg=$(container_env "$DETECTED_CONTAINER" | grep '^ULTRAFEEDER_CONFIG=' | cut -d= -f2-)
            [ -n "$cfg" ] && parse_ultrafeeder_config "$cfg"
            ;;
    esac
    return 0
}

# What to add so the existing software feeds FlightPortrait.
print_add_lines() {
    key="$1"
    echo
    echo "Station key: $key"
    echo "Keep it; it marks the feeds as yours."
    echo
    case "$DETECTED_KIND" in
        readsb)
            echo "readsb: add this to NET_OPTIONS in /etc/default/readsb, then"
            echo "  sudo systemctl restart readsb"
            echo
            echo "  --net-connector $FP_ADSB,uuid=$key"
            ;;
        dump1090-fa|piaware)
            echo "dump1090-fa (6.0 or newer takes --net-connector): add this to"
            echo "NET_OPTIONS in /etc/default/dump1090-fa, then"
            echo "  sudo systemctl restart dump1090-fa"
            echo
            echo "  --net-connector $FP_ADSB,uuid=$key"
            echo
            echo "Older dump1090-fa has no connectors; a small readsb beside it"
            echo "(--net-only, reading its port 30005) can carry the line instead."
            ;;
        docker)
            echo "ultrafeeder: add this entry to ULTRAFEEDER_CONFIG (entries are"
            echo "separated by ;), then recreate the container"
            echo
            echo "  adsb,$FP_ADSB,uuid=$key"
            ;;
        "")
            echo "No receiver was found here. The lines, for when there is one:"
            echo
            echo "readsb, in NET_OPTIONS of /etc/default/readsb:"
            echo "  --net-connector $FP_ADSB,uuid=$key"
            echo "ultrafeeder, an entry in ULTRAFEEDER_CONFIG:"
            echo "  adsb,$FP_ADSB,uuid=$key"
            ;;
        adsbim)
            echo "adsb.im image: aggregators are chosen in the image's own web page."
            echo "FlightPortrait is being added to its list; until it appears there,"
            echo "the image's \"other aggregator\" field takes this line:"
            echo
            echo "  adsb,$FP_ADSB,uuid=$key"
            ;;
        fr24feed)
            echo "FR24 feeder: it does not forward to other networks. Run readsb or"
            echo "the Station beside it on another dongle, or replace it (--replace)."
            ;;
    esac
    echo
    echo "The station appears on https://flightportrait.com/network a minute after"
    echo "the first frames arrive."
}

# The manual path, for a machine where this script cannot run.
print_manual() {
    arch=$(uname -m)
    cat <<EOF
The Station by hand, on a Debian-family machine ($arch):

1. Packages:   sudo apt-get install librtlsdr0 curl ca-certificates
2. Free the dongle from the TV driver:
     printf 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830\n' \\
       | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf
3. Binaries into $HOME_DIR (mkdir -p $HOME_DIR/state; chmod +x each):
     $RX_RELEASE/rx-$arch-unknown-linux-gnu        -> $HOME_DIR/rx
     $MLATC_RELEASE/mlatc-$arch-unknown-linux-musl -> $HOME_DIR/mlatc
     stationd from this repository's release, or built with cargo         -> $HOME_DIR/stationd
4. Service, /etc/systemd/system/stationd.service:
     [Unit]
     Description=Station feeder runtime
     After=network-online.target
     Wants=network-online.target
     [Service]
     User=$USER
     WorkingDirectory=$HOME_DIR
     ExecStart=$HOME_DIR/stationd --config $HOME_DIR/station.toml --state-dir $HOME_DIR/state
     Restart=always
     RestartSec=3
     [Install]
     WantedBy=multi-user.target
   then: sudo systemctl daemon-reload && sudo systemctl enable --now stationd
5. Setup: with no station.toml the service serves a wizard on the LAN at
     http://$(hostname).local:8654/   (or the machine's IP, port 8654)
   or, over SSH:  $HOME_DIR/stationd --init --config $HOME_DIR/station.toml
   A key you kept:  --station-key <uuid> on either.
EOF
}

# Stop the detected receiver so the dongle is free. Nothing is deleted.
stop_receiver() {
    case "$DETECTED_KIND" in
        docker)
            echo "$DETECTED_WHAT: stopping it and turning off its restart, so it"
            echo "  does not take the dongle back at boot (its files stay)"
            docker stop "$DETECTED_CONTAINER" >/dev/null 2>&1 || sudo docker stop "$DETECTED_CONTAINER" >/dev/null
            docker update --restart=no "$DETECTED_CONTAINER" >/dev/null 2>&1 \
                || sudo docker update --restart=no "$DETECTED_CONTAINER" >/dev/null 2>&1 || true
            ;;
        adsbim)
            echo "adsb.im image: turning its services off (sudo); the image stays"
            for u in adsb-setup adsb-docker; do sudo systemctl disable --now "$u" 2>/dev/null || true; done
            if command -v docker >/dev/null 2>&1; then
                for c in $(docker ps --format '{{.Names}}' 2>/dev/null); do
                    docker stop "$c" >/dev/null 2>&1; docker update --restart=no "$c" >/dev/null 2>&1
                done
            fi
            ;;
        *)
            if [ -n "$DETECTED_UNIT" ]; then
                echo "$DETECTED_WHAT: stopping and disabling its service (sudo), so"
                echo "  it does not take the dongle back at boot; its configuration stays"
                sudo systemctl disable --now "$DETECTED_UNIT"
            else
                echo "$DETECTED_WHAT: stopping it (sudo)"
                sudo pkill -x readsb || true
            fi
            ;;
    esac
}

# Where the old configuration still is, for the closing line.
old_config_location() {
    case "$DETECTED_KIND" in
        readsb) echo "/etc/default/readsb" ;;
        dump1090-fa|piaware) echo "/etc/default/dump1090-fa (and /etc/piaware.conf)" ;;
        docker) echo "the container $DETECTED_CONTAINER (stopped, not removed)" ;;
        adsbim) echo "/opt/adsb" ;;
        *) echo "where it was" ;;
    esac
}

# A key from the flag, the environment, a question, or the generator.
choose_key() {
    if [ -n "$KEY" ]; then
        is_uuid "$KEY" || { echo "\"$KEY\" is not a station key (36 characters, like 123e4567-e89b-12d3-a456-426614174000)"; exit 1; }
        return 0
    fi
    if [ -t 0 ]; then
        while :; do
            printf 'Have a station key already? Paste it, or press Enter for a new one: '
            read -r answer
            answer=$(printf '%s' "$answer" | tr -d ' ')
            if [ -z "$answer" ]; then break; fi
            if is_uuid "$answer"; then KEY="$answer"; return 0; fi
            echo "  Not a key: 36 characters, like 123e4567-e89b-12d3-a456-426614174000."
        done
    fi
    KEY=$(new_key)
}

if [ "${STATION_INSTALL_LIB:-0}" = 1 ]; then
    return 0 2>/dev/null || exit 0
fi

# --- arguments -----------------------------------------------------------
set -e
MODE=""; BUILD_READSB=0; KEY="${STATION_KEY:-}"
while [ $# -gt 0 ]; do
    case "$1" in
        --add) MODE=add ;;
        --replace) MODE=replace ;;
        --print) print_manual; exit 0 ;;
        --build-readsb) BUILD_READSB=1 ;;
        --station-key) shift; KEY="${1:-}" ;;
        --station-key=*) KEY="${1#--station-key=}" ;;
        *) echo "unknown option: $1"; exit 1 ;;
    esac
    shift
done

ARCH="$(uname -m)"
case "$ARCH" in
    aarch64|x86_64) ;;
    *) echo "unsupported architecture: $ARCH"; exit 1 ;;
esac

# --- what already owns the dongle ---------------------------------------
if [ -f "$HOME_DIR/station.toml" ]; then
    : # the Station is the receiver here; a second run just refreshes it
elif detect_receiver; then
    echo "This machine already runs $DETECTED_WHAT, and the dongle can serve one program."
    if [ -z "$MODE" ]; then
        if [ -t 0 ]; then
            printf 'Add FlightPortrait to %s (a), or replace it with the Station (r)? [a/r] ' "$DETECTED_WHAT"
            read -r answer
            case "$answer" in
                a|A) MODE=add ;;
                r|R) MODE=replace ;;
                *) echo "Nothing done."; exit 1 ;;
            esac
        else
            echo "Run again with --add (print the line that makes it feed FlightPortrait,"
            echo "install nothing) or --replace (stop it, install the Station, keep its feeds)."
            exit 2
        fi
    fi
    if [ "$MODE" = add ]; then
        choose_key
        print_add_lines "$KEY"
        exit 0
    fi
fi
if [ "$MODE" = add ]; then
    # Nothing detected, but asked to add: print the lines anyway, install nothing.
    choose_key
    print_add_lines "$KEY"
    exit 0
fi

echo "Station installer: $ARCH, into $HOME_DIR"
mkdir -p "$HOME_DIR/state"

# --- replacing: carry the feeds over, then free the dongle ---------------
if [ "$MODE" = replace ] && [ -n "$DETECTED_KIND" ]; then
    choose_key
    lines=$(detected_feed_lines)
    {
        echo "# Feeds carried over from $DETECTED_WHAT by the installer. The wizard"
        echo "# starts from these; edit or drop any of them there."
        echo "station_key = \"$KEY\""
        [ -n "$lines" ] && printf '%s\n' "$lines" | feeds_to_toml
        echo
        echo "[results]"
        echo "beast_connect = \"127.0.0.1:30004\""
    } > "$HOME_DIR/station.imported.toml"
    if [ -n "$lines" ]; then
        echo "feeds: kept $(printf '%s\n' "$lines" | awk '{print $2}' | sort -u | wc -l | tr -d ' ') from $DETECTED_WHAT in $HOME_DIR/station.imported.toml"
    else
        echo "feeds: none found in $DETECTED_WHAT's configuration; the wizard starts from the list"
    fi
    stop_receiver
    echo "The old configuration stays in $(old_config_location)."
fi

# --- the radio's library and the dongle's driver ------------------------
# rx links librtlsdr; the kernel's TV driver would otherwise claim the
# dongle at boot, so it is blacklisted like every feeder image does.
if dpkg -s librtlsdr0 >/dev/null 2>&1; then
    echo "librtlsdr0: already installed"
else
    echo "librtlsdr0: installing (sudo runs apt)"
    sudo apt-get update -qq
    sudo apt-get install -y -qq librtlsdr0 curl ca-certificates
fi
if [ -f /etc/modprobe.d/blacklist-rtlsdr.conf ]; then
    echo "dvb driver: already blacklisted"
else
    echo "dvb driver: blacklisting the TV driver so the dongle is free (sudo writes it)"
    printf 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830\n' \
        | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf >/dev/null
    sudo modprobe -r dvb_usb_rtl28xxu rtl2832 rtl2830 2>/dev/null || true
fi

# --- binaries ----------------------------------------------------------
# rx (the Station radio) and mlatc come from their public releases;
# stationd arrives beside this script until its repo publishes releases.
for bin in stationd mlatc rx; do
    if [ -x "$HOME_DIR/$bin" ]; then
        echo "$bin: already installed"
    elif [ -x "$(dirname "$0")/$bin" ]; then
        cp "$(dirname "$0")/$bin" "$HOME_DIR/$bin"
        echo "$bin: installed from alongside the script"
    elif [ "$bin" = "mlatc" ]; then
        echo "mlatc: downloading the release binary"
        curl -fsSL -o "$HOME_DIR/mlatc" "$MLATC_RELEASE/mlatc-$ARCH-unknown-linux-musl"
        chmod +x "$HOME_DIR/mlatc"
    elif [ "$bin" = "rx" ]; then
        echo "rx: downloading the release binary"
        if curl -fsSL -o "$HOME_DIR/rx" "$RX_RELEASE/rx-$ARCH-unknown-linux-gnu"; then
            chmod +x "$HOME_DIR/rx"
        else
            rm -f "$HOME_DIR/rx"
            echo "rx: no release for $ARCH yet; readsb will be the radio"
            BUILD_READSB=1
        fi
    else
        echo "$bin: missing. Copy it next to this script (a static"
        echo "  $ARCH build; see docs/PI.md for the Docker one-liner)."
        exit 1
    fi
done

# --- readsb ------------------------------------------------------------
# The fallback radio. readsb has no package repository; building it takes
# ten minutes on a Pi 3, so it is only built on request, or when rx could
# not be installed.
if [ -x "$HOME_DIR/readsb" ] || command -v readsb >/dev/null 2>&1; then
    echo "readsb: already present"
elif [ "$BUILD_READSB" = 1 ]; then
    echo "readsb: building from source (sudo installs the build tools)"
    sudo apt-get update -qq
    sudo apt-get install -y -qq git build-essential libusb-1.0-0-dev \
        librtlsdr-dev libncurses-dev zlib1g-dev libzstd-dev pkg-config
    rm -rf /tmp/readsb-src
    git clone -q --depth 1 https://github.com/wiedehopf/readsb /tmp/readsb-src
    make -C /tmp/readsb-src -j"$(nproc)" RTLSDR=yes
    cp /tmp/readsb-src/readsb "$HOME_DIR/readsb"
    echo "readsb: built"
else
    echo "readsb: skipped (rx is the radio; add --build-readsb for a fallback)"
fi

# --- configuration -----------------------------------------------------
if [ -f "$HOME_DIR/station.toml" ]; then
    echo "configuration: station.toml exists, keeping it"
    "$HOME_DIR/stationd" --check --config "$HOME_DIR/station.toml"
else
    # None yet: the service starts in setup mode and serves a browser
    # wizard on the LAN. (stationd --init is the terminal alternative.)
    # The wizard finds rx beside stationd and readsb, if built, as well.
    NEEDS_SETUP=1
    # The station key is decided here, once, so a returning person keeps
    # theirs; the wizard shows it rather than making another.
    if [ ! -f "$HOME_DIR/station.imported.toml" ]; then
        choose_key
        {
            echo "# Written by the installer: the station key the wizard shows."
            echo "station_key = \"$KEY\""
        } > "$HOME_DIR/station.imported.toml"
    fi
fi

# --- service -----------------------------------------------------------
echo "service: installing the systemd unit (sudo writes it)"
IMPORT_ARG=""
[ -f "$HOME_DIR/station.imported.toml" ] && IMPORT_ARG=" --import $HOME_DIR/station.imported.toml"
sed "s|/home/station/station|$HOME_DIR|g; s|^User=.*|User=$USER|; s|--state-dir $HOME_DIR/state|--state-dir $HOME_DIR/state$IMPORT_ARG|" \
    "$(dirname "$0")/../deploy/stationd.service" 2>/dev/null \
    > /tmp/stationd.service || cat > /tmp/stationd.service <<EOF
[Unit]
Description=Station feeder runtime
After=network-online.target
Wants=network-online.target

[Service]
User=$USER
WorkingDirectory=$HOME_DIR
ExecStart=$HOME_DIR/stationd --config $HOME_DIR/station.toml --state-dir $HOME_DIR/state$IMPORT_ARG
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
EOF
sudo mv /tmp/stationd.service /etc/systemd/system/stationd.service
sudo systemctl daemon-reload
sudo systemctl enable --now stationd

sleep 2
sudo systemctl --no-pager --lines 0 status stationd | head -3
echo
if [ "${NEEDS_SETUP:-0}" = "1" ]; then
    echo "Done. Finish setup in a browser on this network:"
    echo "  http://$(hostname).local:8654/  (or this machine's IP, port 8654)"
    echo "The same address becomes the station's status page afterwards."
    [ -n "$KEY" ] && echo "Station key: $KEY — keep it; it marks the feeds as yours."
else
    echo "Done. The station's page: http://$(hostname).local:8654/"
    echo "(or this machine's IP, port 8654)"
fi
