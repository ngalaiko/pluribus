# Memory

Explicit, sourced memories for RLM. Runs inside Wasm with no network or
filesystem access. [Contract](contract.md).

## Build

```sh
nix-shell --run pluribus-sync-plugins
```

The [pinned toolchain](../../rust-toolchain.toml) and its Wasm target are required. The packaged components live in `target/plugins/memory`.

## Install

Merge into the agent's `config.json`, preserving existing entries. Replace the
package path with the absolute location of `target/plugins/memory`.

```json
{
  "plugin_instances": {
    "memory-1": {
      "package": "file:///path/to/pluribus/target/plugins/memory",
      "config": {},
      "components": {
        "main": {}
      }
    }
  },
  "capability_instances": [
    "memory-1/main"
  ],
  "trusted_capabilities": {
    "memory-1/main": [
      "memory.recall",
      "memory.get",
      "memory.remember",
      "memory.supersede",
      "memory.forget"
    ]
  },
  "trusted_constraints": {
    "memory-1/main": {
      "memory.recall": {
        "scopes": [
          "project:pluribus"
        ]
      },
      "memory.get": {
        "scopes": [
          "project:pluribus"
        ]
      },
      "memory.remember": {
        "scopes": [
          "project:pluribus"
        ]
      },
      "memory.supersede": {
        "scopes": [
          "project:pluribus"
        ]
      },
      "memory.forget": {
        "scopes": [
          "project:pluribus"
        ]
      }
    }
  }
}
```

Admitted connector observations receive these grants. Remove write capabilities for
read-only access. Scope names match exactly; `{}` grants no memory scope.
Only one provider per memory capability may be installed in an agent.
The host supplies memory capability schemas in RLM’s `context.tools`.

## RLM

```js
const found = (await capabilities.invoke('memory.recall', {
  scope: 'project:pluribus', query: 'version control', limit: 8
})).output;
return found.records;
```

```js
return (await capabilities.invoke('memory.remember', {
  operationId: context.observationEventId + ':vcs',
  scope: 'project:pluribus',
  kind: 'procedure',
  content: 'Use Jujutsu. Never push to remote.',
  sources: [context.observationEventId],
  basis: 'explicit'
})).output;
```

Await the write receipt before claiming success. Reuse an operation ID only
when retrying identical arguments. Corrections use `memory.supersede` with the
current `expectedId`; forgetting uses `memory.forget` with that same head ID.
Children receive selected records as context and cannot call memory directly.

## Reusable lessons

Save one independently correctable lesson with task keywords and original source
IDs. Recall related records first; keep unchanged records and supersede outdated
ones. Use `basis: "inferred"` for conclusions beyond explicit source statements.

- Workflows: retain the workspace, authoritative template paths, and verified
  steps. Reread templates before edits. For a vault, record how product, type,
  and store notes relate instead of inventing or copying their schemas.
- Environment: retain the executor, workspace, observed limitation, and verified
  workaround. A missing interpreter in one executor is not a global fact.
  Revalidate after environment changes; expire temporary observations.
- Corrections: update the existing lesson and cite the correction. Task progress,
  routine transcripts, credentials, and unsupported claims do not belong here.

RLM reviews these opportunities before completing work. Saving and recall remain
model decisions, measured by the [learning replay](evaluations.md#learning-replay).

## Limits

- Default capacity: 10,000 versions; 4 KiB content each. No automatic eviction.
- Lexical matching, Unicode lowercase, exact scope. No embeddings or extraction.
- Complete results fit a 16 KiB ceiling; oversized reads fail explicitly.
  Corrections cannot extend a chain beyond its eventual forget-receipt budget.
- Scopes organize one agent trust boundary. They do not hide records from that
  agent's authorized history. Separate agents require separate instances.
- Forgetting suppresses retrieval; it does not erase events or prior results.
- Rebuild runs before dispatch, from mutation events and durable retry receipts.
- Memory survives restart; the JS heap remains subject to interpreter recovery.

## Checks

```sh
cargo test --locked --manifest-path plugins/memory/Cargo.toml
cargo test --locked -p pluribus-plugin-rlm-cognition
cargo test --locked --manifest-path plugins/rlm/repl/Cargo.toml
cargo test --locked -p pluribus-cognition --test memory
```
