# Credentials

## Boundary

Provider-specific enrollment belongs to the plugin. Secret custody belongs to the host.

A plugin declares credentials in its manifest `[[credentials]]` entries. The management plane validates input and executes declarative flows or delegates `plugin@1` enrollment to Wasm. It stores secrets and uses the opaque handle from `config.credentials[credential.id]`. Missing bindings default to `<instance>:<credential-id>`.

Static HTTP and OAuth device flows execute in the host. A `plugin@1` flow
executes in its declared Wasm component. An `access = true` credential declaration
grants that component the `credentials` host interface for its bound handle.
Records are isolated by package ID and handle; other components have no access.
Plaintext secrets MUST NOT appear in ordinary configuration, events, or state.

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

`describe` MUST be deterministic and side-effect free. Descriptors are untrusted until validated. Credential IDs MUST be unique. Unknown flow schemas are rejected.

Discovery reads `plugin.toml` and the schema and flow files it names. No guest code runs during discovery. `input_schema` and `flow` are package-relative paths to JSON files.

Operators register packages, instance configurations, and grants in the [plugin-instance registry](../plugin-instances.md). `pluribus auth <instance>` accepts any instance whose manifest declares credentials, independent of its package ID or what else it does.

The host supports these MVP flows:

- `pluribus:credential/static-http@1`, defined by [`credential-static-http-1.schema.json`](../../schemas/credential-static-http-1.schema.json);
- `pluribus:credential/plugin@1`: the CLI stages input in the provider-scoped credential record and emits `credential.enrollment.requested` with an enrollment reference; the declared component replies with `credential.enrollment.started` and an approval URL. The core must be running.
- `pluribus:credential/oauth-device@1`, defined by [`credential-oauth-device-1.schema.json`](../../schemas/credential-oauth-device-1.schema.json).

Adding a flow is an ABI-adjacent host feature. Plugins MUST NOT encode provider logic in an undocumented JSON shape.

## Input schema

The root MUST be an object with `additionalProperties: false`. MVP fields are required or optional strings. `title` is the prompt label. `description` is help text. `default`, `minLength`, `maxLength`, `pattern`, and `enum` have their JSON Schema meanings.

`x-input-file: true` prompts for a file path and reads its contents. Noninteractive multi-field input is JSON on stdin.

Secret fields MUST set `writeOnly: true`. The host hides their terminal input and redacts it from errors and audit events. Refreshable credentials retain only inputs referenced by refresh or injection templates, inside credential storage.

The host validates the complete input object before executing a flow. Unsupported schema keywords fail enrollment; they are not silently ignored.

## Templates

A template is an ordered, non-empty `parts` array. Concatenation uses exact UTF-8 bytes. No escaping or delimiter substitution occurs.

Parts are closed tagged objects:

- `literal`: public text embedded by the plugin;
- `input`: one validated operator input field;
- `response`: a scalar at a JSON Pointer in a named earlier response;
- `accessToken`: the final OAuth access token;
- `jwtClaim`: a scalar JSON Pointer in the decoded access-token JWT payload.

The flow schema restricts which parts are legal at each location. Missing, non-scalar, malformed, or forward references fail closed. Template results have a host-defined 64 KiB maximum.

`jwtClaim` decodes base64url without accepting an invalid JWT. It does not verify the JWT signature: the token was obtained directly from the configured HTTPS token endpoint. Claims are used only to construct credential injection for the same flow.

## Static HTTP flow

`static-http@1` converts validated input into an HTTP credential. `http.origins` lists exact HTTPS origins. `headers` and `pathPrefix` define secret injection.

The host binds the stored credential to:

- the selected plugin instance;
- the descriptor and plugin version;
- origins present in both the flow and the effective `net.http` grant.

An origin outside the grant rejects enrollment. Header names and values use the HTTP validation rules. `pathPrefix` must begin with `/` and cannot contain a query, fragment, backslash, control byte, or whitespace.

## OAuth device flow

`oauth-device@1` is a bounded state machine:

1. Send `start`.
2. Render `prompt.verificationUri` and `prompt.userCode`.
3. Poll no faster than `intervalSeconds` and stop after `expiresInSeconds`.
4. Classify configured pending, slow-down, denial, and expiry responses.
5. If `exchange` exists, send it after successful polling; otherwise use the successful poll response.
6. Extract the token fields.
7. Render HTTP injection and seal the credential.

Requests are HTTPS `POST`. Bodies are JSON objects or URL-encoded forms whose fields are templates. Responses must be JSON objects no larger than 1 MiB. Redirects, proxies, ambient cookies, client-selected headers, and arbitrary response scripting are forbidden.

`pendingStatuses` handles providers that signal pending authorization only by status. For other failures, the host reads `errorPointer`; a JSON object value may expose its string `code`. Unknown errors fail as unavailable and include status only.

`intervalSeconds.default` and `expiresInSeconds.default` are mandatory bounds. A referenced provider value may replace the default only when it is a positive integer within the schema limit. Slow-down adds five seconds. The host enforces its own request, total-runtime, and cancellation limits.

