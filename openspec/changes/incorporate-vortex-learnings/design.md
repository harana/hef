The Pulse side of this change lives in the harana/pulse repository under the same change name (openspec/changes/incorporate-vortex-learnings/).

# Design — incorporating the Vortex source-review learnings

## 1. Disposition register

Every idea in the 2026-08-18 Vortex source review, by its section number,
with where it landed. "Task" means implementation guidance tracked in
`tasks.md` under the existing requirement it implements — no new normative
text needed. "Covered" names the landed work or active change that owns it.

| § | Idea | Disposition |
|---|---|---|
| 2.1 | Estimate-then-verify acceptance | **Spec** — ADDED "Encoded blocks never grow past the plain form" |
| 2.2 | Stratified fixed-seed quantum-aligned sampling | **Spec** — ADDED "Stratified sampling through one shared statistics pass" |
| 2.3 | One merged stats pass + cheap disproofs | **Spec** — folded into the same ADDED requirement |
| 2.4 | Branch-and-bound deferred estimates | Task — "Adaptive per-block encoding selection" |
| 2.5 | Decode-cost tax per family | Task — "Lifecycle-selected cascade strategies" (the bias becomes explicit per-family multipliers) |
| 2.6 | Cascade-anticipating dictionary cost model | Task — "Adaptive per-block encoding selection" |
| 2.7 | Declarative cascade-exclusion rules | Task — carried with the "Recursive cascade selection" implementation |
| 2.8 | Patches (exception lists, chunk-indexed) | Task — "Recursive cascade selection" already names exception streams |
| 2.9 | datetime-parts / decimal byte-part narrowing | Task — new sampled candidates under "Adaptive per-block encoding selection" |
| 2.10 | Dictionary details (bounded cutover, `all_values_referenced`) | Task — feeds the §7 pushdown guards |
| 2.11 | Estimator telemetry | **Spec** — ADDED "Encoder telemetry pairs estimated with achieved ratios" |
| 3.1 | `CpuKernel` one-time dispatch + anti-patterns | Task — "SIMD kernels dispatch on detected CPU features at runtime" |
| 3.2 | Fused unpack+compare, transposed bitmask | Task — the implementation shape for "Compressed-data numeric predicates" |
| 3.3 | Two-tier boolean packing | Task — same requirement family as 3.2 |
| 3.4 | Overflow-check hoisting | Task — resolves the standing `overflow-checks = true` question: keep the checks, hoist them per chunk |
| 3.5 | Rank/select hierarchy + `intersect_by_rank` | Task — "Rank/select over visibility and null bitmaps" |
| 3.6a | View-backed string decode | Covered — `2026-08-18-add-zero-copy-string-column-scans` |
| 3.6b | FSST buffer reuse / slack decode | Covered — sibling change `permit-unsafe-in-hef-encoding` (write side, on the pinned 0.5.11); a 0.6 pin bump stays an open question |
| 3.6c | FSST substring/prefix DFA | **Spec** — MODIFIED "Compressed-data string predicates" |
| 3.7 | ALP in-place decode, encode-the-bound strictness flips | Task — "Compressed-data numeric predicates" |
| 3.8 | Per-ISA benchmark legs | Task — "Acceptance gates are mechanically enforced by the committed harness" |
| 4.1 | Adaptive conjunct reordering | **Spec** — ADDED in `query-execution` |
| 4.2 | Asymmetric pruning/filter mask contracts | Task — shapes the multi-conjunct pushdown when it lands |
| 4.3 | Eager-registration IO scheduler + coalescer | Covered — active `add-hef-remote-read-scheduler` |
| 4.4 | Segment dedup + TinyLFU byte cache | Covered/Task — cache residency is `add-page-granular-object-cache`; the ~40-line in-flight dedup map is a task |
| 4.5 | Count-only fast path, prepared scans, byte budgets, split subdivision, dynamic re-pruning | Task — noted for the scan layer as it grows |
| 5.1 | Open round-trip budget | Covered/Spec — the one-request gate exists ("Metadata-economics gates for cold opens and pruning"); the new piece is ADDED "A cold open retains the bytes its tail read already fetched" |
| 5.2 | Detachable cacheable footers | Covered — active `add-hef-manifest-footer-mirrors` |
| 5.3 | Sequence-ID deterministic parallel writes | Covered — parallel encode behind `EncodeExecutor` landed with byte-identity asserted; the hierarchical sequence-id sink is the recorded design if placement ever becomes streaming |
| 5.4 | Per-column byte attribution | Task — `hef/introspection.rs` system tables |
| 5.5 | Untrusted-alignment cap | Task — hardening under "Footer serialization is pinned and sections decode bounded" |
| 6.1 | Vectorized falsification over the granule directory | **Spec** — ADDED in `hef-query-metadata-and-indexes` |
| 6.2 | Directional-bound stats lattice | Open question — adopt with the next rollup change |
| 6.3 | Audit stats for prune value | Review habit — recorded here; no mechanical change |
| 7 | Compute-on-encoded guardrails (dict pushdown guards, saturation chain, measured thresholds, branchless `search_sorted`, mask representations) | Task — correctness traps for the already-spec'd encoded-domain evaluators |
| 8.1–8.3 | Law suites, oracle fuzzers, golden snapshots | **Spec** — ADDED "Encoder correctness rides law suites, an oracle fuzzer, and decision snapshots" |
| 8.4 | Benchmark trustworthiness (quieting, control rows) | Task — the fairness caveat is already fixed; the control-row model is a harness task |
| 8.5 | File inspector CLI | Open question — its own change |
| 8.6 | Lint enforcement of performance rules | Task — style-doc and `clippy.toml` entries naming the guarding benchmark |
| 9 | Explicitly-not-recommended list | Affirmed — self-describing layouts, checksum slots, flat cascade cap, boolean schemes, hash-order dictionaries, and nightly `std::simd` stay rejected |

