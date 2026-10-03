The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-duckdb-learnings/).

# Design — incorporating the DuckDB storage evaluation learnings

## 1. Disposition register

Every idea in `docs/HEF-vs-DuckDB-storage-evaluation.md`, by section or
proposal id. "Spec" names an ADDED requirement in this change; "Covered" names
the landed work or sibling change that owns it; "Affirmed" records a deliberate
non-borrowing.

| § / id | Idea | Disposition |
|---|---|---|
| 1.3 | Mutability machinery (free lists, dual headers, undo buffers, partial blocks) | Affirmed — solves in-place mutation; immutability + manifest CAS occupy this ground |
| 3.1 / P0-A | Validity as an adaptively compressed first-class stream | **Spec** — ADDED "Presence and null bitmaps are encoded side streams" |
| 3.2 / P0-B | Zero-byte constant segments, stats-derived detection | **Spec** — ADDED "Constant and all-null blocks store no data bytes"; the metadata-only-answers requirement already exists and gains its wiring tasks here. Subsumes `incorporate-lance-learnings` P1-A |
| 3.3 / P1-A(1) | Whole-segment representative sampling | Covered — `incorporate-vortex-learnings` ADDED "Stratified sampling through one shared statistics pass" |
| 3.3 / P1-A(2) | Deterministic per-family penalty multipliers | **Spec** — ADDED "Decode-cost preference is a deterministic score", promoting the Vortex change's task-level note to a guarantee, because "fastest valid pipeline" is otherwise unhonourable without timing |
| 3.3 | Per-2048-value-group mode adaptivity inside a segment | Open question 3 — per-page pipeline ids are the natural home; measurement first |
| 3.3 / P1-C | ALP raw-vector escape; ALP-RD | Task — ALP-RD is already spec-permitted; the sentinel raw vector is interior encoding detail under the recorded pipeline |
| 3.4 / P1-B | DICT_FSST convergence | **Spec** — ADDED "FSST over dictionary values is a sampled cascade level" |
| 3.4 | Overflow-string chained blocks | Affirmed — the payload arena and external payload references solve this differently |
| 3.5 / P0-C | Vacuum policy: bounded width, doubling bound | **Spec** — ADDED "Compaction scheduling with an anti-thrashing bound" in `hef-file-lifecycle` |
| 3.6 / P1-D | Optimistic bulk writing → journal-by-reference ingest | Open question 1 — deferred until the recovery contract gets a design pass; the ack-from-journal invariant is untouched |
| 3.7 / P2-C | Planner NDV statistics | **Spec** — ADDED "Per-stripe distinct-count estimates for the planner" |
| 3.7 | Persisted reservoir row sample | Affirmed — refused; sample rows are data and cross the per-subject encryption and public-output boundaries |
| 3.8 | Cheap per-block checksum alongside BLAKE3 | Affirmed — INV-21 keeps BLAKE3 the sole authority; CRC-64/NVME prechecks already exist |
| 3.8 / P2-A | Storage-version windows per feature | **Spec** — ADDED "Encoding availability windows gate conformance" (also subsumes the Nimble evaluation's frozen-enum hygiene note) |
| 3.8 | Native deletion-vector encoding still unwritten | Covered — the on-disk encoding work is owned by the phase plan; the density-adaptive wire form is `incorporate-lance-learnings` P1-C |
| 3.8 / P2-B | `force_compression` as a test lever | **Spec** — ADDED "Forced-pipeline conformance coverage" (harness-only; the operator-facing form stays forbidden) |
| 3.9 | Where HEF is simply ahead (cold opens, compressed-form predicates, feature directory, crypto-shredding, simulation) | Affirmed — no reverse-borrowing |
| 0/meta | Close the loop on designed-but-unemitted structures before adopting new ones | Covered — the emission paths are `incorporate-lance-learnings` P0-A (index artifacts) and the wiring tasks here; echoed as this change's ordering |
| 5 | Chimp/Patas, tuple-WAL, operator compression forcing, 2048-row pruning unit | Affirmed — recorded so they are not re-litigated |

## 2. Tension: honouring "fastest valid pipeline" without measuring speed

- **Tension.** The adaptive-selection requirement asks for the fastest valid
  pipeline, but the implementation scores on estimated size alone — and the
  obvious fix, timing candidate decodes, is worse than the disease: HEF
  requires byte-identical encodes on any node, and wall-clock is
  node-dependent. Meanwhile `incorporate-vortex-learnings` had already noted
  the bias should become "explicit per-family multipliers", but only as a
  task.
- **Decision.** Promote the multipliers to a requirement: candidates score as
  estimated bytes × a fixed per-family penalty, with a distinct pinned table
  per lifecycle strategy, and timing is explicitly forbidden as a selection
  input. DuckDB's production experience (1.2× on dictionary/FSST, 2× on
  Roaring, an effective size floor on Zstd) is the proof the shape works; the
  values come from HEF's own harness.
