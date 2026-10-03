# Design — incorporating the LiquidCache learnings

Source read: `XiangpengHao/liquid-cache` at `746d639`, workspace version 0.1.13,
Apache-2.0 OR MIT. The reusable surface is the `liquid-cache` core crate
(`src/core`, ~21k LOC): `liquid_array` (the encodings) and `cache` (residency,
budget, policies). The `datafusion-*`, Flight, and Parquet layers are outside any
plausible pulse use.

## 1. Disposition register

Every mechanism in the core crate, with where it landed. "Task" means
implementation guidance under a requirement that already exists. "Covered" names
the landed work or active change that owns it. "Have" means HEF already does it,
often further.

| Mechanism | Upstream location | Disposition |
|---|---|---|
| Order-preserving prefix key (7 bytes + length, packed to 8) | `liquid_array/raw/fsst_buffer.rs` `PrefixKey` | **Spec** — MODIFIED "Compressed-data string predicates"; the one genuinely new mechanism |
| Byte-set fingerprint for substring pruning (32-bit, `byte & 31` buckets) | `liquid_array/byte_view_array/fingerprint.rs` | **Rejected** — §4; the automaton in `incorporate-vortex-learnings` answers the same predicates exactly, without a sidecar |
| FSST string compression | `liquid_array/raw/fsst_buffer.rs` | Have — same `fsst-rs 0.5.11` crate, same pin |
| FSST equality on compressed bytes | `byte_view_array/comparisons.rs` | Have — "Compressed-data string predicates", FSST equality class |
| Dictionary + sorted codes, code-domain predicates | `byte_view_array/mod.rs` | Have — sorted-dictionary codes answer equality *and* range already |
| FastLanes bit-packing | `liquid_array/raw/bit_pack_array.rs` | Have — FastLanes transposed layout, `#[multiversion]`-dispatched |
| Float encoding | `liquid_array/float_array.rs` | Have, further — ALP, ALP-RD, and byte-stream-split |
| Linear/delta integer encoding | `liquid_array/linear_integer_array.rs` | Have — FOR, delta, delta-of-delta |
| Predicate evaluation in the packed-lane domain | `liquid_array/primitive_array.rs` | Have, further — "Compressed-data numeric predicates" translates bounds into the packed-lane domain once |
| Squeeze: keep high bits resident, spill the rest | `liquid_array/hybrid_primitive_array.rs` | **Gated** — §5; needs a column cache to squeeze *into* |
| Squeeze: keep one date field resident | `liquid_array/squeezed_date32_array.rs` | **Gated** — §5 |
| Cache-entry state machine (Arrow → liquid → squeezed → disk) | `cache/core.rs`, `cache/cached_batch.rs` | **Gated** — §5 |
| Memory budget with background transcoding | `cache/budget.rs`, `cache/transcode.rs` | **Gated** — §5 |
| Squeeze / hydration / eviction policies | `cache/policies/` | **Gated** — §5 |
| Concurrent ART index over cache entries (`congee`) | `cache/index.rs` | Not applicable — no column cache to index |
| Liquid array IPC serialization | `liquid_array/ipc.rs` | Not applicable — HEF pages are the serialized form |
| Direct I/O to dodge double-buffering the page cache | cache IO context | Have — "cache fills avoid double buffering the page cache" is a conformance test |
| Parquet/variant shredding, DataFusion pushdown, Arrow Flight transport | `src/datafusion*`, `cache/policies/squeeze.rs` variant paths | Not applicable — HEF is the substrate and the SQL frontend is not DataFusion |

## 2. Neither a dependency nor a vendored copy — port the algorithm

- **Tension.** The prefix key is upstream code that works, under a licence that
  permits both linking and copying. Three ways to get it: depend on
  `liquid-cache`, vendor the parts we want, or reimplement the mechanism against
  pulse's own types.
