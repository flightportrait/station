# stationd

The Station feeder runtime. One binary that runs an ADS-B receiver
for you: it supervises the radio and the MLAT client, feeds the
networks you choose, and shows how the station is doing on one page.
Made for the [FlightPortrait network](https://flightportrait.com/network),
non-exclusive by design: adsb.lol, adsb.fi, adsb.win and any other
aggregator are one line each.

## Install

A Raspberry Pi (3B or newer) or any Debian-family machine, an RTL-SDR
dongle, an antenna:

```sh
curl -fsSL https://flightportrait.com/station/install.sh | sh
```

The installer says what it does before each step. It downloads three
release binaries into `~/station`, installs a systemd service, and
prints the address of the setup page. Open it on your phone: name the
station, put the antenna on the map, tick the networks to feed. Done.

Already running readsb, PiAware, FR24 or ultrafeeder? The installer
notices and asks: `--add` leaves it alone and prints the one line that
makes it feed FlightPortrait; `--replace` imports its feeds and keys
and takes over. `--print` shows the manual path.
[docs/PI.md](docs/PI.md) has the details.

## What it does

- Supervises [rx](https://github.com/flightportrait/rx), the Station
  radio, with readsb as fallback, and
  [mlatc](https://github.com/flightportrait/mlatc), one MLAT client
  for every server. A child that dies is restarted with backoff; all
  output lands in one journal.
- Validates `station.toml` before starting. Every problem is a
  sentence that says what to fix; a half-configured station never
  runs.
- Serves `/status.json` and a status page: aircraft now, message
  rate, today against yesterday, farthest heard, MLAT sync per feed.
  A healthy page has no prose. When something is wrong, one plain
  sentence says what and what to do about it.
- With no configuration file, serves the setup wizard instead;
  `stationd --init` does the same in a terminal over SSH.

## Run from source

```sh
cargo build --release
./target/release/stationd --config station.toml   # no file: setup mode
curl http://127.0.0.1:8654/status.json
```

`station.example.toml` documents every field; `--check` validates and
exits.

## What leaves the machine

Frames go to the networks you tick, each with the station key you
gave it. The setup page, in your browser, uses OpenFreemap for tiles,
Nominatim for address search, Open-Meteo for ground elevation, and
ipwho.is for a first rough map position; nothing is stored there. The
status page is LAN-only and unauthenticated, like a router's first-run
page.

## License

AGPL-3.0-or-later ([LICENSE-AGPL](LICENSE-AGPL)). MapLibre GL is
vendored under BSD-3; see [NOTICE](NOTICE).
