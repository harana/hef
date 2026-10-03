The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-duckdb-learnings/).

Status: Approved (2026-08-18)

## Why

The 2026-08-18 DuckDB storage evaluation (`docs/HEF-vs-DuckDB-storage-evaluation.md`,
reading `harana-oss/duckdb` at `11dc00c89`, storage version 69) compared HEF
against a system whose toolkit is smaller but wired end to end. Most of
DuckDB's machinery answers in-place mutability and does not transfer; where the
two overlap, the evaluation found HEF paying real bytes and real risk in the
unglamorous places DuckDB has already ground smooth: every nullable block
carries a raw one-bit-per-row bitmap even when nothing is null; constant
blocks are spec'd to answer from metadata but full bytes are still written and
fetched; nothing anywhere decides *when* to compact, so the dual roll trigger
guarantees low-volume tenants an unbounded drizzle of small files; encoding
selection scores on size alone so the spec's "fastest valid pipeline" is
unhonourable; and dictionary-or-FSST as an either/or loses code pushdown on
the middle ground DuckDB's DICT_FSST was built for.

This change incorporates those learnings on the `incorporate-vortex-learnings`
pattern: normative borrows become spec deltas, mechanisms become tasks naming
their requirement, and covered/deferred/rejected ideas are recorded in
`design.md`.

## What Changes

Spec deltas (all ADDED requirements):

1. `hef-encodings-and-compression` — ADD "Presence and null bitmaps are
   encoded side streams" (P0-A): `all_present`/`all_absent` at zero bytes,
   roaring for sparse or clustered nulls, raw as fallback and reference;
   required `compressed_presence` feature bit, dual-emit during migration;
   rank/select gains its compressed substrate.
2. `hef-encodings-and-compression` — ADD "Constant and all-null blocks store
   no data bytes" (P0-B): flags derived from stats the footer already carries,
   the constant representable for every stats-bearing type, block bodies
   elided behind zero-length marks; the existing metadata-only-answers
   requirement finally has nothing left to fetch.
3. `hef-encodings-and-compression` — ADD "Decode-cost preference is a
   deterministic score" (P1-A): fixed per-family penalty multipliers per
   lifecycle strategy; wall-clock never an input, so "fastest valid pipeline"
   is honoured without breaking byte determinism.
4. `hef-encodings-and-compression` — ADD "FSST over dictionary values is a
   sampled cascade level" (P1-B): DICT_FSST's three modes recovered inside
   HEF's cascade vocabulary; sorted-code pushdown untouched; 2× worst-case
   expansion budgeted; front coding competes at the same slot.
5. `hef-file-lifecycle` — ADD "Compaction scheduling with an anti-thrashing
   bound" (P0-C): size-tiered adjacent runs, bounded merge width, the doubling
   bound that stops tail-file thrashing, bounded work per cycle, the existing
   WAF gates as the policy's acceptance test.
6. `hef-reader-compatibility` — ADD "Encoding availability windows gate
   conformance" (P2-A): introduced-at/retired-at per pipeline family,
   mechanical rejection of out-of-window blocks.
7. `hef-benchmarks-and-acceptance-gates` — ADD "Forced-pipeline conformance
   coverage" (P2-B): a harness-only forcing input so every codec path is
   exercised deliberately; unreachable in production.
8. `hef-aggregation-metadata` — ADD "Per-stripe distinct-count estimates for
   the planner" (P2-C): exact-or-HLL NDV per promoted column per stripe,
   single-pass at build; persisted row samples explicitly refused.

Covered elsewhere (P1-A part one): stratified whole-block sampling is already
the ADDED requirement "Stratified sampling through one shared statistics pass"
in `incorporate-vortex-learnings`; this change adds only the deterministic
penalty score on top of it.

Implementation-only (P1-C): ALP-RD is already permitted by "Mandatory
representations for money and floats" and lands as tasks, together with the
per-vector raw escape inside ALP blocks (an interior encoding detail governed
by the recorded pipeline, not the pinned footer layout).

Deferred with its design debt named (P1-D): journal-by-reference bulk ingest —
staging sealed HEF files and journaling a reference frame to halve backfill
write amplification — stays an open question until its recovery story
(acknowledgement gating on staged-object durability, replay treating the
reference as a leaf) gets a real design pass.

## Capabilities

### Modified Capabilities

- `hef-encodings-and-compression`: four ADDED requirements (presence side
  streams, constant-block elision, deterministic decode-cost score, dictionary
  +FSST cascade).
- `hef-file-lifecycle`: one ADDED requirement (compaction scheduling policy).
- `hef-reader-compatibility`: one ADDED requirement (availability windows).
- `hef-benchmarks-and-acceptance-gates`: one ADDED requirement (forced-pipeline
  conformance).
- `hef-aggregation-metadata`: one ADDED requirement (per-stripe NDV).

## Impact

- **Real bytes on every nullable and constant block.** The presence side
  stream deletes a kilobyte of zeros per 8192-row nullable block in the
  common case, and constant-block elision reduces constant string columns to
  footer bytes — the two most transferable wins the evaluation found,
  multiplied by cross-stream sparsity being HEF's normal case.
- **The small-file problem gets a policy, not a hope.** The doubling bound and
  bounded merge width translate DuckDB's battle-tested vacuum rules to
  immutable, manifest-published files, with the already-specced WAF gate as
  enforcement.
- **Two required feature bits** (`compressed_presence`, constant-elision)
  because both change how block bytes are framed — governed exactly like
  `columnar_marks`, with dual-emit migration. Everything else is additive:
  penalties and cascades change writer choices inside the recorded-pipeline
  contract; windows and forcing are validation and harness tightening.
- **Determinism strengthened, not spent.** The penalty score exists precisely
  because measuring decode speed at encode time would break byte determinism;
  selection stays a pure function of content, stage, and pinned constants.

## Open Questions

1. **Journal-by-reference bulk ingest (P1-D).** Promote only with a designed
   recovery contract: the reference frame's acknowledgement gates on staged
   object durability, replay verifies the object's BLAKE3 as a leaf, and the
   route stays internal to backfill. Normal-path ingest semantics are
   untouched regardless.
2. **Penalty multiplier values.** The tables are pinned constants; their
   values come from the benchmark harness per family and strategy (DuckDB's
   1.2×/2×/length-floor are reference points, not answers).
3. **Sub-block pipeline adaptivity.** DuckDB picks a bitpacking mode per
   2048-value group inside one segment; HEF pages already carry per-page
   pipeline ids, which is the natural home if mixed blocks ever justify
   splitting their choice — revisit with measurement, no delta now.
