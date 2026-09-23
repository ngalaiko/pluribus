# pluribus-log

Installs the shared stderr tracing subscriber and reads `PLURIBUS_LOG`.

Binaries initialize logging once. Durable agent activity belongs in the event store; logs describe process wiring, transitions, and failures.

[Logging policy](../../docs/logging.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-log
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
