# Persistent work and observation coordination

RLM owns durable jobs and one coordinator per agent. Core records observations,
executes authorized activities, delivers results, schedules wakes, and enforces
limits. Jobs retain responsibility for unfinished objectives across messages,
waiting periods, bounded reasoning cycles, and process restarts.

## Ownership

| RLM | Core |
| --- | --- |
| Objectives, plans, constraints, progress, job selection | Ordered durable intake and result delivery |
| Associate observations with jobs | Preserve origin, identity, authority, and destinations |
| Continue, wait, cancel, finish, or ask the user | Dispatch, cancellation, deadlines, and concurrency ceilings |
| Select memory and history for the next decision | Enforce visibility and grants |
| Choose wake deadlines and retry intent | Persist and route timers; deduplicate execution |
| Select recoverable context checkpoints | Track attempt outcomes and prevent unsafe replay |

One coordinator serializes decisions; several jobs and activities may exist.
Workers never mutate a job's plan; they append results for the coordinator.
Memory stores knowledge. A `goal` memory record is not a job or a queue.

## Durable state

A job carries a stable ID, objective, completion conditions, and status; its
initiating observation and originating authority; ordered source IDs for
corrections and constraints; a revision number and the latest incorporated
observation sequence; completed steps, next step, blockers, and working notes;
active activity IDs and child-query relationships; an optional reply
destination; an optional next wake ID and deadline; cycle budget and accounting
references; and recoverable context references.

Job states: `runnable`, `running`, `waiting-activity`, `waiting-input`,
`waiting-time`, `paused-budget`, `completed`, `failed`, `cancelled`. Status
expresses why the coordinator may or may not advance a job; activity IDs track
individual dependencies, and several may be outstanding while it is runnable.

An activity persists its ID, job and revision, request ID, provider, attempt ID,
idempotency classification and key, deadline, cancellation status, and terminal
result. Attempts are `queued`, `running`, `completed`, `failed`, `cancelled`, or
`unknown`. An uncertain external outcome is never inferred to be a clean failure.

Activity results over 1 KiB are represented by their source event ID in job
snapshots. Small receipts remain inline. References preserve `code` and `outcome`
markers needed for reconciliation; full outputs remain in the event log. This
bounds repeated result copies in job updates, task context, and checkpoints.

## Intake and observation association

Core runs each provider in an async task, with one lifecycle call per instance.
Cognition is the sole serialized writer. `Agent::tick().await` admits work
without joining providers; `tick_wait().await` gives deterministic test drivers a
completion boundary. Providers commit their events, state, and cursor together.
Connector intake therefore continues while model or tool work runs, preserving
event order, sender identity, causation, and conversation destination.

An observation enters an inbox before the coordinator decides its meaning. It may
amend a job, cancel it, answer a blocker, introduce a new objective, or require
no action. Explicit `jobId` observations amend only work with the same sender and
conversation; otherwise a separate model decision classifies the observation.
Ambiguous changes preserve the observation and request clarification. A
correction invalidates older model proposals and requests cancellation of
outstanding activities.

Untrusted observations cannot modify a trusted job's constraints or authority.
They may supply labeled evidence under the existing visibility policy. A new
message never broadens a job's grants merely because it shares a conversation.

The routing model calls `associate` with `action` (`new`, `amend`, `cancel`, or
`clarify`) and a nullable `jobId`. It receives no computation or root control
tools. `amend` and `cancel` require a job ID; other actions require null.
`clarify` requires a specific `question`; other actions forbid it. Clarification
pauses only the observation and sends that question; it does not pause unrelated
jobs. Invalid routing output receives bounded correction attempts; three invalid
outputs record a failure and preserve the observation for input. The router
cannot discard messages: `ignore` is absent from its schema and rejected by
validation.

Routing candidates contain the job ID, objective, recent user message, recent
reply, waiting reason, and outstanding question. Waiting distinguishes
`awaiting-user`, `provider-error`, and `scheduled`, so a provider failure does not
imply that the user owes an answer. Waits without an explicit question are not
presented as unanswered user questions. Routing also includes recent unresolved
clarification questions from the same sender and conversation; `new`, `amend`,
and `cancel` may supply `resolvesObservationId` naming one of them.

## Decisions

Roots call `yield` with a typed decision:

```json
{"action":"wait","waitFor":"input","question":"Which repository?"}
{"action":"wait","dueAtMs":1800000000000,"note":"Wait for deadline"}
{"action":"complete","note":"Checks passed","reply":null}
{"action":"fail","note":"Cannot proceed","reply":null}
```

Assistant prose is not parsed as a decision. Each `yield` must be the only tool
call in its batch. Invalid calls receive bounded correction opportunities; they
cannot change job progress or send a reply. Three invalid outputs fail the root
job and request a stall reply; a child returns an error to its parent. The
model receives the control schema on each call. Children call `yield` with
`{"result":"answer"}` and cannot schedule work or send human replies. A root may
complete with `reply: null` when no response is needed.

