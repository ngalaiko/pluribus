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

## Learning replay

`memory_learning` replays anonymized patterns from live operation through packaged
cognition, REPL, and memory components in an isolated store:

- Reuse vault templates and linked product/type/store conventions.
- Use a verified interpreter workaround instead of repeating a missing command.
- Supersede a corrected template and field name.
- Update an environment lesson when an interpreter becomes available.

Each case learns from an observation, restarts the runtime, and asks for a concrete
next-action decision in a different conversation. The earlier observation is
absent from recent conversation context. Correction cases seed an older memory
with scripted calls; learning and application use the selected provider.

Passing requires exactly one durable learning write, a supersession for correction
cases, a successful recall of the learned version citing the training observation,
and the expected JSON decision. A correct answer without a write or recall fails.
Reports include decisions, writes, corrections, recall calls, model calls, and
latency. Tests reject duplicate writes, stale decisions, fabricated success, and
missing retrieval.

Run the scripted harness and scorer checks after rebuilding packages:

```sh
cargo test --locked -p pluribus-cognition --test memory memory_learning -- --nocapture
```

For model behavior, use the same canonical provider bridge as above:

```sh
PLURIBUS_MEMORY_EVAL_LIVE=1 \
PLURIBUS_MEMORY_EVAL_MODEL='provider/model' \
PLURIBUS_MEMORY_EVAL_PROVIDER_CMD='/path/to/provider-bridge' \
PLURIBUS_MEMORY_EVAL_OUTPUT=/tmp/memory-learning.json \
cargo test --locked -p pluribus-cognition --test memory \
  memory_learning_live_replay -- --ignored --nocapture
```

The fixture specifies response fields; expected values remain in the scorer.
Live responses are not replaced by scripted answers. The live flag is required
even with `--include-ignored`. No production store, shell executor, vault, or
connector is accessed. These are decision replays, not full shell-task executions;
they measure whether the next action repeats discovery or a known failure, not
actual shell latency or command counts. Scripted success validates the harness,
not model learning quality.
