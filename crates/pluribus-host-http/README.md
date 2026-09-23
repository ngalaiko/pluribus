# pluribus-host-http

Executes HTTP requests and streams under per-component grants, with address, origin, method, byte, and time checks.

Implements core HTTP contracts for the Wasm runtime. Inbound public HTTP belongs to the HTTP plugin and native listener.

[Security](../../docs/plugins/security.md), [HTTP plugin](../../plugins/http/README.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-host-http
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
