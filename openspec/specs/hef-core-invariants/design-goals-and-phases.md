# HEF Design Stance, Goals, Non-Goals, Implementation Phases, Verification, and Numbered Invariants

Companion artifact for the `hef-core-invariants` capability — the HEF/HEJ format design stance, performance targets, alignment rules, payload/metadata scope, goals, non-goals, the verbatim numbered core-invariant list, the staged implementation roadmap, the verification strategy, and the final recommendation. The per-area normative requirements live in the various `hef-*` capability specs; this file preserves the cross-cutting framing and the numbered invariant catalogue.

## Design stance

Use a **custom event storage format family**, not a single file that tries to do everything.

```text
HEJ = Harana Event Journal
  Append-only, crash-recovery commit format.
  Written through io_uring: io_uring_cmd NVMe passthrough on the journal char device, with filesystem io_uring as the non-NVMe dev fallback.
  Optimized for durable event acknowledgement and deterministic replay.

HEF = Harana Event File
  One immutable, manifest-published, queryable event file format.
  Many HEF files may exist, but they all use the same HEF format.
  Optimized for time/entity queries, filtering, aggregation, pruning, HEF rewrite,
  chat/investigation evidence retrieval, and QueryEngine execution.

LiveOverlay = Harana live query overlay
  Rebuildable Arrow-native representation of committed HEJ ranges not yet covered
  by manifest-published HEF files.
  Optimized for fresh queries and strict read-after-ack semantics when enabled.
```

The key design decision is that **writes are acknowledged from HEJ**, while **queries read a committed event snapshot made from manifest-published HEF files plus approved LiveOverlay ranges**. HEJ is the durability and freshness source until a manifest-published HEF file covers the same journal range. LiveOverlay is queryable but remains rebuildable acceleration state.

HEF has exactly one file format. HEF files may differ in size, clustering, optional index blocks, optional aggregate blocks, optional context blocks, and optional internal acceleration blocks, but those differences are declared in the HEF feature directory and do not create a second HEF format or lifecycle class. Freshness is provided by HEJ-backed LiveOverlay, not by publishing a special freshness-oriented HEF file type.

The write path follows the autonomous-commit lesson from the NVMe latency paper: per-core workers own local HEJ commit queues, workers flush small aligned journal frames themselves, and the hot path must not funnel commits through a centralized group-commit thread. HEJ durability is decoupled from LiveOverlay query publication. Normal HEJ frames are 4 KiB, 8 KiB, 16 KiB, 32 KiB, or 64 KiB. The default all-round flush target is 16 KiB; the latency-critical and low-load force-commit target is 4 KiB; the maximum normal HEJ frame size is 64 KiB.

Reference: <https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf>

### Performance target

HEJ + HEF + LiveOverlay is the native Harana event storage path. The required target is measurable:

```text
HEJ + HEF + LiveOverlay must meet every required benchmark gate listed by the benchmarks and acceptance gates (see the hef-benchmarks-and-acceptance-gates capability).
```

If any required benchmark class misses its acceptance gate, the implementation must add a format, index, metadata, planner, HEF rewrite, cache, or runtime-path improvement before the class is marked ready.

The software path remains the correctness reference. Optional hardware acceleration may improve performance but must never be required for correctness.

### Harana alignment rules

This file format must align with Harana's storage, public-output, and configuration conventions:

```text
Stable logical fields use unit-neutral names.
Timestamp-like values use TimestampValue.
Duration-like values use DurationValue.
Physical encodings may use nanosecond integers internally, but public and stable schema names must not expose unit suffixes.
```

Internal event identity follows Harana's internal identity model:

```text
internal identity = (tenant_id, epoch, sequence)
public identity  = opaque cursor or owner-defined public resource id
```

Raw `tenant_id`, raw `epoch`, raw `sequence`, object-store paths, row offsets, payload offsets, local cache paths, embedding columns, and other internal analytical columns are not public output. QueryEngine scans must enforce column authorization before returning rows to public routes or tool callers.

