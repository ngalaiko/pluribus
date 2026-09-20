# Memory plugin

Status: implemented; opt-in installation. Package: `dev.pluribus.memory`.

Memory is a capability provider. RLM chooses what to retain, retrieve, correct,
or forget. The plugin validates records and maintains a deterministic search
projection. Core authorizes access and commits events, state, and delivery
progress. Memory makes no model calls.

History remains the evidence. Memory is selected knowledge with source links.
JS `state` is temporary computation; saving a memory does not checkpoint a JS
session. Retrieved text is data, never an authority grant or system instruction.

## Scope

The first version supports explicit retention, lexical recall, corrections,
expiry, and forgetting. RLM may store an inference when it labels it as such.
Automatic extraction, embeddings, summaries, background maintenance, and
cross-agent retrieval are deferred.

One memory instance serves one agent stream. Scopes organize records inside
that boundary; they do not create confidentiality boundaries. Personal and
family agents use separate streams and instances. The initial router must
reject multiple providers for the same memory capability within an agent.

## Package

Use the existing `pluribus:plugin@3.0.0` world and lifecycle. Import only `events`
and `state`; no HTTP, sockets, credentials, or filesystem access.

Provide `memory.recall`, `memory.get`, `memory.remember`, `memory.supersede`, and
`memory.forget`. Reads are inherently idempotent; writes use operation keys.
Each capability declares argument, result, and constraint JSON Schemas.

Subscribe to `memory.remembered`, `memory.superseded`, and `memory.forgotten`.
Accept mutation events only from this instance's host-stamped actor in its
agent stream. Emit those events plus `capability.completed` and
`capability.failed`. Capability requests arrive through `provides` routing.
Do not subscribe directly to all `capability.requested` events or observations.

Configuration bounds storage and responses:

| Field | Default | Meaning |
| --- | --- | --- |
| `max_records` | 10,000 | Retained record versions, including tombstones |
| `max_content_bytes` | 4,096 | UTF-8 bytes per record |
| `max_sources` | 16 | Source event IDs per record |
| `max_result_bytes` | 16,384 | Serialized result bytes |
| `max_results` | 20 | Records per read |

Reject capacity overflow; never silently evict knowledge. Host instance limits
also apply. These defaults require validation against the packaged Wasm memory
and wall-clock ceilings before release.

## Record

```json
{
  "id": "memory:<operation-key-digest>",
  "kind": "procedure",
  "content": "Use Jujutsu for version control in pluribus-v2.",
  "scope": "project:pluribus-v2",
  "sources": ["observation-123"],
  "basis": "explicit",
  "createdAtMs": 1788912000000,
  "supersedes": null,
  "expiresAtMs": null
}
```

Kinds: `fact`, `preference`, `belief`, `goal`, `procedure`. `basis` is `explicit`
or `inferred`; it describes the claim, not independently verified truth.
Require nonempty content, scope, and 1–16 distinct source IDs. Source events
must exist, precede the request, and be readable under the originating authority.
The plugin cannot mechanically prove that prose follows from its sources.

The plugin assigns identity and creation time. Use the request's host-stamped
recording time, including during replay. Hash the compact UTF-8 JSON array `[instanceId, operationId]` with SHA-256; do not derive identity from record content.

Records are immutable. A replacement has a new ID and names its predecessor.
Projection status is `active`, `superseded`, or `forgotten`; expiry is evaluated
against the read request's recorded time. Equal text with different operation
IDs remains separate evidence. No automatic semantic merging.

## Capabilities

All arguments reject unknown fields. All results use
`capability.completed {requestEventId, output}`. Failures use
`capability.failed {requestEventId, code, message}`. Schema names are
`dev.pluribus.memory.<operation>.arguments/1` and
`dev.pluribus.memory.<operation>.result/1`.

### recall

Arguments: `query` (1–512 UTF-8 bytes), exact `scope`, optional `kinds`, `limit`
(default 8), and `maxBytes` (default 6,000). Requested bounds cannot exceed
configuration. Invalid bounds fail instead of being silently widened.

Normalize text with Unicode lowercase; split on non-alphanumeric characters;
deduplicate query terms. Match records containing at least one term. Rank by
distinct matched terms descending, creation time descending, then ID ascending.
Apply scope, kind, authority, status, and expiry filters before ranking.

