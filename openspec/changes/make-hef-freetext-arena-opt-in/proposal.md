Status: Proposed (2026-08-18)

## Why

`add-hef-wide-column-point-access` gave declared free-text columns a per-row
byte-offset index because, at the time it was written, a single-row free-text
read had no cheaper path: `shredded_value_for_row` decoded the whole granule
block to return one row. That premise no longer holds. The per-value byte-range
point-read path landed afterwards (`decode_block_range_shared` in
`shredded_value_for_row`), and FSST is one of the transforms it fast-paths —
`Transform::is_per_value_addressable` names `FsstString` explicitly, and an FSST
block already carries an offset table over its compressed values.

So the file now stores every declared free-text value twice: once inside the
column block as FSST-compressed bytes, and once again as raw, uncompressed bytes
in a per-granule arena with an 8-byte `(offset, len)` entry per row. Measured on
the `format-benchmark` HEF dataset (100,000 rows, 8,192 rows per granule, 13
granules, one declared free-text field averaging ~55 bytes, release build):

| region | bytes | share of file |
| --- | ---: | ---: |
| whole file | 14,527,554 | 100% |
| `note` column blocks (FSST + Zstd1, 13 granules) | 458,976 | 3.2% |
| free-text arena, raw value bytes | 5,489,799 | 37.8% |
| free-text arena, offsets table (8 B × 100,000 rows) | 800,000 | 5.5% |
| **free-text arena, total** | **6,289,799** | **43.3%** |

The arena is 13.7× the size of the column block holding the same values. It sits
inside the stripe, so it is BLAKE3-hashed as part of the stripe checksum and
fsynced on every publication and every rewrite — a permanent write-amplification
and storage cost paid on every file, whether or not anything ever point-reads
free text.

Reading the same file twice — once with `typed_column_row_offsets` declared, once
with the flag cleared so the read falls through to the column block — returns
byte-identical values for all 100,000 rows. The arena is redundant, not load-
bearing.

It is not free to drop, and this proposal does not pretend otherwise. On the same
file, a cold read of one row from a granule costs 459 ns through the arena and
172 µs through the column block (medians over the file's 13 granules, one reader);
once the granule is resident, repeat reads cost 65 ns and 2.4 µs. Dropping the
arena takes the file from 14,522,366 B to 8,181,058 B.

The gap is not FSST addressing. Of the 172 µs, ~127 µs is `remove_trailing`
decompressing the whole 116,075-byte block to reach one value, and the rest is
`decode_fsst_string_range` rebuilding the FSST symbol table and the full
8,192-entry offset table on every call. Neither is inherent to per-value access;
both are listed as open questions below.

An earlier revision of this proposal reported 307 µs and a 3,000× ratio. Those
figures predate `ff827a83` and were inflated by a reader defect — a stripe's first
mark lookup decoded that stripe's whole marks page set, ~4 ms, unrelated to free
text — which happened to fall on the arena-less file far more often, because the
arena changes how many granules a stripe holds. `design.md` §0 records the
correction in full.

That is a real cost on a cold, scattered, one-off free-text point read, and no
cost at all on the repeat-access pattern the column cache serves. Paying 43% of
every file, on every publish and rewrite, to avoid it unconditionally is the wrong
default.

One qualification, since it bounds the claim. The block answers the point read
only in the stored form the encoder picked for it: `choose_string_transform` sends
a free-text column to FSST when its values average 64 bytes or fewer, to a
dictionary when cardinality is low, and to `RawString` otherwise — and `RawString`
is not among the transforms the reader addresses per value, even though its stored
form is a raw byte arena plus an offset table. So free text with long bodies falls
back to one shared whole-granule decode per granule, which is correct and bounded
but is not per-value access. `design.md` §4 states the guarantee against the stored
form rather than unconditionally, and records closing that gap as the follow-up it
is.

## What Changes

**The free-text per-row arena becomes opt-in, and the writer stops emitting it by
default.** The requirement changes from "the reader uses the index when the file
carries it" to "a single-row free-text point lookup does not decode the whole
granule block", with two ways to satisfy it: the column block's own per-value
byte-range path, which every FSST block supports without any extra stored bytes,
or the per-row arena when a deployment has measured cold free-text point reads as
hot enough to buy it back.

**Nothing about the read contract changes.** `typed_column_row_offsets` keeps its
current meaning, encoding, and placement. A file written before this change still
carries its arena, still declares the flag, and is still read through it. A reader
that does not declare the feature still falls back to the column block and still
returns byte-identical values. There is no format-version bump, no new required
feature, and no rewrite of existing files: the flag was already declared per file
and already droppable, which is exactly what makes turning the writer off safe.

**Free-text keeps a point-access guarantee.** The guarantee is now carried by the
column block itself rather than by a duplicate copy of the data, which is what
`is_per_value_addressable` promised for FSST all along.

## Capabilities

### Modified Capabilities

- `hef-column-design` — MODIFY "Free-text shredded by schema declaration" so the
  point-lookup guarantee is "does not decode the whole granule block", satisfied
  by the free-text block's own per-value byte-range path and not requiring a
  duplicate per-row arena; the bulk-egress path is unchanged.
- `hef-column-design` — MODIFY "Per-row byte-offset index makes wide typed columns
  point-accessible" so the per-row arena is opt-in for free-text (a writer SHALL
  NOT emit it by default) while staying available for column kinds whose blocks
  are not per-value addressable, and so a file that carries no arena still meets
  the point-access guarantee through the block's own offsets.

