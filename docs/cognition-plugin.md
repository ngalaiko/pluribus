# Event-driven cognition plugins

Status: event ABI, durable RLM jobs, provider workers, recovery, and pacing implemented. See [persistent work](persistent-work.md).

## Flow

```text
Incoming signal → core persists event → delivers to cognition
Cognition → appends request events → returns
Core → authorizes and dispatches requests to providers
Provider results → core persists events → delivers to cognition
```

Core delivers stream events, including cognition's own events. Fired timers
are delivered only to their requesting instance. The plugin decides whether to act, append events, or do nothing. Core
does not filter events by reasoning relevance. Delivery never implies an LLM
call. Existing visibility and authority boundaries still apply.

## Interface

Conceptually, a cognition plugin exports:

```text
on_event(event) → success | error
```

It uses host APIs to append events, read authorized history, and access its
persistent state. The return acknowledges processing; it contains no reply,
next-action decision, context selection, or reasoning budget.

Core serializes delivery per agent and persists the delivery cursor without
appending an acknowledgement event. Returning without appending anything leaves
cognition idle once pending events have been delivered. Timers and external
signals can wake it again. The plugin must ignore its own bookkeeping events
when no action is needed; core resource ceilings bound runaway event production.

## Requests and results

Cognition requests work by appending typed request events. Core recognizes the
request protocol, checks authority and limits, and dispatches to the appropriate
provider. Merely appending an event does not authorize its requested operation.

Requests cover model completions, code execution, capabilities, and timers.
Results, failures, and cancellations return as correlated events. Request IDs,
causation, authority, and activity references connect each result to its intent.
Exact schemas and WIT signatures remain to be defined.

The cognition plugin chooses the model request and prompt. The model-provider
plugin calls the LLM. Core contains no reasoning loop and does not choose when
to call the model.

A human-facing reply is a capability request, with explicit destination and
authority. Completing cognition does not automatically send a message.

## Ownership

| Cognition plugin | Core |
| --- | --- |
| Whether an event needs action | Ordered, durable event delivery |
| Prompts, context references, cursors, summaries | Authorized event and blob access |
| Memory strategy and reasoning state | Persistent state storage |
| Recursive decomposition and budget allocation | Resource ceilings and cancellation |
| Requests for models, code, and capabilities | Authorized dispatch to providers |
| Wake timing and idle policy | Durable timer execution |

Core's delivery cursor is separate from the plugin's context cursors. Core's
resource ceilings are separate from the plugin's reasoning budget. Plugin state
is opaque to core.

## RLM

The RLM cognition plugin owns the reasoning loop, JS session references,
context selection, child queries, result aggregation, and stopping decisions.
The JS environment executes code and preserves working state; it is a separate
execution role, not the cognition interface.

A model result can cause RLM to request a JS cell. A cell result can cause it to
request child model calls. Their results feed subsequent cells or prompts.
All pending relationships belong to the plugin's state; all external work
travels through request/result events.

## Recovery

Delivery is at least once. Advancing the delivery cursor must be atomic with
committing plugin state and emitted events, or replay must deduplicate those
writes using stable IDs. Handler failure must not silently consume an event.
The implementation must establish this contract before unattended execution.

Providers use durable dispatch records and idempotency keys. Unknown external
outcomes require reconciliation; event replay must not blindly repeat effects.
Persisting cognition state does not by itself checkpoint a JS heap. Environment
recovery needs an explicit checkpoint or reconstruction contract.

## Implementation

The [RLM cognition plugin](../plugins/rlm/README.md) owns model/code
coordination and recursive query state. The interpreter is a separate plugin.
CLI installs both by default, while respecting configured cognition.

Capability authority follows causation to a recognized connector observation.
Trusted senders receive configured grants; other senders receive only
conversation-scoped Telegram grants. Unknown origins are denied. Core checks
the actual provider and JSON arguments before dispatch.

Explicit JSON checkpoints restore selected working values. Suspended stacks
remain nonrecoverable. See the plugin README for tested behavior and limits.

The [memory plugin design](memory-plugin.md) defines explicit retention and
retrieval through capability events, including the JS/RLM request/result loop.

[Persistent work](persistent-work.md) describes durable jobs, observation
coordination, independent execution, continuation, and recovery.
