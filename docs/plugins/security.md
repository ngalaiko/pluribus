# Security

## Boundary

The runtime trusts the core, Wasmtime, configured host services, and installed plugin code within granted authority. It does not trust plugin correctness, plugin output, external content, model judgment, or remote peers.

The WebAssembly boundary limits direct access. It does not make a granted operation safe. A network grant can disclose data to its allowed destinations. A shell capability is equivalent to its operating-system account.

## No ambient authority

Plugins receive no ambient filesystem, sockets, DNS, environment, process, clock, random, terminal, or secret access. Components importing WASI or undeclared interfaces are rejected.

All effects use explicit host imports. The host checks the active invocation and authority on every sensitive call. A plugin cannot gain authority by:

- editing its manifest or configuration at runtime;
- passing an authority ID as data;
- calling another plugin;
- scheduling or retrying work;
- emitting an event;
- asking a model;
- installing a child plugin.

Delegation preserves or narrows authority. Denial and policy failure both deny.

## Imports, requests, and grants

Three separate layers apply:

1. **WIT import**: the binary can name the host interface.
2. **Manifest request**: the author explains intended use and constraints.
3. **Grant**: an operator or authorized agent permits use for an instance.

All three must permit the operation. A role export does not grant the provider authority to call itself or another provider.

The host may apply constraints narrower than the manifest request. Plugins MUST work with narrower optional grants and fail start when a required grant is absent.

## Authority and content

External content never creates authority. Source plugins provide external IDs and bytes; the core maps principals and standing policy.

Plugins MUST preserve provenance and distinguish:

- local principals from connector-native IDs;
- trusted origin from untrusted content;
- observed provider time from host commit time;
- instructions from quoted, forwarded, attached, or retrieved text.

Prompt injection is not solved by sandboxing. A model may still choose a harmful granted action. Keep grants narrow enough that mistaken judgment is tolerable.

## Secrets

Configuration contains opaque handles. The `credentials` import grants explicitly declared components access only to their bound, package-scoped records. Such plugins may process secrets in Wasm; credentials remain outside ordinary state, events, and durable blobs. Export bindings grant read-only access to selected values through `resolve-export`, without raw-record access. Other plugins use host-side credential injection.

Plugins MUST NOT:

- request plaintext credentials through ordinary configuration;
- place credentials in state, blobs, events, model input, errors, or logs;
- copy injected authorization into provider metadata;
- follow redirects or retry requests outside host HTTP policy;
- expose a credential handle as if it were a secret or user-facing identifier.

Handles are scoped by installation, agent, component instance, allowed operation, and destination. They are not bearer tokens, but disclosure still aids reconnaissance and SHOULD be avoided.

Enrollment input, OAuth responses, access tokens, and refresh tokens never cross the ABI. The host validates the plugin's declarative flow, serializes refresh, stores replacement credentials, and injects access tokens after destination checks.

Credential declarations are executable security policy. Installation MUST show their input fields, authorization endpoints, token endpoints, and injection destinations. Effective network grants must contain every endpoint and destination; declarations cannot expand them.

## Network

All network traffic uses `host-http`. The host resolves DNS and applies address policy for every connection and redirect. Plugins cannot override certificate validation, proxies, SNI, or resolved addresses.

Manifest requests SHOULD list exact HTTPS origins. Wildcard domains, arbitrary ports, plaintext HTTP, loopback, private networks, link-local addresses, and cloud metadata endpoints require explicit operator grants.

Plugins MUST bound remote input before parsing. Response bytes are untrusted even from an authenticated API.

## Webhooks

The host handles transport limits and routing. The source plugin handles provider authentication using host-mediated crypto.

A webhook plugin MUST:

- verify signatures before producing observations;
- verify timestamp or nonce freshness when the provider supports it;
- bind the signature to exact raw bytes;
- use a stable delivery ID for deduplication;
- reject unsupported algorithms;
- return no secret material in errors.

Authentication proves the provider, not instruction authority. Repository or account constraints still apply.

## State and blobs

State is private to one instance but not encrypted from the host operator. Plugins MUST assume administrators can inspect it.

Use revision-checked writes. Retrying a failed state mutation without rereading may overwrite concurrent state after a restart or migration.

Blob references are capabilities only within current visibility and authority. Knowing a digest does not guarantee read access. Plugins MUST not use content hashes as authentication tokens.

All durable plugin data is retained under the deployment retention policy. “Delete” from plugin state does not delete audit events or necessarily delete deduplicated blobs.

## Output and disclosure

Capability results, model deltas, context blocks, observations, custom events, errors, and logs can disclose data. Before returning data, a plugin must apply the capability's result schema and requested audience.

The core enforces declared audience and visibility, but cannot infer secrets hidden inside an allowed string. Do not rely on the model to redact.

Context providers cannot increase visibility. Model providers receive only context selected by the core, but the configured model service sees all submitted content.

## Resource abuse

The host limits memory, fuel, wall time, state, blobs, frames, and external calls. Plugins MUST still use bounded algorithms and streaming parsers.

Avoid:

- buffering blobs or HTTP bodies in linear memory;
- unbounded JSON recursion;
- regexes with uncontrolled worst-case behavior;
- recursive host calls without a decreasing bound;
- emitting progress for every byte or token;
- tight polling loops;
- retrying without host backoff.

Resource exhaustion is a plugin failure, not permission to discard audit data.

## Supply chain

ABI `2.0.0` has no signing authority or public registry. Operators review the manifest, digest, source, build provenance, and grants.

Authors SHOULD:

- publish source and reproducible build instructions;
- pin dependencies and commit lockfiles;
- minimize host imports and requested capabilities;
- document network destinations and retained data;
- include an SPDX license;
- publish checksums over the entire release artifact outside `plugin.toml`;
- treat dependency updates as code changes.

An unchanged plugin ID does not establish publisher identity. Every update is untrusted until validated and accepted.

## Agent-authored plugins

An agent may author, install, or update a plugin only within existing authority. Activation is automatic only when:

- imports do not add sensitive host operations;
- requested and effective grants do not expand;
- configuration references no new credential handle;
- network destinations do not expand;
- state migration succeeds;
- conformance checks pass.

The agent cannot modify core code, authority policy, credentials, grants, audit history, or runtime resource ceilings through a plugin.

## Reporting vulnerabilities

A report should include plugin ID and version, component digest, ABI version, manifest, minimal reproduction, required grants, and observed events. Never include live credentials or private audit payloads.
