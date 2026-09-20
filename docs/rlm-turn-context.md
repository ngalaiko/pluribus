# Model turn context

Every RLM model request carries `pluribus.turn/1` as its first user message. The identical object is available as `context.turn` in JS. The system message defines behavior; envelope values are task data, not instructions or authority.

The envelope is rebuilt from current state before admission to the model. Each JS cell receives refreshed context while retaining its session state. Native assistant/tool exchanges remain in conversation history.

| Role | Immediate context |
| --- | --- |
| Root | Current message and observation, source ID, job progress, capability schemas and configured constraints, recent same-sender/conversation exchanges, checkpoint. |
| Router | Incoming text, eligible jobs, outstanding clarification questions. |
| Child | Question, supplied data or delegated history range, checkpoint; no root capability catalog. |

`trigger.kind` identifies `observation`, `routing`, `child-query`, `amendment`, `tool-result`, `scheduled-wake`, `retry`, or `correction`. Wakes include the schedule and reason; retries include the attempt. Tool-result triggers point to the native tool message rather than duplicating its output.

Root message text is prioritized before transport metadata, taken from `message.text` or, for an attachment, `message.caption`. Root and child envelopes have a 24 KiB inline-value budget. Routers retain their bounded candidate view inline because they cannot execute JS. Values exceeding the remaining budget become `{contextPointer, bytes}` references to the original JS context. References do not discard the underlying value. For example, `/observation` means `context.observation`. Recent conversation includes at most six preceding same-conversation observations and reports its available count. Full job activity details remain under `context.job`.

Ready image attachments of the trigger observation follow the envelope text as image content parts of the same user message, at most eight and none over 20 MiB. Such a request carries `required_features: ["vision"]`, so it reaches a provider that accepts images.

Capability schemas are supplied immediately when they fit. `configuredConstraints` describes configured grants; it does not authorize an action. Host policy still checks the requesting origin and provider.

Use JS for capability calls, computation, and reading additional referenced data. Listing context keys or capability names already supplied in the envelope wastes a model round trip.