The token response fields may be strings or positive integers as applicable. Empty access or refresh tokens are rejected. Expiry uses the host clock with overflow checks.

## Refresh

Refresh is a validated recipe, not a fixed form:

- `refresh.request`: fixed HTTPS POST URL, JSON/form template fields, optional fixed headers.
- `refresh.response`: access-token and optional rotated refresh-token JSON pointers.
- `expiry`: relative/absolute pointer sources with explicit units, optional JWT `exp`, explicit skew, and optional bounded refresh interval. The earliest available deadline wins. Missing or invalid expiry fails; replacements must outlive their refresh skew.
- `http`: injection templates re-rendered after each refresh.
- `accountPointer`: immutable account claim; required for JWT-derived injection. Changed identity requires reauthorization.
- `authFailure`: exact 401/403 status and optional JSON error-code matchers.
- `replay`: exact origin, method, and path authorized for one resend after matching rejection.

Refresh templates can use `token: "accessToken"` or `token: "refreshToken"`, literals, and retained inputs. Refresh and injection cannot depend on enrollment response objects. Tokens cannot construct endpoint URLs. The host accepts a rotated refresh token or retains the prior token when omitted, and re-renders every injection field afterwards. It validates current installation enrollment grants before refreshing and runtime injection grants before sending. Refresh claims are serialized per handle through storage, across processes.

`refresh.failure` optionally declares `retryableStatuses` (429/502/503/504) and `backoffSeconds` (1-3600). Declaring these statuses asserts that rejection cannot consume the refresh token. The default treats ambiguous failures as unknown outcomes. OAuth `invalid_grant`, `invalid_token`, and `unauthorized_client` require reauthorization. Error bodies remain private.

Ordinary HTTP and SSE opening share generation-aware recovery and the original deadline. A delayed rejection reuses a newer generation. Unsafe requests, established streams, and uncertain network outcomes never replay. WebSocket transport remains unsupported. The blocking HTTP API exposes a deadline, not a separate cancellation token.

## Migration and health

Static credentials remain unchanged. Adopt an installed declaration over a stored recipe explicitly:

```sh
pluribus auth <instance> --adopt-recipe
```

Adoption sends no network request. It requires matching provider, client, endpoint, account-derived injection, and scope. It uses a generation compare-and-swap; concurrent enrollment, revocation, or refresh wins. An incompatible stored recipe requires normal `pluribus auth <instance>` enrollment. Package updates never silently replace sealed recipes or widen scope.

Refresh health is usable, refresh due, refreshing, backoff, reauthorization required, or unknown refresh outcome. Claims and outcomes persist with generations and timestamps. An expired abandoned claim becomes unknown outcome; lease expiry never authorizes reuse of a possibly consumed rotating token.

## Network constraints

Every start, poll, exchange, refresh, and injection origin MUST be exact HTTPS. User information, fragments, non-default ports, wildcard hosts, and redirects are rejected in the MVP.

Enrollment endpoints must be contained by the effective management grant for the plugin. Injection origins must be contained by its runtime HTTP grant. The flow cannot create a grant.

## Storage and audit

The host stores:

- the opaque handle;
- owning plugin, descriptor, instance, and component principal;
- allowed injection origins;
- secret headers or path prefix;
- renewable token metadata;
- creation, replacement, refresh, use, revocation, and failure events.

Refresh audit payloads contain handles, generations, fixed outcome codes, and timestamps. They never contain input values, rendered templates, authorization codes, device codes, token responses, secret headers, or secret path bytes.

Enrollment replacement is atomic. A refresh with an uncertain remote outcome makes the credential unavailable until reauthorization; the host cannot assume its rotating token remains usable. Revocation removes local material even when provider-side revocation is unavailable. Uninstalling a plugin does not silently delete its credentials.

## Wasm exports

An instance's operator-owned `config.credential_exports` maps binding names to
`{provider, credential, export}` references. These grant the instance's components
read-only access to those exports through `credentials.resolve-export(binding)`.
A binding grants no access to the underlying record or other exports.

Providers publish `exports[name] = {value, expires_at_ms}` in their private record.
The host rejects missing values and values expiring within 30 seconds. Plugins
assign meaning to binding names: shell uses them as environment variable names
and sends values directly over its executor socket, outside the event log.

The `credentials` interface offers scoped get/compare-and-swap and HTTP
credential attachment. Clocks and secure randomness use WASI. Credential writes are atomic but independent of
event commits; plugins must make enrollment and token exchanges replay-safe.
WASI HTTP bodies remain transient unless the guest explicitly persists them.
The guest inline helper bounds credential exchanges to 1 MiB. Destination and
method grants apply to every request.

For `plugin@1`, the private record's `enrollment` field contains
`{id, input, expires_at_ms}`. The request event carries `{component, credential,
enrollment}`; it never carries input values. The plugin checks the reference and
expiry before consuming input, preserves unrelated credential fields, and removes
the input after validation. Successful results must remain replayable after an
event-commit failure. The CLI stages records with compare-and-swap.
