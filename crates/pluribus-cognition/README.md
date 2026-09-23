# pluribus-cognition

Routes events for one agent, admits provider work, enforces origin authority and job revisions, tracks attempts, and routes cancellation.

Depends on core contracts and the Wasm runtime. RLM prompts, job decisions, and context selection belong to the cognition plugin, despite this crate’s name.

[RLM](../../plugins/rlm/README.md), [runtime](../../docs/runtime.md).

## Development

From the repository root:

```sh
cargo test --locked -p pluribus-cognition
```

See [workspace setup](../../docs/development.md) for the pinned toolchain, packaged
fixtures, and integration checks. [Host map](../README.md).
