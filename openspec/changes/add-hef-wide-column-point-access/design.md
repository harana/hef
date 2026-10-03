# Design — A per-row byte-offset index for wide typed columns

Issue #1639 is a "confirm the cost, then decide" item. This document does both:
§0 records the investigation that confirms the cost against the current reader,
and §1–§3 are the resulting decisions in the tension → decision → why → rejected
→ spec-edits style of `docs/design-review-decisions.md`. The running theme: the
residual variant arena already proves that a per-row `(offset, len)` index turns
a random-access read into a bounded-IOP jump; the wide typed columns are the one
place HEF still pays a full-granule decode for a single row.

## 0. The cost is confirmed: wide typed columns decode the whole granule for one row

The residual variant arena is point-accessible today. In
`crates/storage/src/hef/layout/reader.rs`, `residual_slot` reads an 8-byte
`(offset: u32, len: u32)` entry directly at `row_in_granule * 8` from the
`PayloadGranule` offsets block, and `residual_bytes` then slices exactly that
row's bytes out of the residual block. Two indexed reads, no neighbouring-row
decode — this is Lance's `FullZip` per-row byte-offset index in all but name.

The wide typed columns take a different path. `shredded_value_for_row` — the
per-row reader used for both shredded typed columns **and** declared free-text
fields (the merge loop in `payload` calls it for every `footer.freetext` entry) —
calls `cached_column`, which calls `read_column`. `read_column` `decode_block`s
the **entire granule column block** and builds a `RankSelect` index over the
whole granule's presence bitmap before it can return the one requested row. The
per-granule `column_cache` amortizes this across repeat reads of the same
granule, but the *first* touch of any granule pays the full block decode, and a
cold single-row point lookup is exactly that first touch. Embedding/vector blocks
have no per-row reader at all today — only the approximate ANN path is specified.

So the answer to the issue's question is yes: FSST-encoded free-text and
vector/embedding columns require decoding the whole granule block to fetch a
single row, while the residual arena does not. On the exact columns most likely
to be pulled by key — a large free-text body for one event, one row's embedding
for re-ranking — that is a latency cliff the residual path already avoids. The
variant arena proves the fix is implementable in HEF; the rest of this document
decides to extend it.

## 1. Extend the residual arena's per-row `(offset, len)` slot to wide typed columns

- **Tension.** The residual arena's per-row offset slot is the cheapest possible
  point-access primitive — one indexed 8-byte read, then the row's bytes — and it
  already exists in the format. The wide typed columns re-derive the whole
  granule to reach one row instead. But the free-text family is deliberately
  columnar and text-tuned for *bulk* egress, and vector blocks are deliberately
  quantized/graph-laid-out for *approximate* retrieval; a point-access index must
  not disturb either of those paths, and it must decode to the same row bytes the
  full-granule path yields so results never diverge.
- **Decision.** Add a per-row `(offset, len)` byte-offset index for free-text and
  embedding/vector blocks, in the same shape as the residual arena's per-row
  slot. A reader with the index resolves one row's byte range by direct indexed
  lookup and reads only that row's bytes; a reader without it decodes the whole
  granule block as before. The index sits beside the block; the block's own
  encoding (FSST for free-text, quantized/graph layout for vectors) is unchanged.
- **Why.** Dog-fooding the primitive the format already ships is the smallest
  possible fix — no new access pattern, just the residual slot's shape applied to
  two more column kinds — and it closes the one point-access cliff §0 found.
  Keeping the block encoding untouched means the bulk-egress and ANN paths are
  literally the same bytes they are today; only a side index is added.
- **Rejected.** *Rely on the per-granule column cache.* — The cache amortizes
  repeat reads but not the cold first touch, which is precisely the point-lookup
  case; a key-addressed read of a rarely-touched granule still pays the full
  decode. *Shrink the granule so a full decode is cheap.* — Trades one cliff for
  a footer/marks explosion and still decodes sibling rows; the offset index is
  O(1) in block size.
- **Spec edits.** `hef-column-design` ADD "Per-row byte-offset index makes wide
  typed columns point-accessible".

## 2. The index is an additive, droppable optional feature — not refuse

- **Tension.** The columnar-marks change made its encoding a *required* feature
  because marks are the authoritative random-access directory: a reader that
  half-understands them is worse than one that refuses the file. The per-row
  offset index is not authoritative — the whole-granule decode already returns
  the correct value. So forcing every reader to understand the index buys nothing
  and blocks incremental rollout.
- **Decision.** Make the index a `typed_column_row_offsets` **optional** feature
  in the feature directory. A reader that declares it uses it for point access; a
  reader that does not falls back to the whole-granule decode and returns
  byte-identical values. The index is droppable acceleration state — a file
  without it stays fully readable, and rebuilding or discarding it never changes a
  result.
- **Why.** This is exactly HEF's "every acceleration droppable" discipline
  (Decision 16/20): the index accelerates a read that is already correct without
  it, so optional-and-fallback is the right feature class. It also lets the write
  path adopt the index file-by-file with no reader flag day.
- **Rejected.** *A required feature (refusal).* — Correct only for
  authoritative structures; here it would reject files over a pure acceleration,
  contradicting the droppable-acceleration rule. *No feature flag, always
  present.* — Removes the writer's freedom to emit the index only where measured
  point-lookup heat justifies it (Open Question 1) and gives old readers no signal
  to fall back cleanly.
- **Spec edits.** `hef-column-design` ADD "Per-row byte-offset index makes wide
  typed columns point-accessible" (feature-directory clause); the two MODIFY
  requirements carry the matching fallback sentence.

## 3. Point access is added *beside* the bulk and approximate paths, not instead of them

- **Tension.** Free-text exists as its own columnar family precisely so
  re-extraction and erasure range-read it sequentially (Decision 18's bulk-egress
  path); vector blocks exist quantized/graph-laid-out precisely so ANN retrieval
  is cheap, with exact correctness explicitly *not* depending on approximate
  retrieval. A point-access index must not weaken either guarantee or blur the
  ANN-versus-exact distinction.
- **Decision.** The offset index adds only the per-row exact-fetch path. The
  free-text bulk-egress path (sequential range reads) is unchanged; the vector ANN
  index (which finds *candidate* rows approximately) is unchanged. The index
  answers a different question — "give me row N's exact bytes" — and is distinct
  from "find rows near this query vector".
- **Why.** Keeping the three paths separate preserves every existing guarantee:
  bulk egress stays 1× sequential bytes, ANN stays approximate-and-rebuildable,
  and exact per-row fetch becomes bounded-IOP instead of full-block. Conflating
  exact fetch with ANN would risk making exact correctness lean on the
  approximate index, which the spec forbids.
- **Rejected.** *Serve exact single-vector fetch from the ANN index.* — The ANN
  index is approximate and rebuildable; exact fetch must read the stored bytes,
  not an approximate neighbour. *Drop the free-text bulk path in favour of
  per-row reads.* — Bulk re-extraction over a whole corpus via per-row lookups is
  strictly worse than one sequential pass.
- **Spec edits.** `hef-column-design` MODIFY "Free-text shredded by schema
  declaration" and MODIFY "Internal embedding/vector columns isolated from public
  output".
