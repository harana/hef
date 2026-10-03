Status: Approved (2026-08-18)

## Why

The 2026-08-18 Nimble evaluation (`docs/HEF-vs-Nimble-evaluation.md`, reading
`harana-oss/nimble` at `acead74`) compared HEF against Meta's Parquet/ORC
replacement for very wide, sparse ML tables. HEF holds the lead on integrity,
statistics, deletes, encryption, and compatibility governance — but Nimble is
ahead in exactly one structural place that matters for event data: **FlatMap**
stores each sparse map key as its own presence-plus-values column pair, so
touching 5 of 100k keys reads 5 streams, while HEF's binary shred-or-residual
model makes every sub-50%-presence payload path pay a residual probe per
selected row forever. Nimble also amortizes write CPU by capturing and
replaying winning encoding layouts, shares dictionary alphabets across blocks,
and deduplicates identical encoded streams — economics HEF re-derives from
scratch on every block today.

This change incorporates those learnings on the `incorporate-vortex-learnings`
pattern: normative borrows become spec deltas, mechanisms become tasks naming
their requirement, and everything already covered, deferred, or rejected is
recorded in `design.md` — including the evaluation's central caution, that
HEF's claimed lead over Nimble sits partly in designed-but-unemitted sections,
whose emission is owned by the index-artifact pipeline of
`incorporate-lance-learnings` rather than re-proposed here.

## What Changes

Spec deltas (all ADDED requirements):

1. `hef-column-design` — ADD "Sparse shredded columns below the promotion
   threshold" (P0): a third storage tier — per-granule presence bitmap plus a
   dense block of present rows — between dense promotion and the residual
   arena. Key set footer-declared per schema fingerprint, budget-bounded,
   writer-policy selected; values leave the residual, so the tier is a
   **required** feature bit. O(present rows) instead of O(selected rows) for
   every sparse-but-hot field.
2. `hef-encodings-and-compression` — ADD "Encoding selection may capture and
   replay a winning pipeline" (P1-A): replay within a build with a fixed
   re-sample cadence and a size-regression trip-wire, deterministic by
   construction, always under the plain-form acceptance gate; optional catalog
   persistence as a candidate-ordering prior.
3. `hef-encodings-and-compression` — ADD "Dictionary alphabets may be shared
   at file scope" (P1-B): one alphabet per column per file, blocks store code
   streams with a scope marker, sorted-code predicates translate once per
   file; required feature bit per the `columnar_marks` precedent; external
   scope reserved, not defined.
4. `hef-file-layout` — ADD "Marks may alias identical extents" (P2-A):
   hash-then-byte-exact dedup of identical encoded blocks within a stripe,
   reader-side read-region dedup, decode semantics untouched.

Watch item, not a delta (P2-B): OpenZL stays out of the closed compression
set; when it has a maintained Rust decode path it MAY be trialled on optional
blocks only through the implemented escape hatch — recorded as an open
question.

Reinforcements and implementation-side items (R1, R2, I1, I2) land as tasks or
cross-references, not new normative text — see `design.md`.

## Capabilities

### Modified Capabilities

- `hef-column-design`: one ADDED requirement (sparse shredded columns).
- `hef-encodings-and-compression`: two ADDED requirements (capture/replay,
  file-scope shared dictionaries).
- `hef-file-layout`: one ADDED requirement (mark aliasing).

## Impact

- **The sparse tier is the one required-feature format change.** It removes
  values from the residual, so old readers must refuse; the mitigations are the
  existing lockstep fleet deployment and conformance gates, and the payoff is
  structural — every sparse-but-hot event field (rare error codes,
  per-connector attributes, context labels) stops paying residual decode over
  the rows that lack it.
- **Everything else is additive.** Capture/replay changes which pipeline gets
  *tried*, never the recorded-pipeline decode contract, and stays under the
  Vortex acceptance gate; shared dictionaries and mark aliasing are
  writer-side economics whose decode semantics are pinned by the existing
  marks and descriptor discipline.
- **Determinism holds everywhere.** Replay is a pure function of build order;
  aliasing and scope selection are deterministic; the sparse tier's selection
  is writer policy over committed statistics. Byte-identical-encode tests
  extend to every new path.
- **Nimble's minimalism is explicitly not imported.** Whole-file-checksum
  integrity, FlatBuffers metadata, statistics minimalism, dynamic mid-write
  schema growth, and the experimental encoding zoo stay rejected (`design.md`
  §1, affirmed rows).

## Open Questions

1. **OpenZL (P2-B).** Re-evaluate when a maintained Rust decode path exists;
   any trial runs on optional blocks only, through the
   `min_reader_version` + `portable_decoder_ref` escape hatch,
   conformance-gated and benchmarked against `SizeOptimized` Zstd on the same
   corpus before any adoption decision.
2. **Sparse-tier key budget.** The per-file sparse key budget is pinned by
   measurement (footer-bytes-per-key against prune value), like the other
   metadata-economics gates; Nimble's 200k `maxFlatMapKeys` is scale
   reference, not the answer.
3. **Replay cadence and trip-wire bound.** Fixed by benchmark before the
   capture path ships; both are code parameters, never operator keys.
