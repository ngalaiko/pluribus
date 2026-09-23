# REPL component

Boa packaged as Wasm, with lifecycle coordination imports but no WASI,
filesystem, network, or credential access. The separate [cognition plugin](../README.md) owns
reasoning and dispatch.

## JavaScript API

These helpers yield work to cognition; they do not give the interpreter direct
storage or network access:

- `history.read({after, limit, eventTypes})`: authorized history, bounded to
  100 events / 64 KiB. Oversized payloads return `payloadOmitted`; cursors advance.
- `history.search({...})`: bounded lexical [history search](../history.md).
- `rlm.query({question, context})`: a read-only child with at most 64 KiB of context.
- `capabilities.invoke(name, arguments)`: a root capability call; its receipt
  contains the provider result in `output`.
- `checkpoint({named: values})`: preserve up to 32 KiB of JSON working values.

`context.turn` carries the [model turn context](../turn-context.md). Child history
cannot exceed delegated access. Filter history to relevant event types.

## Sessions

Requests identify a `sessionId`. `code.evaluate-requested` starts a cell in that
session, creating its realm if necessary. Subsequent cells preserve `state`.
`code.resumed` resolves a yielded promise. Results carry the same session ID as
`code.yielded`, `code.completed`, or `code.failed`. `code.close-requested` releases
a session and emits `code.closed`.

```js
state.page = await history.read({after: 0, limit: 100, eventTypes: ['observation.received']});
state.answer = await rlm.query({
  question: "What remains unfinished?",
  context: state.page.events,
});
return state.answer;
```

Parents and children use separate sessions. Up to 128 sessions can be resident.
The interpreter bounds loops and output; core supplies memory and
wall-clock ceilings. Date and randomness are deterministic, not cryptographic.

The heap survives deliveries within the instance. Every successful cell
automatically exports up to 32 KiB of JSON `state` at its completion boundary.
`checkpoint({named: values})` remains available for an explicit subset and keeps
the version 1 format; once called, that explicit snapshot remains authoritative
for later cells. New version 1 snapshots carry `mode: "automatic"` or
`mode: "explicit"`; older snapshots without the field use explicit behavior. A
fresh realm restores the latest snapshot into `state`
and sets `context.recovered` to true. State that is invalid JSON or exceeds the
UTF-8 byte limit leaves the cell successful and adds a warning to its result;
larger data needs blob/history references. Missing sessions cannot resume
suspended cells; core reports interruption without replaying admitted source.

The `repl` component of the rlm package, not a plugin of its own: it ships inside
that package beside `cognition`, and an rlm instance configures both.

Context, checkpoints, and host responses enter Boa as JSON values, not source.

## Development

From the repository root, run `cargo test --locked -p pluribus-plugin-rlm-repl`.
See [workspace setup](../../../docs/development.md) for packaged checks.
