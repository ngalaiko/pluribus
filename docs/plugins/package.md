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
abi = "pluribus:plugin@3.0.0"
id = "dev.example.echo"
name = "Echo"
config_schema = "config.schema.json"

[defaults]
label = "Echo"

[component]
world = "pluribus:plugin/plugin@3.0.0"
component = "plugin.wasm"
digest = "sha256:dev"
imports = []
config_schema = "config.schema.json"
```

`[defaults]` contains package configuration defaults. Instance values override
these recursively. Credentials can omit `components` when the package has one
component; multi-component packages name consumers explicitly.

A multi-component manifest uses:

```toml
manifest_version = 1
abi = "pluribus:plugin@3.0.0"
id = "dev.example.telegram"
name = "Telegram"
config_schema = "config.schema.json"
license = "MIT"

[[credentials]]
id = "bot-token"
display_name = "Telegram bot token"
description = "Bot token issued by BotFather."
components = ["receive", "send"]
access = true
input_schema = "schemas/credential.input.json"
flow_schema = "pluribus:credential/static-plugin@1"
flow = "flows/bot-token.json"

[components.receive]
world = "pluribus:plugin/plugin@3.0.0"
component = "components/receive.wasm"
digest = "sha256:dev"
imports = ["pluribus:plugin/state@3.0.0", "pluribus:plugin/blobs@3.0.0", "wasi:http/types@0.3.0", "wasi:http/client@0.3.0", "pluribus:plugin/credentials@3.0.0"]
config_schema = "config.schema.json"
subscribes = []
emits = ["observation.received"]

[[components.receive.requested_capabilities]]
name = "net.http"
required = true
reason = "Poll Telegram updates"
constraints = { origins = ["https://api.telegram.org"], methods = ["GET", "POST"] }

[components.send]
world = "pluribus:plugin/plugin@3.0.0"
component = "components/send.wasm"
digest = "sha256:dev"
imports = ["pluribus:plugin/blobs@3.0.0", "wasi:http/types@0.3.0", "wasi:http/client@0.3.0", "pluribus:plugin/credentials@3.0.0"]
config_schema = "config.schema.json"
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

The pointer resolves inside the full instance configuration. Model
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
| `config_schema` | Schema for the full instance configuration. |
| `requires` | Names of required sibling components. |
| `provides`, `model_provider` | Capability or model routing declarations. |
| `catalog_injection` | Opts a component into host cognition catalog injection at the declared config pointers. |
| `connector` | Declares an observation provider and the sibling component that replies for it. |
| `subscribes`, `emits` | Delivered and permitted proposed event types. |
| `rebuilds` | Mutation events used to restore this component's projection. |
| `pinned_session` | Retain interpreter memory across activity deliveries. |
| `requested_capabilities` | Requested authority; never an automatic grant. |

An observation component declares its connector independently of the package ID:

```toml
[components.receive.connector]
provider = "chat"
reply_component = "send"
```

`provider` must match the observation payload. `reply_component` names a component
in the same package; `""` names the unnamed single component. Omit it for an
input-only connector. The reply component's capabilities receive origin-scoped
grants; their constraint bindings must confine effects to that origin.

A component can opt into catalog injection:

```toml
[components.cognition.catalog_injection]
tools_pointer = "/tools"
components_pointer = "/components"
```

These distinct JSON Pointers name fields in package configuration. Parent objects
must exist. The host supplies installed capability descriptions and component
metadata before configuration validation. Package identity does not select this
behavior.

Capability `constraint_bindings` project actual request arguments into selectors
for operator allowlists. Declare them inside a `provides` entry:

```toml
constraint_bindings = { scopes = { parts = [{ pointer = "/scope" }], required = true } }
```

Each part concatenates its optional literal `prefix` and a string or number at
its JSON Pointer. An `optional = true` part is omitted only when absent; null or
non-scalar values fail. For example, a destination can combine `/chat_id` with
prefix `chat:` and optional `/message_thread_id` with prefix `:thread:`.

The default host policy requires every grant constraint key to be a nonempty
allowlist containing its projected selector. Unknown keys and missing required
bindings deny the request. `required = true` also denies grants that omit that
constraint key, including `{}`. An empty grant permits an unconstrained request
only when no binding is required. Bindings are selected from the routed provider's
manifest, never from caller-supplied metadata, and cannot add allowed values.
Plugins must act on the destination fields their bindings declare.

RLM declares `components.cognition.requires = ["repl"]` and embeds the REPL binary.
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

Each component uses the `plugin` world and exports only
`pluribus:plugin/lifecycle@3.0.0`, including async `run`; other exports are rejected. There is no role list, because there are no role interfaces: what a
component does is `provides`, `model_provider`, `subscribes` and `emits`.

Each component declares its capabilities, model services, event subscriptions,
or emitted events. Every component calls `runtime.ready` during startup; activation releases its async `run` loop.

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

Only the six Pluribus host interfaces are accepted: `events`, `runtime`,
`state`, `blobs`, `socket`, `credentials`, alongside the WASI interfaces of the
world. An import list is an upper bound, not a grant: the core still denies a
call the instance has no grant for.

Guest toolchains may remove unused world imports. List the imports present in the final component, not every import declared by its source world.

An import is necessary but insufficient for use. Installation grants and per-activity authority still apply. Importing `http` does not authorize a destination, and declaring a capability in `provides` does not authorize the requests it will receive.

Ambient WASI imports are rejected. This includes filesystem, sockets, environment, process, random, and clocks. Use Pluribus host interfaces.

## Configuration

Package configuration is validated against its outer schema, then each
component validates the full object against its own schema. The schema MUST use JSON Schema Draft 2020-12 and MUST reject unknown security-sensitive fields.

Configuration is scoped to one package instance; each component receives the full object. Updating it restarts the package components.

Secret references are opaque handles in `config.credentials`, keyed by the manifest credential ID:

```json
{ "credentials": {"bot-token": "telegram:primary"} }
```

The string is an opaque handle, not secret material. A component granted `access` reads the record behind it with `credentials.get(handle)`. A plugin MUST NOT accept plaintext secrets in ordinary configuration.

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

Credential declarations set `access = true` when their Wasm component reads
the sealed record. The host grants only that declaration's configured handle
to its listed components, under the package ID.