## Impact

- **A declared free-text field stops being stored twice.** On the measured
  dataset the file drops from 14,527,554 to about 8.2 MB — 43% smaller — with no
  change to any value it returns.
- **Publication and rewrite stop hashing and fsyncing the duplicate.** The arena
  sits inside the stripe, so its bytes are in the stripe's BLAKE3 input and in
  every durability barrier; removing them removes that work from every publish and
  every rewrite, not just from the file at rest.
- **Cold, scattered free-text point reads get slower.** ~172 µs instead of ~459 ns
  for the first read into a granule, and ~2.4 µs instead of ~65 ns once the granule
  is in the reader's column cache. A deployment that measures this as hot turns the
  arena back on per build.
- **No reader is affected and no file needs rewriting.** Old files keep their
  arena and keep using it; new files use the block path; readers without the
  feature behave exactly as they do today.
- **INV holds end to end.** BLAKE3 stays the sole integrity authority. Both paths
  decode to the same row bytes — verified across all 100,000 rows of the measured
  dataset — so results, ordering, and visibility are unchanged, and the arena
  stays droppable acceleration state under the feature directory (Decision 16/20 —
  closed file shapes, every acceleration droppable).

## Open Questions

1. **Whether the block path's per-read cost should be closed at its source.** Most
   of the 172 µs is the whole-block Zstd undo plus a symbol-table and offset-table
   rebuild that `decode_fsst_string_range` repeats on every call; neither is
   inherent to per-value addressing. Fixing that would make the arena unnecessary
   for every workload rather than most, but it is a decode-path change to the
   encoding capability, not a format change, so it is left out of this one.
2. **Whether the reader's probe budget before it decodes and caches a whole
   granule is right for free-text.** At the measured costs, 32 byte-range probes
   (~5.5 ms) is worse than decoding the granule once (~690 µs) after roughly the
   fourth probe. The threshold is a reader tuning constant shared by every column
   kind, so re-tuning it belongs with the measurement that motivates it.
3. **Whether any column kind still needs the arena in practice.** The writer emits
   it only for free-text today; embedding/vector blocks use the raw-string
   transform, which is not per-value addressable, so the reader path for them is
   kept, but nothing writes one yet.
4. **Whether `RawString` should become per-value addressable.** It already stores a
   raw byte arena plus an offset table — the same shape as the arena this change
   turns off — so a range decoder over it would close the long-bodied free-text gap
   above. It is a decode-path change to the encoding capability, so it is not
   folded in here.
