The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-vortex-learnings/).

# Tasks — incorporate-vortex-learnings

> Spec deltas first; then the implementation-guidance borrows, each naming the
> requirement it implements. The disposition of every review idea — including
> the ones already covered by landed work or active changes — is the register
> in `design.md`.

## hef-encodings-and-compression — spec deltas

- [x] ADD "Encoded blocks never grow past the plain form": full-block (and per-cascade-level) `after < before` verification with plain fallback.
      Implements `hef-encodings-and-compression` — "Encoded blocks never grow past the plain form".
- [x] ADD "Stratified sampling through one shared statistics pass": whole-block evenly spaced runs at fixed positions, quantum-aligned totals, one stats bundle, cheap disproofs.
      Implements `hef-encodings-and-compression` — "Stratified sampling through one shared statistics pass".
- [x] ADD "Encoder telemetry pairs estimated with achieved ratios".
      Implements `hef-encodings-and-compression` — "Encoder telemetry pairs estimated with achieved ratios".
- [x] ADD "Encoder correctness rides law suites, an oracle fuzzer, and decision snapshots".
      Implements `hef-encodings-and-compression` — "Encoder correctness rides law suites, an oracle fuzzer, and decision snapshots".
- [x] MODIFY "Compressed-data string predicates": FSST prefix/substring answered by the compressed-domain automaton; range still declines.
      Implements `hef-encodings-and-compression` — "Compressed-data string predicates".

## query-execution — spec delta

- [x] ADD "Adaptive conjunct ordering from measured selectivity".
      Implements `query-execution` — "Adaptive conjunct ordering from measured selectivity".

## hef-query-metadata-and-indexes — spec deltas

- [x] ADD "Falsification pruning evaluates columnar across a stripe's granules".
      Implements `hef-query-metadata-and-indexes` — "Falsification pruning evaluates columnar across a stripe's granules".
- [x] ADD "A cold open retains the bytes its tail read already fetched".
      Implements `hef-query-metadata-and-indexes` — "A cold open retains the bytes its tail read already fetched".

## Code — encoder honesty (req: hef-encodings-and-compression "Encoded blocks never grow past the plain form", "Stratified sampling through one shared statistics pass", "Encoder telemetry pairs estimated with achieved ratios")

- [ ] Acceptance gate in `encode_block` and each cascade level: keep the winner only when strictly smaller than plain; record the stored form's pipeline (`hef/encoding/mod.rs`)
- [ ] Replace the prefix transform sample with evenly spaced runs at fixed positions; size the total to a multiple of 1024 (`hef/encoding/mod.rs`, reusing the `trailing_sample` window shape)
- [ ] One `BlockStats` bundle per sample (min/max, run count, distinct lower bound via length+prefix hash, top value) read by all candidate estimators
- [ ] Order candidates cheapest-estimate-first and pass the incumbent best size into expensive trials (trial deflate, FSST training) so they can decline early (§2.4)
- [ ] Score `DictionaryString` code streams through the FOR/DELTA/RLE chooser instead of raw width (§2.6); express the lifecycle bias as named per-family multipliers (§2.5)
- [ ] Emit estimated vs achieved ratio per family plus the plain-fallback counter through the storage observability layer

## Code — encoded-domain kernels (req: hef-encodings-and-compression "Compressed-data numeric predicates", "Compressed-data string predicates"; implementation-toolchain "SIMD kernels dispatch on detected CPU features at runtime")

- [ ] One-time CPU-feature dispatch primitive (~120 lines, `AtomicPtr` selector, portable tail as default) in `common` or storage; never dispatch inside a per-element loop, size-gate before dispatch (§3.1)
- [ ] Fused unpack+compare for bit-packed predicates: translate the literal into the packed domain from the descriptor, fold the compare into the unpack loop with a transposed 1024-bit accumulator, one untranspose per block; scalar shape first (§3.2, `hef/encoding/predicate.rs`)
- [ ] Predicate loops materialize `[bool; 64]` per output word and pack branchlessly; inline-always default (§3.3)
- [ ] FSST prefix/substring automaton over code bytes, built per predicate from pattern + symbol table; tested against decode-then-match on the fuzz corpus (§3.6c)
- [ ] ALP range bounds encoded into the int domain with strictness flips when the bound does not encode exactly (§3.7)
- [ ] Dictionary pushdown guards: sliced-dictionary size check, fallibility × unreferenced entries, non-strict functions × null codes; `all_values_referenced` flag on dictionary blocks feeds guard (b) (§7, §2.10)

