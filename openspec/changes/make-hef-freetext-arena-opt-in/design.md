# Design — The free-text per-row arena becomes opt-in

`add-hef-wide-column-point-access` decided a question against a reader that has
since changed underneath it. §0 re-runs that measurement against the reader as it
stands; §1–§3 are the resulting decisions, in the tension → decision → why →
rejected → spec-edits style of `docs/design-review-decisions.md`.

## 0. The premise that justified the arena has expired

The earlier design's §0 recorded that "`shredded_value_for_row` … `decode_block`s
the **entire granule column block** … before it can return the one requested row",
and that free-text is read through exactly that path. That was true when it was
written. It stopped being true when the per-value byte-range point-read path
landed: `shredded_value_for_row` now checks
`raw.pipeline.supports_byte_range_extraction()` and, for a single-page block,
calls `decode_block_range_shared` for the one row it wants.
`Transform::is_per_value_addressable` names `FsstString` among the three
transforms that qualify, and an FSST block stores an offset table over its
compressed values precisely so a byte range can be lifted out of it.

So a declared free-text column is now per-value addressable through its own
column block, and the arena stores a second, uncompressed copy of every value it
already holds.

**Redundancy, measured.** `crates/format-benchmark`'s HEF dataset, 100,000 rows,
8,192 rows per granule, 13 granules, one declared free-text field (`note`,
averaging ~55 bytes), release build:

```text
whole file                                        14,522,366 B   100%
  note column blocks (FSST + Zstd1, 13 granules)     458,976 B     3.2%
  free-text arena, raw value bytes                 5,489,799 B    37.8%
  free-text arena, offsets (8 B x 100,000 rows)      800,000 B     5.5%
  free-text arena, total                           6,289,799 B    43.3%
```

The arena holds the same values as the column block at 13.7× the size. It is
written inside the stripe, so those bytes are in the stripe's BLAKE3 input and in
every durability barrier of every publication and every rewrite.

**Equivalence, verified.** Re-encoding the footer with
`TYPED_COLUMN_ROW_OFFSETS` cleared — every stripe byte untouched, so the file
still physically carries the arena but no longer declares it — and reading all
100,000 rows through both readers returns byte-identical values on every row.

**The cost of dropping it, measured on the same file**, one reader reading one
row from each of the file's 13 granules in turn, then repeating inside a resident
granule.

```text
file size                          arena 14,522,366 B  block 8,181,058 B
cold read into a granule (median)  arena       459 ns  block     172 us
repeat read, granule resident      arena        65 ns  block     2.4 us
```

An earlier draft of this section reported the block path at 307 µs and a 3,000×
cold-read ratio. Both were wrong, and the correction matters enough to record.
The figures were taken before `ff827a83`, when the reader's first mark lookup into
a stripe decoded that stripe's whole marks page set at roughly 4 ms — a cost that
had nothing to do with free text. It landed unevenly between the two files: the
arena inflates the stripe byte estimate, so the indexed file cut one granule per
stripe where the plain file cut two, and only the two-granule stripes tripped the
defect. The arena was partly being credited for dodging a bug elsewhere. The
numbers above are measured against the fixed reader.

What survives the correction is the shape, not the magnitude. One "per-value"
block read still costs ~172 µs, and ~127 µs of that is `remove_trailing`
decompressing the whole 116,075-byte block to reach one value; the remainder is
`decode_fsst_string_range` rebuilding the FSST symbol table and the full
8,192-entry offset table per call. Neither is a property of per-value addressing,
and both are per-read costs the block path pays and the arena does not. The gap
closes to 2.4 µs once the reader's column cache holds the granule, so this is a
cold-and-scattered cost rather than a steady-state one.

## 1. The point-access guarantee moves from the index to the block

- **Tension.** The free-text requirement now guarantees point access *via the
  per-row index*, which reads as though the index is what makes point access
  possible. It is not: the block's own FSST offsets already do, and stating the
  guarantee in terms of the index forces every file to carry a duplicate copy of
  its free text to satisfy a sentence.
- **Decision.** State the guarantee as the property callers actually depend on —
  a single-row free-text point lookup does not decode the whole granule block —
  and name both ways to satisfy it: the block's own per-value byte-range path, or
  the per-row arena when a build opts into one.
- **Why.** It is the same promise, held by the thing that already holds it. A
  requirement should pin the observable behaviour, not one of the two mechanisms
  that produce it; pinning the mechanism is what turned a 3.2% column into a 46.5%
  one.
- **Rejected.** *Drop the point-access sentence from the free-text requirement
  altogether.* — The guarantee is real and worth keeping; a reader that decodes
  8,192 rows to answer one is a regression whether or not an index exists.
  *Keep the index-worded guarantee and shrink the arena instead* (store FSST-
  compressed bytes in it, or offsets only) — that is a second addressing scheme
  over the same block the FSST offset table already addresses, which is more
  format, not less.
- **Spec edits.** `hef-column-design` MODIFY "Free-text shredded by schema
  declaration".

## 2. The arena becomes opt-in, and the writer defaults it off

- **Tension.** 43% of every file, hashed and fsynced on every publish and every
  rewrite, buys a cold free-text point read at 459 ns instead of 172 µs. The
  benefit is real but conditional on a workload — cold, scattered, one-off
  single-row free-text reads — while the cost is unconditional. Defaults should
  fall on the side of the cost that everyone pays.
- **Decision.** The writer SHALL NOT emit the free-text per-row arena by default.
  A build may opt in. The `typed_column_row_offsets` optional feature keeps its
  current meaning and encoding, so a file that opts in is exactly the file the
  writer produces today.
