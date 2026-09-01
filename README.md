# stationd

stationd is the Station feeder runtime, v0. It reads one configuration
file, refuses to start until the file is clean (every problem is a
sentence that says what to fix), supervises the receiver stack, and
serves one status document.

It supervises mlatc — the multiplexing MLAT client: one process, one
Beast decode, every configured MLAT server — and readsb when
`[programs].readsb` is set. A child that dies is restarted with backoff;
its output flows through the stationd journal with a name prefix.
SIGTERM and Ctrl-C stop everything, children included.

The status page at `/` shows the station's numbers — aircraft now,
message rate, today against yesterday, farthest heard — and its feeds'
MLAT sync state. A healthy page carries no sentences; diagnostics
appear only when a rule fires, each one a plain sentence with an
action. The MLAT rules triangulate: one server rejecting the station
is that server's problem, every server rejecting it means the
station's own position or clock is wrong.

Status: v0, private. Visual design and the diagnostic wording await
the founder pass; the setup flow comes next.

## Run

```sh
cargo build --release
./target/release/stationd --config station.toml
curl http://127.0.0.1:8654/status.json
```

`station.example.toml` documents every field. `--check` validates the
configuration and exits.

## Design rules

- Silent degradation is a bug. A configuration problem stops the start
  and produces a sentence a person can act on.
- The daemon publishes machine-readable state; anything that draws
  lives elsewhere.
- readsb stays upstream and untouched; stationd only runs it.

## License

AGPL-3.0-or-later ([LICENSE-AGPL](LICENSE-AGPL)).
