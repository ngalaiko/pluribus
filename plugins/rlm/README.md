# RLM

RLM owns reasoning, durable jobs, and observation association. Its package has
separate [cognition](cognition/README.md) and [JavaScript REPL](repl/README.md)
components. The [host router](../../crates/pluribus-cognition/README.md) delivers
events, admits work, and records results.

## Configure

Add to the agent's existing instance registry:

```json
{"plugin_instances": {"rlm": {"package": "bundled:rlm"}}}
```

The agent also needs a connector and configured model provider. Agent settings
supply identity, model selection, and capability grants. The host derives tool
schemas and the component catalog from installed manifests. The catalog describes
interfaces, not health or authority. Connectors control sender admission.

The package supplies both components and their defaults. See
[configuration schema](config.schema.json) for optional context budgets and
[turn context](turn-context.md) for how they affect model requests.

## Behavior

Observations may create, amend, cancel, or clarify jobs. The model uses `js` for
computation and `yield` for decisions. Replies request the originating connector's
`<provider>.reply` capability; receiving an observation does not itself send one.
Children return results to parents and cannot invoke root capabilities.

| Reference | Contents |
| --- | --- |
| [Event flow](event-flow.md) | One request from observation to delivered reply |
| [Persistent work](persistent-work.md) | Association, decisions, budgets, checkpoints, operator controls |
| [Turn context](turn-context.md) | Model inputs, attachments, summaries, compaction |
| [History](history.md) | Search, pagination, source visibility |
| [REPL](repl/README.md) | JS operations, sessions, checkpoint recovery |

## Limits

Cycles and child recursion are bounded. Waiting for input makes no idle model
calls. Unknown external effects require reconciliation; lost suspended JS cells
are not replayed.
Exact budgets and recovery rules belong to [persistent work](persistent-work.md).

## Development

Build packages using [workspace setup](../../docs/development.md), then run:

```sh
cargo test --locked -p pluribus-plugin-rlm-cognition
cargo test --locked -p pluribus-plugin-rlm-repl
cargo test --locked -p pluribus-cognition --test rlm_cognition -- --include-ignored
```