Harana event-query visibility uses this committed-snapshot model:

```text
Committed event snapshot = manifest-published HEF files
                         + approved HEJ-derived LiveOverlay ranges
                         - HEF-native deletion-vector effects
                         - correction effects
```

Raw ingest buffers, unacknowledged memory, non-replayable batches, and partial HEF files are never queryable.

### Payload and metadata scope

The payload model is:

```text
- exactly one payload format: harana_variant_v1, a Parquet-Variant-compatible binary value
  encoding with external shared key dictionaries;
- ingest-time transcoding of JSON, Protobuf, Avro, MessagePack, and raw bytes into
  harana_variant_v1; source bytes are not preserved;
- frame-level VariantDictionaryV1 in HEJ and granule-level variant dictionary blocks in HEF;
- statistics-driven variant shredding of frequently accessed payload paths into typed
  columns at HEF publication and rewrite;
- residual harana_variant_v1 values for unshredded paths, with offset-based single-field
  extraction that never parses sibling fields;
- Adaptive-encoder cascades for shredded payload columns, preserving random access in
  compressed form;
- path_presence SkipIndex kind and optional MPHF path-lookup acceleration blocks;
- deterministic shredded-plus-residual payload reconstruction.
```

All metadata is HEF-native or manifest-native unless this spec explicitly says otherwise. Puffin sidecars are deliberately excluded. Deletion vectors, sketches, embeddings, context packets, rollups, and other auxiliary structures use HEF-native blocks or manifest-native entries.

---
## Goals

The native HEF/HEJ format must support:

1. **Very fast event ingestion**
   - Per-core append path.
   - Small durable batches.
   - No filesystem append dependency on the hot commit path.
   - Autonomous log flush by the ingest workers; no centralized group-commit writer.
   - Autonomous acknowledgement by the owning worker or a small acknowledgement group; no single global acknowledgement thread on the hot path.
   - Lock-free per-worker serialized commit queues.
   - Optional topology-local log stealing to reduce waiting for small buffers.
   - Low-load force commit so trickle traffic does not wait indefinitely for a full frame.
   - io_uring backend: io_uring_cmd NVMe passthrough as the primary journal write path; filesystem io_uring as the non-NVMe dev fallback, whose buffered mode is outside the microsecond-latency gate unless it meets the same durability benchmark.
   - HEJ as the protected pre-ack durability record.

2. **Queryable committed snapshots**
   - One logical `events` table through QueryEngine.
   - Manifest-published HEF files for committed historical coverage.
   - Node-local LiveOverlay for durable HEJ ranges not yet covered by manifest-published HEF.
   - Query snapshots that combine manifest generation, commit_watermark, visibility_watermark, snapshot_watermark, schema generation, deletion-vector generation, and correction generation.
   - Sequence-disjoint HEF and LiveOverlay ranges so queries cannot double-count events.

3. **Fast event scans**
   - Time-range pruning.
   - Sequence-range pruning.
   - Source/type/entity filtering.
   - Entity/account/opportunity/customer timelines.
   - Projection pushdown.
   - Predicate pushdown with exact/inexact reporting.
   - Limit pushdown where ordering guarantees allow it.
   - Late materialization of large payloads.

4. **Built-in aggregation acceleration**
   - Manifest-level summaries for pruning and coarse metadata-only answers.
   - HEF footer, stripe, page, time-bucket, and sparse-cube aggregate blocks.
   - Exact counters, sums, min/max, null counts, and selected rollups.
   - Approximate sketches only for explicit approximate queries.
   - Exact LiveOverlay aggregate deltas for fresh HEJ-backed rows not yet HEF-covered.
   - Planner rules that merge HEF aggregates, LiveOverlay deltas, and deletion-vector/correction effects safely.

5. **Revenue Intelligence and dashboard acceleration**
   - Fast repeated metric queries.
   - Prepared-view materializations for semantic product metrics.
   - File-level physical metadata for generic event aggregates.
   - Clear boundary between physical file metadata and product-level metric definitions.

