# Resource exhaustion and recovery

## Goal

An oversized operation produces a bounded, model-visible failure. Other jobs
continue. Cognition can recover without loading the state that exhausted memory.

## Evidence and constraints

- Production cognition failed with a 32 MiB Wasm memory limit. Replaying with
  128 MiB passed the failing input. The original allocation error was lost, so
  memory exhaustion is strongly indicated, not conclusively classified.
- Persisted cognition state grew from approximately 5.5 MB to 12.3 MB. Serialized
  state size is not peak memory usage.
- `load_engine` loads all jobs, inbox entries, tasks, calls, budgets, and queue.
  Processing also retains serialized before/after records and JSON intermediates.
- A cognition trap quarantines the shared component indefinitely. The REPL's
  pinned-session recovery has different semantics and cannot simply be enabled
  for cognition.
- Keep the production limit at 128 MiB during implementation. Never let the model
  increase host limits or clear quarantine.
- This covers guest resource exhaustion while the host remains alive. Host or OS
  OOM kills require process supervision and restart reconciliation.

## 1. Preserve and classify failures

Files: `crates/pluribus-runtime-wasm/src/{lib,runner}.rs`.

- Complete the existing pending-delivery error propagation fix and regression.
- Wrap the Wasmtime resource limiter to record denied growth: resource, current
  bytes, requested bytes, and configured limit. Preserve existing table, instance,
  and memory-count restrictions.
- Add a typed runtime failure category. Classify memory-limit failures from the
  limiter evidence, not substring matching on a Wasm backtrace. Reset evidence
  per call; keep unknown traps distinct from resource exhaustion.
- Carry phase (`startup`, `restore`, `delivery`, `execution`) and input identifiers
  into host failure events. Record only available measurements; requested growth
  is not a measured peak.
- Keep event additions backward compatible. Use the existing WIT
  `resource-exhausted` error code where applicable; avoid an ABI change unless a
  required field cannot be carried through existing error details.

Test first: a packaged guest exceeds a deliberately small limit during delivery.
Assert the original category and limit reach the host, the cursor is unchanged,
and no partial state commit occurs. An unrelated trap must remain unclassified.

## 2. Bound cognition state before enabling recovery

Files: `plugins/rlm/cognition/src/{lib,storage,engine,jobs}.rs`.

- Replace whole-engine loading with a small scheduler index and per-job records.
  Resolve results through request-to-job indexes; load only the affected job and
  its tasks. Observation association receives a bounded, paginated candidate
  list. Historical lookup continues through the existing unified event query.
- Replace full before/after serialization with explicit dirty records and
  tombstones. Retain atomic checkpoint/event/state commits.
- Enforce byte limits before materializing records, query results, model input,
  and checkpoint output. Bound individual records as well as pages and batches.
  A single oversized job must produce a resource error without blocking loading
  of the scheduler index.
- Archive settled jobs outside the active index. Preserve authoritative history,
  unresolved effects, and deduplication records. Count limits alone are inadequate
  because record sizes vary.
- Build indexes from existing state incrementally with a durable migration
  cursor. Read legacy records; do not require loading the full legacy engine to
  migrate it. Resume migration safely after a crash.

Test first: many unrelated large jobs must not increase the loaded bytes for one
small job. Cover one oversized record, restart during migration, and equivalent
checkpoint/replay behavior. Measure packaged Wasm memory, not just JSON size.

## 3. Recover and isolate work in the host

Files: `crates/pluribus-cognition/src/{agent,dispatch}.rs`, event schemas.

- Persist recovery state keyed by component, failing delivery/request, and phase.
  Give each failure at most two automatic recovery attempts with backoff. Restart
  must not reset the allowance.
- Reinstantiate a failed guest from the last committed checkpoint. For a
  resource-limited batch, reduce the batch to individual deliveries. Do not
  repeatedly retry an identical oversized single operation.
- If one job remains too large, atomically record its deferred work and resource
  failure before advancing the shared delivery cursor. Resume from that durable
  record, with its job revision and ordering intact. Never silently skip an input.
- Keep blocked jobs out of the active scheduling set so unrelated conversations
  continue. Bound recovery concurrency and memory independently of normal work.
- Keep startup/restore failures separate: rebuild only the bounded scheduler
  state. If that fails, mark cognition unhealthy and notify through a host-owned
  status path; do not depend on cognition to explain its own outage.
- Audit shared REPL sessions: record every session lost when its component dies.
  Do not claim per-job isolation while unrelated sessions share a failing guest;
  use separate bounded guest instances where session isolation is required.

The host persists a `component.backoff` record for each instance and failing
delivery (`requestEventId`) with phase, retry time, and attempt count. A
resource failure permits two automatic attempts; the record is replayed during
health refresh, so a restart cannot reset the allowance. The first failure
switches the instance to one-input deliveries. After the third failure for a
single input, the host commits `cognition.resource-exhausted` and the delivery
cursor in one delivery transaction. The event is keyed by instance and input,
contains the resource measurements and checkpoint status, and remains the
durable deferred record consumed by cognition. The host never advances the
cursor without that record.

Test first: poison one job while another receives a message. The second must
complete. Crash between failure recording and cursor advancement; recovery must
neither lose the first input nor duplicate the second operation.

## 4. Deliver actionable failures to the model

Files: `crates/pluribus-cognition/src/dispatch.rs`,
`plugins/rlm/cognition/src/engine.rs`, RLM prompt sources.

- Route a structured failure into the owning job's next bounded model turn:
  `code: resource-exhausted`, operation/request ID, phase, resource, configured
  limit, available measurements, session/checkpoint status, effect status, and
  remaining retry allowance.
