# Design — incorporating the Nimble evaluation learnings

## 1. Disposition register

Every idea in `docs/HEF-vs-Nimble-evaluation.md`, by section or proposal id.
"Spec" names an ADDED requirement in this change; "Covered" names the landed
work or the sibling change that owns it; "Affirmed" records a deliberate
non-borrowing.

| § / id | Idea | Disposition |
|---|---|---|
| 3.1 | Stripe-group metadata economics, lazy loading | Covered — the columnar/per-stripe marks requirements in `hef-file-layout` already specify it; emission is reinforcement R1 (tasks here) |
| 3.1 | Writer-memory bounding via mid-build metadata flush | Covered — subsumed by `incorporate-lance-learnings` "The builder streams at stripe scope with bounded memory" (marks accumulate per stripe and leave the builder with the stripe) |
| 3.1 | Index bytes adjacent to their stripe's marks page | Task — the marks requirement already permits co-location ("co-located with the stripe or in the footer region"); placement choice tracked as a task, no new normative text |
| 3.2 / P0 | FlatMap-style sparse key storage | **Spec** — ADDED "Sparse shredded columns below the promotion threshold" in `hef-column-design` |
| 3.2 | Dynamic mid-write schema growth | Affirmed — rejected; the key set stays footer-declared and budgeted (Decisions 13/16) |
| 3.3 | Cost model (read factors vs lifecycle strategies) | Affirmed — equivalent outcomes, no change; the deterministic penalty-multiplier sharpening is owned by `incorporate-duckdb-learnings` |
| 3.3 / P1-A | Encoding-layout capture and replay | **Spec** — ADDED in `hef-encodings-and-compression` |
| 3.3 | Unbounded recursion with parent exclusion | Affirmed — `MAX_CASCADE_DEPTH = 3` stands; Nimble's own production set stays shallow |
| 3.4 / P2-B | OpenZL as a codec | Open question — watch item; optional blocks only, through the escape hatch, when a maintained Rust decode path exists |
| 3.5 | Watermark-driven chunk flushing | Affirmed — pressure-reactive layout breaks byte determinism; the residual lesson (never require the whole file resident) is the Lance streamed-build requirement |
| 3.6.1 / P2-A | Stream-level dedup | **Spec** — ADDED "Marks may alias identical extents" in `hef-file-layout` |
| 3.6.2 / P1-B | Shared dictionaries (stripe/file/external scope) | **Spec** — ADDED "Dictionary alphabets may be shared at file scope" in `hef-encodings-and-compression`; external scope reserved, catalog global dictionary keeps cross-file translation |
| 3.6.3 | Deduplicated containers (`ArrayWithOffsets`, `SlidingWindowMap`) | Affirmed — not proposed; residual arena + shredding + trained Zstd dictionaries cover the workload |
| 3.7 / R2 | Emit the designed index tier before designing new structures | Covered — the emission paths are the index-artifact pipeline of `incorporate-lance-learnings` (its build-priority order is R2's order: constant flags → path presence → binary fuse → range filters), with constant/all-null flags owned by `incorporate-duckdb-learnings` P0-B |
| 3.8 | Whole-file running checksum | Affirmed — rejected; BLAKE3 per-scope authority stands |
| 3.8 / I1 | Incremental hashing during layout | Covered — required by the Lance streamed-build delta ("never by re-reading the finished file in a second hashing pass") |
| 3.9 | Frozen-enum annotation habit for pipeline ids | Covered — subsumed by `incorporate-duckdb-learnings` P2-A (encoding availability windows) |
| 3.10 | preadv over deduped regions, unit loader, footer caching, selectivity ordering | Covered — reader-side region dedup lands with the aliasing requirement here; the rest is landed work or `incorporate-vortex-learnings` |
| I2 | Kill the double-compression sample path | Task — falls out of P1-A replay (steady-state granules stop running both LZ4 and Zstd trials); tracked here |
| Part 5 | FlatBuffers metadata, statistics minimalism, sentinel nulls, reject-newer-major, experimental encoding zoo | Affirmed — recorded so they are not re-litigated |

## 2. Tension: FlatMap's storage shape under HEF's closed governance

- **Tension.** FlatMap's win is real — O(present rows) for sparse keys at any
  width — but its mechanism is a writer that mints new streams mid-file as
  keys are discovered, which collides head-on with HEF's closed column
  registry, schema fingerprint, deterministic encode, and manifest governance
  (Decisions 13/16). Import the mechanism and the governance breaks; refuse
  the whole idea and every sparse-but-hot field keeps paying residual decode.
- **Decision.** Borrow the storage shape, not the write model. A sparse
  shredded column is presence bitmap + dense values per granule — the FlatMap
  layout — but the key set is declared in the footer per schema fingerprint
  like the existing presence map, bounded by a pinned per-file budget, and
  chosen by the writer's statistics loop at publication/rewrite, exactly where
  dense promotion is already decided. No mid-write schema growth, no operator
  knob.
- **Why.** Everything the shape needs already exists in HEF vocabulary: the
  roaring-run bitmap containers, the rank/select gather machinery, the
  presence-map footer pattern, and the statistics-driven shredding loop. The
  only genuinely new commitment is the tier itself.
- **Rejected.** *Dynamic key discovery* — breaks determinism and closed
  registries. *An optional feature bit* — an old reader would fall through to
  the residual and silently return missing values; correctness, not
  acceleration, so the bit is required, accepting the fleet-coordination cost
  the evaluation names. *Constant in-map elision and hot-key adjacency* —
  layout micro-mechanics left to the implementation; the requirement pins
  semantics and cost shape only.
- **Spec edits.** The `hef-column-design` ADD. No delta in
  `hef-query-metadata-and-indexes`: presence entries prune through the
  existing falsification expression ("a new statistic adds a term, not a
  branch"), and no delta in `hef-file-layout`: the footer section rides the
  pinned section-directory discipline like `PRESENCE`.

## 3. Tension: how to govern shared-alphabet blocks a reader cannot decode alone

- **Tension.** The evaluation left the feature class open ("optional bit where
  readers lacking it must refuse only blocks that use it, or a required bit —
  decide in the change proposal"). HEF's optional-feature contract says
  *ignorable*: a reader may skip an optional block and still return complete
  results from another path. A shared-scope code stream is not ignorable — the
  column's values are unreadable without the alphabet — but it is also not
  droppable acceleration.
- **Decision.** Required feature bit, per the `columnar_marks` precedent: a
  capability that changes how authoritative bytes decode is required, full
  stop. The escape hatch stays what it is for — new *optional* block
  encodings.
- **Why.** "Optional but refuses per block" would be a third compatibility
  class, weakening the clean refuse-required/ignore-optional line the reader
  contract is built on. `columnar_marks` already established that a decode-path
  change rides a required bit with dual-emit during migration; the shared
  alphabet is the same situation one level down.
- **Rejected.** *Per-block refusal semantics* — new compatibility machinery
  for one feature. *Escape-hatch delivery* — the hatch covers optional blocks;
  a required decode dependency through a portable decoder would make fleet
  correctness hang on decoder resolution.
- **Spec edits.** The governance sentence inside the shared-dictionary ADD.

## 4. Tension: replay speed versus selection honesty

- **Tension.** Re-sampling 1024 values per block and double-running trial
  compressors is measurable write CPU on distribution-stable event columns —
  but a capture replayed forever is how drifting data ends up in a wrong
  encoding, and any speedup that made encode bytes depend on timing would
  break determinism.
- **Decision.** Replay is bounded by two re-arms (fixed cadence, size-ratio
  trip-wire), is a pure function of prior blocks in build order, and sits
  under the Vortex acceptance gate so its worst case is the plain form. The
  catalog-persisted capture seeds candidate order only; it never skips
  verification.
- **Why.** The trip-wire converts the failure mode from "wrong encoding for
  the rest of the file" to "one sub-optimal block, then re-selection", and
  purity keeps the byte-identical-encode tests meaningful. I2 (the
  double-compression hot path) dissolves as a side effect: steady-state blocks
  replay one trailing codec instead of trialling two.
- **Rejected.** *Cross-build replay as binding* — a stale capture from
  yesterday's distribution must never bypass verification. *Wall-clock-guided
  re-arm* — non-deterministic.
- **Spec edits.** The capture/replay ADD.
