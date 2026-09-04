#!/bin/sh
# The installer's parsers, on sample configurations. Sources install.sh
# as a library (STATION_INSTALL_LIB=1 stops it before it installs).
#
#   sh scripts/tests/install_parse.sh
set -e
here=$(cd "$(dirname "$0")" && pwd)
STATION_INSTALL_LIB=1 . "$here/../install.sh"
fails=0
check() {
    if [ "$2" = "$3" ]; then
        echo "ok   $1"
    else
        echo "FAIL $1"; echo "  want: $3"; echo "  got:  $2"; fails=$((fails + 1))
    fi
}

# readsb: /etc/default/readsb as the Debian package writes it
tmp=$(mktemp)
cat > "$tmp" <<'EOF'
RECEIVER_OPTIONS="--device 0 --device-type rtlsdr --gain -10 --ppm 0"
DECODER_OPTIONS="--max-range 450 --write-json-every 1"
NET_OPTIONS="--net --net-heartbeat 60 --net-ro-size 1280 --net-ro-interval 0.2 --net-ri-port 0 --net-ro-port 30002 --net-sbs-port 30003 --net-bi-port 30004,30104 --net-bo-port 30005 --net-connector feed.adsb.fi,30004,beast_reduce_plus_out --net-connector in.adsb.lol,30004,beast_reduce_plus_out,uuid=d5fa1765-183b-4227-998a-9e260d7a2f40 --net-connector 127.0.0.1,30154,beast_in --net-connector=feed.adsb.one,64004,beast_reduce_plus_out"
JSON_OPTIONS="--write-json /run/readsb --json-location-accuracy 2"
EOF
got=$(parse_readsb_connectors "$tmp")
want="adsb feed.adsb.fi 30004 beast_reduce_plus_out
adsb in.adsb.lol 30004 beast_reduce_plus_out d5fa1765-183b-4227-998a-9e260d7a2f40
adsb feed.adsb.one 64004 beast_reduce_plus_out"
check "readsb connectors" "$got" "$want"
rm -f "$tmp"

# ultrafeeder: ULTRAFEEDER_CONFIG with ; and newlines, an input, mlat with extra fields
cfg='adsb,feed.adsb.fi,30004,beast_reduce_plus_out;
 mlat,feed.adsb.fi,31090,uuid=d5fa1765-183b-4227-998a-9e260d7a2f40;
adsb,in.adsb.lol,30004,beast_reduce_plus_out,uuid=d5fa1765-183b-4227-998a-9e260d7a2f40;mlat,in.adsb.lol,31090,uuid=d5fa1765-183b-4227-998a-9e260d7a2f40,--privacy;
adsb,127.0.0.1,30105,beast_in;
mlathub,piaware,30105,beast_in'
got=$(parse_ultrafeeder_config "$cfg")
want="adsb feed.adsb.fi 30004 beast_reduce_plus_out
mlat feed.adsb.fi 31090 - d5fa1765-183b-4227-998a-9e260d7a2f40
adsb in.adsb.lol 30004 beast_reduce_plus_out d5fa1765-183b-4227-998a-9e260d7a2f40
mlat in.adsb.lol 31090 - d5fa1765-183b-4227-998a-9e260d7a2f40"
check "ultrafeeder config" "$got" "$want"

# feed lines to [[feed]] blocks, grouped by host
got=$(printf '%s\n' "$want" | feeds_to_toml)
want2='
[[feed]]
name = "adsb.fi"
adsb = "feed.adsb.fi:30004"
mlat = "feed.adsb.fi:31090"
uuid = "d5fa1765-183b-4227-998a-9e260d7a2f40"

[[feed]]
name = "adsb.lol"
adsb = "in.adsb.lol:30004"
mlat = "in.adsb.lol:31090"
uuid = "d5fa1765-183b-4227-998a-9e260d7a2f40"'
check "feeds to toml" "$got" "$want2"

# keys
if is_uuid d5fa1765-183b-4227-998a-9e260d7a2f40 && ! is_uuid d5fa1765 && ! is_uuid d5fa1765-183b-4227-998a-9e260d7a2f4g; then
    echo "ok   is_uuid"
else
    echo "FAIL is_uuid"; fails=$((fails + 1))
fi
k=$(new_key)
if is_uuid "$k"; then echo "ok   new_key"; else echo "FAIL new_key: $k"; fails=$((fails + 1)); fi

[ "$fails" = 0 ] && echo "all passed" || { echo "$fails failed"; exit 1; }
