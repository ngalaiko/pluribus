# Byte channels

Implemented for ABI `pluribus:plugin@1.0.0`. A channel is two resources: a
`reader` and a `writer`. `socket.connect` returns both halves for one
configured Unix socket; `http.sse` returns a reader alone. Core owns transport
and peer verification; the plugin owns framing and protocol semantics.

## Interface

```wit
interface reader {
  use types.{chunk, error};

  resource reader {
    receive: func(max-bytes: u32, timeout-ms: u32) -> result<chunk, error>;
  }
}

interface writer {
  use types.{error};

  resource writer {
    send: func(bytes: list<u8>) -> result<_, error>;
  }
}

interface socket {
  use reader.{reader};
  use writer.{writer};
  use types.{error};

  connect: func() -> result<tuple<reader, writer>, error>;
}
```

`connect` accepts no endpoint: an instance has at most one granted. Both halves
belong to the active delivery.

There is no `close`. Dropping a reader ends the transfer. Dropping a writer
half-closes: the peer reads EOF while the read half stays open, and an endpoint
MAY treat that as cancellation — the shell executor does. The transport closes
when both halves are gone, or when the delivery ends. A half whose transport is
already closed returns `invalid-argument`.

`receive` returns at most `max-bytes`. A read timeout returns an empty chunk
with `closed = false`, not a deadline error. EOF returns `closed = true`,
possibly with final bytes, and closes the transport. A zero `max-bytes` is
`invalid-argument`. `send` writes the whole buffer or fails; there is no
host-side write buffer. Bytes are inline; plugins can use `blobs` to store
content. HTTP request and response bodies use blob references, while an SSE
reader returns inline bytes.

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
| `max_bytes` | Positive combined send/receive budget per stream. Defaults to 16 MiB. |
| `max_timeout_ms` | Stream lifetime in milliseconds, 1–300000. Defaults to 300000. |

Each opened stream receives its own byte budget; this is not a delivery-wide
quota. Connecting requires an endpoint grant. The manifest must import
`pluribus:plugin/socket@1.0.0`, `pluribus:plugin/reader@1.0.0`, and
`pluribus:plugin/writer@1.0.0`, and request `host.stream` with exactly
`constraints = { unrestricted = true }`. Describe the endpoint's authority in
the manifest. `connect`, `receive`, and `send` require a granted endpoint; without one they
return `permission-denied`, which the plugin turns into a `capability.failed`
event rather than a trap. A required stream capability without a configured grant causes
CLI installation to fail.

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

At connect, the host caps stream lifetime by the grant, runtime call timeout,
call deadline, and the delivery's authority deadline. The transport checks expiry before
operations and during reads. Reads check cancellation every polling iteration
(with a 10 ms sleep when idle).

Connect is synchronous. Sends check stream expiry before writing and use a
one-second socket write timeout; they do not poll cancellation or recheck the
deadline during `write_all`. These limits do not guarantee an exact wall-clock
cutoff for every operation. Idle streams have no background expiry task.

Receive/send transport errors close the handle in the runtime. Delivery cleanup
closes remaining handles after return or failure. Emergency stop signals
component cancellation; closing the shell connection makes the executor cancel
the command. The executor kills the command's process group. Deliberately
detached processes require operator-provided containment.

## Audit

`policy.decision` records the capability and decision; a refused request also
gets a terminal `capability.denied`. Transport events use the delivery's
authority, activity, correlation, and source event:

| Event | Recorded when |
| --- | --- |
| `stream.closed` | The last half drops, receive observes EOF, or a transport error ends the handle. |

Access checks and deadline checks in the runtime can fail before transport
without emitting `stream.closed`. Delivery cleanup closes leftover handles
without terminal stream events. Explicit close ignores audit-write errors. Do
not assume every opened stream has a matching close event.

Transport events contain no command or frame bytes. Capability arguments record
what was requested, not proof of what a plugin sent to its endpoint.

## Shell implementation

| Location | Responsibility |
| --- | --- |
| `crates/pluribus-core/src/stream.rs` | Endpoint, grant, transport trait, errors. |
| `crates/pluribus-host-stream` | Unix connection, server UID checks, byte/time limits, private data-directory check. |
| `crates/pluribus-runtime-wasm` | Delivery access, handle ownership, audit events, cleanup. |
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
`socket` performs no secret injection. Use `http` when the host must inject
credentials into HTTP requests.

There is no crypto import. Inbound webhook signature verification moved into
the core, which owns the credential; a plugin never needs the key. A future
signing operation could authenticate plugin messages to an endpoint, but it is
not required for the Unix peer-credential setup.
