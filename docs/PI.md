# Running on a Raspberry Pi

Proven on a Pi 3B (aarch64), first done 2026-09-02; the Station radio
(rx) has run in readsb's place on it since 2026-09-05. Until the
flashable image exists (S4), the path is `scripts/install.sh` on a fresh
Raspberry Pi OS Lite; the manual version of what it does:

1. `sudo apt-get install librtlsdr0` (rx links it), and blacklist the
   kernel's TV driver so the dongle is free at boot:
   `printf 'blacklist dvb_usb_rtl28xxu\nblacklist rtl2832\nblacklist rtl2830\n' | sudo tee /etc/modprobe.d/blacklist-rtlsdr.conf`.
2. Binaries into `~/station`: `rx` from
   github.com/flightportrait/rx/releases (`rx-aarch64-unknown-linux-gnu`),
   `mlatc` from github.com/flightportrait/mlatc/releases, and `stationd`
   cross-built on any machine with Docker:
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
   --net-bo-port 30005 --write-json <state>/readsb --write-json-every 1`
   with `input.beast = "127.0.0.1:30005"` and `readsb_json` pointing at
   the same directory.
5. `stationd --check`, then install deploy/stationd.service and enable
   it. The service test: reboot the Pi; everything returns with no
   hands.

A Pi 3B runs the full stack in about 30 MB of RAM; rx takes a fifth of
one core with collision recovery on.
