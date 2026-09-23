# pluribus-store-fs

Stores immutable content-addressed blobs, bounds uploads, validates digests, and publishes completed objects atomically.

Implements the core blob contract. Events retain blob references; this store does not decide which events or memories an agent may read.

[Blob contract](../../docs/plugins/abi.md), [operations](../../docs/operations.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-store-fs
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
