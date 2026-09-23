# Runtime execution

Async execution allows independent I/O and cancellation. It does not accelerate
model inference, JavaScript computation, or SQLite transactions.

## Execution

- Tokio drives agent delivery tasks, Wasmtime lifecycle calls, HTTP, DNS, and streams.
- Wasmtime async host imports suspend the guest during I/O. Epoch callbacks yield CPU-bound guests every 10 ms while retaining deadline and cancellation traps.
- Cancelling an agent wait retains ownership of its provider. Dropping a lifecycle future invalidates that instance; reinstantiation reads the durable checkpoint.
- Each component owns one mutable store and receives one lifecycle call at a time. Package components stage and validate first, then initialize and rebuild independently in parallel; each component keeps init before rebuild. Cognition remains serialized. Independent providers share the executor instead of spawning delivery threads.
- HTTP uses bounded async channels. Dropping a stream aborts its reader; abandoned response uploads are cleaned up. Socket operations await readiness without holding the registry lock. Writes remain serialized per socket.
- Committed events and completed deliveries notify the agent. Idle waits remain bounded to discover external database writes.
- Storage imports, projection replay, blob transfers, credential resolution, and delivery commits await asynchronous storage APIs. Compilation uses blocking workers. A database transaction already executing finishes atomically if its awaiting task is dropped.

## Boundaries

SQLite uses `async-sqlite` with one dedicated worker per connection. Event, state, delivery, credential, snapshot, and retention APIs are async. Each transaction stays inside one worker operation. Router projections use an async mutex; readers await storage directly. Blob storage uses an async API and Tokio file I/O. Uploads have separate async locks; the registry lock covers only bookkeeping. Admitted writes and publication retain ownership until completion, preserving offsets when a caller cancels. Temporary-file creation, atomic publication, and cleanup use blocking workers.

The [driver](https://docs.rs/async-sqlite/0.6.0/async_sqlite/) runs SQLite on background threads. This improves executor responsiveness; it does not make SQLite queries faster or permit concurrent writers. Queued operations cancelled before execution are skipped; an operation already running may still commit. Interrupted lifecycle instances require reinstantiation before reuse.

Package downloads use async reqwest and streamed Tokio file writes. Resolution runs up to four packages concurrently. File operations are cancellable while waiting; hashing, extraction, verification, and atomic publication use blocking workers. Configuration updates publish only after all packages validate.

Maintenance filesystem work and terminal prompts remain synchronous. The epoch ticker and emergency-stop monitor retain dedicated threads so runtime scheduling cannot delay their signals.

Rust callers await storage, package resolution, transport, credential, lifecycle, and agent execution APIs. One Tokio runtime belongs at the application boundary; library calls do not create nested runtimes.

## Resource recovery

- Wasmtime denied-growth evidence survives both direct and source-loop delivery.
  Unknown traps retain quarantine behavior.
- The host persists per-request retries, reduces failed batches, and atomically
  records deferred input before advancing its cursor. Two retries follow the
  initial attempt. Shared REPL failures report every lost session; admitted cells
  are not replayed. Startup/restart failures use host-owned unhealthy events.

Denied-growth values are allocation evidence, not continuous peak-memory
measurements.

See [RLM persistence](../plugins/rlm/persistent-work.md) for job recovery,
[REPL checkpoints](../plugins/rlm/repl/README.md) for session limits, and
[development](development.md) for verification commands.
