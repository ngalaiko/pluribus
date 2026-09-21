# Pluribus plugin SDK

Guest bindings and helpers for the `pluribus:plugin/plugin@3.0.0` world.

Add a path dependency from a plugin alongside a Pluribus checkout:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
pluribus-plugin-sdk = { path = "../pluribus/crates/pluribus-plugin-sdk" }
```

The crate generates bindings from the checkout's canonical `wit/` directory.
Plugins use these types and `export!`; they do not generate a second copy.

- `pluribus::plugin`: host imports and shared types.
- `wasi`: WASI imports.
- `exports::pluribus::plugin::lifecycle`: `Guest`, `Context`, and `Outcome`.
- `serve`: dispatch, commit, reject, and checkpoint handling for handler-only plugins.
- `http`: streaming HTTP responses and bounded inline exchanges.
- `socket::Socket`: asynchronous reads and writes to granted endpoints.

A handler-only plugin:

```rust,no_run
use pluribus_plugin_sdk::{exports, pluribus, serve};
use exports::pluribus::plugin::lifecycle::{Context, Guest, Outcome};
use pluribus::plugin::{runtime, types::{Error, Event}};

struct Plugin;

impl Guest for Plugin {
    async fn run(context: Context, _config: Vec<u8>) -> Result<(), Error> {
        runtime::ready(vec![], vec![]).await?;
        serve::<Self>(context).await
    }

    async fn handle(_context: Context, events: Vec<Event>) -> Result<Outcome, Error> {
        Ok(Outcome {
            events: vec![],
            mutations: vec![],
            checkpoint: events.last().map(|event| event.sequence),
        })
    }

    fn stop(_context: Context, _deadline_at_ms: i64) -> Result<Outcome, Error> {
        Ok(Outcome { events: vec![], mutations: vec![], checkpoint: None })
    }
}

pluribus_plugin_sdk::export!(Plugin);
```

Source plugins own their wait loop and use `runtime` directly. The SDK does
not grant authority; manifests and operator grants control every host call.
Build with `--target wasm32-unknown-unknown` and package the resulting module
with `pluribus-package`.