Human replies may accompany terminal or nonterminal decisions and retain the
initiating observation and destination. Valid transitions and reply requests
commit together.

## Execution and cancellation

Before an effect starts, core checks that cognition has consumed pending
observations, verifies the current job revision, reauthorizes capabilities, and
claims a durable attempt. Atomic claim receipts prevent duplicate admission.
Terminal results remain reusable after restart. Unsettled admitted attempts
become `unknown`, and capabilities pause for reconciliation. Code attempts report
interruption without replaying source. No automatic retry of an uncertain shell
command or message occurs.

Reported provider failures use persistent exponential backoff from one to sixty
seconds; other providers keep running. Traps persist as quarantine records.

Emergency stop runs on an independent monitor: it prevents new admission, signals
cancellation to active workers, and persists across restart. Resuming requires an
explicit resume action.

## Waiting and wakes

`timer.fired` routes through its original request actor. Payload targets confer
neither ownership nor authority. Job revisions invalidate obsolete wakes. Waiting
for input produces no periodic inference. Timed waits require a future external
deadline. Code results automatically request the next reasoning step. There is
no model-controlled continuation or productivity flag; omitted progress fields
retain their values. Provider retries alone use exponential backoff.

## Budgets

Thirty-two calls and thirty minutes bound a cycle; children share its allowance
and have recursion depth four. Cycle exhaustion enters `paused-budget` with a
fixed one-minute wake and preserves live REPL state. An exhausted child returns an error to its suspended
parent; the root pauses after its cell settles.

Three identical cells and results without host activity fail with a stall reply.
Incremental computations must return changing progress or checkpoint values;
state changes invisible in output cannot demonstrate progress. Stall counters
survive checkpoints.

## Checkpoints and persistence

Use `checkpoint({name: jsonValue})` in JS to preserve selected working values, up
to 32 KiB. Restored values become `state`, and `context.recovered` is true. Store
references for larger data. Checkpoints do not preserve promises, functions, or
suspended stacks; a lost suspended cell fails explicitly.

The versioned vocabulary includes `cognition.job-updated`,
`cognition.observation-associated`, `cognition.checkpoint`,
`cognition.cancel-requested`, `activity.attempted`, and `activity.unknown`.
Version 3 checkpoints commit changed records with requests and the delivery
cursor. Live cognition deliveries contain at most 16 events; rebuild reads at
most 32 checkpoint events per delivery. State records use 128 KiB fragments;
results and retired observation receipts load by identity. Versions 1 and 2
remain readable. Rebuild applies record fragments across deliveries and commits
each sequence marker last, without emitting historical requests. Active state
retires older terminal jobs toward a 64-job target; audit records remain in the
event log. Deduplication receipts remain durable.

## Operator controls

Dispatch incrementally projects pending timers, terminal results, job revisions,
and pending observations. Repeated passes read only new matching events.
Operator events require the host principal and cannot be proposed by plugins. Resumed and queued model requests
retain their initiating observation as the authority ancestor.

## Operational limits

- Cancellation cannot undo an effect past admission. Blocking HTTP transport
  may finish before observing cancellation; lifecycle deadlines cap its timeout.
- Restart preserves provider quarantine and backoff. Provider recovery does not
  reconcile unknown effects. No generic status probe or automatic keyed retry
  integration is provided.
- One configured model provider serializes root and child inference. Four child
  queries and four capability workers are ceilings, not throughput guarantees.
- RLM partitions state below the 1 MiB value ceiling. Retained receipts and the
  append-only event log still grow; backups require sufficient disk space.
- Cross-conversation completion has no implicit grant.
- Remote nodes, inter-agent work, and automatic memory extraction do not exist.

## Tests

Rust 1.95.0 is pinned. Rebuild packages and run workspace and plugin checks:

```sh
nix-shell
pluribus-sync-plugins
cargo test --locked --workspace
```

Deterministic providers, controllable clocks, and synchronization barriers cover
blocked-provider intake, association and revision handling, corrections during
inference and execution, untrusted references, runnable-job ordering, recursion
and late child results, quiet continuations and owned timer routing, duplicate
observations and wakes, budget exhaustion across restart, stop latches and
explicit resume, admission races and unknown attempts, checkpoint restoration and
interrupted cells, projection rebuild, and a reference scenario that survives a
correction during blocked tests and a restart while retaining reply provenance.

Boundary tests live in `crates/pluribus-cognition/tests/rlm_cognition.rs`. RLM
transition tests live in `plugins/rlm/cognition/src/persistent_tests.rs`. The
reference scenario uses packaged echo as its deterministic test provider: it runs
no shell command, opens no real PR, and sends no message. Runtime regressions
cover restricted destructive grants, provider backoff across restarts, uncertain
effects, incremental dispatch projections, and reserved operator events. Live
integrations remain separate.
