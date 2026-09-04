# Package and manifest

## Layout

Packages load from a directory or a pinned `.tar.gz` archive. Remote and
`file://` references use `{ "url": "...", "sha256": "..." }` in instance config;
see [installation](../release-design.md). Archives contain this layout at their root:

```text
example-plugin/
├── plugin.toml
├── config.schema.json
├── components/
│   ├── receive.wasm
│   └── send.wasm
├── schemas/
└── flows/
```

Paths are relative to the package root. Absolute paths, `..`, symlinks, and
platform-specific separators are rejected. Every referenced binary, schema,
and flow must exist.

## Manifest

`plugin.toml` is UTF-8 TOML validated against
[`plugin-manifest.schema.json`](../../schemas/plugin-manifest.schema.json).
A manifest declares either one `[component]` or named `[components.<name>]` tables. The forms cannot be mixed. Single-component plugins need no name such as `main`.

A single-component manifest uses:

```toml
manifest_version = 1
abi = "pluribus:plugin@1.0.0"
id = "dev.example.echo"
name = "Echo"
config_schema = "config.schema.json"

[defaults]
label = "Echo"

[component]
world = "pluribus:plugin/plugin@1.0.0"
component = "plugin.wasm"
digest = "sha256:dev"
imports = []
config_schema = "config.schema.json"
config_pointer = ""
```

`[defaults]` contains package configuration defaults. Instance values override
these recursively. Credentials can omit `components` when the package has one
component; multi-component packages name consumers explicitly. Credential
`enrollment_origins` declares allowed enrollment endpoints.

A multi-component manifest uses:

```toml
manifest_version = 1
abi = "pluribus:plugin@1.0.0"
id = "dev.example.telegram"
name = "Telegram"
config_schema = "config.schema.json"
license = "MIT"

[[credentials]]
id = "bot-token"
display_name = "Telegram bot token"
description = "Bot token issued by BotFather."
components = ["receive", "send"]
config_pointer = "/credential_handle"
input_schema = "schemas/credential.input.json"
flow_schema = "pluribus:credential/static-http@1"
flow = "flows/bot-token.json"

[components.receive]
world = "pluribus:plugin/plugin@1.0.0"
component = "components/receive.wasm"
digest = "sha256:dev"
imports = ["pluribus:plugin/state@1.0.0", "pluribus:plugin/blobs@1.0.0", "pluribus:plugin/http@1.0.0"]
config_schema = "config.schema.json"
config_pointer = ""
subscribes = ["timer.fired"]
emits = ["observation.received", "timer.set"]

[[components.receive.requested_capabilities]]
name = "net.http"
required = true
reason = "Poll Telegram updates"
constraints = { origins = ["https://api.telegram.org"], methods = ["GET", "POST"] }

[components.send]
world = "pluribus:plugin/plugin@1.0.0"
component = "components/send.wasm"
digest = "sha256:dev"
imports = ["pluribus:plugin/blobs@1.0.0", "pluribus:plugin/http@1.0.0"]
config_schema = "config.schema.json"
config_pointer = ""
emits = ["capability.completed", "capability.failed"]

[[components.send.provides]]
capability = "telegram.send-message"
description = "Send a message to a granted conversation."
arguments_schema = "schemas/send.arguments.json"
result_schema = "schemas/send.result.json"
constraints_schema = "schemas/send.constraints.json"
idempotency = "non-idempotent"

[[components.send.requested_capabilities]]
name = "net.http"
required = true
reason = "Send Telegram messages"
constraints = { origins = ["https://api.telegram.org"], methods = ["POST"] }
```

Each component has independent memory, state, delivery cursor, worker, and
host grants. A blocked receiver does not block the sender. Shared credentials
are declared once and explicitly assigned to component names.

A model provider declares its model configuration pointer on its component:

```toml
[components.main.model_provider]
models_pointer = "/models"
```

The pointer resolves inside that component's projected configuration. Model
entries are strings or `{id, features?, context_tokens?, max_output_tokens?}`.

### Fields

Package fields are `manifest_version` (exactly `1`), `id`, `name`,
`abi`, `config_schema`, `credentials`, `components`, and optional `license`.
One package ABI applies to every component; component overrides are rejected. Unknown keys are rejected.

| Component field | Meaning |
| --- | --- |
| `world` | Exported lifecycle world. |
| `component`, `digest` | Binary path and SHA-256 of its bytes. |
| `imports` | Callable Pluribus interfaces; must exactly match the binary. |
| `config_pointer` | Required JSON Pointer into package configuration; empty selects the root. |
| `config_schema` | Schema for the selected configuration. |
| `requires` | Names of required sibling components. |
| `provides`, `model_provider` | Capability or model routing declarations. |
| `subscribes`, `emits` | Delivered and permitted proposed event types. |
| `rebuilds` | Mutation events used to restore this component's projection. |
| `pinned_session` | Retain interpreter memory across activity deliveries. |
| `requested_capabilities` | Requested authority; never an automatic grant. |

