The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-vortex-learnings/).

Status: Approved (2026-08-18)

## Why

The 2026-08-18 Vortex source review (companion to
`docs/HEF-vs-Vortex-evaluation.md`, reading the current Vortex tree at
`b825c4f`) surfaced mechanisms the June evaluation did not capture: how Vortex
keeps an adaptive encoder honest (verify after encode, stratified sampling,
one shared stats pass, estimator telemetry), how it gets SIMD-class speed on
stable Rust without intrinsics (shaped scalar loops, fused kernels, one
dispatch primitive), how it prices and prunes work at scan time (measured
conjunct selectivity, vectorized zone pruning), and the testing trio that
keeps ~20 encodings honest with almost no expected-output maintenance. None
of it disturbs the verdicts the earlier evaluation settled — HEF keeps its
closed layout shapes, refuse-on-required posture, sorted dictionaries, and
aggregate tier.

This change incorporates those learnings: the ones that change normative
behaviour become spec requirements; the ones that are implementation shape
for already-spec'd requirements become tracked tasks referencing the
requirement they implement; the ones already covered by landed work or active
changes are recorded as such in `design.md` so they are not re-proposed.

## What Changes

Spec deltas (normative additions, one MODIFIED requirement):

1. `hef-encodings-and-compression` — ADD "Encoded blocks never grow past the
   plain form": after the sampled winner encodes the full block (and each
   cascade level), keep it only if strictly smaller than plain; unlucky
   sampling becomes a non-event, never a size regression.
2. `hef-encodings-and-compression` — ADD "Stratified sampling through one
   shared statistics pass": evenly spaced whole-block sample runs at fixed
   positions, quantum-aligned totals, one stats bundle all candidate
   estimators read, cheap disproofs allowed.
3. `hef-encodings-and-compression` — ADD "Encoder telemetry pairs estimated
   with achieved ratios": per-family estimated vs achieved compression plus
   an acceptance-gate fallback counter, through storage observability.
4. `hef-encodings-and-compression` — ADD "Encoder correctness rides law
   suites, an oracle fuzzer, and decision snapshots": the algebraic-law
   conformance shape, an encoder fuzz target with generation-time oracles,
   and golden pipeline/byte-count snapshots per lifecycle strategy.
5. `hef-encodings-and-compression` — MODIFY "Compressed-data string
   predicates": FSST prefix and substring predicates are now answered from
   compressed bytes by a pattern-and-symbol-table automaton (one transition
   per code byte); only range still declines to full decode.
6. `query-execution` — ADD "Adaptive conjunct ordering from measured
   selectivity": descriptor-based estimate as the prior, measured selectivity
   as the posterior, re-sorted across granules; results identical under any
   order.
7. `hef-query-metadata-and-indexes` — ADD "Falsification pruning evaluates
   columnar across a stripe's granules": one vectorized pass over the
   stripe's stats arrays, same keep/drop set as the per-granule loop.
8. `hef-query-metadata-and-indexes` — ADD "A cold open retains the bytes its
   tail read already fetched": compact files are fully read by open; composes
   with the existing requests-per-cold-open economics gate.

Implementation guidance for already-spec'd requirements lands as tasks
(`tasks.md`), each naming the requirement it implements: the fused
unpack+compare predicate kernel, the one-time CPU-feature dispatch primitive
and its loop-shape rules, overflow-check hoisting in reductions, the
rank/select hierarchy and `intersect_by_rank`, FOR/DELTA exception lists
under recursive cascades, datetime-parts and decimal narrowing candidates,
dictionary pushdown guards, branchless `search_sorted`, ALP bound-encoding
strictness flips, per-ISA benchmark legs, and per-column byte attribution in
introspection.

## Capabilities

### Modified Capabilities

- `hef-encodings-and-compression`: four ADDED requirements (acceptance gate,
  stratified sampling, estimator telemetry, encoder correctness suites) and
  one MODIFIED requirement (FSST prefix/substring answered from compressed
  bytes).
- `query-execution`: one ADDED requirement (adaptive conjunct ordering).
- `hef-query-metadata-and-indexes`: two ADDED requirements (columnar
  falsification evaluation, tail-read byte retention at open).

## Impact

- **No format change.** Every delta is encoder policy, scan policy, or
  testing discipline; on-disk bytes stay governed by the recorded pipelines
  and the pinned footer serialization. The acceptance gate can only swap a
  block to the already-legal plain form.
- **Determinism preserved everywhere.** Fixed sample positions, pure
  verification, and order-insensitive conjunct results keep the
  byte-determinism and same-decision scenarios intact.
- **No overlap with active changes.** IO scheduling/coalescing stays with
  `add-hef-remote-read-scheduler`, cache residency with
  `add-page-granular-object-cache`, out-of-band footers with
  `add-hef-manifest-footer-mirrors`; `design.md` records the mapping.
- **FSST buffer reuse is the sibling change.** The unsafe allowance and the
  write-path reuse land in `permit-unsafe-in-hef-encoding`, not here.

## Open Questions

1. **`hef inspect` CLI (§8.5 of the review).** An offline inspector reading
   header/footer/marks on damaged files pays for itself in the first
   incident, but it is an operator-surface addition that belongs in its own
   change against the CLI capability.
2. **Directional-bound stats lattice (§6.2).** Adopt when the aggregation
   tier's stripe→file rollups next change; it generalizes the existing
   `exact`/`inexact_no_false_negative` split but touches many merge sites.
3. **fsst-rs 0.6 slack decode.** The review's view-backed decode borrows are
   largely landed (zero-copy string scans); whether to bump the pinned fsst
   crate for slack decode is a wire-format validation question tracked with
   the pin, not here.