6. **Chat, investigation, and LLM-tool acceleration**
   - Fast retrieval of authorized context columns.
   - Event evidence locators that avoid raw payload scans.
   - Optional text token, summary, and embedding/vector blocks for internal tool use.
   - Payload snippets and context packets that are late-materialized only after authorization and row selection.
   - No direct QueryEngine sessions from tools; tools go through the owner service or table provider facade.

7. **Crash safety and recovery**
   - Journal replay.
   - Torn-write detection.
   - Idempotent HEF publication.
   - Atomic manifest publication.
   - Node-local LiveOverlay rebuild from HEJ or replicated journal fragments.
   - Safe retry metadata linked to HEJ, with no duplicate event payload storage in KV.

8. **Rewrite-friendly immutable HEF files**
   - One HEF format for all published event files.
   - HEJ-backed LiveOverlay provides freshness.
   - HEF rewrite/repack jobs may merge files, improve clustering, add feature blocks, and remove rows covered by safe deletion vectors.
   - Workload-driven clustering, richer rollups, and optional layout projections are declared as HEF feature/layout metadata.
   - Deletion vectors and corrections are handled through HEF-native/manifest-native immutable metadata and HEF rewrite, never by mutating published HEF files.

9. **Object-store and local-NVMe compatibility**
   - Immutable HEF files work on S3-compatible storage.
   - Local cache can use filesystem, direct I/O, or FDP-placed NVMe allocation.
   - Local cache and HEF placement remain rebuildable and never become correctness sources.

10. **Runtime-selected acceleration with software parity**
    - Intel QPL/IAA, DSA, QAT, SIMD, and future accelerators are optional.
    - Startup detects providers and self-tests them.
    - Fallback to software is required.
    - Acceleration defines no operator config keys and does not alter query results or visibility semantics.

---

## Non-goals

The format is **not** trying to be:

```text
A general replacement for Parquet.
A mutable database page format.
A row-level update format.
A universal JSON document store.
A format where every arbitrary JSON field is fully indexed.
A place to store unbounded user-defined aggregate cubes.
A Puffin sidecar metadata format.
A centralized group-commit subsystem for the journal acknowledgement hot path.
A system where QueryEngine queries raw HEJ frames on the normal analytical path.
A SQL-level UNION view that leaves snapshot consistency, deduplication, and watermarks to users.
A hardware-acceleration feature that operators configure per query.
A Vortex-style self-describing layout tree that abolishes row-group/granule semantics.
A replacement for PreparedViewService semantic metric definitions.
A replacement for MemoryService, ChatService, or RevenueInsightService saved product resources.
```

It is purpose-built for:

```text
append-heavy event ingestion
fresh committed-event visibility
historical event queries
dashboard and alert aggregation
entity/account/opportunity timelines
Revenue Intelligence analytics
chat/investigation evidence retrieval
QueryEngine execution
```

---


## Implementation phases

### Phase 1 — HEJ foundation, AWUPF, and hash-chain correctness

```text
Implement HEJFrameHeaderV1 with header-only CRC-64/NVME and authoritative frame BLAKE3.
Implement segment-level BLAKE3 hash chain.
Implement HEJ segment descriptors, generation checks, and segment recycling eligibility.
Probe NVMe AWUPF per journal shard and record atomicity metadata.
Query NVMe Identify Namespace AWUN/AWUPF once at startup per journal device, record untorn-write capability per shard, and claim untorn frame writes only when AWUN >= 1 is confirmed (RWF_ATOMIC submission on the dev fallback where the kernel accepts it).
Implement harana_hej_compact_batch_v1.
Reject Arrow IPC, Arrow files, Arrow streams, and Arrow RecordBatch payloads in HEJ.
Implement per-worker lock-free serialized commit queues.
Implement autonomous log flush for 4 KiB, 8 KiB, 16 KiB, 32 KiB, and 64 KiB normal frames.
Implement 16 KiB default flush target and 4 KiB latency/force-commit target.
Implement READY/HARDENED/COMMITTED state tracking.
Implement low-load force commit with jitter/probabilistic trigger.
Implement optional topology-local log stealing behind benchmark gates.
Implement append-only fast path where HARDENED equals COMMITTED.
Implement transactional-dependency hook for future autonomous acknowledgement routes without adding dependency metadata to ordinary event records.
Implement deterministic replay and safe-retry reconstruction linkage.
Benchmark 4-64 KiB normal frame flush units and 1 MiB large-event frames.
```