RLM declares `components.cognition.requires = ["js"]` and embeds the JS binary.
Installation validates every component and configuration before activating any
worker. Selectors use package-instance/component, such as `telegram-1/receive`
and `telegram-1/send`.

For `[component]`, pass a module path without `name=`. For named components, supply every binding:

```sh
cargo run -p pluribus-plugin-package --bin pluribus-package -- \
  plugins/telegram target/plugins/telegram \
  receive=target/wasm32-unknown-unknown/release/pluribus_plugin_telegram_receive.wasm \
  send=target/wasm32-unknown-unknown/release/pluribus_plugin_telegram_send.wasm
```

## Identifiers

Plugin IDs match:

```text
[a-z0-9]+([.-][a-z0-9]+)*
```

Capability names use lowercase dotted segments, for example `telegram.send` or `battery.read`. A provider MUST namespace private capabilities under its plugin ID. Public capability names require coordination with Pluribus.

Instance, activity, event, and authority identifiers are host-assigned opaque
strings. Plugins MUST compare them byte-for-byte and MUST NOT parse them.

Event types use lowercase dotted segments. A type outside the core vocabulary
MUST be prefixed `plugin.<plugin-id>.`, so a plugin cannot shadow or extend a
core type.

## Declaring what a plugin does

Each component exports exactly `pluribus:plugin/lifecycle@1.0.0`. Any other export is
rejected. There is no role list, because there are no role interfaces: what a
component does is `provides`, `model_provider`, `subscribes` and `emits`.

Each component MUST declare at least one of `provides`, `model_provider`, or
`subscribes`. A component that declares none would never be delivered anything.

`subscribes = ["*"]` requests the whole event stream. It is intended for
cognition, which filters internally rather than enumerating types.

One package may contain several components. Telegram receives through
`receive` and provides send capabilities through `send`; their execution and
state remain independent.

Concrete Component Model binaries preserve interface shape but not the source
world name. The installer validates `world` syntax and proves the exact imports
and exports of the binary. It does not treat an embedded synthetic world label
as identity.

## Imports

The component MUST import only callable interfaces listed in `imports`. The manifest MUST list every callable Pluribus import encoded in the component. Type-only WIT dependencies such as `pluribus:plugin/types` are derived from the
component and omitted from the manifest.

Only the seven host interfaces are accepted: `events`, `state`, `blobs`,
`reader`, `writer`, `http`, `socket`. An import list is an upper bound, not a
grant: the core still denies a call the instance has no grant for.

Guest toolchains may remove unused world imports. List the imports present in the final component, not every import declared by its source world.

An import is necessary but insufficient for use. Installation grants and per-activity authority still apply. Importing `http` does not authorize a destination, and declaring a capability in `provides` does not authorize the requests it will receive.

Ambient WASI imports are rejected. This includes filesystem, sockets, environment, process, random, and clocks. Use Pluribus host interfaces.

## Configuration

Package configuration is validated against its outer schema, then each
component selects its `config_pointer` and validates that value against its own schema. The schema MUST use JSON Schema Draft 2020-12 and MUST reject unknown security-sensitive fields.

Configuration is scoped to one package instance; each component receives only
its selected value. Updating it restarts the package components.

Secret references are opaque handle strings in fields identified by the credential descriptor:

```json
{ "credential_handle": "telegram:primary" }
```

The string is an opaque handle, not secret material. A plugin passes it to `http.send` as `credential`, and the host injects the secret after its policy checks. A plugin MUST NOT accept plaintext secrets in ordinary configuration.

## Requested capabilities

Each request has:

- `name`: stable capability name;
- `required`: whether activation fails without a grant;
- `reason`: short operator-facing explanation;
- `constraints`: the narrowest useful constraint object.

Installation never grants a request automatically if it expands authority. Agent-authored upgrades activate automatically only when the effective grants do not expand.

Changing a request requires a source update; package identity is selected by source and hash, not an in-manifest version field. Removing a request does not revoke an existing grant; the operator or agent must change grants separately.

## Digests and publication

Each component `digest` covers its binary, not the manifest or documentation.
The package builder replaces template `sha256:dev` values with binary digests.
Installed packages require concrete digests.

Signing and registry trust are deferred. Operators trust the installed bytes and recorded digest. An update is a new installation decision, even when `id` and publisher are unchanged.

## JSON

Every WIT `json` value MUST be UTF-8 JSON in [RFC 8785 JSON Canonicalization Scheme](https://www.rfc-editor.org/rfc/rfc8785) form. The host rejects malformed or non-canonical bytes.

Schemas MUST be valid JSON Schema Draft 2020-12. Payload schema identifiers use an absolute URI or:

```text
plugin:<plugin-id>/<schema-name>/<integer-version>
```

Payload schema versions are independent of plugin, manifest, and ABI versions.

## Projection replay

`rebuilds` optionally lists exact mutation event types, also declared in
`subscribes` and `emits`. The host replays this instance's own mutations before
registering its capabilities. Replay commits state and a separate replay cursor;
request delivery progress stays unchanged. Handlers must consume each replay
batch without emitting events. Replay providers may import only `events` and
`state`. See [memory](../memory-plugin.md).
