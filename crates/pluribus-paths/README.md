# pluribus-paths

Resolves shared configuration, state, cache, and runtime directories using platform conventions.

Native binaries share these defaults. CLI overrides are applied by callers; platforms without a runtime directory use the state directory for endpoints.

[Operations](../../docs/operations.md#paths).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-paths
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
