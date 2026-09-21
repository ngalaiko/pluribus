# Byte channels

Implemented for ABI `pluribus:plugin@3.0.0`. `socket.connect` exchanges bytes
with an endpoint granted to an instance: a local Unix socket, or a remote TLS
endpoint. HTTP uses WASI request/response body streams. Core owns transport
and peer verification; the plugin owns framing and protocol semantics.

## Interface

```wit
interface socket {
  use types.{error};

  connect: async func(endpoint: string, outgoing: stream<u8>)
    -> result<tuple<stream<u8>, future<result<_, error>>>, error>;
}
```

`endpoint` is a name, not a destination. It selects among the endpoints the
operator granted this component; a name the grant does not carry is denied.
The plugin therefore still cannot choose where bytes go — the operator's
configuration decides that — and a component needing two destinations, as the
email connector needs IMAP and submission, asks for two named endpoints
instead of two components. A component granted one endpoint whose manifest
names none reaches it as `default`.

The plugin passes the read end of a stream it writes to, and its bytes reach
the peer. The returned stream carries the peer's bytes. The future resolves
after that stream ends, `ok` on clean EOF and carrying the transport error if
one ended it.

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

Two endpoint kinds, two capabilities. `host.stream` grants a local socket;
`net.tls` grants one remote endpoint. A grant of one is never a grant of the
other: installation refuses an endpoint the manifest did not ask for.

`stream` is a map from endpoint name to one grant. A manifest names its
endpoint in `constraints.name`; one that names none is installed as `default`,
which is the name its component passes to `connect`. A component may request
several, and each entry is matched against the request carrying that name: a
second endpoint cannot be smuggled in under the first one's declaration.

Each named endpoint has its own transport, so `max_connections` bounds that
endpoint rather than the component's traffic as a whole.

### Local socket

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
            "default": {
              "socket": "/run/pluribus-workspace/executor.sock",
              "peer_uids": [1002],
              "max_bytes": 16777216,
              "max_timeout_ms": 300000
            }
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
| `max_connections` | Connections one instance may hold open at once. Unbounded by default: how many a local endpoint needs is the plugin's business, and the HTTP listener holds several. |

The manifest must import `pluribus:plugin/socket@3.0.0` and request
`host.stream` with exactly `constraints = { unrestricted = true }`: a local
endpoint exposes whatever authority the process answering it holds, which no
constraint can narrow.

### TLS endpoint

```json
{
  "components": {
    "main": {
      "stream": {
        "imap": {
          "tls": { "hostname": "imap.mail.me.com", "port": 993 },
          "max_bytes": 67108864,
          "max_timeout_ms": 15000,
          "max_connections": 1
        },
        "smtp": {
          "tls": {
            "hostname": "smtp.mail.me.com",
            "port": 587,
            "starttls": "smtp"
          },
          "max_bytes": 33554432,
          "max_timeout_ms": 30000,
          "max_connections": 1
        }
      }
    }
  }
}
```

| Field | Meaning |
| --- | --- |
| `hostname` | Required DNS name. Both the name resolved and the name the certificate must match. An address literal is refused: nothing verifies it. |
| `port` | Required. |
| `allow_private_network` | Permits a loopback or private-network endpoint, as the HTTP grant does. Off by default. |
| `starttls` | Names a plaintext preamble the host speaks before the handshake, for an endpoint that upgrades rather than answering in TLS. `smtp` is the only one. Absent by default, which means implicit TLS. |
| `max_connections` | Connections held open at once. One by default for a TLS endpoint: a remote session is usually singular, and a component wanting more says so in its manifest. |

The manifest must import `pluribus:plugin/socket@3.0.0` and request `net.tls`
with `constraints = { name = "...", hostname = "...", port = N }`, plus
`starttls` where the endpoint needs it. Configuration may lower the ceilings
but may not move the endpoint: a configured hostname, port or preamble the
manifest did not declare fails validation at `run`, not at first connect.

The two shapes are mutually exclusive. A `stream` entry carrying both a
`socket` and a `tls` endpoint, or neither, is refused.

### STARTTLS

Some submission endpoints answer in plaintext and upgrade on request;
`smtp.mail.me.com:587` is one, and Apple documents no implicit-TLS port beside
it. The host performs the whole preamble — read the greeting, `EHLO`,
`STARTTLS`, expect `220` — and hands the guest a connection that is already
encrypted.

