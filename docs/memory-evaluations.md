# Memory evaluations

The runner uses isolated SQLite stores, packaged cognition and REPL components,
and canonical provider requests/completions. It covers recall after eight
unrelated turns, correction of a stale fact, compaction, restart, and citations.
Production state is never opened.

Build packages with `nix-shell --run pluribus-sync-plugins`. Run the offline
scripted workflow and scoring checks:

```sh
cargo test -p pluribus-cognition --test rlm_cognition memory_eval -- --nocapture
```

Scripted answers verify the runner, transitions, and scoring. They do not measure
model recall quality.

## Live provider

Supply a bridge executable that reads one canonical `pluribus_model::Request`
JSON object from stdin and writes one canonical `pluribus_model::Completion` JSON
object to stdout. Preserve `call_id`. The runner substitutes the configured model
for the fixture binding. Keep credentials in the bridge environment; stdout must
contain only the completion. Each bridge call has a 120-second deadline and a
1 MiB output limit.

```sh
PLURIBUS_MEMORY_EVAL_LIVE=1 \
PLURIBUS_MEMORY_EVAL_MODEL='provider/model' \
PLURIBUS_MEMORY_EVAL_PROVIDER_CMD='/path/to/provider-bridge' \
PLURIBUS_MEMORY_EVAL_OUTPUT=/tmp/memory-evaluation.json \
cargo test -p pluribus-cognition --test rlm_cognition \
  memory_evaluation_uses_packaged -- --ignored --nocapture
```

Live calls can incur provider charges. Ordinary tests and `--include-ignored`
make no provider calls unless `PLURIBUS_MEMORY_EVAL_LIVE=1` is also set. Offline
fixtures never use the live bridge.

## Scoring

Final questions request JSON with `answer` and `sourceIds`. Answers are compared
exactly after case/whitespace normalization. Citations must match the supporting
fixture observations, including the correction event where applicable. Missing,
malformed, or additional fabricated citations fail; the runner never supplies
citations on the model's behalf.

The compaction case injects bounded JS source cells to create context pressure, then
lets the provider produce the summary and answer. It requires an accepted summary
with verified source metadata. The restart case destroys and reinstalls the
runtime against the same store before asking its final question.

The JSON report records accuracy, stale-fact resistance, citation validity,
observed transitions, total scenario latency (including runtime setup), model calls, and scripted pressure cells. Token
and cost totals are available only when every provider completion reports the
field. Missing values remain `null`; prices are not inferred. A failed case gives
a nonzero test exit after writing the report.

This is a small regression benchmark, not a statistical estimate of reliability.
Repeat live runs across models and compare reports before tuning retrieval.
