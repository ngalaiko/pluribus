# pluribus-cli

Provides the `pluribus` binary: `init`, `run`, `logs`, `stop`, and `plugins list|auth|install`.

`plugins list` reads configured instances without loading packages. Directory flags
are independent; `--help` displays their platform defaults.

Wires packages, stores, transports, runtime, and router. Resolves operator configuration and platform paths; plugin protocol logic belongs in plugins.

[Installation](../../docs/installation.md), [operations](../../docs/operations.md), [instances](../../docs/plugin-instances.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-cli
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