Return `{records, truncated, asOfSequence}`. Each item includes the complete
record and matched terms. Never truncate content or omit provenance inside a
record. Stop before exceeding the result byte budget and set `truncated` when
eligible matches remain. If even the first record cannot fit, return
`result-too-large`. There is no recall pagination in version 1; callers narrow
the query. Recall is a relevance query, not a complete-history search.

### get

Arguments: `ids` (1–20 distinct IDs), exact `scope`.
Return active, unexpired records in requested order and `unavailableIds`.
Missing, forbidden, expired, superseded, and forgotten IDs are indistinguishable.
Audit inspection remains a separately authorized history operation.

### remember

Arguments: `operationId` (1–128 bytes), `kind`, `content`, `scope`, `sources`,
`basis`, optional `expiresAtMs`. Expiry must be after the request's recorded
time. Return `{id}` after the transaction commits.

### supersede

Arguments: `operationId`, `expectedId`, and the replacement fields accepted by
`remember`. `expectedId` must identify the active head, in the same scope.
An expired head may be corrected; a forgotten or superseded head cannot.
Return `{id, supersedes}`. A stale head produces `conflict` without writing.
Two concurrent corrections cannot both replace the same head. Reject a
correction with `result-too-large` if its eventual forget receipt would exceed
the result budget. This keeps accepted chains forgettable.

### forget

Arguments: `operationId`, `expectedId`, exact `scope`.
Require the current head; mark it and its predecessor chain forgotten.
Return `{forgottenIds}`. Reject a stale head with `conflict`.

Forgetting excludes the chain from subsequent memory reads and rebuilds. It
does not erase audit events, already returned results, model transcripts, or
independent records derived from that chain. There is no restore operation.

### Errors and operation keys

Domain codes: `invalid-argument`, `source-unavailable`, `unavailable`,
`conflict`, `operation-conflict`, `capacity-exceeded`, `result-too-large`.
Authorization denial is the core's `capability.denied` event.

For each accepted write, retain `(instance, operationId)`, a digest of canonical
arguments, and its receipt. Identical retries return the receipt without a new
mutation. Different arguments with the same key fail `operation-conflict`.
Reauthorization precedes receipt lookup. Receipts contain IDs, not old content.
A failed validation does not reserve the key.

## Authority

Reads and writes require separate capability grants. Constraints contain an
allowlist of exact scopes; content, source IDs, and operation IDs cannot widen
it. Unknown senders receive no memory grants by default. Memory is root-only
in the first RLM integration.

`OriginConstraints` checks the exact requested memory scope against the grant. Merely granting
`memory.*` with empty constraints is insufficient for scoped deployments.
Source validation must use the originating request's authority, not the memory
instance's broader ability to rebuild its own log.

The host must enforce visibility on results, source reads, history reads, and
whole-stream delivery. Filtering only `memory.recall` cannot protect content
already present in memory mutation or capability-result events. Version 1
therefore uses scopes only for organization within one agent trust boundary;
record-level confidentiality requires host enforcement across all these paths.

## Events, projection, and recovery

Mutation schemas use `dev.pluribus.memory.<remembered|superseded|forgotten>/1`.
Each payload includes the operation ID, canonical argument digest, receipt,
and data needed to reconstruct the mutation:

- `memory.remembered`: complete new record.
- `memory.superseded`: complete replacement and predecessor ID.
- `memory.forgotten`: affected IDs.

Use the request event as causation. In one `Outcome`, return the mutation event,
capability result, state mutations, and delivery checkpoint. Validate and
apply requests in sequence order against an in-batch overlay, so reads and
corrections observe earlier writes in the same batch.

Projection keys: `record/<digest>`, `head/<root-digest>`, and
`operation/<digest>`. Records contain a precomputed lexical term set; recall
scans records in bounded pages. This avoids an unbounded number of term-key
mutations per write under the host’s 256-mutation ceiling. Applied operation
receipts make subsequent delivery of the instance's own mutation a no-op.
Head pointers select current records; forgotten chains have a null head.
Expiry is checked at read time. No heap-only state is required between handlers.

