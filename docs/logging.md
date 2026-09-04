# Logging

The event store already records what the agent does: `observation.received`,
`model.requested`, `capability.requested`, `policy.decision`, attempts,
quarantines. Logs describe the process instead: how a run is wired, what
transports are doing, why something failed before it could become an event. A
log line that restates an event does not belong.

## Levels

- `error`: this run cannot do what it was asked and nothing upstream reports it.
  Always carries the identifier that finds it in the event log.
- `warn`: degraded but continuing.
- `info`: one bounded line per state change a person would ask about.
- `debug`: transport detail, off by default.

## What never appears

Message text, model prompts or completions, shell commands or their output,
credential values, blob contents, configuration values beyond names.
Identifiers only: instance, component, event, capability, digest, ms.

Nothing per tick, per poll, or per event. Log transitions, not repetitions.

## Form

One line, `key=value`, stable keys, stderr in every binary. The cli bridge owns
stdout for the conversation, so a log line on stdout corrupts it.

`PLURIBUS_LOG` selects the filter, default `info`. Levels do not change when
stderr is not a terminal; only colour does.
