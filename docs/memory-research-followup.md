# Memory research follow-up

## Scope

Baseline: Jujutsu commit `4ffa7da5`.

1. Add opt-in evaluations through the production cognition and provider path.
   Cover distraction, correction, compaction, and restart. Report answer accuracy,
   stale answers, citation validity, latency, and available provider usage. Keep
   unavailable accounting distinct from zero. Test scoring offline; isolate live
   runs from production state and require explicit configuration.
2. Budget compaction against configured model context and output headroom. Include
   prompts and tool schemas. Retain the independent transport byte ceiling. State
   whether token counts are exact or estimated; reject invalid configurations.
3. Resolve summary citations against accessible original events before accepting
   summaries. Apply the same rule to explicit working-summary checkpoints. Respect
   delegated history ranges. Existence and access checks do not prove that a fact
   follows from the cited content.

## Acceptance

- Observe failing regressions before fixes; run focused tests after each fix.
- Reject fabricated, missing, and out-of-range citations without losing history.
- Exercise multibyte inputs, prompt/tool overhead, and insufficient context space.
- Distinguish scripted mechanism checks from measured live-model behavior.
- Rebuild packaged components, then run workspace tests and formatting checks.
- Preserve raw events, checkpoint compatibility, and the single WIT query API.

## References

- [RLM](https://github.com/alexzhang13/rlm/blob/main/rlm/core/rlm.py): token-based
  context thresholds and reserved context space.
- [DSPy RLM](https://github.com/stanfordnlp/dspy/blob/main/dspy/predict/rlm.py):
  bounded recursive calls and batched subqueries.

Batched subqueries, embeddings, and background extraction remain deferred pending
evaluation evidence. Provenance validation is a local design requirement.

## Implemented

- Configurable context, output reserve, and framing headroom; compaction at 85%
  of estimated input capacity or the transport threshold. The estimator counts
  UTF-8 bytes conservatively; it is not a model tokenizer.
- Host resolution of summary citations, delegated-range checks, preserved
  checkpoint payloads, and verified-summary metadata. Fresh verification clears
  errors left by stale responses.
- Five packaged evaluation workflows, strict citation scoring, nullable provider
  accounting, and an opt-in live provider bridge. See `memory-evaluations.md`.

## Validation

All 560 tests are covered across workspace and focused runs: 467 outside the
cognition crate and 93 cognition tests. The compaction workload initially failed
to cross the threshold because REPL previews truncate output; bounded source cells
exercise the transition successfully. An unrelated lifecycle cancellation test
passed on a serial rerun.

The final evaluation run passed all five workflows and five scoring tests. The
live entry point exited without calling a provider because live configuration
was absent. These results measure mechanisms, not live-model recall quality.

Formatting passed. Clippy completed with existing warnings. Nix packages built;
the final retry-state fix was rebuilt as Wasm and packaged locally, then checked
with the evaluation and provenance integration tests. Nothing was deployed or
pushed. The follow-up changes remain uncommitted after baseline `4ffa7da5`.
