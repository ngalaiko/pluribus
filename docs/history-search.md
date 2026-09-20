# Searchable history

`history.search` is an additive REPL capability for finding older events in the
root agent stream. It keeps the existing `history.read` cursor and payload
behavior unchanged.

Both JS helpers use WIT `events.query`. Its filter combines metadata predicates,
optional text search, and sort direction. There is no WIT `history` interface.
The expanded filter belongs to ABI 3; ABI 2 plugins require rebuilding.

```js
const page = await history.search({
  query: "deployment failed",
  before: 500,                         // newest-first cursor; omit initially
  limit: 20,
  eventTypes: ["component.failed"],
  recordedFromMs: 0,
  recordedToMs: Date.now(),
});
```

The result contains bounded rows with `eventId`, `sequence`, `type`,
`recordedAtMs`, and an excerpt. `nextBefore` is the next cursor for another
page, or `null` when the search is exhausted. The host limits query and page sizes, applies the same stream and
delegated-range grants as `history.read`, and filters payload visibility before
returning rows. Read source payloads through `history.read`; its existing payload
size limits still apply.
`conversationId` matches the ID in the event payload, including events whose
correlation ID is absent.

The SQLite store maintains an FTS5 projection of event type and up to 8,192
Unicode characters of canonical JSON payload text. Search input is tokenized as
literal Unicode words before it is passed to SQLite, so FTS operators and
punctuation cannot change the query.
Existing databases are migrated in place and backfilled; raw events remain the
source of truth. Blob payloads contribute no searchable source text. Internal
checkpoint and job projection payloads are excluded from the text projection;
their event types can still be selected as metadata matches.

Search is newest-first. The cursor advances past returned or filtered events,
using an exclusive sequence bound so equal timestamps do not produce ties.
Continue until `nextBefore` is `null`, including after an empty filtered page.
Empty queries are rejected; punctuation-only queries return an empty page.
Queries are limited to 4,096 UTF-8 bytes. A conversation filter narrows results;
it does not confer authority. Existing stream and delegated-range grants apply.
