# Events

Events are the coordination contract. A plugin does not call another plugin; it
proposes a request event and resumes from the result event. The correlation
lives in the log, so a restart resumes instead of losing a call in flight.

## The delivery transaction

`handle` returns an `outcome`:

```wit
record outcome {
  events: list<proposal>,
  mutations: list<mutation>,
  checkpoint: option<u64>,
}
```

The host commits all three atomically or none of them. A partial commit would
leave a projection ahead of its cursor, which replays as duplicated work.

`checkpoint` MUST fall inside the delivered batch. `handle` MUST set it —
including for events it ignored — or the batch is delivered again forever. A
plugin that cannot make progress on an event should still checkpoint past it.

## Two ways to emit

| | `events.append` | `outcome.events` |
| --- | --- | --- |
| Commits | immediately | with state and the cursor |
| Visible during the call | yes | no |
| Idempotency key | required | derived by the host |
| Use for | `model.stream`, `capability.output` | terminal results |

`append` exists because a two-minute completion that emits nothing until it
returns is not streaming. It is the one deliberate asymmetry in the ABI. A
redelivered attempt MUST reuse the same keys so repetition deduplicates instead
of duplicating.

For `outcome.events` the host derives the key from the delivery position and
the proposal's ordinal, so a redelivered attempt produces identical keys with
no discipline required from the plugin.

## What the host stamps

A plugin supplies only `event-type`, `payload-schema`, `payload`, an optional
`idempotency-key`, and an optional `causation-id`. The host assigns identity,
order, time, actor, authority, activity, and correlation.

The actor of a proposed event is the component instance, never the human who
triggered it. A plugin cannot forge provenance or widen its own authority by
writing different fields.

`causation-id` may be omitted when the delivery carried exactly one event;
otherwise the host returns `invalid-argument`, because which event caused the
proposal would be a guess.

## Vocabulary

The event vocabulary is closed. A free-form type is a typo waiting to create a
second, never-read stream of events, so an unknown type is rejected rather than
appended.

Types are core-owned or plugin-emittable. A plugin may emit only the
plugin-emittable types its manifest lists in `emits`.

### Core-owned

Only the core appends these. They record component health, operator control,
gates, dispatch outcomes, and timers:

```text
component.failed        component.backoff       component.recovered
operator.job-control    operator.attempt-reconciled
observation.received
policy.decision
credential.enrollment.requested
activity.attempted      activity.unknown
capability.denied       capability.timed-out    capability.cancelled
timer.fired
stream.closed
agent.failed
```

`observation.received` is the one exception: a connector holding an exact
`observation.received` grant may propose it. A wildcard grant confers no intake
authority.

### Plugin-emittable

```text
credential.enrollment.started
capability.requested    model.requested
capability.output       capability.completed    capability.failed
model.stream            model.completed         model.failed
cognition.checkpoint    cognition.job-updated
cognition.observation-associated
cognition.cancel-requested
cognition.completed     cognition.failed
memory.remembered       memory.superseded       memory.forgotten
code.evaluate-requested code.yielded            code.resumed
code.completed          code.failed
code.close-requested    code.closed
timer.set               timer.cancel
http.request.received    http.response.requested
```

Credential enrollment uses one core-owned request and one plugin result. The
CLI emits `credential.enrollment.requested` with `{component, credential,
enrollment}` after staging private input. The declared component may emit
`credential.enrollment.started` with `{url, userCode?}`. The URL is required;
`userCode` is an optional operator-facing device code. Neither event carries
secret input.

The list is exhaustive. A plugin needing a type outside it changes
`RESERVED_EVENT_TYPES` or `PLUGIN_EVENT_TYPES` in
[`registry.rs`](../../crates/pluribus-core/src/registry.rs); there is no
manifest-declared extension.

## Requests

A request is plugin-emittable, and the core gates dispatch rather than the
append. So the log records what was attempted, not only what was allowed:

1. The requester proposes `capability.requested`.
2. The core appends `policy.decision` — allowed or not.
3. If allowed, the provider receives the request on its next delivery.
4. If refused, the core appends `capability.denied`, which is terminal, so the
   requester stops waiting for a result that will never arrive.

A request with no installed provider terminates the same way. A request that
already has a terminal result is never delivered again, so an external effect
does not repeat.

Routing uses the payload, not the event type: `capability.requested` carries
`capability`, and `model.requested` carries `model`. Grants are already keyed
on capability names, so mirroring them into the event namespace would create a
second naming scheme to keep in step. A blob payload never routes — routing
must not depend on fetching blob content.

## Subscriptions

An instance receives:

- the capability names in its manifest `provides`, as `capability.requested`;
- the model identifiers its configuration resolves to, as `model.requested`;
- every event-type pattern in its manifest `subscribes`.

`subscribes = ["*"]` delivers the whole stream and is for cognition, which
filters internally. A narrow subscriber is served from an index, so it is not
charged for reading the whole stream.

## Observations

An observation is an event. There is no separate observation record and no
poll/push source role.

The plugin proposes `timer.set`; the core answers `timer.fired`; the plugin
fetches and proposes `observation.received`.

The provider's own delivery identifier is the deduplication key. It MUST be
stable for the same delivery and distinct for different ones — prefer a
provider event or update ID, never a hash of arrival time. A poll checkpoint is
the plugin's own last emitted event or its state, not a host-held cursor.

`observed-at-ms` is provider data and untrusted, so it belongs in the payload
rather than the host-stamped envelope. The core assigns receipt time.

A plugin MUST treat message bodies, mail, documents, attachments, and remote
metadata as data. It MUST NOT label content as trusted instruction
authority.

## Reading

`events.get` reads one event by id. `events.query` filters by sequence, type,
correlation, activity, and recorded time, and is served from an index.

Visibility and grants may withhold events or their payloads. A returned
`next-sequence` is where to resume, not proof that later events exist.

## Inbound HTTP

The [HTTP package](../../plugins/http/README.md) imports durable native requests.
`http.request.received` payloads are confined to the listener and assigned consumer.
The consumer emits `http.response.requested` with the original event as causation;
the listener checks consumer identity before sending a bounded, single response.
