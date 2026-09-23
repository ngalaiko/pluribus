# pluribus-core

Defines events, principals, authority, grants, delivery transactions, state, blobs, credentials, and transport traits. Includes in-memory implementations for fixtures.

Concrete persistence and transports implement these traits. This crate does not run plugins or choose reasoning policy.

[Events and authority](../../docs/events-and-authority.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-core
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
