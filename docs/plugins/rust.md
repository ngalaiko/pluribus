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
├── wit/
│   └── deps/pluribus-plugin/
│       ├── types.wit
│       ├── host.wit
│       └── plugin.wit
├── package/
│   ├── plugin.toml
│   └── config.schema.json
└── tests/
```

Copy the exact ABI package into `wit/deps/pluribus-plugin` or consume the released WIT dependency by immutable digest. Do not track an unpinned branch.

## World

There is one world, so most plugins target it directly:

```rust
wit_bindgen::generate!({ generate_all,
    path: "wit",
    world: "plugin",
});
```

The generated `Guest` trait has three methods — `run`, `handle`, `stop` —
whatever the plugin does.

A plugin that wants fewer imports linked than the standard world offers may
declare its own:

```wit
package dev-example:battery@0.1.0;

world battery {
  import pluribus:plugin/runtime@2.0.0;
  import pluribus:plugin/events@2.0.0;
  import wasi:http/types@0.3.0;
  import wasi:http/client@0.3.0;
  export pluribus:plugin/lifecycle@2.0.0;
}
```

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
wit-bindgen = { version = "0.62", default-features = false, features = ["macros", "realloc", "async", "async-spawn"] }
```

Pin the guest binding version and `Cargo.lock`. Generated binding module paths may change between tool versions; the WIT interface does not.

## Implementation shape

Generated bindings expose one guest trait per exported interface and functions for imported interfaces. A component normally uses one zero-sized type:

```rust
wit_bindgen::generate!({ generate_all,
    path: "wit",
    world: "battery",
});

use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::types::{Error, Event, Payload, Proposal};

struct Plugin;

impl Guest for Plugin {
    async fn run(mut context: Context, _config: Vec<u8>) -> Result<(), Error> {
        use pluribus::plugin::runtime;
        runtime::ready(vec![], vec![]).await?;
        loop {
            match runtime::next().await? {
                runtime::Wake::Events(events) => match Self::handle(context.clone(), events).await {
                    Ok(outcome) => {
                        runtime::commit(&outcome.events, &outcome.mutations, outcome.checkpoint)?;
                        context.state_checkpoint = outcome.checkpoint.unwrap_or(context.state_checkpoint);
                    }
                    Err(error) => runtime::reject(&error)?,
                },
                runtime::Wake::Stop(_) => return Ok(()),
            }
        }
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

Treat this as structure, not a vendored binding. Generate it from the pinned WIT and toolchain. Do not hand-write canonical ABI exports.

## Calling imports

Generated imports live under `pluribus::plugin`. For example `state.get`
becomes a Rust function similar to:

```rust
let stored = pluribus::plugin::state::get("offset")?;
```

There is no clock import: the time an event was recorded is on the event, which
also makes a plugin's decisions replayable.

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
  package target/plugins/battery main=target/wasm32-unknown-unknown/release/battery_plugin.wasm
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
