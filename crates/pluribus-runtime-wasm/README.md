# pluribus-runtime-wasm

Instantiates validated Wasm components, implements host imports, enforces grants and resource limits, and commits delivery outcomes.

Uses generated host bindings and injected core storage/transport services. Each component owns one mutable store; lifecycle calls for that component are serialized.

[Lifecycle](../../docs/plugins/lifecycle.md), [runtime](../../docs/runtime.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-runtime-wasm
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
