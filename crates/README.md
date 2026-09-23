# Host crates

The host assembles plugins, enforces authority, and records work. Reasoning lives
in [RLM](../plugins/rlm/README.md). See [architecture](../docs/architecture.md)
for the end-to-end flow.

| Crate | Responsibility |
| --- | --- |
| [pluribus-cli](pluribus-cli/README.md) | Application assembly, commands, enrollment, package resolution |
| [pluribus-core](pluribus-core/README.md) | Event, authority, storage, transport, and credential contracts |
| [pluribus-cognition](pluribus-cognition/README.md) | Event routing, admission, and provider coordination |
| [pluribus-runtime-wasm](pluribus-runtime-wasm/README.md) | Wasmtime execution, lifecycle, and host imports |
| [pluribus-plugin-package](pluribus-plugin-package/README.md) | Package validation and component packaging |
| [pluribus-plugin-bindings](pluribus-plugin-bindings/README.md) | Host bindings generated from WIT |
| [pluribus-plugin-sdk](pluribus-plugin-sdk/README.md) | Guest bindings and Rust plugin helpers |
| [pluribus-model](pluribus-model/README.md) | Canonical model request, completion, and stream payloads |
| [pluribus-store-sqlite](pluribus-store-sqlite/README.md) | Events, state, delivery transactions, credentials, and search |
| [pluribus-store-fs](pluribus-store-fs/README.md) | Content-addressed blobs |
| [pluribus-host-http](pluribus-host-http/README.md) | Granted outbound HTTP |
| [pluribus-host-stream](pluribus-host-stream/README.md) | Granted Unix/TLS byte streams |
| [pluribus-paths](pluribus-paths/README.md) | Shared platform directories |
| [pluribus-log](pluribus-log/README.md) | Shared stderr logging |

The CLI wires concrete stores and transports into the runtime and router.
Storage and transport implementations depend on core contracts. Host bindings
and the guest SDK share [WIT](../wit), while [schemas](../schemas) describe JSON
payloads and manifests.

Run commands from the repository root; see [development](../docs/development.md).
