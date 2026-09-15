# RLM cognition

RLM owns durable jobs and one decision queue. Core delivers observations while
providers run, checks revisions and authority at admission, and records results.
Observations enter an inbox: they can amend work, create a job, cancel, clarify,
or require no action. Merely receiving one does not send a reply.

Core selects the model for every RLM request.

```json
{"plugin_instances": {"rlm": {"package": "bundled:rlm"}}}
```

Core supplies identity, model selection, and capability schemas.
Connectors control sender admission.
RLM consumes observations from the agent stream. Replies use the observation's
`<provider>.reply` capability. The package supplies its JS component and defaults.

JS operations:

- `history.read({after, limit, eventTypes})`: admitted root history or a delegated child range;
  at most 100 events / 64 KiB. All event types remain accessible, including
  internal checkpoints. Filter aggressively to the evidence needed. Oversized
  payloads return `payloadOmitted` metadata; the cursor still advances.
- `rlm.query({question, context})`: read-only child; at most 64 KiB of context.
- `memory.recall/get/remember/supersede/forget(...)`: root-only
  [memory capabilities](../memory/README.md), subject to scoped grants.
- `capabilities.invoke(name, arguments)`: authorized root activity.
- `checkpoint({named: values})`: up to 32 KiB of JSON working values.

Children inherit the job revision. Delegated history
cannot exceed the parent's access. Children share the job's 32-call cycle budget
and have depth limit four. Jobs
can continue silently, wait for input or time, complete, or fail. Waiting for
input makes no idle model calls. Wakes survive restart.

The model uses `js` to compute and `yield` to return control:

```json
{"action":"complete","reply":"pong"}
{"action":"continue","note":"Tests remain"}
{"action":"wait","waitFor":"input","question":"Which repository?"}
```

Each control call must stand alone. Its schema is supplied with the model
request; assistant prose does not finish jobs or send replies. Invalid calls
receive bounded correction attempts. Children yield only a result to their
parent. An optional root `reply` requests `<provider>.reply` to the original
conversation, including while continuing or waiting.

Observation routing uses a separate `associate` tool to create, amend, cancel,
or clarify work. It cannot discard messages, execute JS, or yield a root decision.
Messages needing no response may complete through root `yield` with `reply: null`.
Input waits require a specific `question`. Routing distinguishes unanswered
questions, provider failures, and scheduled work. Routing stays within the same
sender and conversation, including a Telegram topic. `clarify` requires a question
and pauses only the unresolved observation.

[Durable jobs, budgets, recovery, and operational limits](../../docs/persistent-work.md).

The [event flow](../../docs/rlm-event-flow.md) traces one request end to end.

Model inputs use a shared [turn context](../../docs/rlm-turn-context.md), also available as `context.turn` in JS.

Persistence stores independent records in 128 KiB fragments under
`engine/record/<field>/<key-hash>/<fragment>`. Version 1 `cognition.checkpoint`
events carry `sequence` and up to four `mutations`, each containing a state `key`
and base64 `value` (null deletes). Replay also accepts legacy byte arrays. Fragments encode `{field,key,value}` JSON;
the sequence record commits last. Replay applies deltas across delivery boundaries.
Any other checkpoint version is rejected.

Retention targets 64 inactive terminal jobs, 64 settled activities per job, and
64 source contexts. Each delivery retires bounded groups to stay within
transaction limits. Earlier records remain in the audit log. Result receipts
and observation tombstones use individual records, fetched only for matching
incoming events. Retired records are deleted.

Operator commands require node actor `operator:<agent-id>`:

- `operator.job-control`: `{version:1,jobId,action:"cancel"|"resume",revision,reason}`.
  Revision must match. Cancel invalidates requests and closes sessions. Resume
  requires a paused live task and no unresolved outcomes; original authority stays.
- `operator.attempt-reconciled`: `{version:1,requestEventId,outcome:"completed"|"failed",output?,reason}`.
  Records a verified result and clears its reconciliation blocker. Execution
  requires a separate resume command.

Deferred Telegram media events enrich their original observation and task context;
only the configured connector can supply them.

A trapped session component fails suspended calls with an unknown outcome and
restarts with fresh memory. Failed cells are not replayed; cognition can replan
or report failure. Cancellation records an uncertain terminal result and recreates
the provider without replaying the interrupted request. Startup also recovers a
recorded crash when the request's actor cancelled it before the failure. Other
components remain quarantined after a trap.

Amendments close outstanding model tool calls with interrupted results. Late
results remain activity evidence without resuming an obsolete revision. Malformed
tool histories fail locally instead of entering provider retries.

The CLI derives `context.components` from installed manifests: instance IDs,
plugin IDs, capabilities, subscriptions, and emissions. This interface map helps
select history filters; it does not assert component health or permissions.
