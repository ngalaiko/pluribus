# Development

Run commands from the repository root. `rust-toolchain.toml` pins Rust and the
Wasm target; `Cargo.lock` pins dependencies. Nix reads `flake.lock`.

## Build

```sh
nix-build -A pluribus
nix-build -A release
```

The first builds the binary and plugin packages. The second builds release
archives and their catalog. See [installation](installation.md#releases).
Use `nix-build` in a working tree: `nix build .#` copies the tree, including
`target`.

## Check

```sh
nix-shell
pluribus-sync-plugins
cargo fmt --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace -- --include-ignored
```

Sync rebuilds Wasm components and packages under `target/plugins`. Repeat it
after guest or WIT changes before running packaged integration tests. Loopback
fixtures require local socket access. Build dependencies may download on a fresh
machine; `CARGO_NET_OFFLINE=true` requires cached Cargo dependencies.

[CI](../.github/workflows/ci.yml) rejects compiler and Clippy warnings and runs
these checks, both Nix builds, and `nix flake check`. Ordinary tests use fixtures.
The live memory evaluation requires explicit environment variables; leave them
unset for the local suite.

## Focused checks

| Area | Command |
| --- | --- |
| Package execution and grants | `cargo test --locked -p pluribus-runtime-wasm -- --include-ignored` |
| Agent routing and RLM integration | `cargo test --locked -p pluribus-cognition -- --include-ignored` |
| Installation and enrollment | `cargo test --locked -p pluribus-cli -- --include-ignored` |
| Native bridge and shell executor | `python3 scripts/e2e-native.py` |

Crate and plugin READMEs identify local checks. See
[Memory evaluations](../plugins/memory/evaluations.md) for retention, compaction, restart,
and citation scoring. Scripted responses verify mechanisms, not model quality.

## Live checks

Use a separate agent directory and an explicit shell workspace. Follow the
[root quickstart](../README.md#try-it), setting all four directory flags on the runtime and auth commands. Point the
bridge and executor sockets at the runtime directory. Select test accounts and destinations
before sending messages or changing webhook configuration.

Verify a plain reply, a shell command in the selected workspace, and restart
recovery. Connector READMEs own their setup. Fixture success does not establish
provider availability, account permissions, or delivery through a real service.

## Documentation

Update the page that owns changed behavior. Keep commands runnable from the
stated directory and links relative to their document. See
[documentation ownership](README.md#maintain-these-docs).