### Phase 2 — HEJ-backed LiveOverlay and explicit watermarks

```text
Publish HEJ-to-LiveOverlay conversion as the public deterministic mapping.
Decode valid HEJ frames into the fixed LiveOverlay Arrow RecordBatch schema.
Track commit_watermark, visibility_watermark, and snapshot_watermark.
Implement node-local LiveOverlay snapshot and replay rebuild.
Register a single QueryEngine events table provider.
Return UnionExec-style HEF/LiveOverlay plans, even if the HEF side is initially empty.
Validate no duplicate sequence ranges.
Enforce column authorization in the scan path.
```

### Phase 3 — Single HEF minimum viable format with granules and marks

```text
Header and footer.
Required envelope columns.
Payload arena.
Stripe metadata.
Granule directory.
Per-column marks directory.
Minmax SkipIndex for required columns.
Sequence and time SkipIndexes.
Basic exact counts.
BLAKE3 file verification.
Basic HefExec reader with marks-based range reads.
Manifest integration.
HEF/LiveOverlay query snapshots.
```

### Phase 4 — Adaptive encoding stack

```text
Sample-based per-block encoding selection.
FastLanes bitpacked FOR and DELTA cascades for sequence/time/monotonic columns.
ALP and ALP RD for f64 metric columns.
FSST for high-cardinality strings.
Dictionary/RLE/bitpacking candidates for low-cardinality columns.
Codec pipeline ids in marks and page metadata.
Software fallback and accelerator parity tests.
```

### Phase 5 — Uniform indexes and built-in aggregates

```text
Required and optional feature flags.
SkipIndex<kind, granularity> model.
Ribbon filters and split-block Bloom filters.
Low-cardinality bitmap indexes.
Entity/account/opportunity hash filters.
Time bucket indexes.
Equivalent lightweight LiveOverlay indexes.
Exact counts.
Time rollups.
Source/type/entity_type rollups.
Measure sum/min/max/count.
Sparse cubes for bounded high-value dimensions.
Metadata-only aggregate planner.
HEF aggregate + LiveOverlay delta aggregate planner.
Deletion-vector/correction aggregate delta handling.
```

### Phase 6 — HEF-native deletion vectors and structured payload fields

```text
HEF-native Iceberg v3-compatible binary deletion-vector semantics.
Manifest-native deletion-vector generation publication.
DeletionVectorAntiJoinExec.
Exact deletion-vector aggregate subtraction.
Correction/latest-view metadata.
variant_shredded_field_blocks, variant dictionary blocks, and residual variant values.
path_presence SkipIndexes and optional variant_path_mphf_blocks.
Adaptive payload arena granularity for wide-payload tenants.
No Puffin sidecars.
```

### Phase 7 — Layout class, projections, and rewrite lifecycle

```text
Compact and Wide layout classes.
Automatic compact/wide crossover and rewrite transitions.
Projection rowsets with own marks and column data.
Time-major/entity-major/source-type-major/context-major/revenue-metric projections.
Active/Outdated/DeleteOnDestroy part states.
Merge small HEF files.
Better clustering.
Better rollups.
Deletion-vector removal.
Late-event reclustering.
Manifest coverage and projection updates.
LiveOverlay eviction after published HEF coverage.
```

### Phase 8 — io_uring_cmd NVMe passthrough journal backend

