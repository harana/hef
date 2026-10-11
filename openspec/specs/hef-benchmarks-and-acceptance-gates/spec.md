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

### Requirement: Log object framing and streaming gate
Write-ahead log objects SHALL be gated on their fixed framing and their verified forward read. The header plus trailer of a log object SHALL be at most 32 bytes, whatever its record count. A verified forward read SHALL hash at most one 1 MiB chunk group before returning the first record, and no more bytes than the object holds by the time it has returned every record.

#### Scenario: Log object passes the framing and streaming gate
- **WHEN** a log object is written and read forward against its pinned root
- **THEN** its measured header plus trailer bytes, the bytes hashed before its first record, and the bytes hashed for the whole read are within the gate, and the gate fails when any of them is not

### Requirement: Graph and time-series profile gates
Every block the graph and time-series profiles add SHALL be held to a gate in `benchmarks.rs`, evaluated by a committed test that measures it, with the measured value recorded here beside the gate. A graph file's event-id index SHALL cost at most 12 footer bytes per indexed event, its filter included, and the filter SHALL admit at most 2% of absent ids. Chain-cover labels SHALL add at most 4 edge-block bytes each (half their raw 8), and the chain-cover footer section SHALL cost at most 8 bytes per link (half its raw 16), chain ranges included. State events in a 100,000-member room SHALL write at most 16 KiB of new state chunks each on average. A lookup of one key in the state at an event (its state root, then the key) SHALL fetch at most one chunk per state-tree level when its chunk cache is empty, and SHALL spend at most 1 ms at the 99th percentile on its own work, beyond its object-store round trips when cold and in all when warm. Replaying a 100,000-member room's join history into state trees and graph files SHALL write at most 16 KiB of new state chunks per event, with its wall time and bytes written recorded. Chimp float blocks SHALL store a slowly varying full-precision gauge in fewer bytes than every earlier float encoding and in at most 3.5 bytes per value. A time-series aggregate over whole rollup buckets SHALL read no column block, and one whose range cuts a bucket at each end SHALL read at most 8, fewer than scanning the range. The measurements that build 100,000-event inputs in full (state lookups, state-chunk writes and the room replay) SHALL run as ignored release-mode tests, run by hand when the gates are re-baselined; the others run with every test run.

#### Scenario: The event-id index stays small and selective
- **WHEN** 100,000 protocol ids are indexed and 200,000 absent ids are probed against the filter
- **THEN** the section costs at most 12 bytes per event and the filter admits at most 2% of the absent ids (measured: 10.01 bytes per event, 1.23 of them filter; 0.70% admitted)

#### Scenario: Chain cover stays compact
- **WHEN** the room-shaped graph carries a chain cover of 33 chains, one label per event
- **THEN** labels add at most 4 bytes each and the section at most 8 bytes per link (measured: 1.76 bytes per label; 37,654 links in 114,323 bytes, 3.04 per link)

#### Scenario: State events write little
- **WHEN** a 100,000-member room takes 1,024 state events after its initial state
- **THEN** the chunks new to each event's tree average at most 16 KiB (measured: 13,878 bytes and 3.00 chunks per event)

#### Scenario: State at an event is read in one fetch per level
- **WHEN** 1,000 lookups of a random member at a random one of those state events run through the graph's state reader, each cold against an empty chunk cache with a simulated 20 ms round trip per fetch and then warm against the cache it filled
- **THEN** a cold lookup fetches at most one chunk per tree level and both percentiles stay within 1 ms of local work (measured: at most 3 fetches over 3 levels, cold p99 60.073 ms, warm p99 63 us)

#### Scenario: Room replay recorded
- **WHEN** a 100,000-member room's join history (create, power levels, then one join per member, each a state event with its own root) is replayed into full state-tree builds and graph files of 16,384 events
- **THEN** its new state chunks average at most 16 KiB per event, and its wall time and bytes written are recorded (measured: 100,002 events in 234,229 ms; 9,650,717 bytes of graph files and 820,149,980 bytes of new state chunks, 8,201 per event; the time is dominated by the full state-tree build each event takes, so it grows with the square of the member count)

#### Scenario: XOR floats beat earlier encodings
- **WHEN** 8192 readings of a memory-utilization gauge are stored as a time-series float block
- **THEN** Chimp stores them in fewer bytes than every earlier float encoding and in at most 3.5 bytes per value (measured: 2.957 bytes per value, against 3.067 for the best earlier encoding)

