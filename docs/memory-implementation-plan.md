# Memory implementation plan

## Objective

Recover relevant prior evidence, preserve working knowledge across context trimming
and restart, and reduce redundant persistence. Keep raw events authoritative.
No production changes or remote pushes are part of this implementation.

## Evidence

The inspected installation had five memory writes, four recalls, no explicit JS
checkpoints or recursive queries, and fifteen cells using history reads. Model
requests included six recent exchanges. Cognition discards older exchanges above
48 KiB. A restart-related burst contained 162 lost-session errors in 30 seconds.
Checkpoint and job-update payloads occupied approximately 437 MB and 187 MB.
These observations establish missing mechanisms, not a measured RAM leak.

## 1. Searchable history

- Add `history.search` with text query, conversation ID, time bounds, event types,
  bounded result count, and newest-first pagination.
- Search source text in SQLite using an indexed projection/FTS where compatible
  with the existing store. Preserve stream and delegated-range authorization.
- Return event IDs, sequence, time, bounded excerpts, and an unambiguous cursor.
  Full source remains available through history reads; no duplicated corpus in JS.
- Keep existing `history.read` behavior and old databases compatible. Validate
  arguments, cap input/output sizes, and make empty results explicit.
- Test matches older than six exchanges, conversation isolation, time/range
  boundaries, pagination ties, Unicode, hostile search syntax, and migrations.

## 2. Retention and context compaction

- Add concrete memory guidance: recall earlier preferences/decisions before
  answering historical questions; retain explicit durable facts and corrections;
  use source IDs and await receipts; supersede outdated records.
- Keep memory optional and root-only. Never infer authority from retrieved text.
- Replace silent deletion with a bounded compaction handoff. Preserve objective,
  constraints, decisions, completed work, unresolved questions, and source IDs.
- Keep the summary in durable task context and surface it in subsequent turns.
  Preserve assistant/tool pairing and the existing hard context ceiling.
- Support explicit structured working summaries through the JS checkpoint where
  useful. Do not fabricate a semantic summary by truncating arbitrary prose.
- Test retained decisions after trimming, repeated compaction, restart, malformed
  summaries, oversized summaries, and tool-history validity.

## 3. Working-state durability

- Automatically checkpoint bounded JSON `state` after successful cell completion.
  Explicit checkpoints remain supported; large/non-JSON state must produce a
  visible diagnostic without changing a completed side effect into a failed cell.
- Count UTF-8 bytes, not JavaScript string length. Reject cycles, nonfinite values,
  and unsupported values without silently dropping fields.
- Restore snapshots in a fresh environment. Do not replay source or suspended
  promises. Pending external operations retain their existing durable receipts.
- Test automatic restore, explicit-checkpoint compatibility, oversized and invalid
  state, multibyte limits, and interrupted operations without duplicate effects.

## 4. Persistence amplification

- Inspect the current record-delta checkpoint path before changing the format.
  Avoid replacing existing incremental persistence with whole-engine snapshots.
- Remove redundant copies from job projections where recovery can reconstruct
  them from authoritative events/state. Preserve backward decoding and idempotency.
- Add a size regression using repeated large results; measure serialized bytes
  for a small state update, not just event counts.
- Do not delete historical production events or run VACUUM as part of this work.

## 5. Validation

- For each demonstrated bug: add a regression, observe failure, implement, observe
  success. Use deterministic scripted models; no paid inference or messages.
- Add an integration scenario that retains knowledge, crosses the recent-context
  limit, retrieves evidence, applies a correction, restarts, and retrieves again.
- Build packaged Wasm components; run focused tests, workspace tests, formatting,
  and Clippy with the pinned toolchain. Record commands and any environmental
  limitations. Behavioral prompting is not a guarantee of live-model recall.
- Review diffs with Jujutsu. No Git commands, commits with authorship trailers,
  pushes, deployments, or production database mutations.

## Delegation

Three Luna agents own searchable history, cognition retention/compaction, and REPL
durability respectively. The coordinator owns persistence amplification, review,
integration tests, documentation, and final validation. Shared-file edits must be
coordinated at section boundaries; agents must not overwrite one another.

## References

- [RLM execution](https://github.com/alexzhang13/rlm/blob/main/rlm/core/rlm.py) — persistent
  environments and optional compaction.
- [RLM prompts](https://github.com/alexzhang13/rlm/blob/main/rlm/utils/prompts.py) — filter source
  data, delegate selected chunks, return bounded evidence.
- [DSPy RLM](https://github.com/stanfordnlp/dspy/blob/main/dspy/predict/rlm.py) — explicit
  recursion budgets and bounded interpreter output.

## Deferred evaluation

Embeddings, semantic reranking, background extraction, and live-model quality
benchmarks require evidence from the deterministic baseline first. Recursion is
available for large evidence sets; it is not mandatory for simple recall.

## Implementation contracts

| Area | Implementation | Acceptance |
| --- | --- | --- |
| History | `EventQuery`, SQLite schema 10/FTS5, one WIT `events.query` operation, cognition handler, REPL bridge | Literal search; newest-first sequence cursor; conversation/time/type filters; JS read behavior preserved |
| Compaction | `cognition/src/compaction.rs` and engine transitions | Trigger above 48 KiB; summary at most 12 KiB; whole request at most 64 KiB; three malformed-response attempts; restart and amendment handling |
| State | REPL bootstrap serializer and restore path | At most 32 KiB UTF-8; JSON-only; automatic/explicit modes; legacy explicit restore; no source replay |
| Invalid state | Cognition completion handling | Unsaved successful cells invalidate automatic snapshots; failed cells and explicit snapshots retain existing semantics |
| Persistence | `cognition/src/jobs.rs` result references | Results above 1 KiB become event references; uncertain outcomes remain visible; 32 results of 8 KiB keep the job below 20 KiB |
| Integration | Cognition memory and RLM tests | Older evidence retrieval, rejected arguments, exhausted cursors, correction, restart, automatic restore, and blocked execution after unsaved state |

Search indexes at most 8,192 Unicode characters per source payload. Large records
require reading the original event. Summary citations are model claims, not
independently verified provenance. Deterministic scripts verify the mechanisms;
live-model retention and recall remain unmeasured.

Before deployment, time the schema migration against a database copy and measure
index growth. Preserve a schema-9 snapshot for rollback; older code cannot open
schema 10. This implementation does not migrate the production database.

The unified WIT query expands its filter and uses ABI 3. Upgrade host and bundled
plugins together; external ABI 2 plugins need rebuilding. The event format and
explicit checkpoint version remain unchanged.

## Verification results

| Command | Result |
| --- | --- |
| `nix-shell --run pluribus-sync-plugins` | Passed; ABI 3 host, Wasm components, and packages built |
| `cargo fmt --all --check` | Passed |
| `cargo clippy --locked --workspace --all-targets` | Completed with existing warnings |
| `cargo test --locked --workspace -- --include-ignored --test-threads=4` | 537 passed, 0 failed |

Observed failing regressions before fixes for automatic restart recovery,
history-search availability, duplicated large activity results, and stale
automatic checkpoints. The completed suite covers old evidence retrieval,
pagination exhaustion, rejected arguments, correction after unrelated turns,
restart recovery, explicit-checkpoint compatibility, semantic compaction, and
blocked execution when current state cannot be restored.

Tests use deterministic providers. Live-model recall quality and production
migration cost remain unmeasured. No production data changed; nothing was pushed
or deployed.