```text
Per-core rings on the NVMe character device (/dev/ngXnY).
Registered/PBUF_RING buffer pools.
Raw LBA journal regions.
Active and recyclable segment tables.
AWUN/AWUPF-aware atomic frame policy from the startup Identify query.
Linked write -> flush -> watermark SQE chains.
Segment_chain_blake3 anchors.
Completion polling (SQPOLL/IOPOLL default-on).
DSM TRIM after segment GC confirmation on non-FDP namespaces.
Recovery scan.
Filesystem io_uring remains the non-NVMe dev fallback.
LiveOverlay publication remains unchanged.
```

### Phase 9 — Chat, investigation, and LLM-tool acceleration

```text
Context projection columns.
Context locator SkipIndex.
Text token indexes for selected summary fields.
Context packet reader API.
Optional internal embedding/vector blocks stored as HEF-native blocks.
Tool-safe public evidence refs.
```

### Phase 10 — Operability, advanced blocks, and runtime acceleration

```text
system.hef_files, system.hef_columns, system.hef_granules, and system.hef_rewrites.
Approximate distinct counts.
Quantile sketches.
Top-k metadata.
Quality input stats blocks.
Rule evaluation support blocks.
Alert event locator blocks.
Graph extraction support blocks.
Cause snapshot locator blocks.
Classification propagation blocks.
Encrypted footers.
Hybrid memory+NVMe (foyer-style) local HEF cache (see the hef-hardware-deployment capability), optionally FDP-placed.
FDP/ZNS data placement (see the hef-hardware-deployment capability).
LiveOverlay overflow via Arrow IPC / mmap segments.
Runtime-selected hardware acceleration below QueryEngine.
```

## Verification and testing strategy

The numbered invariants below are protocol-level properties. They are validated with the following mandatory techniques, in addition to conventional unit and integration tests.

### Deterministic simulation testing (DST)

```text
scope
  HEJ commit, void/lease, watermark advancement, LiveOverlay publication and
  eviction, HEF publication, manifest CAS, deletion-vector/correction
  generations, crash recovery, and node-local rebuild.

model
  TigerBeetle-VOPR-style: the whole node (or a small cluster) runs single-threaded
  inside a simulated environment with seeded PRNG control over scheduling, I/O
  completion order, I/O errors, torn writes, partial frames, clock skew, restarts,
  and object-store conditional-write races.

implementation
  all time, randomness, I/O, and scheduling behind traits so production code runs
  unmodified under simulation; madsim or turmoil style runtimes for the
  distributed/replication layer; a custom storage fault simulator for HEJ frames
  (torn tails, reordered flushes, bit rot caught by BLAKE3).

requirements
  every simulated run is reproducible from its seed;
  invariant checkers for the numbered invariants run continuously inside the
  simulation, not only at end-of-run;
  CI runs a fixed seed corpus plus a continuous random-seed soak;
  Antithesis-style autonomous fault exploration is the recommended hosted
  extension when available, not a replacement for in-repo DST.
```

### Model checking of the snapshot/watermark protocol

```text
TLA+ (or P) specification of:
  commit_watermark / visibility_watermark / snapshot_watermark advancement,
  HEF/LiveOverlay sequence-range disjointness,
  manifest generation publication and pointer CAS,
  LiveOverlay eviction safety,
  deletion-vector and correction generation visibility.

checked properties
  no double-count, no lost acknowledged event, no snapshot observing partial
  generations, eviction never precedes published coverage.

rule
  the model is normative documentation: protocol changes must update the model
  and re-check before implementation merges.
```

### Concurrency and code-level verification

```text
Loom
  exhaustive interleaving checks for the lock-free per-worker commit queues,
  clean-cursor CAS log stealing, and watermark publication atomics.

Kani (or equivalent CBMC-based checker)
  bounded proofs for HEJ frame encode/decode round-trips, harana_variant_v1
  offset navigation (no out-of-bounds reads on adversarial input), and
  deletion-vector application.

fuzzing
  cargo-fuzz coverage-guided fuzzing for every decoder that touches untrusted
  or on-disk bytes: HEJ frames, HEF footers, marks, SkipIndex blocks, variant
  values, manifest objects.
```

