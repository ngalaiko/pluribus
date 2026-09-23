# Async runtime inspection

Async improves concurrent I/O and cancellation. It does not accelerate model inference, JavaScript computation, or SQLite transactions.

## Evidence

The synchronous socket service held its entire stream registry locked during reads and 10 ms polling sleeps. A ready stream waited 357.898 ms behind an unrelated 400 ms idle read. The regression failed before the transport change.

On Darwin arm64 with Rust 1.95.0, the async regression measured 7.958 µs for the ready stream. A separate single-thread test with 127 idle streams measured 177.5 µs. These are local socket latency samples, not end-to-end throughput estimates.

A pending HTTP cancellation test also failed before cancellation handling. It passes with a 100 ms ceiling after cancellation is requested at 10 ms. An upstream socket test confirms that closing an idle HTTP stream disconnects its server.

## Execution

- Tokio drives agent delivery tasks, Wasmtime lifecycle calls, HTTP, DNS, and streams.
- Wasmtime async host imports suspend the guest during I/O. Epoch callbacks yield CPU-bound guests every 10 ms while retaining deadline and cancellation traps.
- Cancelling an agent wait retains ownership of its provider. Dropping a lifecycle future invalidates that instance; reinstantiation reads the durable checkpoint.
- Each instance still owns one mutable store and receives one lifecycle call at a time. Package components stage and validate first, then initialize and rebuild independently in parallel; each component keeps init before rebuild. Cognition remains serialized. Independent providers share the executor instead of spawning delivery threads.
- HTTP uses bounded async channels. Dropping a stream aborts its reader; abandoned response uploads are cleaned up. Socket readiness replaces polling sleeps and registry-wide locking. Writes remain serialized per socket.
- Committed events and completed deliveries notify the agent. Idle waits remain bounded to discover external database writes.
- Storage imports, projection replay, blob transfers, credential resolution, and delivery commits await asynchronous storage APIs. Compilation uses blocking workers. A database transaction already executing finishes atomically if its awaiting task is dropped.

## Boundaries

SQLite uses `async-sqlite` with one dedicated worker per connection. Event, state, delivery, credential, snapshot, and retention APIs are async. Each transaction stays inside one worker operation. Router projections use an async mutex; readers await storage directly. Blob storage uses an async API and Tokio file I/O. Uploads have separate async locks; the registry lock covers only bookkeeping. Admitted writes and publication retain ownership until completion, preserving offsets when a caller cancels. Temporary-file creation, atomic publication, and cleanup use blocking workers.

Single-thread executor regressions measured heartbeat delays of 317 ms behind a busy database connection and 326 ms behind a locked blob upload. Both async implementations pass a 100 ms responsiveness bound while contention lasts 300 ms. Another regression verifies that an unrelated upload progresses and a cancelled write can be replayed without duplicating bytes.

The [driver](https://docs.rs/async-sqlite/0.6.0/async_sqlite/) runs SQLite on background threads. This improves executor responsiveness; it does not make SQLite queries faster or permit concurrent writers. Queued operations cancelled before execution are skipped; an operation already running may still commit. Interrupted lifecycle instances require reinstantiation before reuse.

Package downloads use async reqwest and streamed Tokio file writes. Resolution runs up to four packages concurrently. File operations are cancellable while waiting; hashing, extraction, verification, and atomic publication use blocking workers. Configuration updates publish only after all packages validate.

Maintenance filesystem work and terminal prompts remain synchronous. The epoch ticker and emergency-stop monitor retain dedicated threads so runtime scheduling cannot delay their signals.

The guest WIT contract and packaged Wasm files are unchanged. Rust callers must await storage, package resolution, transport, credential, lifecycle, and agent execution APIs. One Tokio runtime belongs at the application boundary; library calls do not create nested runtimes.

## Checks

Verified: the workspace tests pass with opt-in tests skipped. Separate runs pass the package tests, the HTTP tests, and the packaged credential-refresh fixture. Clippy and formatting checks pass. Checks cover transaction atomicity, recovery, package integrity, cancellation, and executor responsiveness.

```sh
cargo test --locked --workspace
cargo test --locked -p pluribus-host-http -- --include-ignored
cargo test --locked -p pluribus-host-stream -- --nocapture
cargo clippy --locked --workspace --all-targets
```

Network fixtures require loopback access. Provider-backed evaluations remain opt-in; they consume external API resources.
