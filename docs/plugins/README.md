# Plugin author specification

Status: draft for ABI `pluribus:plugin@1.0.0`

This is the public contract between Pluribus and plugin authors. A plugin is a
WebAssembly Component plus a manifest and configuration schema. The runtime
calls it only through WIT.

The words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are normative.

## The shape of a plugin

A plugin exports three functions and nothing else:

```wit
init:   func(context: context, config: json)        -> result<outcome, error>;
handle: func(context: context, events: list<event>) -> result<outcome, error>;
stop:   func(context: context, deadline-at-ms: s64) -> result<outcome, error>;
```

Its role is not an interface. A plugin that answers `capability.requested` for
`shell.execute` is a shell provider; one that answers `model.requested` is a
model provider. What it handles and what it may emit are declared in its
manifest and enforced by the core.

Everything a plugin decides or produces is an event. Bytes move over eight host
imports: `events`, `state`, `blobs`, `reader`, `writer`, `http`, `socket`, `credentials`. An
interface is a direct import only when it moves bytes, needs a secret, or must
be polled mid-call.

## Read in this order

1. [Package and manifest](package.md)
2. [ABI and host imports](abi.md)
3. [Events](events.md)
4. [Credentials](credentials.md)
5. [Lifecycle and upgrades](lifecycle.md)
6. [Security](security.md)
7. [Rust authoring](rust.md)
8. [Conformance](conformance.md)

For endpoint access, see [byte channels](stream.md).

The canonical WIT package is [`wit/`](../../wit). The manifest schema is
[`plugin-manifest.schema.json`](../../schemas/plugin-manifest.schema.json).
The [`echo` plugin](../../plugins/echo) is the executable Rust reference
package; [`shell`](../../plugins/shell) is the reference for a local endpoint.

## Terms

- **Plugin**: the installable package.
- **Component**: its `.wasm` Component Model binary.
- **Instance**: one configured runtime installation of a plugin.
- **Host import**: a Pluribus function the component may call.
- **Capability**: an operation subject to core authorization.
- **Grant**: standing authority to use a capability under constraints.
- **Delivery**: one `handle` call and the events it carries.
- **Outcome**: the events, state mutations, and cursor advance one call
  returns, committed as a single transaction.

“Component” and “plugin” are not synonyms. One component binary is part of one
plugin package. The same plugin may have many configured instances.

## Sources of truth

Conflicts are resolved in this order:

1. WIT types and function signatures.
2. The manifest JSON Schema.
3. This specification.
4. Examples.

WIT defines shape, not behavior or permission. This specification defines
behavior. The core decides permission.

## ABI scope

ABI `1.0.0` defines:

- one world, `plugin`;
- one export, `lifecycle`;
- eight host imports: `events`, `state`, `blobs`, `reader`, `writer`, `http`,
  `socket`, `credentials`;
- the event vocabulary a plugin may consume and emit;
- the manifest fields that declare what a plugin offers.

ABI `1.0.0` uses synchronous WIT functions. The Rust host may suspend them
without blocking its executor. Plugins MUST NOT require WASI 0.3 native
`async`, `stream`, or `future` types. Explicit cursors, chunks, and handles
carry asynchronous work across the ABI.

## Non-goals

The ABI does not provide:

- a native Rust ABI;
- ambient WASI filesystem, sockets, environment, processes, or clocks;
- direct secret reads;
- background threads that survive an exported call;
- exactly-once external effects;
- confidentiality enforced by an LLM;
- package signing or a public registry in the MVP;
- MCP compatibility.

## Background

WIT describes Component Model imports and exports but not their behavior.
Worlds collect the imports a component requires and exports it provides. See
the [Component Model WIT reference](https://component-model.bytecodealliance.org/design/wit.html),
[worlds](https://component-model.bytecodealliance.org/design/worlds.html), and
[component composition](https://component-model.bytecodealliance.org/composing-and-distributing/composing.html).
