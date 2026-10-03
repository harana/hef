The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-lance-learnings/).

# Tasks — incorporate-lance-learnings

> Spec deltas first; then the implementation work, each task naming the
> requirement it implements. The disposition of every evaluation idea —
> including the ones folded into `add-hef-remote-read-scheduler` or owned by
> `incorporate-duckdb-learnings` — is the register in `design.md`.

## Spec deltas

- [x] ADD `hef-query-metadata-and-indexes` — "Heavy skip indexes publish as index artifacts outside the file".
- [x] ADD `hef-manifest-integration` — "Index artifacts ride the manifest generation".
- [x] ADD `hef-write-path` — "The builder streams at stripe scope with bounded memory".
- [x] ADD `hef-benchmarks-and-acceptance-gates` — "Writer peak-memory gate".
- [x] ADD `hef-deletes-and-corrections` — "Deletion vectors choose their wire encoding by density".
- [x] ADD `hef-apis` — "Reader decoded-block caches are bounded".
- [x] ADD `query-execution` — "Late materialization follows a pinned policy".
- [x] ADD `hef-physical-artifacts` — "External payload references carry a pinned descriptor".
- [x] ADD `hef-reader-compatibility` — "Released-version fixture corpus after format freeze".
- [x] ADD `object-store` — "Durable object keys carry a high-entropy prefix".
- [x] Fold the §3.6 scheduler rules into `add-hef-remote-read-scheduler` (requirement text, scenarios, tasks, design §5).

## Code — index artifacts (req: hef-query-metadata-and-indexes "Heavy skip indexes publish as index artifacts outside the file"; hef-manifest-integration "Index artifacts ride the manifest generation")

- [x] Artifact object format: header (kind, column/path, projection, exactness, fpr, granularity), coverage record (`(file_id, granule range)` set, deletion-vector generation, schema fingerprint), directory-then-pages layout for progressive load, BLAKE3-sealed (`hef/indexes/artifact.rs`).
- [x] Persistence paths for the built-but-unpersisted kinds, in pruning-value order: `path_presence`, `binary_fuse` / `split_block_bloom`, `range_filter` (Grafite), `learned_position`, `bitmap` — each writing an artifact, none touching the data file.
- [ ] Manifest generation references: artifact identity + BLAKE3 + kind + coverage summary published atomically with the generation; drop = omit from next generation; sweeper retirement per the safety window. (The `ManifestGeneration` reference type, drop-is-omission diff, and sweeper key computation landed; no publisher populates them yet.)
- [ ] Artifact reader feeding the existing `PruningTerm` variants: covered granules gain artifact terms, uncovered granules keep footer-tier terms; staleness rules (inexact survives a superseded DV generation, exact does not; fingerprint mismatch never used). (`term_for` and the staleness matrix landed with tests; no scan path invokes them yet.)
- [ ] Async build driver gated by the workload-heat signal that already gates `PageMinMax`; build stats counted through storage observability.
- [ ] Tests: sealed-file index add (bytes/`file_id` unchanged), partial-coverage split equivalence against full scan, drop-and-fallback correctness, staleness matrix, cold open fetches zero artifact bytes. (Sealed-file add and the staleness matrix are tested; the scan-level equivalence, query fallback, and cold-open legs wait on the reader wiring above.)

## Code — streamed build (req: hef-write-path "The builder streams at stripe scope with bounded memory"; hef-benchmarks-and-acceptance-gates "Writer peak-memory gate")

- [ ] Restructure `build_hef_file_with_executor` to hand off each sealed stripe for upload/append and retain only footer directories (`writer/build.rs`, `writer/publish.rs`).
- [ ] Incremental digests: per-stripe BLAKE3 at layout, running whole-file BLAKE3 + CRC-64/NVME over streamed bytes, outboard tree composed from streamed chunk-group digests (subtree composition per `writer/compaction.rs`), no second hashing pass. (The one-pass whole-file BLAKE3 + CRC-64/NVME seal landed; the streamed path attaches no outboard tree yet.)
- [x] Byte-identity test: streamed build vs materialized build under `SerialEncodeExecutor` — identical `file_id`, stripe hashes, `file_blake3`, tree.
- [x] Splice-rewrite composition: reused stripes stay server-side copies; assert rewrite peak memory bounded by one rebuilt stripe.
- [ ] Named writer peak-memory benchmark at fresh publish and rewrite on the declared profile; commit baselines; wire the gate.

## Code — deletion vectors, caches, scan policy (req: hef-deletes-and-corrections "Deletion vectors choose their wire encoding by density"; hef-apis "Reader decoded-block caches are bounded"; query-execution "Late materialization follows a pinned policy")

- [x] Two-form deletion-vector encoder/decoder behind `DeletionVectorWireForm` with the pinned density threshold; identical intersection results equivalence-tested across both forms (`hef/deletes.rs`).
- [x] Byte-budgeted eviction for `column_cache` / `inflated_residuals` / `point_probe_counts` (`layout/reader.rs`); bulk-read bypass asserted; budget distinct from `add-page-granular-object-cache`'s verified-bytes budget.
- [ ] Pinned late-materialization defaults (width × tier × selectivity) in the scan's code-parameter registry entry; fetch-schedule test proving result identity under both schedules. (The width threshold on the local tier and the result-identity tests landed; the durable-tier threshold is unused and selectivity is not yet an input.)

## Code — deferred-shape items (req: hef-physical-artifacts "External payload references carry a pinned descriptor"; hef-reader-compatibility "Released-version fixture corpus after format freeze"; object-store "Durable object keys carry a high-entropy prefix")

- [x] External-payload descriptor type with closed `kind` domain and BLAKE3 verification on resolve; writer emission stays refused until a placement kind's machinery lands (`artifacts/batch.rs`).
- [x] Fixture-corpus harness skeleton (generator asserting writer version, CI reader over `test_data/`), armed at format freeze.
- [x] High-entropy key prefix derivation from `file_id` in the durable-tier key builder; applies to new objects only.

## Verification

- [ ] `openspec validate incorporate-lance-learnings --strict` green
- [ ] Each landed code task closes with its equivalence oracle green (full-coverage scan, materialized build, both DV forms, both fetch schedules) and `cargo test -p storage --features write --lib` green
