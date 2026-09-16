# Memory

Explicit, sourced memories for RLM. Runs inside Wasm with no network or
filesystem access. [Contract](../../docs/memory-plugin.md).

## Build

```sh
nix-shell --run pluribus-sync-plugins
```

Rust 1.95.0 and its `wasm32-unknown-unknown` target are required by the build
scripts. The packaged components live in `target/plugins/memory`.

## Install

Merge into the agent's `config.json`, preserving existing entries. Replace the
package path with the absolute location of `target/plugins/memory`.

```json
{
  "plugin_instances": {
    "memory-1": {
      "package": "file:///path/to/pluribus-v2/target/plugins/memory",
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
          "project:pluribus-v2"
        ]
      },
      "memory.get": {
        "scopes": [
          "project:pluribus-v2"
        ]
      },
      "memory.remember": {
        "scopes": [
          "project:pluribus-v2"
        ]
      },
      "memory.supersede": {
        "scopes": [
          "project:pluribus-v2"
        ]
      },
      "memory.forget": {
        "scopes": [
          "project:pluribus-v2"
        ]
      }
    }
  }
}
```

Admitted connector observations receive these grants. Remove write capabilities for
read-only access. Scope names match exactly; `{}` grants no memory scope.
Only one provider per memory capability may be installed in an agent.
Default RLM discovery includes the memory schemas in `context.tools`. Explicit
RLM configurations must include those tool descriptions and schemas themselves.

## RLM

```js
const found = (await capabilities.invoke('memory.recall', {
  scope: 'project:pluribus-v2', query: 'version control', limit: 8
})).output;
return found.records;
```

```js
return (await capabilities.invoke('memory.remember', {
  operationId: context.observationEventId + ':vcs',
  scope: 'project:pluribus-v2',
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
cargo +1.95.0 test --manifest-path plugins/memory/Cargo.toml
cargo +1.95.0 test --manifest-path plugins/rlm/Cargo.toml
cargo +1.95.0 test --manifest-path plugins/rlm/repl/Cargo.toml
cargo +1.95.0 test -p pluribus-cognition --test memory
```
