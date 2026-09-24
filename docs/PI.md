# Running on a Raspberry Pi

Proven on a Pi 3B (aarch64), first done 2026-09-02; the Station radio
(rx) has run in readsb's place on it since 2026-09-05. Until a
flashable image exists, the path is `scripts/install.sh` on a fresh
Raspberry Pi OS Lite (64-bit: the releases are aarch64 and x86_64, and
the installer stops on a 32-bit system); the manual version of what it
does:

1. `sudo apt-get install librtlsdr0` (rx links it), and blacklist the
   kernel's TV driver so the dongle is free at boot:
   `printf 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830\n' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf`.
2. Binaries into `~/station`: `rx` from
   github.com/flightportrait/rx/releases (`rx-aarch64-unknown-linux-gnu`),
   `mlatc` from github.com/flightportrait/mlatc/releases, and `stationd`
   from github.com/flightportrait/station/releases
   (`stationd-aarch64-unknown-linux-musl`), or cross-built on any machine
   with Docker:
   `docker run --rm -v $PWD:/src -w /src rust:slim sh -c
   "rustup target add aarch64-unknown-linux-musl &&
   RUSTFLAGS='-C linker=rust-lld' cargo build --release
   --target aarch64-unknown-linux-musl"`.
3. readsb is the fallback radio, not required. To have it, build it on
   the Pi (build-essential, librtlsdr-dev, libncurses-dev, zlib1g-dev,
   libzstd-dev, libusb-1.0-0-dev; then `make -j3 RTLSDR=yes`), or run
   the installer with `--build-readsb`.
4. Write station.toml (the wizard or `stationd --init` does; see
   station.example.toml). With a dongle, `[programs].radio` is the rx
   command line and `[programs].readsb`, when present, the fallback; both
   take the same flags. `[results].beast_connect = "127.0.0.1:30004"`
   brings MLAT positions back into the radio. Without an SDR, the radio
   relays another receiver:
   `readsb --net-only --net-connector <source>,30005,beast_in
   --net-bo-port 30005 --write-json /run/station/readsb --write-json-every 1`
   with `input.beast = "127.0.0.1:30005"` and `readsb_json` pointing at
   the same directory.
5. `stationd --check`, then install deploy/stationd.service and enable
   it. The service test: reboot the Pi; everything returns with no
   hands.

## Installer options

`sh install.sh` on a machine that already runs a receiver (readsb,
dump1090-fa, PiAware, FR24, an adsb.im image, or ultrafeeder in docker)
stops before touching anything: the dongle can serve one program. It
offers two doors, as a question on a terminal, as flags otherwise:

- `--add`: install nothing; print the line that makes the existing
  software feed FlightPortrait, with a station key, and exit.
- `--replace`: stop and disable the existing receiver (its configuration
  stays where it is), install the Station, and carry its feeds and keys
  over into `~/station/station.imported.toml`, which the wizard and
  `stationd --init` start from (`--import`).
- `--print`: install nothing; print the manual path on one screen, for a
  machine where the script cannot run.
- `--station-key <uuid>` (or `STATION_KEY=` in the environment): use a
  key you kept instead of a new one. On a terminal the installer asks
  once before the wizard.
- `--build-readsb`: also build readsb from source as the fallback radio.

`stationd --init --import ~/station/station.imported.toml` and
`stationd --station-key <uuid>` are the same options on the daemon.

A Pi 3B runs the full stack in about 30 MB of RAM; rx takes a fifth of
one core with collision recovery on.

## The card

SD cards die from small writes, not age: a card can only erase in large
blocks, so a file rewritten every second costs many times its size in
wear, and a power cut during a write can corrupt the card's own map.
The rule the Station keeps is that nothing rewritten all day touches
the card while it runs:

- The Station radio serves `aircraft.json` over a socket
  (`--json-listen 127.0.0.1:30006`, `input.radio_json`): nothing is
  written anywhere. readsb, the fallback, can only write files, so its
  `aircraft.json` (every second) and mlatc's stats files (every 15 s)
  live in `/run/station`, which is RAM: the service unit's
  `RuntimeDirectory=station` creates it, the wizard writes that path,
  and stationd refuses to start when the path cannot be made. It warns
  on the page and in its journal when the configuration still points
  the radio at the card.
- `metrics.json`, the 48 h of history behind the page, is saved once
  an hour and at shutdown, to the disk before its name (fsync, then
  rename), so a power cut costs at most an hour of history and never a
  broken file.
- The page's footer shows what the card took since stationd started,
  read from the kernel's counter for the root disk. A station in good
  health shows well under 1 MB an hour; above 20 MB an hour a sentence
  names the rate so the writer (graphs, a heat map, a chatty log) can
  be found with `sudo iotop -ao`.

Raspberry Pi OS keeps the journal in RAM unless `/var/log/journal`
exists; leave it that way. High-endurance cards (the "Endurance" lines)
take ten times the writes of an ordinary one for a few dollars more and
are the right choice for a station that must not be touched again.

### Measuring it

The kernel counts every sector written to the card; field 7 of
`/sys/block/mmcblk0/stat` is sectors written since boot, 512 bytes
each. Two readings some minutes apart are the whole bench:

```sh
a=$(awk '{print $7}' /sys/block/mmcblk0/stat); sleep 600
b=$(awk '{print $7}' /sys/block/mmcblk0/stat)
echo "$(( (b - a) * 512 / 1024 / 1024 )) MB in 10 min"
```

The dogfood Pi 3B measured 62–79 MB an hour (1.5 GB a day) with the
radio's JSON on the card, before this rule. The page footer shows the
same counter since stationd started, so no shell is needed after the
first time; `/status.json` carries it as `card.written_bytes` and
`card.bytes_per_hour`.

### Moving a running station to the rule

A station installed before 0.1.4 has its JSON under `state/` on the
card. Three changes, then a restart:

1. The service gets the RAM directory, as a drop-in (or reinstall the
   unit from `deploy/stationd.service`):
   ```sh
   sudo mkdir -p /etc/systemd/system/stationd.service.d
   printf '[Service]\nRuntimeDirectory=station\n' \
     | sudo tee /etc/systemd/system/stationd.service.d/card.conf
   sudo systemctl daemon-reload
   ```
2. In `station.toml`, every `state/readsb` becomes `/run/station/readsb`
   (`readsb_json` and the `--write-json` of both program lines). With an
   rx that has `--json-listen` (0.1.4 and later), the radio line takes
   `--json-listen 127.0.0.1:30006` instead of `--write-json …
   --write-json-every 1`, and `[input]` gains
   `radio_json = "127.0.0.1:30006"`; the readsb line keeps its
   `--write-json`, since readsb can only write files.
3. `stationd --check`, then `sudo systemctl restart stationd`. The page
   footer should read a few hundred kB after an hour, with no sentence.
