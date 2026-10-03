The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-lance-learnings/).

# Design — incorporating the Lance evaluation learnings

## 1. Disposition register

Every idea in `docs/HEF-vs-Lance-evaluation.md`, by section or proposal id,
with where it landed. "Spec" names an ADDED requirement in this change;
"Folded" means text amended into an active change; "Covered" names the landed
work or active change that owns it; "Affirmed" records a deliberate
non-borrowing.

| § / id | Idea | Disposition |
|---|---|---|
| 3.1 | Cold-open economics, footer mirrors | Covered — landed `tail_range` open and active `add-hef-manifest-footer-mirrors`; the evaluation confirms the aim, nothing to adopt |
| 3.1 | Retain tail-read bytes opportunistically | Covered — `incorporate-vortex-learnings` ADDED "A cold open retains the bytes its tail read already fetched" |
| 3.2 | Full-zip per-row offsets for wide columns | Covered — active `add-hef-wide-column-point-access` (`TYPED_COLUMN_ROW_OFFSETS`), which the evaluation itself cites |
| 3.2 | 8 MiB-page philosophy, boolean expansion | Affirmed — no change; HEF's 64–256 KiB pages plus columnar marks are the chosen trade, and the evaluation recommends none |
| 3.3 | Ship the columnar/per-stripe marks flip | Covered — spec'd in `hef-file-layout` ("Granule directory and authoritative marks"); emission tracked as reinforcement R1 in `incorporate-nimble-learnings` |
| 3.4 / P0-A | Index accelerators live outside the file | **Spec** — ADDED in `hef-query-metadata-and-indexes` and `hef-manifest-integration` |
| 3.5 / P0-B | Stream the build at stripe scope | **Spec** — ADDED in `hef-write-path`, gated by ADDED writer peak-memory gate in `hef-benchmarks-and-acceptance-gates` |
| 3.6 / P1-B | Scheduler priority, admissions, refund, un-coalescing, lite mode | **Folded** — amended into `add-hef-remote-read-scheduler` (requirement text, five scenarios, tasks, design §5) |
| 3.7 / P1-C | Density-adaptive deletion-vector encoding | **Spec** — ADDED in `hef-deletes-and-corrections` |
| 3.8 | Multi-writer taxonomy, stable row IDs, branches/tags | Affirmed — solve problems HEF's per-tenant commit authority and born-stable row identity make unreachable |
| 3.8 / P2-D | Object-key entropy | **Spec** — ADDED in `object-store` |
| 3.9 | Integrity/security/determinism comparison | Affirmed — HEF's lead; every proposal here stays outside the sealed file or inside existing feature discipline |
| P1-A | Persisted constant/all-null flags | Covered — owned by sibling `incorporate-duckdb-learnings` P0-B, whose treatment (derive from stats, materialize from metadata, elide bytes) subsumes this |
| P1-D | Bounded reader decoded-block caches | **Spec** — ADDED in `hef-apis` |
| P2-A | Late-materialization policy | **Spec** — ADDED in `query-execution` |
| P2-B | External-payload reference shape | **Spec** — ADDED in `hef-physical-artifacts` |
| P2-C | Released-version fixture corpus | **Spec** — ADDED in `hef-reader-compatibility` |
| Part 5 | Protobuf-`Any` plugins, abolishing stripes/granules, multi-writer conflicts, stable row IDs, branches, per-page FSST tables, dropping in-file stats | Affirmed — recorded so they are not re-litigated; Decision 16 and the closed pipeline-id set stand |

## 2. Tension: where an index lives — footer blocks or external artifacts

- **Tension.** `hef-query-metadata-and-indexes` places skip indexes in footer
  sections of the sealed, BLAKE3-hashed file; the implementation inventory
  shows the cost — of nine built index kinds only text-token persists, because
  adding any other to a published file means a full rewrite and a new
  `file_id`. Lance's model (indexes as external, partially covering, droppable
  artifacts) fixes that, but taken absolutely it would evict even the min/max
  tier the exact-scan fallback depends on. Its sibling evaluation
  (`incorporate-nimble-learnings` R2) pushes the other way: emit the designed
  in-footer tier before designing anything new.