- Use existing operation failure events for tool/REPL failures. Use a distinct
  host-owned job resource event for cognition loading failures; do not pretend a
  model or tool call occurred when none was admitted.
- Tell the model to reduce query limits, paginate, process smaller chunks, retain
  references instead of large results, and reduce parallel work. Require a smaller
  bounded request on retry; enforce this in the host where measurable.
- Preserve `outcome-unknown` for operations whose external effects may already
  have occurred. Resource exhaustion does not prove an action was rolled back.
  Reconcile receipts before repeating an effect; restore only committed REPL
  checkpoints and report lost transient variables.
- End the affected job with a clear explanation when its retry budget is spent.
  Deduplicate failure notifications and scope them to the original conversation.

`cognition.resource-exhausted` is core-owned and host-authenticated. Its
payload carries `instanceId`, `requestEventId`, `inputEventId`, optional
`jobId`/`sessionId`, `code`, `resource` (`resource`, `currentBytes`,
`requestedBytes`, `limitBytes`, `phase`), `remainingAttempts`,
`effectStatus`, `checkpointStatus`, and `deferred`. Cognition handles this as a
job resource failure and creates the next bounded model turn. Existing
operation failures keep `outcome: "unknown"` (or the equivalent resource
error field) whenever admission crossed an external effect boundary.

Test first: a scripted model receives a resource error, retries a smaller read,
and succeeds. Cover repeated oversized retries, stale job revisions, missing
checkpoints, and an external action completed before the failure.

## 5. Visibility and rollout

- Report cognition as unhealthy when quarantined, even if ingestion is alive.
  Expose memory allocation/growth, denied allocations, loaded state bytes,
  recovery attempts, deferred jobs, and oldest pending-message age.
- Alert once on failure-state transitions; recovery emits a matching event.
  Keep alerts independent of model inference.
- Run packaged Wasm integration tests with synthetic large state, deliberately
  low limits, multiple jobs, and repeated crash/restart cycles. Prove bounded
  memory growth and no duplicated external effects.
- Run formatting, focused runtime/cognition tests, then required workspace checks.
  Recheck the existing loopback test outside the network sandbox.
- Replay a fresh production backup without external providers. Verify migration,
  cursor progress, deferred work, and memory measurements before deployment.
- Back up production; deploy compatible host/plugins/config together. Keep
  automatic recovery disabled until bounded-state replay passes. Enable recovery,
  verify a user reply and continued processing, then observe memory growth.
- Retain the previous package/config and database backup. If migration is not
  backward readable, roll back the matching database too, accounting explicitly
  for any effects issued after the backup.

## Acceptance

1. A guest memory-limit failure reaches the model with a typed resource error.
2. The model can complete a smaller retry within an enforced allowance.
3. One oversized job cannot silence unrelated conversations.
4. Recovery survives process restarts without losing inputs or duplicating effects.
5. Historical state growth does not force proportional per-delivery memory growth.

Order: failure classification, bounded state, host isolation, model adaptation,
then production recovery rollout. The 128 MiB limit is operational headroom,
not the acceptance criterion.


## Implementation

- Wasmtime denied-growth evidence survives both direct and source-loop delivery.
  Unknown traps retain quarantine behavior.
- The host persists per-request retries, reduces failed batches, and atomically
  records deferred input before advancing its cursor. Two retries follow the
  initial attempt. Shared REPL failures report every lost session; admitted cells
  are not replayed. Startup/restart failures use host-owned unhealthy events.
- Cognition loads selected records, compact routing metadata, and at most four
  queued sessions. Records are capped at 256 KiB; the selected view at 2 MiB.
  Incremental legacy migration keeps a durable cursor. Large observation fields
  become references to their authoritative events. Mutation diffs cover only
  loaded records; indexes and sequence are included in checkpoint replay.
- Oversized stored jobs/tasks recover from compact metadata. The triggering input
  remains in the delivery. Recovery retains conversation identity, revisions,
  child history scope, and parent continuation IDs. Unavailable checkpoints are
  reported as missing.
- Model recovery exposes resource details, lost-session/effect status, and its
  remaining allowance. History reads/searches are capped at 16 rows after the
  first failure and 8 after the second. Identical failed cells are rejected.
  The third failure ends the affected task.

Compact-index scans still grow with historical record count; loaded payload
memory does not. Further indexing can reduce scan time. Denied-growth values
are allocation evidence, not continuous peak-memory measurements.

## Verification

Regression coverage includes real guest OOM followed by a bounded successful
model retry; restart-persisted host attempts; atomic deferral; an oversized
active job beside another message; a 19 MB archive; large-observation replay;
checkpoint/child-scope restoration; and escaped-Unicode migration boundaries.

Production-data replay and deployment remain separate rollout gates. No new
host/plugin code was deployed during implementation. Automatic approval review
rejected exporting private production state for a local fixture; that replay
requires approval or an isolated replay on the remote host.

Checked locally:

- Core: 19 unit tests.
- Cognition host: 30 unit tests and 4 failure/restart integration tests.
- Cognition plugin: 122 unit tests; strict native/Wasm Clippy passes.
- Packaged RLM: 39 tests pass; two live-provider/export tests are skipped.
- Runtime: 48 unit tests and 15 packaged integration tests, including loopback
  HTTP outside the network sandbox.
- Real packaged REPL OOM after a history-read resume reaches the model; its next
  1,000-row request is capped at 16, then the job completes.
- Formatting passes. Workspace-wide strict Clippy still reports pre-existing
  warnings in HTTP/stream/WASI/shared HTTP code.