### Crash-consistency and storage-fault testing

```text
LazyFS / dm-log-writes
  replay-every-prefix testing of HEJ write sequences: after any crash point,
  recovery must reach a state where replay is idempotent and commit_watermark
  is correct.

fault matrix
  power-fail at frame boundaries and mid-frame, with and without
  AWUPF/RWF_ATOMIC claims, on buffered-fallback and direct-I/O paths;
  object-store faults: lost PUT, duplicated PUT, CAS race loss, stale reads
  within the store's consistency model.

gate
  no required benchmark class is marked ready until its write path passes the
  crash-consistency matrix.
```

## Core invariants

```text
1. HEJ is the source of truth until a manifest-published HEF file covers the journal range.

2. LiveOverlay is queryable but not authoritative; it must be rebuildable from HEJ.

3. Nodes use node-local LiveOverlay, not EventManager-forwarded live event reads.

4. HEF files are immutable after footer finalization.

5. Public queries only read manifest-published Active HEF files plus the approved node-local LiveOverlay snapshot.

6. Every query uses one snapshot containing manifest generation, commit_watermark, visibility_watermark,
   snapshot_watermark, schema generation, authorization generation, deletion-vector generation, and correction generation.

7. HEF and LiveOverlay sequence ranges must be disjoint inside one query snapshot.

8. A LiveOverlay segment may be evicted only after its full sequence range is covered by a manifest-published HEF file.

9. Late events are included by snapshot_watermark first, then pruned by occurred_at; LiveOverlay scanning must not be limited to recent event-time ranges only.

10. Granule is the minimum unit of pruning and parallel scan. A granule contains a contiguous, sequence-ordered row range within one stripe.

11. Per-column marks are the authoritative mapping from (column, projection, granule) to compressed offsets.

12. Metadata aggregates must declare exact vs approximate.

13. Exact queries must never silently use approximate metadata.

14. Exact HEF-plus-LiveOverlay aggregate queries must include exact LiveOverlay deltas and exact deletion-vector/correction effects or fall back to scan.

15. Raw payload reads must be late-materialized and authorized.

16. Payloads have exactly one canonical format, harana_variant_v1. Structured payload-field pushdown uses variant_shredded_field_blocks; unshredded paths remain in residual variant values in the payload arena.

17. Internal columns such as tenant_id, epoch, sequence, payload_ref, row offsets, local paths, and embeddings must not be public output.

18. Unknown required format features must refuse.

19. Crash recovery must be idempotent and must rebuild missing LiveOverlay state from HEJ before serving fresh queries.

20. Hardware accelerators and NVMe device capabilities (FDP placement, AWUN/AWUPF untorn-write support, DSM TRIM) are optimizations, not correctness dependencies.

21. BLAKE3 is the authoritative integrity check; CRC-64/NVME is a header-only fast precheck.

22. The manifest, not file existence, defines HEF visibility.

23. PreparedView materializations answer semantic product metrics; HEF aggregate blocks answer physical event aggregates.

24. HEJ v1 normal frame sizes are exactly 4 KiB, 8 KiB, 16 KiB, 32 KiB, or 64 KiB.

25. HEJ hot-path acknowledgement uses autonomous worker flush and must not depend on a centralized group-commit writer or a single global acknowledgement thread.

26. Buffered-filesystem HEJ writes are correctness fallbacks and are outside the microsecond-latency claim unless fdatasync/fsync-equivalent durability meets the same benchmark gate.

27. Ordinary append-only event ingest must not pay GSN/RFA/barrier dependency overhead; that machinery is permitted only for routes with real transactional dependencies.

28. Barrier transactions are never user events, never HEF rows, never QueryEngine-visible rows, and never public output.

29. HEF-native deletion vectors are immutable once published and must be applied before rows are returned.

30. Puffin sidecars are not used; auxiliary metadata is HEF-native or manifest-native.

31. Projections are read alternatives over the same logical rowset and must not cause double-counting.

32. Compact and Wide are layout classes within one HEF format, not separate formats.

33. A row's payload is reconstructed as the deterministic merge of shredded typed values and the residual variant value; a shredded path's value exists in exactly one of the two.

34. Extracting a single payload path must not require decoding sibling fields; residual access is offset-based navigation against the governing variant dictionary.

35. Manifest publication on object storage uses conditional writes (create-only generation objects, If-Match CAS pointer advance); a lost CAS race must rebase and retry, never overwrite.

36. RWF_ATOMIC, AWUPF, FDP, and ZNS are torn-write-cost and write-amplification optimizations only; HEJ correctness never depends on them.

37. Range-filter and point-filter SkipIndexes are inexact_no_false_negative pruning structures; point filters must never answer range predicates.

38. Incrementally maintained PreparedView output must equal full recomputation from the same committed event snapshot; views that cannot satisfy this fall back to recompute.

39. Caches (LiveOverlay, local HEF cache, vector indexes, trained compression dictionaries) are rebuildable acceleration state; losing or erasing them never loses authoritative data, and subject-encrypted blocks remain encrypted in every cache tier.
```