#### Scenario: Rollups answer without scanning
- **WHEN** a 2,000-reading time-series file with one-second rollups is aggregated over whole buckets, over a range cutting a bucket at each end, and over that range by scanning
- **THEN** whole buckets read no column block and the cut range at most 8, fewer than the scan (measured: 0, 4 and 62 blocks)

### Requirement: Audit file overhead gate
An audit file SHALL add at most 192 bytes over an event file of the same rows, all of them in its footer (the profile and audit sections and their directory entries), and SHALL add exactly as much at many rows as at few, so the hash chain stores no bytes per row. The cost of hashing the chain at build time and the throughput of verifying a run of audit files SHALL be reported by a release-mode benchmark.

#### Scenario: Audit file passes the overhead gate
- **WHEN** the same rows are built as an event file and as an audit file, at a small and a large row count, with and without a retention
- **THEN** the audit file's added bytes are within the ceiling and equal at both row counts, and the gate fails when either is not

### Requirement: Snapshot lookup gate
A cold point lookup in a snapshot file opened from an object store SHALL issue at most two requests, one for the file's tail and one for the granule that can hold the key, whether the key is present or not, and SHALL fetch less than the whole file. Lookup time, epoch build time, and epoch publish time SHALL be reported by release-mode benchmarks against the per-partition archive a snapshot file replaces.

#### Scenario: Snapshot lookup passes the request gate
- **WHEN** a snapshot file is opened remotely and a stored or absent key is looked up
- **THEN** the open and the lookup together issue at most two requests and fetch fewer bytes than the file holds, and the gate fails when either is not so

### Requirement: Inbox read gate
An inbox file's recipient directory SHALL cost 48 bytes per recipient plus at most 64 bytes of framing, and reading one recipient's queue from a file with the directory SHALL touch no granule beyond those holding that recipient's rows. Queue read latency with and without the directory, the fully-acknowledged check, and build time SHALL be reported by a release-mode benchmark over at least 100,000 rows across 10,000 recipients.

#### Scenario: Inbox read passes the directory gate
- **WHEN** each recipient's queue is read from an inbox file of many recipients in small granules
- **THEN** the directory's footer bytes are within the per-recipient cost plus framing, and the granules read for each recipient are exactly those holding its rows, and the gate fails when either is not so

### Requirement: File profile overhead gate
Declaring a file profile other than `Event` SHALL cost at most 64 bytes over the same rows built as an event file: one footer directory entry and its one-byte profile section, with the 4 KiB header block no larger. An event file SHALL carry no profile section, so its footer stays byte-identical to one written before profiles existed.

#### Scenario: Graph profile with no edges stays within the overhead ceiling
- **WHEN** the same rows are built once as an event file and once as a graph-profile file with no edges
- **THEN** the event file's footer holds no profile section, both header blocks are 4096 bytes, and the graph file is at most 64 bytes larger, and the gate fails when any of them is not

### Requirement: Graph edge block size and throughput
A graph-profile file's edge blocks SHALL store at most 6 bytes per edge, counting the forward and reverse copies of a parent link against the one edge. The committed harness SHALL measure this on a deterministic 100,000-event room-shaped graph in 8192-row granules, and SHALL carry a release-mode benchmark that reports, for the same graph, edges per second and megabytes per second for building every granule's edge block and for decoding them all.

#### Scenario: Room graph stays within the edge size ceiling
- **WHEN** the 100,000-event room graph's edge blocks are built
- **THEN** their stored bytes divided by the edge count are at most 6, and the gate fails when they are not

### Requirement: Graph walk request gate
A breadth-first walk over a subject's graph spread across several remote files SHALL send at most one range request per file per level, every edge block a level needs from one file joined into a single request. The committed harness SHALL measure levels, files, requests, and bytes fetched for a walk over a multi-file graph, and SHALL carry a release-mode benchmark over a room of at least 100,000 events split across several files that reports requests, bytes fetched, and time per level for a cold walk and for a second walk over the same graph, which SHALL send no request.

#### Scenario: Walk across four files meets the request gate
- **WHEN** an event's ancestors are walked level by level across four remote graph files
- **THEN** the requests sent are at most the number of levels times the number of files, and the gate fails when they are not
