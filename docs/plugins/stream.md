# Byte channels

Implemented for ABI `pluribus:plugin@3.0.0`. `socket.connect` exchanges bytes
with the one configured Unix socket endpoint granted to an instance. HTTP uses
WASI request/response body streams. Core owns transport and peer verification;
the plugin owns framing and protocol semantics.

## Interface

```wit
interface socket {
  use types.{error};

  connect: async func(outgoing: stream<u8>)
    -> result<tuple<stream<u8>, future<result<_, error>>>, error>;
}
```

`connect` names no endpoint: an instance has at most one granted. The plugin
passes the read end of a stream it writes to, and its bytes reach the peer. The
returned stream carries the peer's bytes. The future resolves after that stream
ends, `ok` on clean EOF and carrying the transport error if one ended it.

Closing the outgoing writer half-closes: the peer reads EOF while the incoming
stream stays open, and an endpoint MAY treat that as cancellation — the shell
executor does — so hold the writer until the exchange is done. Dropping the
incoming stream stops reading. The transport closes when both directions are
finished. A send or read failure closes it.

Bytes are inline; plugins can use `blobs` to store content.

## Framing

A stream socket gives the peer no write boundaries. Framing is the plugin's
job: the shell, CLI, and HTTP plugins frame with newline-terminated JSON.

## Grant

Merge into the runtime configuration:

```json
{
  "plugin_instances": {
    "shell-1": {
      "package": "file:///opt/pluribus/plugins/shell",
      "config": {},
      "components": {
        "main": {
          "stream": {
            "socket": "/run/pluribus-workspace/executor.sock",
            "peer_uids": [1002],
            "max_bytes": 16777216,
            "max_timeout_ms": 300000
          }
        }
      }
    }
  },
  "capability_instances": [
    "shell-1/main"
  ],
  "trusted_capabilities": {
    "shell-1/main": [
      "shell.execute"
    ]
  }
}
```

| Field | Meaning |
| --- | --- |
| `socket` | Required absolute Unix socket path. |
| `peer_uids` | Required non-empty list of non-root UIDs allowed to answer the socket. Listing the runtime's own UID gives the endpoint the runtime's authority, so only an endpoint that cannot act with it, such as a terminal bridge, belongs there. |
| `max_bytes` | Positive byte budget combined across both directions of one connection. Defaults to 16 MiB. |
| `max_timeout_ms` | Bound on connection establishment, in milliseconds, 1–300000. Defaults to 300000. |

Each connection receives its own byte budget; this is not a delivery-wide
quota. Connecting requires an endpoint grant. The manifest must import
`pluribus:plugin/socket@3.0.0` and request `host.stream` with exactly
`constraints = { unrestricted = true }`. Describe the endpoint's authority in
the manifest. Without a granted endpoint `connect` returns `permission-denied`,
which the plugin turns into a `capability.failed` event rather than a trap. A
required stream capability without a configured grant causes CLI installation
to fail.

## Identity and isolation

The host checks the socket peer's UID through `SO_PEERCRED` on Linux/Android or
`getpeereid` on supported other Unix targets. Both the runtime account and the
configured peer must be non-root and distinct. The shell executor independently
checks the connecting runtime UID; other endpoint implementations must provide
their own client checks.

The socket transport requires a runtime-owned data directory with no group
or other permission bits. Operators must also isolate runtime credentials,
processes, binaries, and administrative sockets from the workspace account.
An endpoint grant grants the authority exposed by that endpoint; core does not
inspect commands or restrict their filesystem effects.

## Deadlines and cleanup

`connect` is denied during replay, and while the instance is stopping or
cancelled. Establishment is bounded by the grant's `max_timeout_ms`, capped by
the delivery's authority deadline and by the remaining call budget when the
call carries one.

Reads have no idle deadline. They end on EOF, transport error, byte-budget
exhaustion, or cancellation and stop. A plugin wanting a bounded read races the
stream read against `wasi:clocks/monotonic-clock.wait-for`, the same way it
reads a WASI HTTP body.

A connection opened inside a host-driven `handle` call closes when that call
ends. A connection opened by a source loop lives until its streams are dropped
or the source stops.

Emergency stop signals component cancellation; closing the shell connection
makes the executor cancel the command. The executor kills the command's process
group. Deliberately detached processes require operator-provided containment.

## Audit

`policy.decision` records the capability and decision; a refused request also
gets a terminal `capability.denied`. Transport events use the delivery's
authority, activity, correlation, and source event:

| Event | Recorded when |
| --- | --- |
| `stream.closed` | Both directions finish, a direction is dropped, or a transport error ends the connection. |

Access checks and deadline checks in the runtime can fail before transport
without emitting `stream.closed`. Delivery cleanup closes leftover connections
without terminal stream events. Explicit close ignores audit-write errors. Do
not assume every opened connection has a matching close event.

Transport events contain no command or frame bytes. Capability arguments record
what was requested, not proof of what a plugin sent to its endpoint.

## Shell implementation

| Location | Responsibility |
| --- | --- |
| `crates/pluribus-core/src/stream.rs` | Endpoint, grant, transport trait, errors. |
| `crates/pluribus-host-stream` | Unix connection, server UID checks, byte/time limits, private data-directory check. |
| `crates/pluribus-runtime-wasm` | Delivery access, connection ownership, audit events, cleanup. |
| `plugins/shell/protocol` | Versioned request/response JSON and validation. |
| `plugins/shell/client` | Wasm plugin, request encoding, response decoding and size limit. |
| `plugins/shell/executor` | Client UID checks, newline framing, shell execution and process-group cleanup. |

The executor serves one connection at a time. Commands use `/bin/sh -c`, a
cleared environment, fixed PATH, workspace HOME and working directory, and a
combined 1 MiB output cap before UTF-8 decoding. The client keeps its write side
open while awaiting the response; a half-close cancels execution.

The shell workspace builds the component and executor separately. Package
loading validates the declared component as Wasm; the runtime does not execute
native package files. The operator installs and supervises the executor out of
band. See [shell deployment](../../plugins/shell/README.md).

## Scope

Only same-machine Unix endpoints are supported. TCP and TLS are deferred.
`socket` performs no secret injection. Use WASI HTTP with
`credentials.authorize-http` for requests needing host-injected credentials.

The GitHub plugin signs and verifies inside Wasm using libraries. Its
`credentials` grant provides access to its own core-stored credential record.
The HTTP plugin uses a Unix socket only to communicate with its native listener.

## Async source I/O

A source loop connects, writes its protocol request to the outgoing stream, and
awaits bytes from the returned incoming stream. After EOF it awaits the
completion future to detect a transport error. Framing and reconnects belong to
the plugin. Connections stay alive across source commits and close when their
streams are dropped or the source stops. Peer checks and byte budgets still
apply.

`wasi:http/client.send(request).await` performs a granted HTTP exchange.
The host enforces deadlines, destination policy, and credential constraints.
`credentials.authorize-http` selects the credential before sending. The plugin
decides when to retry. Guest helpers provide blob-backed and inline responses
without defining another HTTP ABI.

A plugin can select between I/O, `runtime.next()` for internal events, and
`wasi:clocks/monotonic-clock.wait-for()` for timers. Only plugin commits produce
events; transport bytes never enter the event log implicitly.
See [source loops](lifecycle.md#source-loops).