---
## Final recommendation

Use this design:

```text
HEJ for durability and freshness.
HEJ v1 as a Harana journal format with payload_encoding = harana_hej_compact_batch_v1 only.
HEJ write path aligned with autonomous commit: worker-owned queues, small direct writes, autonomous flush, autonomous acknowledgement where needed, force commit under low load, and topology-local log stealing only when benchmark-positive.
HEJ io_uring path with io_uring_cmd NVMe passthrough as the primary write path, registered buffers/files, PBUF_RING completion buffers, AWUN-gated untorn writes, SQPOLL/IOPOLL on by default, and linked write -> flush -> watermark SQE chains.
HEJ integrity with header-only CRC-64/NVME precheck, frame BLAKE3, and segment-level BLAKE3 hash chain.
HEJ storage with AWUN/AWUPF-aware frame atomicity, optional FDP/ZNS placement, formal raw-NVMe direct-LBA layout, and recyclable segments.
No Arrow IPC, Arrow file, Arrow stream, or Arrow RecordBatch storage inside HEJ.
Node-local LiveOverlay for durable HEJ-backed query visibility before HEF coverage.
LiveOverlay as the mandatory Arrow representation of fresh HEJ ranges.
Explicit commit_watermark, visibility_watermark, and snapshot_watermark semantics.
One HEF format for immutable queryable event storage.
Many immutable HEF files selected by the manifest.
HEF granules as the minimum unit of pruning and parallel scan.
HEF per-column marks for constant-time range reads.
HEF compact and wide layout classes inside the same format.
HEF projections for alternate sort orders without double-counting.
HEF adaptive encoding with FastLanes, DELTA/FOR/bitpacking, ALP/ALP RD, FSST, dictionaries, trained Zstd residual dictionaries, and measured fallbacks.
One canonical payload format (harana_variant_v1) with shared variant dictionaries, statistics-driven shredding into adaptively encoded typed columns, and offset-navigable residual values.
HEF-native deletion vectors and corrections; no Puffin sidecars.
HEF feature blocks for indexes, rollups, context, quality, rule, graph, cause, and internal model acceleration.
SkipIndexes including binary fuse point filters and Grafite/Memento-style range filters alongside Ribbon and split-block Bloom.
Manifest for HEF visibility, projection selection, part state, watermarks, and generation consistency, published with object-store conditional-write CAS.
HEF aggregate blocks for exact physical aggregate acceleration.
PreparedViews for semantic Revenue Intelligence and dashboard materializations, maintained incrementally DBSP-style with recompute equivalence.
QueryEngine TableProvider for one logical events table across HEF and LiveOverlay data, scanning Utf8View/dictionary/REE Arrow forms.
HEF rewrite/repack with incremental liquid-style reclustering for compact/wide transitions, richer indexes, safe deletion-vector removal, projection rewrites, and part-state lifecycle.
Internal vector blocks using RaBitQ quantization with DiskANN/Vamana or IVF layouts, always rebuildable.
Hybrid memory+NVMe (foyer-style) local HEF cache as rebuildable acceleration state.
Hardware accelerators as optional runtime-selected performance paths.
Deterministic simulation testing, TLA+/P model checking, Loom/Kani verification, fuzzing, and crash-consistency testing as mandatory verification gates.
System introspection through system.hef_files, system.hef_columns, system.hef_granules, and system.hef_rewrites.
```

