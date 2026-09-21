# Rust authoring

Rust is the reference guest language. Other languages are valid if they produce the same Component Model interfaces.

## Tools

The repository reference path uses stable Rust, `wit-bindgen`, and the `wasm32-unknown-unknown` target:

```sh
rustup target add wasm32-unknown-unknown
```

`cargo-component` is also valid. Pin it in CI and upgrade deliberately. Its [project documentation](https://github.com/bytecodealliance/cargo-component) describes installation and generated bindings.

Use `wasm-tools` to inspect the built component:

```sh
wasm-tools validate --features component-model plugin.wasm
wasm-tools component wit plugin.wasm
```

## Project layout

```text
battery-plugin/
├── Cargo.lock
├── Cargo.toml
├── src/lib.rs
├── package/
│   ├── plugin.toml
│   └── config.schema.json
└── tests/
```

Use [`pluribus-plugin-sdk`](../../crates/pluribus-plugin-sdk) from a pinned Pluribus checkout. It owns the guest bindings and the HTTP, socket, and lifecycle helpers.

## World

The SDK targets the standard `plugin` world and exposes its bindings:

```rust
use pluribus_plugin_sdk::{exports, pluribus, wasi};
```

The `Guest` trait has three methods: `run`, `handle`, and `stop`.
For a custom world, generate separate bindings from the pinned WIT with
`wit-bindgen`; SDK helpers require the SDK's generated types.

The manifest `world` records the build target, and its `imports` must equal the
resulting component shape. Grants are still enforced per call: a narrow world
is defense in depth, not the permission boundary.

## Cargo manifest

```toml
[package]
name = "battery-plugin"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
pluribus-plugin-sdk = { path = "../pluribus/crates/pluribus-plugin-sdk" }
```

Pin the SDK checkout and commit `Cargo.lock`.

## Implementation shape

Generated bindings expose one guest trait per exported interface and functions for imported interfaces. A component normally uses one zero-sized type:

```rust
use pluribus_plugin_sdk::{export, exports, pluribus, serve};

use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::types::{Error, Event, Payload, Proposal};

struct Plugin;

impl Guest for Plugin {
    async fn run(context: Context, _config: Vec<u8>) -> Result<(), Error> {
        pluribus::plugin::runtime::ready(vec![], vec![]).await?;
        serve::<Self>(context).await
    }

    async fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        let mut proposals = Vec::new();
        let mut checkpoint = None;

        for event in &events {
            // Checkpoint every delivered event, including ignored ones, or the
            // batch is delivered again forever.
            checkpoint = Some(event.sequence);
            if event.event_type != "capability.requested" {
                continue;
            }
            proposals.push(Proposal {
                event_type: "capability.completed".into(),
                payload_schema: "dev.example.battery-result/1".into(),
                payload: Payload::Json(br#"{"percent":80}"#.to_vec()),
                idempotency_key: None,
                causation_id: Some(event.event_id.clone()),
            });
        }

        Ok(Outcome {
            events: proposals,
            mutations: Vec::new(),
            checkpoint,
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(empty())
    }
}

const fn empty() -> Outcome {
    Outcome {
        events: Vec::new(),
        mutations: Vec::new(),
        checkpoint: None,
    }
}

export!(Plugin);
```

Return an `Err` only when the whole delivery should fail and commit nothing. A
failure that answers a request is a `capability.failed` event, because a
returned error discards the result the requester is waiting for.

Use `pluribus_plugin_sdk::export!` for canonical ABI exports.

## Calling imports

Generated imports live under `pluribus::plugin`. For example `state.get`
becomes a Rust function similar to:

```rust
let stored = pluribus::plugin::state::get("offset")?;
```

Event timestamps support replayable decisions. Use `wasi::clocks` for live
time and waits.

Use generated types. Do not serialize WIT records yourself. JSON fields are the exception: serialize them to canonical UTF-8 JSON and validate against the declared schema.

## Rust constraints

- Do not use `std::fs`, `std::net`, `std::process`, environment variables, or system clocks.
- Avoid crates that initialize those facilities implicitly.
- Keep all durable data out of globals and linear memory.
- Avoid `unsafe`; it can violate component memory safety before the host boundary.
- Use bounded collections and streaming parsers.
- Convert panics into `plugin-error.internal` at role boundaries when possible.
- Never log request authorization or secret-bearing headers.

The host rejects unexpected WASI imports even if the code path is unused. Inspect the final component, not only source dependencies.

## Build

Use `sha256:dev` only in the source template. Build the core module, then let the package builder componentize, hash, and validate it:

```sh
cargo build --target wasm32-unknown-unknown --release
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  package target/plugins/battery target/wasm32-unknown-unknown/release/battery_plugin.wasm
```

The output directory contains `plugin.toml`, `plugin.wasm`, and the configuration schema. Keep the build reproducible from `Cargo.lock`.

Do not optimize by stripping Component Model type information; the installer needs it to validate imports and exports.

## Tests

Test business logic as native Rust where possible. Keep WIT adapters thin. Add component-level tests for:

- manifest and world agreement;
- denied optional imports;
- malformed JSON and provider data;
- cancellation and deadlines;
- repeated idempotency keys;
- restart with empty linear memory;
- state revision conflicts;
- blob chunk boundaries;
- source checkpoint replay;
- traps and resource limits;
- secret absence from output and logs.

Run the [conformance checklist](conformance.md) against the final release component.
