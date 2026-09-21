# Events and authority

Status: draft

This document defines the canonical facts stored by the core and the authority carried by every activity.

## Invariants

- Every durable change is an event.
- Sequence numbers define order within one stream.
- Event identifiers are globally unique UUIDv7 values.
- Events are immutable.
- Payload schemas are versioned independently.
- Activities carry immutable authority.
- Delegation may preserve or reduce authority, never increase it.
- Capability dispatch always passes through core enforcement.
- Policy decisions are events.
- External content never creates authority by itself.
- Wall-clock time never defines event order.

## Streams

Each agent and executor node owns one event stream.

```text
StreamId   = UUID
StreamKind = agent | node
Sequence   = unsigned 64-bit integer
```

Sequence numbers start at one and have no gaps after commit. A transaction assigns the next sequence and appends the event atomically.

There is no global ordering across streams. `event_id`, `activity_id`, `correlation_id`, and `causation_id` connect related events.

The primary agent stream is authoritative for agent state. Executor streams are authoritative only for their local receipt, execution, and result records.

## Event envelope

```text
Event {
  schema:          "pluribus.event/1"
  event_id:        UUIDv7
  stream_id:       UUID
  stream_kind:     agent | node
  sequence:        u64
  recorded_at_ms:  i64
  observed_at_ms:  i64?
  event_type:      string
  payload_schema:  string
  payload:         JSON | BlobRef
  actor:           PrincipalRef
  authority_id:    UUID?
  activity_id:     UUID?
  correlation_id:  UUID?
  causation_id:    UUID?
}
```

`recorded_at_ms` is assigned by the committing node. `observed_at_ms` records an external timestamp and is untrusted.

`causation_id` names the event that directly caused this event. `correlation_id` groups one logical interaction. `activity_id` groups one bounded execution.

Payloads are canonical JSON. Large or binary values use content-addressed blobs.

## Blob reference

```text
BlobRef {
  algorithm:   "sha256"
  digest:      string
  size:        u64
  media_type:  string
}
```

Blob bytes are immutable. The event remains valid only while every referenced blob is available.

## Principals

```text
PrincipalRef {
  kind: human | agent | node | component | external
  id:   string
}
```

External principals are namespaced by connector instance:

```text
external:<connector-instance>:<provider-id>
```

Telegram numeric user identifiers are never treated as global identities outside their connector instance.

## Authority envelope

The core creates an immutable authority envelope before starting an activity.

```text
Authority {
  schema:             "pluribus.authority/1"
  authority_id:       UUIDv7
  agent:              PrincipalRef
  origin:             Origin
  delegation_chain:   Delegation[]
  grants:             Grant[]
  audiences:          Audience[]
  parent_authority:   UUID?
  issued_at_ms:       i64
  expires_at_ms:      i64?
  max_depth:          u32
  current_depth:      u32
}
```

### Origin

```text
Origin {
  kind: autonomous | telegram | email | github | webhook | timer | agent | tool
  principal:       PrincipalRef?
  connector:       PrincipalRef?
  conversation_id: string?
  source_event_id: UUID
  trusted:         bool
}
```

`trusted` is policy input, not permission. Grants remain the enforceable authority.

### Delegation

```text
Delegation {
  from:             PrincipalRef
  to:               PrincipalRef
  on_behalf_of:     PrincipalRef?
  purpose:          string
  source_event_id:  UUID
}
```

The complete chain crosses agent and node boundaries.

### Grant

```text
Grant {
  capability:  string
  provider:    PrincipalRef?
  constraints: JSON
}
```

Capability names are stable dotted identifiers such as:

```text
telegram.send
memory.remember
memory.recall
shell.exec
github.repo.comment
github.repo.pull-request.create
agent.request
battery.read
```

Constraints are capability-owned, versioned JSON. Examples include allowed Telegram conversations, repository identifiers, filesystem roots, command limits, or a specific executor node.

An empty grant list permits no capability calls.

### Audience

```text
Audience {
  connector:       PrincipalRef
  conversation_id: string
  recipients:      PrincipalRef[]
}
```

Outbound messaging must match both a capability grant and an audience.

## Authority derivation

For a root activity:

```text
authority = standing_policy(agent, origin)
```

For a child or delegated activity:

```text
authority = intersect(parent, requested, recipient_policy)
```

Intersection applies to:

- capability names;
- providers;
- capability constraints;
- audiences;
- expiry;
- recursion depth.

An unrepresentable constraint intersection is denial.

Authority is fixed for the activity. A changed policy affects new activities only. Long-running activities may still be cancelled by emergency stop or node revocation.

Scheduled continuations inherit the authority that created them. Scheduling cannot launder a restricted observation into autonomous authority.

This model limits causal authority. It does not provide information-flow security: later autonomous cognition may recall untrusted content and act on it.

## Standing policy

### Personal agent

| Origin | Authority |
| --- | --- |
| Operator Telegram identity | All personal-agent grants |
| Autonomous cognition | All personal-agent grants |
| Timer created with full authority | Creator authority |
| Family-agent request | `agent.respond`, memory read, and explicitly configured disclosure capabilities |
| Unknown Telegram identity | None |

### Family agent

| Origin | Authority |
| --- | --- |
| Either spouse | All family-agent grants |
| Autonomous cognition | All family-agent grants |
| Unknown group member | Reply to the originating conversation only |
| Timer | Creator authority |

### Email

Email-originated cognition receives only:

- mail read and classification;
- memory remember and recall;
- notification to configured owner conversations;
- scheduling with the same authority.

Email bodies, attachments, senders, and headers are observations, never principals with instruction authority.

### GitHub

An authenticated webhook receives grants derived from its repository and event type. It never receives unrestricted shell.

