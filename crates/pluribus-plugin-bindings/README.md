# pluribus-plugin-bindings

Generates Wasmtime host bindings from the canonical plugin world in `wit/`.

The runtime implements these imports. Guest code uses the plugin SDK instead. Change WIT at its source; do not maintain a second interface definition.

[ABI](../../docs/plugins/abi.md), [SDK](../pluribus-plugin-sdk/README.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-plugin-bindings
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