- **Why.** This is what the optional-feature discipline is for: the flag is
  declared per file and the reader already handles both states, so the default
  can move without a format version, a required feature, or a rewrite. The
  workload that wants the arena can measure and ask for it; the workloads that do
  not stop paying for it silently.
- **Rejected.** *Remove the free-text arena entirely.* — It is a genuine 170×
  on the cold path, and the reader must keep decoding it for files already
  written, so removing the writer side would strand reader code and leave no way
  to buy the latency back. *Gate it on measured point-lookup heat inside the
  writer.* — The earlier change already left this open as a tuning question; heat
  statistics for a column nobody has queried yet do not exist at write time, and
  inventing a heuristic is more machinery than a flag. *Keep it on by default and
  compress the arena.* — Compressing it destroys the offset-jump the arena exists
  for, which is the same trap the residual arena's `ResidualCompression::None`
  already documents for hot granules.
- **Spec edits.** `hef-column-design` MODIFY "Per-row byte-offset index makes wide
  typed columns point-accessible".

## 3. Reader compatibility is a non-event, and that is the point

- **Tension.** This is a change to what bytes a file contains, so the reflex is a
  format-version bump and a compatibility story. But every reader already has to
  handle both states of this flag.
- **Decision.** No format-version bump, no new required feature, no migration, no
  rewrite. Old files keep their arena, keep declaring `typed_column_row_offsets`,
  and keep being read through it. New files declare it only when the build opted
  in. A reader that does not understand the feature keeps falling back to the
  block and keeps returning identical values — the fallback the earlier change
  already specified and conformance already pins.
- **Why.** The feature directory's contract is that an optional feature may be
  present or absent per file and a reader must be correct either way. A writer
  changing how often it emits one exercises that contract; it does not extend it.
  The one thing worth pinning is the direction nothing tested before: that a file
  carrying *no* arena still meets the point-access guarantee, through the block.
- **Rejected.** *Bump `format_version`.* — Version gates readers out of files
  they can read correctly; nothing here is unreadable by any reader that could
  read the previous file. *Rewrite existing files to drop their arenas.* — A
  rewrite is a durable, checksummed, fsynced operation over the whole corpus to
  reclaim space that costs nothing to leave alone; files shed the arena on the
  next rewrite they were going to do anyway.
- **Spec edits.** Covered by the two MODIFYs above; the reader contract in
  `hef-reader-compatibility` is unchanged.

## 4. Which stored form the block lands in decides whether it can answer the point read

- **Tension.** §0's equivalence holds for the measured dataset because the writer stored `note` as FSST. It does not
  hold unconditionally. `choose_string_transform` picks FSST only when the sampled values average 64 bytes or fewer
  over at least 16 of them; a low-cardinality column goes to `DictionaryString`, and everything else — notably free
  text with bodies longer than 64 bytes on average — goes to `RawString`. FSST and dictionary blocks are named in
  `Transform::is_per_value_addressable`; `RawString` is not, even though its stored form is literally a raw byte arena
  plus an offset table, the same shape as the arena this change turns off. So the longer the free-text bodies, the
  more likely the block is the one form the reader will not address per value, and a point read falls back to
  decoding the granule.
- **Decision.** State the guarantee as conditional on the stored form, and require that where the form is not
  per-value addressable the reader still returns the correct value by decoding the granule's block once and sharing
  that decode across the granule's rows — which is what the reader's column cache already does. Do not change the
  transform's addressability in this change.
- **Why.** A requirement that promises per-value access for every free-text column would be false the moment a tenant
  declares a field whose bodies run long, and no writer default can make it true. The conditional statement is
  accurate, and the fallback it names is bounded and already implemented. Teaching the reader to address `RawString`
  per value is the right fix, but it is a decode-path change in the encoding capability with its own correctness
  surface, and folding it in here would mean this change could not be reviewed on the format question alone.
- **Rejected.** *Force free-text columns to FSST regardless of value length.* — Compression selection is sampled per
  block for a reason, and pinning a transform to satisfy a point-access sentence trades bulk-egress size for a
  latency the arena opt-in already covers. *Keep the arena on by default for long-bodied free-text columns.* — A
  per-column heuristic keyed on average value length is exactly the write-time tuning §2 rejected, and it is worst
  where the duplication is largest.
- **Spec edits.** Covered by the two MODIFYs; both state the guarantee against the block's stored form rather than
  unconditionally.

## Open sub-tensions

- **`RawString` should be per-value addressable, and is not.** Its stored form is a null stream, a present count, an
  offset table, and the concatenated bytes — the same framing `decode_fsst_string_range` walks, minus the symbol
  table. Adding it to `Transform::is_per_value_addressable` with a matching range decoder would close §4's gap and
  make the arena redundant for long-bodied free text too, which is where the duplication costs most.
- **The block path's per-read cost is mostly fixable, and left unfixed here.** The
  whole-block Zstd undo and the per-call symbol-table and offset-table rebuild in
  `decode_fsst_string_range` are decode-path work in the encoding capability, not
  format. Closing them would make the arena unnecessary for every workload rather
  than most, and would change the answer to §2's default question by making the
  gap small enough that nobody opts in.
- **The reader's 32-probe budget before it decodes and caches a whole granule is
  mistuned for this shape.** At ~172 µs per probe against ~690 µs for the whole
  block, the granule should be decoded and cached after about the third probe, not
  the thirty-second. It is a reader constant shared by every column kind, so
  re-tuning it needs its own measurement across kinds.
- **No writer emits an embedding/vector arena.** The reader path for one exists
  and the requirement covers it, but `build.rs` writes `embedding_row_offsets:
  Vec::new()` unconditionally. The raw-string transform those blocks use is not
  per-value addressable, so the arena is the only point-access mechanism available
  to them — which is why the opt-in is scoped to free-text rather than to the
  feature as a whole.
