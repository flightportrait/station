# stationd

stationd is the Station feeder runtime. It reads one configuration
file, refuses to start until the file is clean (every problem is a
sentence that says what to fix), supervises the receiver stack, and
serves one status document with one page reading it.

The stack it supervises: the radio ([rx](https://github.com/flightportrait/rx),
with readsb as its fallback when a dongle is local), and
[mlatc](https://github.com/flightportrait/mlatc), the multiplexing MLAT
client: one process, one Beast decode, every configured MLAT server. A
child that dies is restarted with backoff; its output flows through the
stationd journal with a name prefix. SIGTERM and Ctrl-C stop everything,
children included.

The status page at `/` shows the station's numbers (aircraft now,
message rate, today against yesterday, farthest heard) and its feeds'
MLAT sync state. A healthy page carries no sentences; diagnostics
appear only when a rule fires, each one a plain sentence with an
action. The MLAT rules triangulate: one server rejecting the station
is that server's problem, every server rejecting it means the
station's own position or clock is wrong.

Setup happens in a browser. Started with no configuration file,
stationd serves a wizard on the LAN (the terminal prints the URL and a
QR code): name, antenna position on a map (ground elevation fills in
from the position), frame source, and an aggregator list:
FlightPortrait, adsb.lol, adsb.fi, adsb.win, or anywhere else. On
submit the same validation sentences run, station.toml is written, and
the process continues into normal operation; the same URL becomes the
status page. `stationd --init` is the terminal equivalent for SSH.

## Install

On a fresh Debian-family machine (Raspberry Pi OS Lite included):

```sh
curl -fsSL https://flightportrait.com/station/install.sh | sh
```

The installer says what it does before each step, downloads the three
release binaries into `~/station`, installs the service, and opens the
setup wizard. On a machine that already runs a receiver it stops first
and offers `--add` (leave it, print the one line that makes it feed
FlightPortrait) or `--replace` (import its feeds and keys, then take
over). `--print` shows the manual path. [docs/PI.md](docs/PI.md) has
the details and the hand-built path.

## Run from source

```sh
cargo build --release
./target/release/stationd --config station.toml   # no file: setup mode
curl http://127.0.0.1:8654/status.json
```

`station.example.toml` documents every field. `--check` validates the
configuration and exits.

## What leaves the machine

Frames go to the aggregators you tick, each with the station key you
chose to give it. The setup page, in your browser, asks three public
services on your behalf and stores nothing there: OpenFreemap for map
tiles, Nominatim (OpenStreetMap) when you type an address, and
Open-Meteo for the ground elevation at the pin; the coarse first map
position comes from ipwho.is. The status page is served on the LAN
only, without authentication, like a router's first-run page.

## Design rules

- Silent degradation is a bug. A configuration problem stops the start
  and produces a sentence a person can act on.
- The daemon publishes machine-readable state; anything that draws
  lives elsewhere.
- rx and readsb stay untouched; stationd only runs them.

## License

AGPL-3.0-or-later ([LICENSE-AGPL](LICENSE-AGPL)). MapLibre GL (BSD-3)
is vendored in `src/vendor/` with its licence; see [NOTICE](NOTICE).