That division is deliberate. The guarantee a `starttls` endpoint makes is that
the guest never sees a plaintext byte, and that is a property of the grant,
not of the protocol: a guest holding the plaintext connection could decline to
upgrade, and nothing above it would know. So the host owns the upgrade and
there is no way to opt out of it. An endpoint whose preamble does not reach
`220` is refused rather than downgraded — `4xx` as `unavailable`, since the
endpoint asked to be tried later, and anything else as `denied`. Bytes already
buffered when the handshake would start are refused too: after the handshake
they would be indistinguishable from the encrypted session's own output.

The preamble is not the protocol. The guest speaks the whole of SMTP from its
own `EHLO`, which RFC 3207 requires after the handshake anyway; the host's
`EHLO` exists only to reach `STARTTLS` and its answer is discarded.

### Both kinds

Each connection receives its own byte budget, drawn from its own endpoint's
grant; this is not a delivery-wide quota. On a long-lived connection the
budget is closer to a connection lifetime than a safety limit: exhausting it
ends the connection mid-exchange, and a source loop must treat that as a
reconnect. Size it for the traffic the endpoint carries. Describe the
endpoint's authority in the manifest. A name the grant does not carry, and a
component with no grant at all, get `permission-denied` from `connect`, which
the plugin turns into a `capability.failed` event rather than a trap. A
required stream capability without a configured grant causes CLI installation
to fail.

## Identity and isolation

A local endpoint and a remote one rest on different evidence.

For TLS the kernel vouches for nothing: the certificate is the peer's whole
identity. The host resolves the configured hostname, rejects the endpoint
unless every resolved address is public — a split answer is how a rebinding
attack reaches a local service — connects to an address it accepted, and
verifies the chain and the name against the roots the binary ships. Roots
travel with the installation rather than coming from the machine, so an
installation verifies the same chains wherever it runs.

`net.tls` is a network authority. A component holding it can reach the hosts
and ports the operator granted it, one per named endpoint; it can reach
nothing else, and a component holding `host.stream` can reach no network at
all. Naming endpoints widens nothing: an unknown name is `permission-denied`.

For a Unix endpoint the host checks the socket peer's UID through `SO_PEERCRED`
on Linux/Android or `getpeereid` on supported other Unix targets. Both the runtime account and the
configured peer must be non-root and distinct. The shell executor independently
checks the connecting runtime UID; other endpoint implementations must provide
their own client checks.

A local endpoint requires a runtime-owned data directory with no group or
other permission bits; an instance granted only a TLS endpoint has no local
socket to protect and requires none. Operators must also isolate runtime credentials,
processes, binaries, and administrative sockets from the workspace account.
An endpoint grant grants the authority exposed by that endpoint; core does not
inspect commands or restrict their filesystem effects.

## Deadlines and cleanup

`connect` is denied during replay, and while the instance is stopping or
cancelled. Establishment is bounded by the grant's `max_timeout_ms`, capped by
the delivery's authority deadline and by the remaining call budget when the
call carries one. A source loop reconnecting after a long wait should let a
timer elapse first, which renews the call budget the connect is measured
against.

Reads have no idle deadline. A connection may sit open for hours waiting for
the peer to push; arriving bytes renew a source loop's call deadline, so a
source parked on a read does not exhaust the budget it would resume under. A
delivery may not extend its own deadline that way. Reads end on EOF, transport
error, byte-budget exhaustion, or cancellation and stop. A plugin wanting a
bounded read races the stream read against
`wasi:clocks/monotonic-clock.wait-for`, the same way it reads a WASI HTTP
body; the email connector's IDLE loop is that pattern held open indefinitely.

Writes are bounded: one second for a local endpoint, which is either reading
or gone, and thirty seconds for a remote one, where a slow network is not a
dead peer.

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
| `crates/pluribus-host-stream` | Unix and TLS connections, peer verification, byte/time/connection limits, private data-directory check. |
| `crates/pluribus-core/src/net.rs` | Address policy shared with the HTTP transport. |
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

`socket` performs no secret injection in either endpoint kind, and neither
does WASI HTTP. A component that authenticates reads its own credential with
`credentials.get`, holds the secret in its memory, and therefore holds
whatever authority the secret carries. `pluribus:credential/static-plugin@1`
enrolls one.

Plain TCP without TLS is not offered. It would be an unauthenticated peer with
no kernel check standing in for the certificate.

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
The host enforces deadlines and destination policy. The plugin sets its own
authentication headers and decides when to retry. Guest helpers provide blob-backed and inline responses
without defining another HTTP ABI.

A plugin can select between I/O, `runtime.next()` for internal events, and
`wasi:clocks/monotonic-clock.wait-for()` for timers. Only plugin commits produce
events; transport bytes never enter the event log implicitly.
See [source loops](lifecycle.md#source-loops).
