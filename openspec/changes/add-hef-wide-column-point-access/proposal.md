Status: Approved (2026-07-09)

## Why

Issue #1639 asks a "confirm the cost, then decide" question: do HEF's **wide
typed columns** — schema-declared free-text (FSST-class) blocks and internal
embedding/vector blocks — still pay a full-granule-block decode for a single-row
point lookup, and if so, is the residual variant arena's per-row offset index
worth extending to them?

The cost is confirmed against the current reader. The residual variant arena is
already point-accessible: `reader.rs::residual_slot` reads an 8-byte
`(offset: u32, len: u32)` entry indexed directly at `row_in_granule * 8` from the
`PayloadGranule` offsets block and jumps straight to that one row's bytes — a
small, bounded number of IOPs, exactly Lance's `FullZip` per-row byte-offset
index. Free-text and shredded typed columns take a different path:
`reader.rs::shredded_value_for_row` calls `cached_column` → `read_column`, which
`decode_block`s the **entire granule column block** and builds a rank index over
the whole granule's presence bitmap before it can return one row. Free-text is
read through that exact path. So a cold single-row point lookup of a large
free-text field decodes the whole granule block; a per-granule cache amortizes
repeat access, but the first touch of any granule pays the full decode.

The spec side matches the code. The free-text requirement guarantees only that
**bulk** single-field reads range-read the free-text blocks sequentially — it
says nothing about single-row point access. The embedding/vector requirement
addresses only **approximate** ANN retrieval, not exact per-row fetch of one
stored vector. Neither wide-column kind has any per-row point-access guarantee,
while the residual arena has had one all along.

This is a latency cliff on exactly the columns most likely to be queried by key —
a large free-text body pulled for one event, one row's embedding pulled for
re-ranking. The variant arena proves the offset-index pattern is implementable in
HEF; this change extends it to wide typed columns as a droppable, additive
acceleration.

## What Changes

**Wide typed columns get a per-row byte-offset index.** For schema-declared
free-text blocks and internal embedding/vector blocks, the writer MAY emit a
per-row `(offset, len)` index alongside the block — the same shape as the
residual arena's per-row slot — so a reader resolves one row's byte range with a
direct indexed lookup and fetches only that row's bytes, a small bounded number
of IOPs, instead of decoding the whole granule block. The index is a new
additive `typed_column_row_offsets` optional feature governed by the feature
directory: a reader that does not understand it falls back to the existing
full-granule decode and returns byte-identical values, so the acceleration is
fully droppable per HEF's feature discipline (Decision 16/20 — every
acceleration droppable).

**The free-text requirement gains a point-access path.** Free-text stays shredded
by declaration and its bulk-egress path (sequential range reads for whole-corpus
re-extraction and per-subject erasure) is unchanged. Added: a single-row point
lookup of a declared free-text field uses the per-row offset index when present
and does not decode the whole granule block.

**The embedding/vector requirement gains exact per-row fetch.** All isolation,
crypto-shredding, and rebuild rules are untouched, and exact event-query
correctness still does not depend on approximate retrieval. Added: fetching one
row's stored vector by row ordinal (e.g. late-materialized re-ranking of a single
candidate) uses the per-row offset index — bounded IOPs, no full-block decode —
distinct from the approximate ANN index that finds candidates.

Nothing here changes any query result, durable byte other than the added index,
checksum authority, ordering, or visibility. The offset index decodes to the same
row bytes the full-granule path yields; it is a physical fetch-timing accleration
governed by the feature directory, with readers that lack it falling back
identically.

## Capabilities

### Modified Capabilities

- `hef-column-design` — MODIFY "Free-text shredded by schema declaration" to add a
  per-row point-access path over the per-row byte-offset index (bulk-egress path
  unchanged); MODIFY "Internal embedding/vector columns isolated from public
  output" to add exact per-row fetch of a stored vector over the same index
  (isolation, crypto-shredding, rebuild, and no-correctness-dependence-on-
  approximate-retrieval rules unchanged).

### Added Capabilities

- `hef-column-design` — ADD "Per-row byte-offset index makes wide typed columns
  point-accessible": for free-text and embedding/vector blocks, an additive
  per-row `(offset, len)` index (mirroring the residual arena's per-row slot)
  under a `typed_column_row_offsets` optional feature, giving bounded-IOP point
  access, fallback-identical for readers that lack it, and preserving the
  bulk-egress and approximate-retrieval paths.

## Impact

- **Point lookups on wide typed columns stop paying a full-granule decode.** A
  single-row read of a large free-text field or one stored vector costs a bounded
  number of IOPs — resolve the row's offset entry, read that row's bytes — the
  same profile the residual arena already enjoys.
- **A new additive `typed_column_row_offsets` optional feature.** A reader that
  does not understand it falls back to the existing full-granule decode and
  returns identical values, so the index is droppable and no reader is forced to
  upgrade.
- **The bulk-egress and approximate-retrieval paths are untouched.** Free-text
  whole-corpus re-extraction still range-reads blocks sequentially; the vector ANN
  index still finds candidates approximately; the offset index only adds the
  per-row exact-fetch path beside them.
- **INV holds end to end.** BLAKE3 stays the sole integrity authority; the index
  decodes to the same row bytes as the full-granule path, so results, ordering,
  and visibility are unchanged; the index is a droppable, feature-directory-
  governed acceleration (Decision 16/20 — closed file shapes, every acceleration
  droppable).

## Open Questions

1. **Whether the index is warranted for every wide column or only measured-hot
   ones.** The requirement fixes only that the index exists and is additive; the
   writer's policy for *which* free-text/vector blocks earn one (always, or gated
   by measured point-lookup heat like per-page stats) is a tuning question left to
   the write path.
2. **Where the per-row index sits relative to its block.** Co-located with the
   block or pooled with the granule's other offset indices is a fetch-locality
   question for the write path and benchmark harness; the requirement fixes only
   that the index is independently addressable and that point access reads only
   the index entry and the row's bytes.
3. **Whether free-text and vector indices share one on-disk encoding.** Both are
   per-row `(offset, len)` in the same shape as the residual slot; whether they
   reuse one serialization or stay per-kind is an implementation choice the
   requirement does not pin.
