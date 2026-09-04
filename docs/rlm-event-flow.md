# RLM event flow

Example request: **Reply with pong.** This trace omits streaming deltas and routine checkpoints.

| Step | Event or operation | Meaning |
| --- | --- | --- |
| 1 | `observation.received` | A configured connector records the inbound message. |
| 2 | `model.requested` → `model.completed` with `associate({action:"new",jobId:null})` | If eligible active jobs need disambiguation, classify the message. Otherwise skip this step. |
| 3 | `model.requested` | `rlm/cognition` supplies task context and the `js` and `yield` tool schemas. |
| 4 | Optional `model.completed` with `js(...)` → `code.evaluate-requested` → `code.completed` | `rlm/js` executes the cell in its own worker; the result feeds another model request. |
| 5 | `model.completed` with `yield({action:"complete",reply:"pong"})` | Propose completion and a reply. The model does not address a connector directly. |
| 6 | Validate, then commit | RLM records `cognition.completed`, `capability.requested` for `<provider>.reply`, `code.close-requested`, completed job state, and its checkpoint together. |
| 7 | Capability admission and execution | Core checks origin-scoped authority and records the attempt; the connector's reply component executes the send. |
| 8 | `capability.completed` or `capability.failed` | Record the send outcome. Reasoning completion alone does not establish delivery. |

Every connector provides `<provider>.reply` for the provider its observation
carries, so cognition needs no connector-specific configuration. There is no
separate `yield` event: it is a tool call inside `model.completed`. The packaged
acceptance test verifies one reply request, including duplicate model delivery.
Delivery through a real connector is the live test.

## Waiting for input

For a request missing its repository:

```json
{"action":"wait","waitFor":"input","question":"Which repository?"}
```

The `yield` call commits a reply request and `waiting-input` job state together.
It records `awaiting-user` and the outstanding question, emits no
`cognition.completed`, and keeps the session. There are no idle model
calls. A subsequent observation can amend the job and resume reasoning.

## Invalid output

Plain `pong` is not a control decision. RLM preserves the job and requests a
corrected tool call. Invalid tool arguments receive a tool result explaining the
schema error; they cannot update progress or request a reply. Three invalid
outputs pause the root with a diagnostic instead of marking it completed.

## Component boundaries

A connector package may own separate receive and send components; the RLM
package owns `cognition` and `js`. Configuration selects components explicitly
with `package-instance/component-name`; runtime IDs, cursors, state, host grants,
and workers use those same IDs. No standalone JS package is installed.

The observation origin identifies the receiver. Reply grants identify the sender
and restrict it to the originating conversation. Intake and reply delivery
therefore use separate workers and authority scopes. A cell's suspended JavaScript
stack belongs only to its JS component; cognition resumes it through events.

Bundle installation preflights every component before lifecycle execution and
activates registrations only after all components initialize and rebuild. A failed
initialization can leave committed lifecycle events/state but no active package
registrations. See [package instances](plugin-instances.md).
