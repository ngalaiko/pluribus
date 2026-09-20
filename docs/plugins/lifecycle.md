# Lifecycle and upgrades

## States

```text
discovered → validated → installed → configured → starting → active
                                      ↘ failed
active → draining → stopped
active → failed
installed → removed
```

Only `active` instances receive deliveries.

## Installation

Before storing a plugin, the host:

1. parses `plugin.toml` with duplicate-key rejection;
2. validates the manifest schema;
3. rejects unsafe paths and symlinks;
4. verifies component SHA-256;
5. parses the component without executing it;
6. validates the declared WIT world identifier;
7. checks exact ABI version;
8. compares actual imports and exports with the manifest;
9. validates the `lifecycle` export signature;
10. validates configuration, capability, credential, and flow schemas;
11. registers declared event types and records requested capabilities.

Installation does not instantiate, configure, grant, or activate the plugin.

## Configuration and grants

An instance binds:

- plugin ID and version;
- agent;
- configuration revision;
- state namespace and schema version;
- granted host imports;
- capability grants;
- resource limits.

Changing configuration restarts the instance. Changing grants affects new activities immediately; existing activities retain their immutable authority but may be cancelled by emergency stop or plugin revocation.

Required capability requests without matching grants prevent activation. Optional requests remain denied.

## Startup

The host calls `lifecycle.run(context, config)` once with validated configuration
and opaque credential handles. The plugin validates remaining constraints,
loads local configuration, and calls `runtime.ready(events, mutations)` promptly.

`ready` submits startup output for an atomic commit and suspends the loop.
The host replays history through `handle` before activation releases `ready`.
Startup must tolerate empty state and must not send messages or begin polling.
An error or timeout before readiness prevents activation.

Nothing is discovered by calling the plugin. Capabilities, models and
credentials are manifest data, so activation needs no descriptor round trip.

There is no health call. Liveness is whether the instance answered its last
delivery.

## Stop

The host stops dispatch, waits for active calls until the supplied deadline, then calls `lifecycle.stop`. `stop` releases ephemeral resources and closes sessions. It MUST NOT start new activities.

The host may interrupt at the deadline. `stop` is best-effort and is not called after every trap, crash, host shutdown, or machine failure. Durable correctness cannot depend on it.

## State schema changes

Plugin maintainers own state compatibility and migration. Breaking changes may
use a new major package version. The host neither compares state schema versions
nor clears namespaces on version changes.

## Updates

An update with the same plugin ID is staged beside the active version. The host
validates it as a fresh package, checks grant expansion, drains the old
instance, runs the candidate to `ready`, and switches routing atomically.

If startup fails, the host restores old code, state, configuration, and routing.
External effects made before failure cannot be rolled back; startup forbids them
for this reason.

Active calls finish on the old version during draining unless cancelled. New calls use only one version. A call never changes plugin code midway.

## Removal

Removal revokes dispatch, drains or cancels calls, and calls `stop`. State, configuration, package bytes, and audit events remain retained unless an operator separately purges operational state.

Removing a plugin does not remove capability grants by name. A request for a
capability with no installed provider terminates with `capability.denied`
rather than waiting, so a requester is never left hanging on a removed plugin.

## Failure and restart

A component reports a failure by returning an error from an export. A component
that traps reports nothing: the call unwinds with no outcome, and the registry
refuses a plugin that proposes `component.failed`, so only the host can witness
a death. The two are handled differently.

A returned failure commits nothing, so the cursor does not advance and the same
events are delivered again. Terminal results already appended through
`events.append` deduplicate on their keys rather than duplicating.

A trap records `component.failed`, carrying the instance, the reason, and the
delivered sequence range. The host then advances the cursor past that batch and
withdraws the instance from delivery, because redelivering a batch that killed
a component only kills it again. Restoring an instance is an operator decision;
`component.failed` is the durable record of why it left.

Automatic restart under bounded exponential backoff is not implemented. A
restart keeps configuration and durable state but discards linear memory, open
HTTP streams, blob uploads, and delivery-scoped handles, and loses any pinned
session, which fails its activity.

The host does not automatically retry:

- non-idempotent capability calls with ambiguous outcomes;
- failed startup calls;
- invalid arguments;
- permission denials.

A request that already has a terminal result in the log is never delivered
again, which is what stops an external effect from repeating.

## Compatibility

Four versions are independent:

- manifest version;
- Pluribus ABI package version;
- plugin version;
- plugin-owned payload and state formats.

ABI `3.0.0` requires an exact match. The host may support several ABI versions side by side later.

Minor versions may add a standard world; patch versions may clarify behavior or fix documentation. Neither changes an existing interface type; that requires a new major ABI package version.

Plugin SemVer describes plugin behavior. Changing capability semantics, schemas, provider IDs, state interpretation, or required authority is breaking even when WIT is unchanged.

## Source loops

Every plugin starts through `lifecycle.run(context, config)` and waits at
`runtime.ready` until activation and replay finish. Handler-only plugins then
await `runtime.next` and dispatch events to their handler through a shared loop.

The plugin owns polling, framing, downloads, retries, and pacing. It awaits
`wasi:http/client.send`, native byte streams, WASI clock waits, or `runtime.next`. These suspend
without occupying a worker thread. Core never repeats a network request.

A source selects between its pending operation and `runtime.next()`. Internal
deliveries invoke its own handler, then commit or reject the result before
waiting for another delivery. One task owns the plugin state; core never
re-enters its Wasm instance. Requests keep running while the loop handles
internal events.

`runtime.commit(events, mutations, checkpoint)` commits atomically. External
observations require stable idempotency keys and omit the checkpoint. Internal
deliveries must supply a checkpoint from their batch. `runtime.reject(error)`
leaves that batch available for retry. Empty commits write no events; an idle
source simply awaits work.

Shutdown wakes the loop, waits for its async task and subtasks to exit, then
calls `stop`. Cancellation interrupts waits. A restart starts
a fresh `run` call with the committed state. Compute between waits remains bounded.