## 2. Tension: spec the mechanisms or spec the guarantees

**Tension.** The review is full of concrete mechanisms (a 16×64 sampler, a
`[u64; 16]` transposed accumulator, DDSketch quantiles). Baking mechanisms
into requirements would freeze implementation detail the code should be free
to improve; leaving everything to tasks would lose the guarantees the
mechanisms exist to deliver.

**Decision.** Spec the guarantees, task the mechanisms. The deltas pin what
must be true — no block grows past plain, samples span the block and
selection stays deterministic, prefix/substring answered exactly from FSST
bytes, conjunct order never changes results, columnar pruning equals the
per-granule loop, telemetry pairs estimate with outcome — and name the
mechanism only where it is the contract (the automaton runs one transition
per compressed byte; the sample total is quantum-aligned because packing
padding otherwise skews estimates).

**Why.** This matches how the corpus already treats the adaptive encoder:
"Adaptive per-block encoding selection" pins sampling-decides, not the
sampler. It also keeps every delta verifiable by an equivalence test against
an existing oracle (plain form, per-granule loop, decode-then-filter), which
is what makes the change safe to land incrementally.

**Implied spec edits.** The eight deltas listed in `proposal.md`; nothing
else. Rejected alternative: a single "adopt Vortex learnings" mega-
requirement — unverifiable and unownable; each borrowed guarantee belongs to
the capability that already owns its subject.

## 3. Tension: where FSST substring pushdown may say SHALL

**Tension.** The existing requirement says FSST range **and prefix**
predicates decline to full decode. The review shows prefix/contains are
answerable exactly from code bytes with a per-predicate automaton, but making
them SHALL commits a non-trivial kernel.

**Decision.** SHALL, scoped to prefix and substring, with range still
declining, and the evaluator's blanket fallback ("whenever the block's
encoding or the predicate is unsupported") retained verbatim.

**Why.** Half-hearted MAYs rot; the evaluator contract is only trustworthy
if callers can rely on which predicate classes push down. The exactness
scenario ("Compressed-data filter equals full decode") already supplies the
oracle the new kernel is tested against, and the fallback sentence keeps
correctness independent of the kernel's reach.
