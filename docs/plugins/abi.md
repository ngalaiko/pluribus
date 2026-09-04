# ABI and host imports

## WIT package

The ABI package is `pluribus:plugin@1.0.0` in [`wit/`](../../wit):

- [`types.wit`](../../wit/types.wit): shared values;
- [`host.wit`](../../wit/host.wit): host imports;
- [`plugin.wit`](../../wit/plugin.wit): the `lifecycle` export and the world.

There is one world, `plugin`, holding all five imports. Grants are enforced per
call, so an import present in the world may still return `permission-denied`.
A plugin MAY declare a narrower custom world for defense in depth; its manifest
records which world the component encodes.

The component MUST encode exactly one export, `lifecycle`, and its imports MUST
equal those the manifest declares. The host rejects unresolved imports,
unexpected exports, type mismatches, and ABI version mismatches before
instantiation.

`manifest.world` is not checked against the component: encoding renames the
component's own world to `root:component/root`, so the authored name does not
survive. Import and export set equality is the real check.

## Execution model

The host creates one component instance per configured plugin instance and
serializes exported calls to it. It does not re-enter an instance concurrently.

The host may suspend a synchronous WIT call while waiting for network or
storage I/O. This does not permit plugin work after the export returns.

Plugins MUST:

- return control while idle;
- keep durable state in `outcome.mutations` or blobs;
- treat instance memory as disposable;
- tolerate restart between any two exported calls;
- tolerate an empty state namespace at any time;
- stop before the delivery deadline.

A trap, panic, fuel exhaustion, memory exhaustion, or deadline interruption may
terminate the instance without calling `lifecycle.stop`.

### Pinned sessions

One exception to restart tolerance. A plugin whose continuation is a suspended
call stack — an interpreter holding a paused coroutine — cannot serialize it
into an event. Such a plugin sets `pinned_session` in its manifest. The host
then keeps its linear memory for the duration of one activity and routes that
activity's deliveries back to the same instance.

A pinned session is explicitly not durable. Losing the instance fails the
activity; it does not resume against a fresh one. For such a plugin the
guarantee weakens from "between any two exported calls" to "between any two
activities".

## Shared types

### `principal`

`principal.kind` is `human`, `agent`, `node`, `component`, or `external`. IDs
are opaque and scoped by the host. Plugins MUST NOT infer trust from an ID.

### `payload`

