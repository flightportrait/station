# Running on a Raspberry Pi

Proven on a Pi 3B (aarch64), first done 2026-09-02. Until the flashable
image exists (S4), the manual path is:

1. Cross-build static binaries on any machine with Docker:
   `docker run --rm -v $PWD:/src -w /src rust:slim sh -c
   "rustup target add aarch64-unknown-linux-musl &&
   RUSTFLAGS='-C linker=rust-lld' cargo build --release
   --target aarch64-unknown-linux-musl"`.
   The same command in the mlat-bench workspace builds mlatc. Copy both
   binaries to the Pi; they have no dependencies.
2. Build readsb on the Pi (build-essential, librtlsdr-dev,
   libncurses-dev, zlib1g-dev, libzstd-dev, libusb-1.0-0-dev; then
   `make -j3 RTLSDR=yes` in the readsb checkout). One binary results.
3. Write station.toml (see station.example.toml). Without an SDR, readsb
   relays another receiver:
   `readsb --net-only --net-connector <source>,30005,beast_in
   --net-bo-port 30005 --write-json <state>/readsb --write-json-every 1`
   with `input.beast = "127.0.0.1:30005"` and `readsb_json` pointing at
   the same directory.
4. `stationd --check`, then install deploy/stationd.service and enable
   it. The service test: reboot the Pi; everything returns with no
   hands.

A Pi 3B runs the full stack in ~25 MB of RAM above readsb.
