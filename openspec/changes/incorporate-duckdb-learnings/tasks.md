The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-duckdb-learnings/).

# Tasks — incorporate-duckdb-learnings

> Spec deltas first; then the implementation work, each task naming the
> requirement it implements. Dispositions — including the ideas covered by
> `incorporate-vortex-learnings`, owned by the sibling Lance/Nimble changes, or
> deliberately refused — are the register in `design.md`.

## Spec deltas

- [x] ADD `hef-encodings-and-compression` — "Presence and null bitmaps are encoded side streams".
- [x] ADD `hef-encodings-and-compression` — "Constant and all-null blocks store no data bytes".
- [x] ADD `hef-encodings-and-compression` — "Decode-cost preference is a deterministic score".
- [x] ADD `hef-encodings-and-compression` — "FSST over dictionary values is a sampled cascade level".
- [x] ADD `hef-file-lifecycle` — "Compaction scheduling with an anti-thrashing bound".
- [x] ADD `hef-reader-compatibility` — "Encoding availability windows gate conformance".
- [x] ADD `hef-benchmarks-and-acceptance-gates` — "Forced-pipeline conformance coverage".
- [x] ADD `hef-aggregation-metadata` — "Per-stripe distinct-count estimates for the planner".

## Code — presence side streams (req: hef-encodings-and-compression "Presence and null bitmaps are encoded side streams")

- [x] Replace the fixed `presence_len | bitmap | body` frame with an encoded presence side stream: `all_present` / `all_absent` / set-run and set-position lists / raw, chosen smallest deterministically, recorded form per block (`writer/build.rs`, `layout/mod.rs`).
- [x] Retire the unconditional `split_nulls`/`read_null_bitmap` raw bitmap inside string transforms in favour of the shared side stream.
- [x] `compressed_presence` required feature bit, legacy-frame reader fallback (pre-feature files stay readable; new files refuse on old readers by the bit), old-reader refusal test.
- [x] Rank/select over the compressed forms without materializing the raw bitmap; equivalence-tested against the raw path (`indexes/rank_select.rs`, `layout/reader.rs`).

## Code — constant/all-null wiring and elision (req: hef-encodings-and-compression "Constant and all-null blocks store no data bytes"; hef-query-metadata-and-indexes "Constant and all-null flags enable metadata-only answers")

- [ ] Derive the flags from existing `PageStats`/`PageDirectoryEntry` min/max and null counts (`min == max && null_count == 0`; `null_count == row_count`) and feed `MetadataAnswer::{AllRowsMatch, NoRowsMatch}` into the falsification expression — the currently dead `indexes/constant_flags.rs` path. (Derivation and the `i128` widening landed; the falsification-expression feed has no caller yet.)
- [ ] Widen `ColumnFlags::constant_value` beyond `i64` to every stats-bearing type (strings, decimals, timestamps) and materialize constant/all-null projections from metadata. (Widened to `i128` — decimals and timestamps; strings still outstanding.)
- [ ] Elide constant/all-null block bodies: zero-length mark extents, required-feature governance, dual-emit migration, round-trip tests against un-elided files. (Constant integer blocks elide end to end; all-null and non-integer constants still outstanding.)

## Code — selection score and dictionary cascade (req: hef-encodings-and-compression "Decode-cost preference is a deterministic score", "FSST over dictionary values is a sampled cascade level", "Stratified sampling through one shared statistics pass" from incorporate-vortex-learnings)

- [ ] Pinned per-family penalty tables per lifecycle strategy applied to estimated sizes in `choose_*_transform` and trailing selection; no timing input; same-decision determinism tests (`encoding/mod.rs`, `encoding/constant.rs`). (Landed for `choose_u64_transform` with determinism tests; the string/float choosers and trailing selection still score untaxed.)
- [x] `Fsst(dictionary_values)` inner cascade level for `DictionaryString` blocks with the 2× worst-case expansion budget and per-value length cap; sampling across plain/dict+FSST/FSST-only; code-predicate pushdown untouched and equivalence-tested.
- [x] ALP-RD kernel for the high-entropy doubles ALP rejects, competing where byte-stream-split wins today (P1-C; already spec-permitted).
- [x] Per-vector raw escape inside ALP blocks behind a sentinel exponent, so one pathological 1024-value vector no longer disqualifies the block at the exception threshold; round-trip + conformance coverage.

## Code — compaction scheduling (req: hef-file-lifecycle "Compaction scheduling with an anti-thrashing bound")

- [ ] Scheduling policy over manifest state: size-tiered adjacent-run candidate selection per tenant, bounded merge width, doubling bound, bounded work per cycle; drives the existing `compaction.rs` planning (`lifecycle.rs`). (The policy and its simulation tests landed; nothing calls `plan_compaction_cycle` yet, so it does not drive `compaction.rs`.)
- [x] Simulation tests: a trickle-append tenant never rewrites its tail file twice without the doubling bound met; accumulated runs converge to roll-target-sized files.
- [ ] Rewrite-WAF benchmark leg exercising the policy; the existing WAF gate as pass/fail.

## Code — validation, harness, planner stats (req: hef-reader-compatibility "Encoding availability windows gate conformance"; hef-benchmarks-and-acceptance-gates "Forced-pipeline conformance coverage"; hef-aggregation-metadata "Per-stripe distinct-count estimates for the planner")

- [ ] Availability-window table per pipeline family (introduced-at / retired-at) and the out-of-window rejection in footer validation and conformance, generalizing the retired-transform-id-11 rule (`compat/`). (The pinned table and block-read-time rejection landed; the conformance harness does not yet exercise the windows.)
- [x] Harness-only forced-pipeline builder input (injected like the simulation clock); conformance loop forcing every family; unreachable from production construction.
- [ ] Per-stripe exact-or-HLL NDV per promoted column emitted in the build pass; planner wiring for join/grouping estimates; no persisted row samples. (Emit, footer persistence, and the reader accessor landed; the planner does not consume the estimates yet.)

## Verification

- [ ] `openspec validate incorporate-duckdb-learnings --strict` green
- [ ] Each landed code task closes with its equivalence oracle green (raw-bitmap path, un-elided blocks, per-block dictionaries, ungoverned compaction runs under simulation) and `cargo test -p storage --features write --lib` green
