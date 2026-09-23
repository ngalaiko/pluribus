# Scheduler

Owns durable timers and scheduled observations. Core routes events and enforces
origin authority; it does not execute timers. Install one scheduler per agent.
RLM waits and GitHub/OpenAI credential timers also require this plugin.

## Tools

| Capability | Arguments |
| --- | --- |
| `schedule.create` | `name`, `prompt`, `timing` |
| `schedule.list` | Optional `after`, `limit` (1–100) |
| `schedule.get` | `id` |
| `schedule.update` | `id`, any of `name`, `prompt`, `timing` |
| `schedule.pause` | `id` |
| `schedule.resume` | `id` |
| `schedule.delete` | `id` |

Timing forms:

```json
{"kind":"at","at":"2026-09-24T09:00:00+02:00"}
{"kind":"after","milliseconds":600000}
{"kind":"cron","expression":"0 9 * * MON-FRI","timezone":"Europe/Stockholm"}
```

`at` requires an RFC 3339 timestamp with an offset. `after` runs once after the
positive delay, measured when the request is handled. Cron uses five fields:
minute, hour, day of month, month, weekday. Timezones are IANA names; UTC is
explicit. Croner supplies calendar and daylight-saving semantics.

Only the originating sender in the same conversation can manage a schedule.
IDs come from creation receipts. Results expose revision, next and last scheduled
time, pause state, and recurrence calculation errors. Updating timing resets
the next occurrence; updating a paused schedule leaves it paused. Completed
one-time schedules need a new timing rule before resuming.

## Execution

Each occurrence emits `observation.received` with the saved prompt and a
`schedule` object containing its ID, revision, scheduled time, and emission time.
The observation retains the originating provider, sender, conversation, and
trust. `originEventId` and causation link it to the creation or update request;
the host validates inherited authority and rechecks current grants for effects.
Replies use the original conversation. Scheduling does not reserve authority.

Missed cron occurrences coalesce into one observation on restart or resume;
the next deadline is calculated after the current time. Overdue one-time
schedules fire once. Occurrences may overlap; each creates independently
routed work. Pausing or deleting prevents future emissions but does not cancel
work already emitted. The process must be running to deliver observations.

Occurrence, next-state receipt, and private state commit atomically. Stable
occurrence keys prevent duplicate observations on retry. `schedule.updated`
receipts rebuild state after loss. Timers replay `timer.set`, `timer.cancel`, and
`timer.fired`, including receipts from the former host executor. Cancellation
requires the original request actor. WASI clock waits suspend the source loop;
wall-clock rechecks are bounded to one second.

## Installation

```json
{"plugin_instances":{"scheduler":{"package":"bundled:scheduler","config":{}}}}
```

Configure its component with `{}`. Add `scheduler` to `capability_instances` and
grant the required `schedule.*` capabilities individually through
`trusted_capabilities`, with `{}` constraints. `init --example` includes these
grants. Existing agents must install and configure this plugin to retain timers.
Removing it suspends timers and schedules until it is installed again.

## Checks

```sh
cargo test --locked -p pluribus-plugin-scheduler
cargo test --locked -p pluribus-cognition --test agent_loop scheduler_plugin
```

Build and package the Wasm component before integration tests; see
[development checks](../../README.md).
