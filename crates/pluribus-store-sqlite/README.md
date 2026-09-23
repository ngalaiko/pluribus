# pluribus-store-sqlite

Implements event, state, delivery, and credential storage, schema migrations, and the FTS history projection.

Implements core traits using SQLite worker connections. Delivery events, mutations, and cursor advances share a transaction. Blob bytes live in the filesystem store.

[History search](../../plugins/rlm/history.md), [runtime storage boundaries](../../docs/runtime.md#boundaries).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-store-sqlite
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
