# Credentials

## Boundary

Provider-specific enrollment belongs to the plugin. Secret custody belongs to the host.

A plugin declares credentials in its manifest `[[credentials]]` entries. The management plane validates input and either seals it or delegates `plugin@1` enrollment to Wasm. It stores secrets and uses the opaque handle from `config.credentials[credential.id]`. Missing bindings default to `<instance>:<credential-id>`.

The host speaks no provider protocol and attaches nothing to an outgoing
request. A `plugin@1` flow executes in its declared Wasm component. A
`static-plugin@1` flow executes nowhere: the validated input becomes the
record, and the component reads it back. An `access = true` credential
declaration grants that component the `credentials` host interface for its
bound handle. Records are isolated by package ID and handle; other components
have no access. Plaintext secrets MUST NOT appear in ordinary configuration,
events, or state.

A component that reads a record holds the secret in its own memory and
therefore holds whatever authority the secret carries. Placing the secret in a
header, a URL path, or a protocol frame is the component's work, as is
refreshing it.

## Descriptor

Each descriptor contains:

| Field | Contract |
| --- | --- |
| `id` | Stable within the plugin version; lowercase dotted or hyphenated ASCII. |
| `display-name` | Short operator-facing name. |
| `description` | Exact account and authority being granted. |
| `config-pointer` | RFC 6901 pointer to the string handle in instance configuration. |
| `input-schema` | Draft 2020-12 schema for operator input. |
| `flow-schema` | Exact supported flow identifier. |
| `flow` | One JSON value satisfying that flow schema. |

Descriptors are untrusted until validated. Credential IDs MUST be unique. Unknown flow schemas are rejected.

Discovery reads `plugin.toml` and the schema and flow files it names. No guest code runs during discovery. `input_schema` and `flow` are package-relative paths to JSON files.

Operators register packages, instance configurations, and grants in the [plugin-instance registry](../plugin-instances.md). `pluribus auth <instance>` accepts any instance whose manifest declares credentials, independent of its package ID or what else it does.

The host supports these flows:

- `pluribus:credential/static-plugin@1`, defined by [`credential-static-plugin-1.schema.json`](../../schemas/credential-static-plugin-1.schema.json): the CLI validates input against the input schema and seals it as the record itself. Nothing is staged and no event is emitted, so the core need not be running. The declaration MUST set `access = true`; a record nothing can read is an unusable secret at rest.
- `pluribus:credential/plugin@1`: the CLI stages input in the provider-scoped credential record and emits `credential.enrollment.requested` with an enrollment reference; the declared component replies with `credential.enrollment.started` and `{url, userCode?}`. The core must be running. The CLI displays both fields when present.

Adding a flow is an ABI-adjacent host feature. Plugins MUST NOT encode provider logic in an undocumented JSON shape.

## Input schema

The root MUST be an object with `additionalProperties: false`. Fields are required or optional strings. `title` is the prompt label. `description` is help text. `default`, `minLength`, `maxLength`, `pattern`, and `enum` have their JSON Schema meanings.

`x-input-file: true` prompts for a file path and reads its contents. Noninteractive multi-field input is JSON on stdin.

Secret fields MUST set `writeOnly: true`. The host hides their terminal input and redacts it from errors and audit events.

The host validates the complete input object before sealing or staging it. Unsupported schema keywords fail enrollment; they are not silently ignored.

## Rotation

A plugin that holds a renewable secret rotates it itself. It reads the record
with `credentials.get`, exchanges the renewal against an origin its `net.http`
grant allows, and seals the replacement with
`credentials.compare-and-swap(handle, expected, value)` so a concurrent writer
cannot be overwritten. A lost swap adopts whatever the winner stored.

Credential writes are atomic but independent of event commits, so a plugin
must make enrollment and token exchanges replay-safe. Re-running
`pluribus auth <instance>` replaces the whole record rather than merging into
it.

## Network constraints

Every enrollment and rotation origin MUST be exact HTTPS and MUST be contained
by the component's runtime HTTP grant. User information, fragments,
non-default ports, wildcard hosts, and redirects are rejected. The flow cannot
create a grant.

## Storage and audit

The host stores the opaque handle, the owning package ID, and the record bytes. It reads and writes them only for components the manifest grants access.

Enrollment replacement is atomic. Revocation removes local material even when provider-side revocation is unavailable. Uninstalling a plugin does not silently delete its credentials.

## Upgrade from legacy storage

The schema 11 upgrade converts recognized legacy OpenRouter, Telegram, and
Codex records into plugin-scoped records under the same handles. Existing
plugin records are retained. An unknown or malformed legacy record aborts the
migration transaction; it is not silently discarded. Back up the state
directory before upgrading. If schema 11 has already completed and a record
was dropped, restore that backup or enroll the credential again.

## Wasm exports

An instance's operator-owned `config.credential_exports` maps binding names to
`{provider, credential, export}` references. These grant the instance's components
read-only access to those exports through `credentials.resolve-export(binding)`.
A binding grants no access to the underlying record or other exports.

Providers publish `exports[name] = {value, expires_at_ms}` in their private record.
The host rejects missing values and values expiring within 30 seconds. Plugins
assign meaning to binding names: shell uses them as environment variable names
and sends values directly over its executor socket, outside the event log.

The `credentials` interface offers scoped get and compare-and-swap. Clocks and
secure randomness use WASI. WASI HTTP bodies remain transient unless the guest
explicitly persists them. The guest inline helper bounds credential exchanges
to 1 MiB. Destination and method grants apply to every request.

For `plugin@1`, the private record's `enrollment` field contains
`{id, input, expires_at_ms}`. The request event carries `{component, credential,
enrollment}`; it never carries input values. The plugin checks the reference and
expiry before consuming input, preserves unrelated credential fields, and removes
the input after validation. Successful results must remain replayable after an
event-commit failure. The CLI stages records with compare-and-swap.
