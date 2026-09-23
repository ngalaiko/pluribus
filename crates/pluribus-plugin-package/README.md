# pluribus-plugin-package

Loads manifests, checks schemas and component digests, validates WIT compatibility, and builds component packages with `pluribus-package`.

Operates on package contents. Remote resolution and operator configuration belong to the CLI; execution belongs to the Wasm runtime.

[Package contract](../../docs/plugins/package.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-plugin-package
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
