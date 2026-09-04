# Operations

Halt cognition and cancel active shell commands, then restart:

```sh
pluribus stop
pluribus run --resume
```

The stop flag persists across restarts. Cancellation cannot undo effects already
admitted. Provider failures use backoff; trapped providers remain quarantined.

The data directory holds the SQLite database, configuration, content-addressed
blobs, and STOP marker. Credentials are in the database. Copy it with running
processes stopped. Package references remain those in the configuration; install
those packages on the restore host. A `bundled:<name>` reference resolves against
the restore host's own installation.

## Verification

```sh
nix-shell
pluribus-sync-plugins
cargo clippy --locked --workspace --all-targets
cargo test --locked --workspace
```

`.github/workflows/ci.yml` adds `--include-ignored` for the loopback fixtures. Everything needs the pinned Rust toolchain,
`wasm32-unknown-unknown`, and local socket access. Dependencies are locked; set
`CARGO_NET_OFFLINE=true` to require cached dependencies. Nothing makes model
requests, sends Telegram messages, or pushes.