Small structured values use `payload.json`. Large or binary values use
`payload.blob`. JSON follows the canonical rules in
[Package and manifest](package.md#json).

`blob-ref.algorithm` MUST equal `sha256`. The digest is lowercase hexadecimal.
A reference is immutable. Missing or corrupt content is an error; plugins MUST
NOT invent replacement bytes.

### `chunk`

One `chunk { bytes, closed }` serves blob reads, HTTP response streams, and
local sockets. `closed` means no bytes follow the ones returned.

### `error`

| Code | Use |
| --- | --- |
| `invalid-argument` | Input violates the interface or its schema. |
| `not-found` | Named durable object does not exist. |
| `permission-denied` | Core policy rejected the operation. Retrying unchanged is pointless. |
| `unsupported` | Interface exists but this plugin mode does not implement the operation. |
| `conflict` | Revision, checkpoint, or state precondition failed. |
| `unavailable` | Dependency is temporarily unavailable. |
| `resource-exhausted` | A configured or hard limit was reached. |
| `cancelled` | The activity was cancelled. |
| `deadline-exceeded` | The activity deadline passed. |
| `internal` | Plugin or host invariant failed. |

`retryable` is advice, not an instruction. The core also considers idempotency,
attempt count, deadline, and authority. `message` and `details` are retained
and may reach a model or operator. They MUST NOT contain secrets.

An error returned from `handle` fails the whole delivery and commits nothing. A
failure that is *the answer to a request* is an event, not a returned error —
otherwise the requester waits forever for a result that was thrown away.

### `context`

Host-assigned facts about the current call. A plugin cannot forge or widen
them:

- `instance-id`: configured instance;
- `agent`: agent served by the call;
- `state-checkpoint`: sequence the state namespace is current as of. Zero means
  the namespace is empty and must be rebuilt;
- `depth`: recursion depth of the activity;
- `deadline-at-ms`: wall-clock deadline, if any.

Actor, authority, activity, correlation, and causation are on each delivered
event rather than in `context`, because they belong to the event, not the call.

## Host imports

Every host import is deny-on-error. The host records security-sensitive calls
as events.

### `events`

`append(proposal)` commits one event immediately and returns its sequence. An
idempotency key is required. Use it only for non-terminal progress; terminal
results belong in `outcome`. See [Events](events.md).

`get(event-id)` reads one event. `query(filter, limit)` reads matching events
in ascending sequence order, served from an index.

### `state`

State is namespaced by plugin instance. Plugins cannot address another
namespace.

State is a **rebuildable projection** of the event stream, not an audit record.
The host MAY discard a namespace. The plugin MUST rebuild it by replaying events. Do not keep here
anything the event stream cannot reproduce.

`get` and `scan` read. There is no write function: writes travel in
`outcome.mutations` so they commit with the events and the cursor. This also
fuses the old revision counter with the delivery checkpoint into one number.

Keys and values are opaque to the host. Authors own their schemas.

### `blobs`

`put(media-type, bytes)` writes a whole blob that fits one message.
`open-write`/`write`/`finish` write larger content; `write` accepts only the
next contiguous offset, or a byte-identical repeat of the previous chunk.

`finish` verifies expected size, computes SHA-256, commits atomically, and
returns an immutable reference. Equal bytes deduplicate. The host reaps
abandoned uploads at the end of the delivery.

`read` returns at most `max-bytes`; `closed` means the blob ends after the
returned bytes. A plugin MUST verify that cumulative length equals
`blob-ref.size`.

### `http`

`send` performs one policy-controlled request. Bodies are blobs. Header order
and duplicate names are preserved; names compare case-insensitively.

The host enforces:

- allowed schemes, origins, ports, and methods;
- DNS and resolved-address policy on every connection;
- redirect policy on every hop;
- request and response size limits;
- deadline and cancellation;
- credential destination constraints;
- removal of hop-by-hop headers.

The plugin MUST NOT set `authorization`, `cookie`, or any other header the
granted credential injects. The host injects credentials after policy checks.
Redirects never forward credentials to a different origin.

A credential may instead define a secret URL path prefix for APIs, including
Telegram, that authenticate in the path. Components never receive or construct
the resulting URL.

`sse` opens the response body as server-sent events and returns a `reader`. The
plugin owns record framing.

### `reader` and `writer`

The two halves of a byte channel. `reader.receive` returns at most `max-bytes`
and may wait up to `timeout-ms`; a timeout returns an empty chunk with
`closed = false`, not an error. `writer.send` writes the whole buffer or fails;
there is no host-side write buffer, so the plugin frames by choosing when to
call it.

Neither half has a `close`. Dropping a reader ends the transfer. Dropping a
writer half-closes the channel: the peer reads EOF while the read half stays
open, and an endpoint MAY treat that as cancellation. The transport closes when
both halves are gone, or when the delivery ends.

Which halves exist is a property of the opener, so a receive-only channel
cannot be written: `http.sse` returns a reader alone.

### `socket`

`connect` opens the one Unix socket endpoint granted to this instance and
returns both halves, sharing one transport and one byte budget. The host owns
transport, peer-credential verification, and byte and time budgets; the plugin
owns framing. No credential is injected: an endpoint grant conveys whatever
authority that endpoint exposes. See [byte channels](stream.md).

## Cancellation

There is no `cancelled()` poll. The core enforces cancellation and limits out
of band, through fuel and epoch interruption, so a plugin that ignores or
cannot observe cancellation is still stopped. Emergency stop does not depend on
plugin cooperation.

## Logging

There is no log import and no log event. Operational logs would be retained
forever in an append-only stream that never deletes, which is the wrong home
for debug output. The host traces ABI calls, which is what a plugin's behavior
can be reconstructed from.

## Limits

These are ABI `1.0.0` hard maxima. A deployment may configure lower limits.

| Item | Maximum |
| --- | ---: |
| Component binary | 64 MiB |
| Linear memory per instance | 512 MiB |
| Default linear memory | 128 MiB |
| One WIT string or byte list | 8 MiB |
| One JSON value | 1 MiB |
| State key | 512 UTF-8 bytes |
| State value | 1 MiB |
| State mutations per outcome | 256 |
| Events per outcome | 1,000 |
| Event page | 1,000 events |
| Blob chunk | 1 MiB |
| HTTP headers | 64 KiB total |
| HTTP redirects | 5 |
| HTTP timeout | 5 minutes |

Passing zero as a page or chunk limit returns an empty page. Passing a value
above a maximum is clamped unless a function states otherwise. Resource budgets
may stop work before these maxima.

## Encoding and memory

Generated Component Model bindings own canonical ABI allocation, lifting, and
lowering. Plugins MUST NOT depend on host pointer width, byte order, struct
layout, or allocator behavior.

Strings are Unicode scalar values encoded by the generated binding. IDs, keys,
header names, schema identifiers, URLs, media types, capability names, and
event types are further restricted to their documented ASCII syntax.
