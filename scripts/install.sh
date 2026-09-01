#!/bin/sh
# Station installer. Fresh Debian-family machine (Raspberry Pi OS
# included) to a running station. Run as a normal user; sudo is used
# where needed and the script says why before each use.
#
#   sh install.sh
#
# Idempotent: everything already present is detected and kept.
set -e

HOME_DIR="$HOME/station"
ARCH="$(uname -m)"
case "$ARCH" in
    aarch64|x86_64) ;;
    *) echo "unsupported architecture: $ARCH"; exit 1 ;;
esac

echo "Station installer: $ARCH, into $HOME_DIR"
mkdir -p "$HOME_DIR/state"

# --- binaries ----------------------------------------------------------
# Prebuilt releases come later; today the binaries arrive beside this
# script (copied from a build machine) or get built here with Docker.
for bin in stationd mlatc; do
    if [ -x "$HOME_DIR/$bin" ]; then
        echo "$bin: already installed"
    elif [ -x "$(dirname "$0")/$bin" ]; then
        cp "$(dirname "$0")/$bin" "$HOME_DIR/$bin"
        echo "$bin: installed from alongside the script"
    else
        echo "$bin: missing. Copy it next to this script (a static"
        echo "  $ARCH build; see docs/PI.md for the Docker one-liner)."
        exit 1
    fi
done

# --- readsb ------------------------------------------------------------
if [ -x "$HOME_DIR/readsb" ] || command -v readsb >/dev/null 2>&1; then
    echo "readsb: already present"
else
    echo "readsb: building from source (sudo installs the build tools)"
    sudo apt-get update -qq
    sudo apt-get install -y -qq git build-essential libusb-1.0-0-dev \
        librtlsdr-dev libncurses-dev zlib1g-dev libzstd-dev pkg-config
    rm -rf /tmp/readsb-src
    git clone -q --depth 1 https://github.com/wiedehopf/readsb /tmp/readsb-src
    make -C /tmp/readsb-src -j"$(nproc)" RTLSDR=yes
    cp /tmp/readsb-src/readsb "$HOME_DIR/readsb"
    echo "readsb: built"
fi

# --- configuration -----------------------------------------------------
if [ -f "$HOME_DIR/station.toml" ]; then
    echo "configuration: station.toml exists, keeping it"
else
    "$HOME_DIR/stationd" --init --config "$HOME_DIR/station.toml"
fi
"$HOME_DIR/stationd" --check --config "$HOME_DIR/station.toml"

# --- service -----------------------------------------------------------
echo "service: installing the systemd unit (sudo writes it)"
sed "s|/home/station/station|$HOME_DIR|g; s|^User=.*|User=$USER|" \
    "$(dirname "$0")/../deploy/stationd.service" 2>/dev/null \
    > /tmp/stationd.service || cat > /tmp/stationd.service <<EOF
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
EOF
sudo mv /tmp/stationd.service /etc/systemd/system/stationd.service
sudo systemctl daemon-reload
sudo systemctl enable --now stationd

sleep 2
sudo systemctl --no-pager --lines 0 status stationd | head -3
echo
echo "Done. The station's page: http://$(hostname).local:8654/"
echo "(or this machine's IP, port 8654)"
