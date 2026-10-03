## Purpose

Defines the performance benchmarks and the pass/fail gates that decide when HEF/HEJ code is allowed to ship:

- The benchmark matrices set out what must be measured for write paths, query types, and accelerator backends.
- The acceptance gates set the pass/fail thresholds, the target numbers, and the rules for picking defaults.
- Nothing may be marked ready or made the default until it passes these gates.

The concrete write/query benchmark tables and acceptance/accelerator gate tables are embedded in [benchmark-tables.md](benchmark-tables.md).
## Requirements
### Requirement: Required write and query benchmark coverage
Write-path benchmarks SHALL compare filesystem io_uring and `io_uring_cmd` NVMe passthrough autonomous-flush targets, the internal centralized group-commit and flush-pipelining prototypes, and the filesystem-only reference path across the flush-size, worker-count, payload-size, queue-depth, ack-group-size, log-stealing, force-commit, load, replication, storage-backend, and storage-class matrix; reported metrics SHALL include ack latency p50/p90/p99/p999, READY/HARDENED/COMMITTED transition latencies, throughput per core, and write amplification. Query benchmarks SHALL cover the required query classes (range/bucket counts, sparse cube, entity/account/opportunity timelines, metadata-only and PreparedView revenue aggregates, HEF-plus-LiveOverlay dashboard/timeline, payload-predicate fallback, shredded payload-field filter/projection, residual variant single-path extraction, late-event, deletion-vector-heavy, correction/latest-view, chat/investigation context retrieval, text token search, semantic retrieval, and large-payload late materialization) across cold-cache, warm-cache, object-store-cache, aggregate-shortcut, fresh, and PreparedView reference paths; reported metrics SHALL include pruning counts, hit rates (including local HEF cache and PreparedView incremental-maintenance lag and manifest CAS retry rate), and p95/p99 latencies.

#### Scenario: Default flush target requires separate measurement
- **WHEN** choosing the default HEJ flush target
- **THEN** the 4 KiB and 16 KiB autonomous-flush targets are benchmarked separately before either is selected

### Requirement: Latency-paper alignment gates
The HEJ acknowledgement hot path SHALL NOT use a centralized group-commit writer or depend on a single global background acknowledgement thread. The 16 KiB flush target SHALL be the default only if it preserves low p99 acknowledgement latency while sustaining required throughput, and the 4 KiB target SHALL remain available for latency-critical and low-load force-commit paths. A normal flush unit of 128 KiB or larger SHALL be benchmarked only as a negative control and SHALL NOT be selected for normal HEJ frames. Topology-local log stealing SHALL either improve p90/p99 acknowledgement latency under small-record workloads or remain disabled. Append-only event ingest SHALL NOT pay dependency-tracking overhead for nonexistent page-level dependencies, and routes with real transactional dependencies SHALL prove autonomous acknowledgement correctness before enabling dependent acknowledgements. Buffered-filesystem fallback SHALL be reported separately from NVMe-passthrough/direct-I/O results, AWUPF atomicity claims SHALL be enabled only for shards where probing proves support, and segment recycling SHALL NOT advance before HEF coverage, recovery safety, and segment_chain_blake3 retention rules are satisfied. A single stalled or crashed ingest worker SHALL NOT stall a tenant's `commit_watermark` beyond the reservation-lease deadline (the void-record path closing the abandoned range within that bound). RWF_ATOMIC untorn-write claims SHALL be enabled only when the startup NVMe Identify Namespace query confirms `AWUN >= 1` and the kernel accepts the atomic submission (rejected submissions falling back without losing the frame); FDP/ZNS placement and range-filter SkipIndexes SHALL be enabled only when measured benefit exceeds their cost, with correctness holding when placement is absent; and incremental PreparedView maintenance SHALL produce byte-/value-identical output to full recomputation on the equivalence suite before a view class is marked ready.

#### Scenario: Oversized flush unit rejected as default
- **WHEN** a 128 KiB normal flush unit is benchmarked
- **THEN** it is treated as a negative control only and is not selected for normal HEJ frames

#### Scenario: Stalled worker does not stall the watermark
- **WHEN** an ingest worker stalls or crashes holding a sequence reservation
- **THEN** the void-record path closes the abandoned range within the reservation-lease deadline so the tenant's `commit_watermark` advances

