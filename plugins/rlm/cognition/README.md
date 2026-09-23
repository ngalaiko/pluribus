# Cognition component

Owns RLM job transitions, observation association, model context, recursive
queries, and request/result coordination. It persists reasoning state through
host imports and asks providers to perform external work through events.

The sibling [REPL](../repl/README.md) executes JavaScript. The native
[router](../../../crates/pluribus-cognition/README.md) owns admission and delivery.
Neither chooses RLM's prompts or job decisions.

See [persistent work](../persistent-work.md) for transitions and recovery,
[turn context](../turn-context.md) for model inputs, and [package setup](../README.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-plugin-rlm-cognition
```

Packaged integration checks live in
[`crates/pluribus-cognition/tests`](../../../crates/pluribus-cognition/tests).
Rebuild packages before running them; see [development](../../../docs/development.md).
