# Operations

## Paths

Without overrides, configuration, state, cache, and endpoints use platform
conventions from [pluribus-paths](../crates/pluribus-paths/README.md): XDG directories
on Linux and application directories on macOS. Endpoints use the runtime directory
where available, otherwise the state directory.

| Location | Contents | Override |
| --- | --- | --- |
| Configuration | `config.json` | `--config-dir` |
| State | SQLite database, credentials, blobs, STOP marker | `--data-dir` |
| Cache | Verified package archives and extracted contents | `--cache-dir` |
| Runtime | Local helper endpoints | `--runtime-dir` or helper socket flag |

Directory flags are independent. `--data-dir` changes only state; configuration,
cache, and runtime directories retain their defaults. `pluribus --help` displays
the resolved platform defaults. Native helpers accept `--socket` or their own
`--data-dir` endpoint-directory override.

For an isolated agent, set all four locations:

```sh
pluribus --data-dir ./agent --config-dir ./agent --cache-dir ./agent --runtime-dir ./agent init --example
pluribus --data-dir ./agent --config-dir ./agent --cache-dir ./agent --runtime-dir ./agent run
```

See [instances](plugin-instances.md) for configuration and
[installation](installation.md) for package resolution. `pluribus plugins auth <instance>`
enrolls or replaces credentials; configuration stores handles, not secret values.

## Event log

```sh
pluribus logs
pluribus logs --limit 20
pluribus logs --follow # -f
```

Prints the configured agent's latest 100 stored events as JSON lines, oldest
first. `--follow` prints subsequent events until interrupted. Reads the existing
database without migrations or plugin startup. Use the runtime's configuration
and state directory overrides.

## Stop and resume

```sh
pluribus stop
pluribus run --resume
```

Use the same path overrides as the running agent. The stop flag persists across
restart. Stop prevents new admission and signals active work; cancellation cannot
undo an admitted external effect. Unknown outcomes require reconciliation.

Provider failures use backoff. Traps quarantine components except for bounded
resource recovery and pinned-session recovery; see [runtime](runtime.md) and
[RLM operator controls](../plugins/rlm/persistent-work.md#operator-controls).

## Backup and restore

Stop the runtime and helpers. Copy configuration and the entire state directory,
including credentials and blobs. Include completed package caches for offline
restore. When directories are separated, copying state alone omits configuration
and cache.

Restore package references as well as state. Absolute directory URLs must exist
on the restore host; `bundled:<name>` uses that host's installation. Upgrading or
rolling back package code does not undo state migrations or external effects.
Operators coordinate concurrent runners and installations; the CLI does not lock
them.

## Diagnose

[Logs](logging.md) describe process transitions; the event store records agent
activity. Check component failures, capability outcomes, and unresolved attempts
before retrying effects. Reasoning completion alone does not prove reply delivery.
See [development](development.md) for fixture-based verification.
