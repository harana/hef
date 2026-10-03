# Tasks — incorporate-nimble-learnings

> Spec deltas first; then the implementation work, each task naming the
> requirement it implements. Dispositions — including the reinforcements R1/R2
> and implementation items I1/I2 the evaluation raised — are the register in
> `design.md`.

## Spec deltas

- [x] ADD `hef-column-design` — "Sparse shredded columns below the promotion threshold".
- [x] ADD `hef-encodings-and-compression` — "Encoding selection may capture and replay a winning pipeline".
- [x] ADD `hef-encodings-and-compression` — "Dictionary alphabets may be shared at file scope".
- [x] ADD `hef-file-layout` — "Marks may alias identical extents".

## Code — sparse shredded columns (req: hef-column-design "Sparse shredded columns below the promotion threshold")

- [x] Sparse column writer: per-granule presence bitmap (roaring-run container form) + dense value block via the rank/select gather; values removed from the residual at shred time (`hef/columns.rs`, `writer/build.rs`).
- [x] Footer key-set section keyed by schema fingerprint with the pinned per-file key budget; required feature bit wired into `required_features` and refusal tested.
- [x] Tier selection in the shredding statistics loop: dense ≥ the existing promotion threshold, sparse for hot-but-sparse paths under it, residual otherwise; never an operator input.
- [ ] Reader: presence-entry pruning as a falsification term, dense-block gather for present rows, merge semantics tests proving payload reconstruction identical to the two-tier form. (The gather and merge-semantics tests landed; sparse presence entries do not yet contribute a falsification term — pruning rides ordinary page stats.)
- [x] Conformance: budget overflow leaves paths residual; old-reader refusal; byte-determinism of tier selection under the serial executor.

## Code — capture/replay (req: hef-encodings-and-compression "Encoding selection may capture and replay a winning pipeline")

- [x] Capture the winning `(transform, side streams, trailing)` pipeline + parameters per column after full selection; replay for subsequent blocks of the build (`hef/encoding/mod.rs`).
- [x] Re-arm rules: fixed granule cadence and the encoded/raw trip-wire bound, both pinned code parameters.
- [x] Retire the double-compression sample path for replayed blocks (I2): a replayed block runs its one captured trailing codec, not both trials.
- [ ] Optional catalog persistence per `(tenant, event_type, column)` seeding candidate order on the next build; seeded winners still verified.
- [x] Determinism tests: replay decisions are a pure function of build order; byte-identical files across nodes; acceptance gate holds on replayed blocks.

## Code — shared dictionaries (req: hef-encodings-and-compression "Dictionary alphabets may be shared at file scope")

- [x] File-scope alphabet section per column; block scope marker (`block`/`file`); sampling decides per block; sorted-code assignment preserved (`hef/encoding/global_dict.rs` sibling, `writer/build.rs`).
- [x] Required feature bit + refusal test; external scope value reserved and rejected if seen.
- [x] Predicate translation once per file for shared-scope blocks, feeding the existing compressed-form dictionary evaluator; equivalence-tested against per-block translation.

## Code — mark aliasing (req: hef-file-layout "Marks may alias identical extents")

- [x] Build-side dedup: BLAKE3 of each finished block (already computed for page checksums) → byte-exact confirm → alias the mark within the stripe; savings in build stats (`writer/build.rs`).
- [x] Reader-side read-region dedup so an aliased extent is fetched and verified once per scan (`layout/reader.rs`).
- [x] Tests: aliased columns decode originals (including shared-alphabet aliases, which never share a decode); collision-without-equality is closed by the byte-exact confirm (a real BLAKE3 collision is not constructible in a test); deterministic aliasing under the serial executor.

## Reinforcement R1 — finish columnar, per-stripe marks emission (req: hef-file-layout "Granule directory and authoritative marks")

- [x] Writer emits the columnar, per-stripe marks form and promotes `COLUMNAR_MARKS` into `required_features::ALL` (no dual-emit: pre-feature row-oriented files stay readable through the eager reader; new files refuse on old readers by the bit), closing the gap between the implemented lazy reader and the row-oriented writer (`layout/mod.rs`, `writer/build.rs`).
- [x] Co-locate each stripe's marks page with its filter bytes so one IO fetches both (placement task from §3.1; the spec already permits co-location).
- [ ] Metadata-economics gates re-run: pruned-file planner bytes hit the near-zero gate.

## Verification

- [ ] `openspec validate incorporate-nimble-learnings --strict` green
- [ ] Each landed code task closes with its equivalence oracle green (two-tier payload merge, per-block dictionaries, unaliased build, full-selection build) and `cargo test -p storage --features write --lib` green
