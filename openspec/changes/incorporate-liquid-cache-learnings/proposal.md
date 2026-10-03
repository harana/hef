Status: Proposed (2026-08-26)

## Why

LiquidCache (`XiangpengHao/liquid-cache`, read at `746d639`, workspace version
0.1.13, Apache-2.0 OR MIT) is a DataFusion cache that transcodes Parquet into
in-memory encodings a filter can run against directly. The question this change
settles is how — or whether — to bring it into pulse: as a dependency, as a
vendored copy, or as ported pieces.

The answer is ported pieces, and far fewer of them than the project's headline
suggests. LiquidCache exists because Parquet pages must be fully decompressed
before anything can filter them; HEF pages are already random-access queryable
in compressed form, with page-directory pruning, sorted-dictionary code tests,
lane-domain numeric comparison, and — once `incorporate-vortex-learnings` lands —
a compressed-domain automaton for FSST prefix and substring. Most of what
LiquidCache is *for*, HEF already does.

Taking the crate is closed on four independent grounds, each a committed rule
rather than a preference: `liquid-cache` 0.1.13 declares `tokio ^1.52` (excluded
by `INV-RUNTIME`, gated in `deny.toml` behind a closed wrapper list),
`arrow ^58.1` against the workspace's arrow 59.2 (two arrow versions cannot
exchange an `ArrayRef` at all), `datafusion-physical-expr ^53` (the exact crate
`query::expr_eval` was copied to avoid), and `object_store ^0.13` (pulse reaches
object storage through its own runtime-clean clients). Upstream also states it is
not production-ready. Vendoring is closed too: `query/src/expr_eval/VENDORING.md`
makes that module the one sanctioned location for copied upstream source, and
Decision 43 already settled that upstream I/O-bearing crates are reimplemented
natively rather than vendored.

What survives that filter is one mechanism HEF does not have and cannot get from
the automaton: **order comparison on FSST data without decoding**. FSST codes are
not order-preserving, so range predicates on FSST columns fall back to a full
decode — the last declined predicate class in the compressed-data string
evaluator. LiquidCache answers it with a small per-value key of leading
uncompressed bytes plus a length byte, which decides almost every row by
comparison and leaves only genuine ties to decode.

## What Changes

One spec delta:

1. `hef-encodings-and-compression` — MODIFY "Compressed-data string predicates":
   an FSST block MAY carry an optional **prefix-key stream** — per present value,
   its leading uncompressed bytes to a descriptor-recorded width plus the value's
   length, saturating at a sentinel — and the evaluator SHALL answer range
   predicates from it, decompressing only the rows whose keys tie across the whole
   width with both values continuing past it. The stream is a sampled candidate
   under the existing adaptive selection rules, its presence and width are recorded
   in the page's encoding descriptor, and a block without it declines range to full
   decode exactly as today.

Everything else from the source review lands as tasks under requirements that
already exist, or is recorded in `design.md` as covered, superseded, or gated on
measurement. Two dispositions are worth naming here because they are the ones a
reader will expect to see and will not find:

- **The byte-set fingerprint is rejected, not deferred.** LiquidCache prunes
  substring candidates with a 32-bit bitmask over `byte & 31` buckets. The
  automaton `incorporate-vortex-learnings` adds answers the same predicates
  exactly, in one transition per compressed byte, with no per-value sidecar and
  no decompression of survivors. A probabilistic pre-filter in front of an exact
  compressed-domain evaluator is strictly worse.
- **The column-residency cache is gated on measurement, not proposed.** Every
  pulse cache today is a byte-range cache, so a query re-decodes HEF pages from
  cached bytes and LiquidCache's cache-entry state machine, memory budget, and
  squeeze/hydration policies have no analogue here. But the win they buy over
  Parquet is mostly the win HEF already has, and `add-page-granular-object-cache`
  is about to make hot pages individually RAM-resident. A benchmark task lands;
  a requirement does not.

## Capabilities

### Modified Capabilities

- `hef-encodings-and-compression` — MODIFY "Compressed-data string predicates":
  FSST range predicates answered from an optional, sampler-earned prefix-key
  stream, with only whole-width ties decompressed; absence of the stream keeps
  the existing decline-to-decode behaviour.

## Impact

- **The last declined string predicate class closes.** Range on high-cardinality
  FSST columns is the one filter the compressed-data evaluator still hands to a
  full decode. Dictionary columns already answer range from sorted codes, so this
  reaches exactly the columns dictionary encoding rejects — identifiers, emails,
  URLs, free-form keys.
- **Format change is additive and optional.** The stream is one more sampled
  candidate recorded in the per-page encoding descriptor. A page that does not
  carry it is byte-identical to what the writer produces today, and a reader that
  ignores it decodes the same rows, so no existing file is invalidated and no
  reader is obliged to understand it.
- **Correctness never depends on the stream.** Absent, unreadable, or ignored, the
  evaluator declines and the reader decodes — the same "optimisation, never a
  correctness dependency" rule the rest of the evaluator already carries. Results
  are exact either way, and null rows are never selected.
- **No dependency and no vendored source.** The mechanism is ported as an
  algorithm against pulse's own FSST path and its own `query::expr_eval` types.
  The dependency graph, the arrow pin, and the one-vendored-module rule are all
  unchanged.
- **Ordering, not conflict, with the Vortex change.** This delta rewrites the same
  requirement `incorporate-vortex-learnings` modifies, and is written on top of
  that change's text. It must be applied after it; `design.md` records the
  dependency.

## Open Questions

1. **Key width.** LiquidCache packs 7 prefix bytes plus a length byte into 8. The
   requirement fixes only that the width is recorded per page; whether 7 is right
   for HEF's string columns — against the ambiguity rate it leaves and the bytes
   it costs — is a measurement question for `crates/format-benchmark`.
2. **Shared-prefix columns.** Columns whose values share a long head (URLs under
   one origin, prefixed identifiers) tie across any fixed width and would decode
   anyway. The sampler is specified to decline in that case; whether it should
   instead key on the bytes *after* a detected common prefix, as LiquidCache does
   for its dictionary values, is left open — it is a strictly better key when a
   common prefix exists and pure overhead when it does not.
3. **Sharing the stream with the shared dictionary.** For blocks whose dictionary
   entries are themselves FSST-compressed, one key per distinct value would be far
   cheaper than one per row. Whether the stream attaches to the shared dictionary
   or to the value stream is an implementation choice the requirement does not fix.
4. **Promoting the dependency verdict to the decision register.** The
   neither-depend-nor-vendor reasoning in `design.md` §2 generalises past
   LiquidCache to any DataFusion-adjacent crate. Whether it becomes a numbered
   entry in `docs/design-review-decisions.md` alongside Decision 43 is the
   reviewer's call, not this change's.
