# Conformance

A plugin conforms only when its package, component, runtime behavior, and upgrade behavior pass this section.

## Package checks

- `plugin.toml` parses with duplicate-key rejection.
- Parsed manifest validates against the published schema.
- Required files exist and remain inside the package root.
- Paths contain no traversal, symlinks, or absolute names.
- Component SHA-256 matches `digest`.
- Configuration schema is Draft 2020-12 and itself validates.
- SPDX license expression parses when present.
- Plugin ID and version match the release identity.

## Component checks

- Binary is a WebAssembly Component, not a core module.
- Manifest world identifier is valid and the component shape matches its declared imports.
- ABI is exactly `pluribus:plugin@3.0.0`.
- Actual Pluribus imports exactly equal `imports`.
- No ambient WASI or unknown imports exist.
- `lifecycle` is exported.
- The single `plugin` world exports `lifecycle` with `run`, `handle`, and `stop`.
- Async source loops suspend while idle and commit events with state atomically.
- Event consumers declare subscriptions; source loops declare emitted events.
- Every type in `emits` is plugin-emittable or carries the `plugin.<id>.` prefix.
- No unknown Pluribus role export exists.
- Every descriptor and schema call succeeds within limits.

The repository validates its WIT package with `wit-parser` in [`crates/pluribus-core/tests/wit.rs`](../../crates/pluribus-core/tests/wit.rs).

## General runtime checks

- Start, health, and stop respect deadlines.
- Restart loses no durable state.
- Empty or malformed IDs, schema IDs, media types, and JSON are rejected.
- Calls above hard limits return `resource-exhausted` or `invalid-argument` without trapping.
- Cancellation stops host calls and bounded CPU work.
- Errors contain no secrets.
- Optional denied imports produce useful permission errors.
- A trap records failure and does not corrupt state.
- Custom events use the plugin namespace.

## Source checks

- `describe` is deterministic.
- Unsupported modes return `unsupported`.
- Poll respects `limit`.
- Repeating a checkpoint returns replay-safe observations.
- Checkpoint advances only with returned observations.
- Deduplication keys are stable across restart.
- Raw payloads are complete blobs.
- External IDs do not claim local trust.
- Invalid push signatures produce no observations.
- Duplicate pushes do not duplicate committed observations.
- Backoff avoids a tight empty poll loop.

## Capability checks

- Descriptors are deterministic and names unique.
- Argument, result, and constraint schemas validate.
- Invalid arguments fail before external effects.
- Denied calls cause no external effects.
- `inherent` calls tolerate repetition.
- `keyed` calls return one durable terminal outcome per key across restart.
- `non-idempotent` descriptors disclose ambiguity.
- Streaming frame order matches execution order.
- No frames appear after terminal return.
- Deadline and cancellation propagate to external work.

## Model checks

- Advertised features match behavior.
- Required unsupported features fail before dispatch.
- Message and content order round-trip.
- Media uses blobs rather than unbounded byte lists.
- Text deltas reconstruct terminal text.
- Tool fragments reconstruct canonical argument JSON.
- Tool call IDs remain stable.
- Usage is cumulative and missing differs from zero.
- Rate-limit windows preserve provider IDs and reset times.
- Continuation rejects incompatible account, model, or lineage.
- Credentials never enter component state or returned metadata.
- Cancellation closes network streams.

## Credential checks

- Descriptors are deterministic and contain no secret values.
- Input and flow schemas validate before any network request.
- Unknown fields, flow schemas, template parts, and response references fail closed.
- Enrollment and injection origins fit effective grants.
- Secret input is hidden and absent from configuration, state, events, logs, and errors.
- Failed replacement preserves the prior credential.
- Concurrent refresh sends one request and accepts refresh-token rotation.
- Revocation removes local secret material without loading it into the component.

## Event sink checks

- Events apply in sequence order.
- Repeated batches do not duplicate projection data.
- Unknown event types do not stop progress.
- Partial failure does not advance checkpoint.
- Returned checkpoint never exceeds the batch.
- Restart resumes from the last host-committed checkpoint.

## Context checks

- Projection has no external effects.
- Blocks fit count and value limits.
- Every block has valid provenance.
- Visibility never exceeds the request.
- Token estimates are conservative.
- Missing blobs fail explicitly.
- Unprivileged plugins cannot emit instruction blocks.
- Identical state and request produce stable output.

## Migration checks

- Every supported prior state version upgrades.
- Migration is restartable.
- Migration has no network, model, capability, message, or event effect.
- Failure restores old code and state.
- Successful migration records the new state version once.
- Old plugin remains usable after failed candidate activation.

## Security review

Reviewers must identify:

- every host import;
- every requested capability and constraint;
- every network origin and redirect behavior;
- every credential handle and allowed use;
- every retained state or blob category;
- every output path to model or human;
- idempotency and retry behavior;
- worst-case memory, CPU, storage, and response size;
- webhook authentication;
- parser and dependency attack surface.

## Release evidence

A release SHOULD include:

- plugin package;
- component and package checksums;
- source revision;
- locked dependency graph;
- toolchain versions;
- conformance results;
- requested grants;
- state migration matrix;
- known limitations.
