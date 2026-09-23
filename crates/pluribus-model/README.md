# pluribus-model

Defines serializable model requests, completions, features, tools, and stream payloads shared by the host and model plugins.

Payloads cross the component boundary as schema-identified JSON. This crate contains no provider transport or reasoning loop.

[Schemas](../../schemas), [model providers](../../plugins/README.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-model
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