### Requirement: Target numbers and query-ready gate
The implementation SHALL meet the targets: HEJ commit p99 latency below 50 microseconds (single-node PCIe Gen4 NVMe, 4 KiB frame); HEJ throughput above 250k events/sec/core at 16 KiB frame; HEF numeric-column scan above 10 GB/s/core decompressed on an AVX-512 host; HEF size ratio 0.6x–0.7x versus Parquet+Zstd; BLAKE3 verify above 5 GB/s/core on an AVX-512 host; and Compact-layout overhead 30%–50% smaller than Wide for files under 10 MiB. The HEF `entity_id` point-lookup latency target SHALL be stated against a named cache profile rather than a single unqualified "cold" figure, because the read cost is dominated by which tier holds the file's metadata. Three profiles SHALL be measured and reported separately: `warm` (file footer and marks resident in the local HEF cache), `cold-cache/NVMe-durable` (footer and marks absent from the in-process cache but served from a durable node-local NVMe metadata tier), and `cold-cache/S3-durable` (footer and marks fetched from an object-store durable tier that pays real per-request round-trip latency). The `entity_id` point lookup SHALL be below 5 ms on the `warm` and `cold-cache/NVMe-durable` profiles when granule and marks pruning apply; on the `cold-cache/S3-durable` profile the target SHALL be no more than 2 dependent round trips plus a budgeted per-request millisecond allowance, and a sub-5 ms figure SHALL NOT be claimed for a lookup that pays object-store round trips. The earlier unqualified "below 5 ms cold from object storage" figure silently assumed a warm NVMe metadata tier and SHALL NOT be read as an object-store durable-tier guarantee. A query class SHALL NOT be marked ready unless HEF + QueryEngine meets its acceptance target under the warm, `cold-cache/NVMe-durable`, and `cold-cache/S3-durable` profiles that apply to its declared deployment, each measured separately, or the class is explicitly excluded from the ready set.

#### Scenario: Query class not marked ready on cold-cache miss
- **WHEN** a query class meets its target on warm cache but not on a cold-cache profile that applies to its deployment
- **THEN** it is not marked ready unless it is explicitly excluded from the ready set

#### Scenario: S3-durable cold lookup is not measured against the NVMe target
- **WHEN** the `entity_id` point-lookup gate is evaluated on the `cold-cache/S3-durable` profile
- **THEN** it is held to the object-store budget (at most 2 dependent round trips plus the budgeted per-request millisecond allowance) and is not passed or failed against the sub-5 ms NVMe-cold figure

### Requirement: Real-time query safety gates
Real-time query acceptance SHALL require: p95 LiveOverlay rows scanned per dashboard query below the benchmark target; p99 visibility_watermark lag behind commit_watermark below the freshness target; p99 durable-to-HEF-published lag below the HEF publication target; no event_id/(epoch, sequence) returned from both HEF and LiveOverlay in one snapshot; granule and marks pruning reducing object-store range reads versus stripe-only pruning; occurred_at queries including LiveOverlay rows by snapshot_watermark rather than recency-only heuristics; exact aggregate equal to exact HEF aggregate plus exact LiveOverlay delta minus exact deletion-vector/correction effects; rebuild-from-HEJ before serving fresh queries after restart (or an explicit bounded-staleness/manifest-only mode); and blocked internal fields never appearing in public route, chat, investigation, export, or support outputs.

#### Scenario: Duplicate-safety gate
- **WHEN** validating a real-time query path for readiness
- **THEN** it must demonstrate that no event_id/(epoch, sequence) appears from both HEF and LiveOverlay in the same query snapshot

### Requirement: Bulk-egress throughput gate
Bulk-egress benchmarks SHALL measure sequential single-family read throughput (free-text blocks, cold from object storage and warm from local cache) at corpus scale, and the measured throughput SHALL underwrite the declared release-migration blackout window: a release SHALL NOT declare a blackout window shorter than the corpus's free-text re-extraction read time implied by the gated throughput. The gate SHALL fail if bulk egress regresses to residual-arena reads (bytes read materially exceeding the requested family's stored bytes).

#### Scenario: Blackout window must be underwritten
- **WHEN** a release migration declares its size-scaled blackout window
- **THEN** the declared window is computed from the gated bulk-egress throughput over the tenant corpus, not from an unmeasured estimate

### Requirement: Maintenance coexistence gate
A fleet-level coexistence gate SHALL run ingest, live query, and chat at target rates while all background materialization (compaction, projection and File-B builds, cube-cache maintenance, vector-index rebuilds, reclustering) is active under the maintenance governor across the fleet: live p99 latencies SHALL hold their individual gate bounds, ingest durability SHALL NOT degrade, and maintenance SHALL make measurable forward progress (no livelock). A build SHALL NOT ship if maintenance pressure breaks live bounds or live load starves maintenance indefinitely.

#### Scenario: Maintenance pressure breaks live latency
- **WHEN** the mixed-load gate runs with maintenance active and live query p99 exceeds its bound
- **THEN** the gate fails and the build does not ship

### Requirement: Accelerator enablement gates
An accelerator backend SHALL be used only when provider detection succeeds, self-test passes, software parity tests pass, input encoding is compatible, batch size exceeds the measured threshold, observed runtime benefit is positive, and a fallback is available; if any condition fails, the software path SHALL be used.

#### Scenario: Accelerator falls back on failed parity
- **WHEN** an accelerator backend fails software-parity tests
- **THEN** the software path is used for that operation