- **Decision.** Split by weight, not by kind-list sentiment. The always-on
  statistics tier — granule/page min/max, null counts, sequence/time ranges,
  exact counts, constant/all-null flags — stays in the footer: it is cheap,
  wanted for every file, known at build time, and load-bearing for the
  exact-scan fallback. Everything heavier — probabilistic membership filters,
  range filters, path presence, learned position, bitmaps, vector structures —
  publishes as manifest-referenced index artifacts, buildable after seal from
  workload heat. Inline emission stays legal where the spec already places it
  (text-token bytes in the data area), and both forms feed identical pruning
  terms. R2's emission ordering survives as the artifact pipeline's build
  priority.
- **Why.** The always-on/heavy split follows what each tier needs: the footer
  tier must exist with no artifact present (droppable-acceleration invariant),
  while the heavy tier's whole value depends on workload knowledge the writer
  does not have at publish time. The falsification expression already makes
  index sources pluggable ("a new statistic adds a term, not a branch"), so the
  artifact reader slots in with no planner surgery. And HEF's own manifest
  precedents — deletion-vector generation records, footer mirrors — already
  established "manifest-native auxiliary metadata" as legal ground.
- **Rejected.** *All indexes external* — breaks fallback pruning economics and
  Lance's own zone-map-as-index choice only works because its pages are ~1000×
  larger. *Finish the in-footer emission first (R2 taken literally)* — pours
  effort into the coupling that caused the nine-built-one-persisted state; the
  artifact path makes the same emission work incremental and rewrite-free.
  *Lance's protobuf-`Any` index registry* — open-ended executable-metadata
  surface; HEF's closed `SkipIndexKind` enum plus the portable-decoder escape
  hatch is the right openness.
- **Spec edits.** The two P0-A ADDs; `hef-file-lifecycle` needed no delta —
  artifact retirement rides the manifest-integration requirement's sweeper
  language, and inventing a parallel artifact lifecycle would duplicate the
  part-state machinery.

## 3. Tension: streaming the build without breaking determinism

- **Tension.** Bounding writer memory means uploading stripes before the
  footer exists, but HEF's identity and simulation story assume the encoder is
  a pure function — content-derived `file_id`, byte-identical output under the
  serial executor. A pressure-*reactive* writer (Nimble's watermark chunking)
  would make bytes depend on runtime memory conditions, which is forbidden.
- **Decision.** Stream at the stripe boundary only, and require byte-identity
  with the materialized build. Stripes are already self-contained (offsets
  stripe-relative, per-stripe BLAKE3, arenas and filters inside the stripe
  range), multipart parts are already stripe-aligned, and every whole-file
  digest (BLAKE3, CRC-64/NVME, outboard tree) composes incrementally — so
  streaming changes *when* bytes leave the builder, never *which* bytes.
- **Why.** The stripe is the largest unit that is both memory-bounded and
  deterministic; anything finer (page/chunk flush under pressure) imports
  Nimble's non-determinism, anything coarser is the status quo. The
  incremental digests also retire the ~85 ms seal-time hashing pass
  (`incorporate-nimble-learnings` I1) as a side effect.
- **Rejected.** *Watermark-driven chunk flushing* — bytes would depend on
  memory pressure. *Keeping the materialized build as the only path* — peak
  RSS scaling with file size caps rewrite scope and doubles on compaction.
- **Spec edits.** The `hef-write-path` ADD plus the benchmark-gate ADD, which
  turns the bound into a regression-proof number rather than an intention.

## 4. Tension: pinning policy numbers the workload has not chosen yet

- **Tension.** Three deltas want numbers HEF cannot honestly pick today: the
  deletion-vector density threshold, the late-materialization width thresholds,
  and the fixture-corpus budget. Baking in Lance's constants would launder
  another system's workload into HEF's spec; leaving them open would repeat the
  unmeasured-policy gap the evaluation criticizes.
- **Decision.** Spec the shape and the determinism, pin the constants at the
  gate that already owns measurement. The density requirement pins two forms,
  recorded encoding, and deterministic selection, with the threshold fixed
  from the measured correction workload before the native-block feature ships;
  the late-materialization requirement pins that defaults are written,
  per-tier, and result-neutral; the fixture budget is an open question settled
  at freeze.
- **Why.** This is the same "spec the guarantees, task the mechanisms" line
  `incorporate-vortex-learnings` drew, applied to constants: what must be true
  is normative, what must be measured goes to the harness.
- **Spec edits.** As listed; no additional ones.