An empty namespace must rebuild before serving capabilities. The `rebuilds` manifest field enables a host
activation barrier: capture the mutation high-water sequence, replay this
instance's mutation events through that sequence in bounded pages, then enable
request dispatch. Recovery applies mutations only; it must not execute old
requests, revalidate historical sources, emit new mutation/result events, or
discard operation receipts. Persist replay progress atomically with state.
Mutations after the captured sequence are delivered normally before later
requests. Do not use a full-history scan inside every capability handler.

The existing delivery cursor alone is insufficient: answered requests are
filtered out, and an empty projection must still consume their mutation events
before handling newer requests. The runtime persists a separate `__host/rebuild` cursor atomically with replay
mutations. The request cursor remains unchanged, preserving pending requests.
Replay rejects event emission; replay providers may import only events/state.

## RLM integration

Use the generic `capabilities.invoke(name, arguments)` bridge and supplied
capability schemas to emit ordinary `capability.requested` events. Preserve the root observation as the authority
origin and persist request/session/yield correlation in RLM state. Successful
calls return the capability receipt; callers read its `output` field. Failed or
denied requests reject the suspended promise.
Handle all terminal outcomes, including cancellation and timeout, so cells
cannot wait forever. These calls count toward the existing cell yield ceiling.

Do not add a host memory import or inject records into every model request.
RLM selects retrieval, keeps results in JS state, and returns bounded evidence
to the model. Memory bookkeeping events do not start a new reasoning cycle.
Surface failed writes before any reply claiming that information was saved.

Children receive selected records through `rlm.query({question, context})`.
They cannot invoke memory capabilities. The root evaluates a child's proposed
memory before writing it. Direct child recall requires a separate delegated
read-authority design; a read-only label alone is insufficient.

Example observation: “What version-control workflow applies here? Remember
that remote pushes are forbidden.” RLM exposes the host-verified observation
ID as `context.observationEventId` in the proposed JS context.

First model-generated cell retrieves evidence:

```js
state.workflow = (await capabilities.invoke('memory.recall', {
  query: "version control workflow",
  scope: "project:pluribus-v2",
  limit: 8,
  maxBytes: 6000
})).output;
return state.workflow;
```

The cell yields; core authorizes recall; memory answers; RLM resumes the cell.
The next model call sees the Jujutsu record and its source, and requests a write:

```js
state.saved = (await capabilities.invoke('memory.remember', {
  operationId: context.observationEventId + ":no-push",
  kind: "procedure",
  content: "Never push to remote in pluribus-v2.",
  scope: "project:pluribus-v2",
  sources: [context.observationEventId],
  basis: "explicit"
})).output;
return state.saved;
```

The write follows the same request/result path. Only after its receipt does
RLM complete with `{"note":"Saved restriction.","reply":"Use Jujutsu. Never
push to remote. Saved."}`. A failed write produces an accurate reply instead.
Restart preserves the memory record; recovering the suspended JS cell remains
the separate interpreter checkpoint problem.

## Acceptance

1. Remember, recall, and get round-trip through packaged Wasm and core grants.
2. Recall ranking, byte bounds, empty matches, and expiry are deterministic.
3. Two corrections to one head yield one replacement and one conflict,
   including when both requests arrive in the same batch.
4. Forget excludes the predecessor chain from recall/get after state rebuild.
5. Identical retries emit one mutation; changed arguments conflict. Replayed
   receipts remain available after later correction or forgetting.
6. A crash before commit leaves no partial record/result; a crash after commit
   cannot duplicate the mutation on redelivery.
7. Clearing state reconstructs records, tombstones, receipts, and ranking from
   mutation events across multiple pages without replaying requests.
8. Requests arriving during rebuild wait behind the activation barrier.
9. Forged mutation actors, unauthorized scopes, and unreadable sources fail;
   unavailable-ID responses do not distinguish existence from access denial.
10. Scripted RLM recall → JS resume → remember → reply succeeds; denied, failed,
    timed-out, and cancelled calls settle the cell without false success.
11. Child memory calls fail; explicitly passed records remain usable by children.
12. Capacity and host ceilings fail explicitly without advancing partial state.

See [installation and limits](../plugins/memory/README.md). No new WIT import
is required. Packaged integration tests use scripted model responses.