- **Decision.** Reimplement. No new dependency, no second vendored tree.
- **Why.** The dependency is closed four times over, and any one of them is fatal
  on its own: `tokio ^1.52` is excluded by `INV-RUNTIME` and gated in `deny.toml`
  behind a wrapper list this crate is not on; `arrow ^58.1` cannot unify with the
  workspace's 59.2, and two arrow versions do not merely convert slowly between
  `ArrayRef`s — they are different types, so there is no interchange at all;
  `datafusion-physical-expr ^53` is precisely the crate `query::expr_eval` was
  copied to avoid depending on, so taking it would undo the vendoring's reason to
  exist; and `object_store ^0.13` re-enters through a door pulse's runtime-clean
  clients were written to close. Upstream additionally states it is not
  production-ready, at 0.1.x, having recently reshuffled its crate names.
  Vendoring fails a different rule: `query/src/expr_eval/VENDORING.md` makes that
  module "the one sanctioned location in the workspace for copied upstream
  source", and Decision 43 already settled the general form of this question — the
  upstream I/O-bearing crates are reimplemented natively, not vendored and ported.
  Reimplementation is also *cheap* here, which is what makes rejecting the other
  two easy rather than merely principled: the mechanism is a few hundred lines of
  byte comparison, and upstream's DataFusion coupling across all of `liquid_array`
  is 53 lines, almost entirely `ScalarValue`, `Operator`, `ColumnarValue`,
  `PhysicalExpr`, `BinaryExpr`/`Column`/`Literal`/`CastExpr` — every one of which
  `query::expr_eval` already has.
- **Rejected.** *Depend on `liquid-cache` and feature-gate it off by default.* — A
  gated dependency is still in the lockfile and still fails the deps gate; and the
  arrow mismatch means the gated build could not exchange arrays anyway.
  *Vendor `liquid_array` and strip the DataFusion imports.* — Buys ~21k LOC of
  which HEF already implements the majority, better; opens a second vendored tree
  against a committed rule; and inherits an upstream re-vendor obligation for code
  we would immediately diverge from.
- **Spec edits.** None. This decision is why the change is one requirement and not
  a dependency addition; the provenance note it implies is a task.

## 3. The prefix key is an optional, sampler-earned stream — not a format field

- **Tension.** The key restores order comparison on FSST data, but it costs bytes
  per value, on disk, forever. LiquidCache pays that cost in RAM only, for a cache
  entry it can drop; HEF would pay it in the file. Making it mandatory taxes every
  FSST block for a predicate class many columns never see; making it a runtime
  toggle puts a performance cliff in an operator's hands.
- **Decision.** The stream is an optional per-page candidate under the existing
  adaptive selection rules, its presence and width recorded in the page's encoding
  descriptor. The writer emits it only when its sample shows it earns its bytes —
  both its size against the block and the share of rows it would leave undecided.
- **Why.** This is how HEF already decides every other encoding question, so the
  key needs no new machinery and no new operator surface: "Adaptive per-block
  encoding selection" governs what is emitted, "Self-describing per-page encoding
  descriptor" tells a reader what it got, and the sampler is exactly the right
  place to catch the case the mechanism handles worst (§ shared-prefix columns,
  where every key ties and the decode happens anyway). It also keeps the
  fallback honest: no stream means the reader declines range and decodes, which
  is today's behaviour unchanged, so the change cannot regress a page that does
  not opt in.
- **Rejected.** *Always emit for FSST blocks.* — Taxes every high-cardinality
  string column for a predicate class that may never run against it, and is
  actively wasteful on shared-prefix data. *An operator config key.* — The spec
  bans statically assigned and operator-selected encodings for exactly this
  reason; the sampler has the measurement and the operator does not.
- **Spec edits.** `hef-encodings-and-compression` MODIFY "Compressed-data string
  predicates" (the sampled-candidate paragraph and the "earned, not assumed"
  scenario).

## 4. The byte-set fingerprint is rejected, not deferred