This is the native Harana event storage design. Event-specific indexes, rollups, context projections, sequence-aware freshness, HEF/LiveOverlay aggregate deltas, HEF-native deletion vectors, granules, marks, projections, and workload-driven layout classes are part of the storage and QueryEngine planning path.

---

## References

- Moving on From Group Commit: Autonomous Commit Enables High Throughput and Low Latency on NVMe SSDs: <https://www.cs.cit.tum.de/fileadmin/w00cfj/dis/papers/latency.pdf>
- Linux io_uring NVMe passthrough (io_uring_cmd): <https://lwn.net/Articles/895961/>
- Apache Parquet File Format Documentation: <https://parquet.apache.org/docs/file-format/>
- Apache DataFusion User Guide: <https://datafusion.apache.org/user-guide/>
- Apache DataFusion Custom Table Providers: <https://datafusion.apache.org/library-user-guide/custom-table-providers.html>
- Parquet Variant Binary Encoding: <https://github.com/apache/parquet-format/blob/master/VariantEncoding.md>
- Parquet Variant Shredding: <https://github.com/apache/parquet-format/blob/master/VariantShredding.md>
- Vortex Columnar Format and Toolkit: <https://docs.vortex.dev/>
- Linux untorn (atomic) write support, RWF_ATOMIC: <https://docs.kernel.org/block/atomic-writes.html>
- NVMe Flexible Data Placement (TP4146) overview: <https://nvmexpress.org/nvmeflexible-data-placement-fdp-blog/>
- Grafite: Taming Adversarial Queries with Optimal Range Filters (SIGMOD 2024): <https://dl.acm.org/doi/10.1145/3639258>
- Memento Filter: A Fast, Dynamic, and Robust Range Filter (2024): <https://dl.acm.org/doi/10.1145/3698811>
- Binary Fuse Filters: Fast and Smaller Than Xor Filters (2022): <https://arxiv.org/abs/2201.01174>
- DBSP: Automatic Incremental View Maintenance for Rich Query Languages (VLDB 2023): <https://www.vldb.org/pvldb/vol16/p1601-budiu.pdf>
- Feldera (DBSP implementation): <https://www.feldera.com/>
- RaBitQ: Quantizing High-Dimensional Vectors with Theoretical Error Bound (SIGMOD 2024): <https://dl.acm.org/doi/10.1145/3654970>
- DiskANN: Fast Accurate Billion-point Nearest Neighbor Search (NeurIPS 2019): <https://suhasjs.github.io/files/diskann_neurips19.pdf>
- Apache Arrow StringView (Utf8View) layout: <https://arrow.apache.org/docs/format/Columnar.html#variable-size-binary-view-layout>
- Apache DataSketches: <https://datasketches.apache.org/>
- Zstandard dictionary builder (ZDICT): <https://facebook.github.io/zstd/zstd_manual.html>
- Amazon S3 conditional writes: <https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-requests.html>
- foyer hybrid cache: <https://foyer.rs/>
- TigerBeetle VOPR deterministic simulation: <https://docs.tigerbeetle.com/about/vopr/>
- madsim deterministic simulation runtime: <https://github.com/madsim-rs/madsim>
- turmoil network simulation: <https://github.com/tokio-rs/turmoil>
- Antithesis autonomous testing: <https://antithesis.com/>
- Loom concurrency permutation testing: <https://github.com/tokio-rs/loom>
- Kani Rust verifier: <https://model-checking.github.io/kani/>
- LazyFS crash-consistency testing: <https://github.com/dsrhaslab/lazyfs>