Repository constraints survive recursion and scheduling.

## Capability enforcement

Before dispatch, the core:

1. Resolves the advertised capability and provider.
2. Finds matching grants.
3. Validates capability-owned constraints.
4. Validates audience for outbound communication.
5. Checks expiry, recursion, node revocation, and resource ceilings.
6. Appends `policy.decision`.
7. Delivers `capability.requested` to the provider only when allowed.

The requester appends `capability.requested` itself, so the gate controls
dispatch rather than the append. An attempt is therefore auditable whether or
not it passed: the log records what was tried, not only what was permitted.

A refusal appends `capability.denied`, which is terminal. The requester
correlates it like any other result and can choose another action, rather than
waiting for a result that will never arrive. A request naming a capability with
no installed provider terminates the same way.

A request that already carries a terminal result is never dispatched again.
That, rather than a retry counter, is what stops an external effect from
repeating after a restart.

Policy plugins may calculate decisions. The core owns the enforcement gate and deny-on-error behavior.

## Event types

The vocabulary is closed and lives in
[`registry.rs`](../crates/pluribus-core/src/registry.rs). A type is core-owned
or plugin-emittable; an unknown type is rejected rather than appended. A plugin
may emit only the plugin-emittable types its manifest declares.

### Observations

```text
observation.received
```

The payload records connector-native identity, mapped principal, conversation,
trust classification, raw payload blob, normalized content, and deduplication
key.

The type is core-owned, with one exception: a connector holding an exact
`observation.received` emission grant may propose it. Wildcards grant no
intake. The host stamps the connector actor. Origin policy derives authority
from that actor and the configured sender mapping.

### Cognition

Plugin-emitted:

```text
cognition.checkpoint              pluribus.cognition-checkpoint/1
cognition.job-updated             pluribus.job/1
cognition.observation-associated  pluribus.observation-association/1
cognition.cancel-requested        pluribus.cognition-cancel/1
cognition.completed
cognition.failed
```

Core-owned, recording what a delivery attempted:

```text
activity.attempted                pluribus.activity-attempt/1
activity.unknown                  pluribus.activity-attempt/1

Core-owned, recording deferred cognition work after bounded host recovery:

```text
cognition.resource-exhausted     pluribus.cognition-resource-exhausted/1
```

The host stamps the node actor. It carries the failing input and request IDs,
resource measurements, phase, remaining retry allowance, effect status, and
checkpoint status. Its deduplication key is scoped to the cognition instance
and failing input.
```

See [persistent work](persistent-work.md).

### Models

Plugin-emitted:

```text
model.requested
model.stream
model.completed
model.failed
```

`model.stream` may batch provider deltas. Returned encrypted reasoning data is
stored as an opaque blob.

Model deltas are appended outside the delivery transaction so a completion is
visible while it streams. They carry a plugin-supplied idempotency key, so a
redelivered attempt deduplicates instead of doubling the stream.

### Capabilities

Plugin-emitted:

```text
capability.requested
capability.output
capability.completed
capability.failed
```

Core-owned:

```text
capability.denied
capability.timed-out
capability.cancelled
```

### Agent calls

```text
agent.failed
```

Core-owned. It terminates a cross-agent request the core could not complete.

### Memory

Plugin-emitted:

```text
memory.remembered
memory.superseded
memory.forgotten
```

### Code sessions

Plugin-emitted. The interpreter releases the named session on
`code.close-requested`:

```text
code.evaluate-requested
code.yielded
code.resumed
code.completed
code.failed
code.close-requested
code.closed
```

### Components

Core-owned:

```text
component.failed
component.backoff     pluribus.component-health/1
component.recovered   pluribus.component-health/1
```

### Operator control

Core-owned:

```text
operator.job-control          pluribus.operator-job-control/1
operator.attempt-reconciled   pluribus.operator-attempt-reconciled/1
```

### Streams

```text
stream.closed
```

Core-owned. It records the transport close; access and deadline checks can fail
before transport without producing it.

### Timers

A timer is a request and a result, not a scheduler table. A plugin appends
`timer.set`; the core appends `timer.fired` when due. Outstanding timers are
recovered by reading `timer.set` events with no matching `timer.fired` or
`timer.cancel`, so a restart neither loses a timer nor fires one twice.

Plugin-emitted:

```text
timer.set
timer.cancel
```

Core-owned:

```text
timer.fired
```

### Policy

Core-owned:

```text
policy.decision
```

### Telegram media

Plugin-emitted, both carrying `dev.pluribus.telegram.media.v1`:

```text
```

## Deduplication

Every external observation has a connector-owned deduplication key. Repeated delivery returns the previously committed event identifier.

Every capability invocation has an idempotency key derived from its activity and call identifier. Executors retain terminal results and return the prior result for repeated keys.

Non-idempotent capabilities must declare their behavior. The core does not retry them automatically after an ambiguous outcome.

## Failure rules

- Invalid payload: reject before append, then record an operational error outside the agent stream.
- Unknown event version: preserve but do not project.
- Policy error: deny.
- Missing authority: deny.
- Missing blob: mark the projection incomplete; never invent content.
- Storage commit failure: perform no external dispatch.
- Event append after an external effect fails: reconcile by idempotency key.

## Encoding boundary

This document defines semantics, not transport encoding.

- SQLite stores indexed envelope fields and canonical JSON payloads.
- WIT uses equivalent typed records and variants. The plugin ABI carries the
  same envelope minus `deduplication_key`, which is host-only, and minus
  `observed_at_ms`, which is untrusted provider data and belongs in the
  payload.
- Iroh uses a versioned binary frame format defined separately.
- Conversion must preserve identifiers, authority, causation, and unknown payload fields.