- **Tension.** LiquidCache carries two sidecars, and the review that produced this
  change originally proposed taking both: the prefix key for order, and a 32-bit
  byte-set fingerprint that proves a substring *cannot* be present so the value
  need not be decompressed. On pulse's current baseline the fingerprint looks like
  a clear win — `filter_fsst_contains` decompresses the whole arena and runs
  Aho-Corasick over it, at a cost proportional to arena size regardless of
  selectivity.
- **Decision.** Reject it. Take the prefix key only.
- **Why.** The baseline it wins against is already gone. `incorporate-vortex-learnings`
  MODIFY "Compressed-data string predicates" answers prefix and substring with a
  deterministic automaton over the compressed code stream — one transition per
  compressed byte, decompressing nothing, *exactly* rather than probabilistically.
  Against that, the fingerprint costs 4 bytes per value to produce a maybe-set
  whose survivors still need the real test, and it cannot beat an exact evaluator
  that never decompresses in the first place. The two mechanisms are not
  complementary; the automaton strictly dominates.
- **Rejected.** *Take both and let the sampler choose.* — Offers the sampler a
  choice with no winning branch, and spends spec surface and per-page descriptor
  bits on it. *Take the fingerprint now and drop it when the automaton lands.* —
  Ships a format field with a known expiry.
- **Spec edits.** None. Recorded here so it is not re-proposed from the same
  reading of the same source.

## 5. No column-residency cache yet — a benchmark, not a requirement

- **Tension.** Every pulse cache is a byte-range cache: the in-memory tier
  (`storage::memory::InMemoryStore`), the disk tier, the peer cache. Nothing
  caches decoded or encoded *columns*, so a repeated query re-decodes HEF pages
  from bytes it already has in RAM. LiquidCache's cache half — entry state
  machine, memory budget, background transcode, squeeze and hydration policies —
  is a complete design for exactly that gap, and pulse has no analogue.
- **Decision.** Do not spec it in this change. Land a benchmark task that measures
  what a column-residency tier would actually save on HEF, and propose it — or
  don't — on the number.
- **Why.** LiquidCache's headline gain is measured against Parquet, where a page
  must be fully decompressed before anything can filter it, so caching a
  filterable in-memory form replaces a decompress per query. HEF pages are already
  random-access queryable in compressed form with page-directory pruning, so the
  same cache replaces much less: the marginal win is one decode of the surviving
  pages, not one decompress of everything. `add-page-granular-object-cache` is
  also about to make hot pages individually RAM-resident, which moves the
  baseline again before any measurement would be taken. Speccing a residency tier,
  a budget, and a policy interface on the strength of someone else's benchmark
  against a different substrate is the speculative-requirement failure mode the
  corpus exists to prevent.
- **Rejected.** *Spec the cache now and tune later.* — Commits normative surface —
  entry states, budget semantics, policy interfaces — before knowing whether the
  tier pays for itself here. *Drop the idea.* — The gap is real and the design is
  good; it deserves a measurement, not silence.
- **Spec edits.** None. A `format-benchmark` task in `tasks.md`.

## 6. Ordering against `incorporate-vortex-learnings`

Both changes MODIFY `hef-encodings-and-compression` — "Compressed-data string
predicates". They do not conflict, but they are not commutative: this change's
delta is written on top of the Vortex text, keeping its prefix/substring automaton
bullet and its scenario intact, and narrowing the decline-to-decode sentence from
"range predicates" to "range predicates on a block with no prefix-key stream".
Applied in the other order, the Vortex delta would silently drop the prefix-key
paragraph, because an OpenSpec MODIFY replaces the whole requirement block.

Practically: `incorporate-vortex-learnings` archives first. If this change is
approved before that one lands, its delta is rebased onto whatever text that
change actually archived — the §4 rejection depends on the automaton existing, so
if the automaton were ever dropped from the Vortex change, the fingerprint
question reopens and should be re-decided, not inherited from this document.
