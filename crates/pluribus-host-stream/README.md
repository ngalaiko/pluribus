# pluribus-host-stream

Connects to granted Unix and TLS endpoints, authenticates peers, and enforces transfer and connection limits. Includes native IPC helpers.

Implements core stream contracts. Plugins own application protocols such as IMAP and SMTP; the host owns TLS and STARTTLS transport establishment.

[Stream contract](../../docs/plugins/stream.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-host-stream
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