## Code — reductions and rank/select (req: hef-query-metadata-and-indexes "Rank/select over visibility and null bitmaps"; the release-profile overflow posture)

- [ ] Hoist overflow checks in accumulation loops (aggregates, `sum_of_squares`, stats passes, delta encode): prove per-chunk non-overflow (2^16-element chunks), bare vectorizable inner loops, one checked step per chunk — keeping `overflow-checks = true` (§3.4)
- [ ] Rank/select descend-the-hierarchy select and word-at-a-time set-bit iteration in `hef/indexes/rank_select.rs`; `intersect_by_rank` composition for the deletion-vector anti-join, portable fallback first (§3.5)
- [ ] Branchless `search_sorted` shape for marks binary searches and the learned-index confirmation step (§7)

## Code — cascade growth (req: hef-encodings-and-compression "Recursive cascade selection")

- [ ] FOR/DELTA exception lists with the width choice swept over the exception rate; chunk the exception index at 1024; one shared exception container across families (§2.8)
- [ ] Carry the declarative exclusion table from day one of deeper cascades (§2.7)
- [ ] datetime-parts transform candidate for the timestamp columns and mantissa byte-part narrowing for ≤64-bit decimal blocks, both sampled, never statically assigned (§2.9)

## Code — scan and read path (req: query-execution "Adaptive conjunct ordering from measured selectivity"; hef-query-metadata-and-indexes "Falsification pruning evaluates columnar across a stripe's granules", "A cold open retains the bytes its tail read already fetched")

- [ ] Per-conjunct observed-selectivity statistic with lock-free updates; re-sort evaluation order across granules; descriptor estimate seeds the first order (`query` scan)
- [ ] Columnar falsification pass over per-stripe stats arrays, AND-ing term verdict bitmaps; equivalence-tested against the per-granule loop (`hef/indexes/pruning.rs`)
- [ ] Retain data bytes captured by the open's tail read and serve them through normal verification (`hef/layout/reader.rs`)
- [ ] In-flight decode dedup map for hot granules (~40 lines, self-evicting weak futures) (§4.4)
- [ ] Document the pruning/filter mask-intersection contract asymmetry when multi-conjunct pushdown lands (§4.2)

## Tests and harness (req: hef-encodings-and-compression "Encoder correctness rides law suites, an oracle fuzzer, and decision snapshots"; hef-benchmarks-and-acceptance-gates "Acceptance gates are mechanically enforced by the committed harness")

- [ ] Algebraic-law suite run by every transform: range = slice of whole decode, point = indexed element, predicate = decode-then-filter, over empty/single/repeated/null edge cases
- [ ] Encoder oracle fuzz target in `server/fuzz`: arbitrary `ColumnData` → encode under both strategies → diff decode/range/point/predicate against generation-time expectations; refused inputs rejected from the corpus
- [ ] Golden decision snapshots: seeded corpus (>1024 values per entry), recorded pipeline + exact byte counts per lifecycle strategy, every entry encoded twice for determinism
- [ ] One avx2 and one neon walltime benchmark leg before crediting any SIMD change; control-row drift model in the benchmark comparison (§3.8, §8.4)
- [ ] Untrusted `io_alignment_bytes` cap on parse (bounding attacker-forced allocation) (§5.5)
- [ ] Per-column byte attribution rollup in `hef/introspection.rs` (every segment counted exactly once) (§5.4)

## Verification

- [ ] `openspec validate incorporate-vortex-learnings --strict` green
- [ ] Each landed code task closes with its equivalence oracle green (plain-form fallback, per-granule loop, decode-then-filter) and `cargo test -p storage --features write --lib` green