### Requirement: Acceptance gates are mechanically enforced by the committed harness
Every target number and pass/fail gate this capability defines SHALL be backed by a named benchmark in the committed harness, so a gate is satisfied only when its benchmark demonstrates it on the declared profile. No capability, default, accelerator backend, or fast path SHALL be marked ready on a gate that has no backing benchmark, and a capability whose committed baseline later misses its gate or regresses beyond tolerance SHALL drop from the ready set until the regression is resolved. Each accelerated path that declares observable equivalence to a portable path SHALL be benchmarked with both paths under the same workload, so the equivalence diff (identical durable bytes, offsets, extents, read results, and error taxonomy) and the performance benefit are established in one run. Benchmark results that establish a gate SHALL be committed as baselines together with their host/kernel/CPU profile. This requirement adds enforcement only; it changes none of the target numbers, matrices, or thresholds this capability already defines.

#### Scenario: Capability cannot be marked ready on an unmeasured gate
- **WHEN** a capability or fast path is proposed for the ready set but the gate it must meet has no backing benchmark in the committed harness
- **THEN** the readiness check fails, identifying the unmeasured gate

#### Scenario: Equivalence and benefit are measured together
- **WHEN** an accelerated fast path is benchmarked
- **THEN** its portable fallback is benchmarked under the same workload, the two results are compared for byte-for-byte equivalence, and the performance benefit is recorded against the committed baseline

#### Scenario: Regressed baseline drops a capability from the ready set
- **WHEN** a later harness run shows a previously-ready capability's baseline missing its gate or regressing beyond tolerance
- **THEN** the capability is removed from the ready set until the regression is resolved

### Requirement: Write-amplification gates per lifecycle stage
Write amplification is already a reported metric on the write-path matrix, but it SHALL also be a numeric pass/fail gate with a floor per lifecycle stage, because a low write-amplification factor (WAF) is the stated justification for FDP/ZNS placement and for splice-based rewrite, and an improvement that is only observed but never gated can silently regress. WAF SHALL be defined as bytes physically written to durable storage divided by the logical bytes the stage produces, and a numeric WAF ceiling SHALL be gated for at least two lifecycle stages: fresh publish (first durable write of a newly ingested HEF file or generation) and rewrite (compaction or SuperHEF re-publication that folds sidecars or merges files). A build SHALL NOT mark an FDP/ZNS placement path or a splice-based rewrite path ready unless its measured WAF at the relevant stage meets the gated ceiling, and a later baseline whose WAF regresses beyond tolerance SHALL drop that path from the ready set until resolved. The gate SHALL hold placement- and splice-driven improvements to a floor without changing any durable byte, checksum, ordering, or visibility outcome: WAF is a cost measurement over the same content the portable path would write.

#### Scenario: Rewrite path held to its write-amplification floor
- **WHEN** a splice-based rewrite path is benchmarked at the rewrite lifecycle stage
- **THEN** its measured WAF is compared against the gated ceiling for that stage, and the path is not marked ready if the WAF exceeds it

#### Scenario: Regressed publish WAF drops a placement path
- **WHEN** a later harness run shows a previously-ready FDP/ZNS publish path's WAF regressing beyond tolerance
- **THEN** the placement path is removed from the ready set until the regression is resolved, with durable bytes and checksums unchanged

### Requirement: Metadata-economics gates for cold opens and pruning
Metadata economics SHALL be gated as its own dimension, because footer fetches, cold-open requests, and planner metadata reads dominate object-store query cost yet are currently unmeasured. Three numeric gates SHALL be enforced. First, requests-per-cold-open SHALL target one object-store request to open a file (or to open every file of a manifest generation) on the `cold-cache/S3-durable` profile, the economics that manifest-native footer mirrors and single-request tail opens exist to deliver; a path SHALL NOT be marked ready on that profile if it issues more than the gated request count without a recorded justification. Second, footer bytes decoded per surviving granule SHALL be gated, so that a query decodes footer/marks bytes in proportion to the granules that survive pruning rather than the whole footer. Third, planner metadata bytes read per pruned-out file SHALL target approximately zero when lazy, per-stripe marks apply, so a file eliminated by pruning costs no granule-level marks bytes; a build SHALL NOT claim the lazy-marks economics unless the measured planner metadata bytes per pruned file meets that near-zero gate. Each gate SHALL be a cost measurement only, changing no query result, durable byte, checksum, pruning decision, or visibility outcome, and each SHALL be backed by a named benchmark in the committed harness on its declared profile.

#### Scenario: Cold open exceeds the one-request budget
- **WHEN** a cold open on the `cold-cache/S3-durable` profile is benchmarked and issues more than the gated requests-per-cold-open count without a recorded justification
- **THEN** the metadata-economics gate fails and the path is not marked ready on that profile

#### Scenario: Pruned file costs near-zero planner metadata
- **WHEN** a query prunes a file out during planning on a build claiming lazy per-stripe marks
- **THEN** the planner metadata bytes read for that file meet the near-zero gate, and the gate fails if the eliminated file still costs its full share of granule-level marks bytes