- **Why.** A task can express a preference; only a requirement can forbid the
  tempting wrong fix (benchmarking at encode time) and make the
  `DecodeOptimized`/`SizeOptimized` split mean something testable. This stays
  inside the Vortex change's "spec the guarantees" line: the guarantee is
  determinism plus an honoured speed preference; the mechanism (which
  families, which numbers) stays pinned companion material.
- **Rejected.** *Measured decode speed* — breaks byte determinism.
  *Size-only argmin* — the status quo the spec's own words contradict.
- **Spec edits.** The penalty-score ADD; the Vortex change's task now
  implements this requirement rather than an informal bias.

## 3. Tension: translating a checkpoint vacuum to immutable, remote files

- **Tension.** DuckDB's vacuum rules assume cheap local rewrites inside one
  checkpoint; HEF's every merge is a new manifest generation plus object-store
  round trips, so both failure modes are amplified — unmerged small files cost
  per-file opens and manifest weight forever, while an eager policy rewrites
  the newest data over and over at object-store prices. And the spec today has
  the mechanism (`hef-write-path` rewrite, `compaction.rs` stripe reuse) with
  no trigger at all.
- **Decision.** Specify the trigger in `hef-file-lifecycle` — the capability
  that owns file states — referencing, not duplicating, the rewrite mechanics
  in `hef-write-path`. The policy is DuckDB's shape with HEF units:
  size-tiered adjacent runs per tenant, bounded merge width, the doubling
  bound, bounded work per cycle; enforcement is the WAF gate that already
  exists rather than a new number.
- **Why.** The doubling bound is the piece that cannot be rediscovered
  cheaply: without it, any "merge when there are two small files" heuristic
  thrashes the tail file quadratically, and HEF's version of that failure
  costs manifest generations, not just IO. Placing *when* in the lifecycle
  spec keeps *how* unduplicated.
- **Rejected.** *Policy in `hef-write-path`* — that spec owns mechanics and
  publication; mixing in scheduling would blur two ownerships. *Operator-tuned
  thresholds* — violates the no-knobs rule; pinned code parameters, measured
  by the harness.
- **Spec edits.** The `hef-file-lifecycle` ADD only.

## 4. Tension: how far to take byte elision before readers must know

- **Tension.** Presence-bitmap compression and constant-block elision both
  change the block frame — a reader that expects `presence_len | bitmap |
  body` bytes cannot parse a zero-byte or roaring-framed block. They could
  ship as optional features (ignorable), as escape-hatch blocks, or as
  required bits.
- **Decision.** Required feature bits for both, dual-emit during migration —
  the `columnar_marks` precedent: whatever changes how authoritative bytes
  decode is required. Optionality is reserved for accelerations a reader can
  skip while staying correct, which a reframed block is not.
- **Why.** The alternative — per-block fallback framing kept alongside forever
  — would forfeit most of the byte savings on exactly the constant-heavy
  files that motivate the change, and a silent misparse of a presence prefix
  is a correctness bug, not a degraded plan.
- **Rejected.** *Optional bits* — not ignorable. *Escape hatch* — it exists
  for optional blocks; framing changes to required data are the refuse case
  by design.
- **Spec edits.** The governance sentences inside the two P0 ADDs.
