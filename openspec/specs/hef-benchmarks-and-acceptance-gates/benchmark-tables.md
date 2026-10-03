# HEF Benchmarks and Acceptance Gates

Companion artifact for the `hef-benchmarks-and-acceptance-gates` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


### Write benchmarks

Compare:

```text
filesystem io_uring HEJ autonomous flush, 4 KiB target
filesystem io_uring HEJ autonomous flush, 16 KiB target
io_uring_cmd NVMe passthrough HEJ autonomous flush, 4 KiB target
io_uring_cmd NVMe passthrough HEJ autonomous flush, 16 KiB target
internal centralized group-commit prototype
internal flush-pipelining prototype
filesystem-only reference path
HEJ -> HEF minimum required feature set
HEJ -> HEF with selected optional feature blocks
HEF rewrite/repack with selected optional feature blocks
```

Matrix:

```text
flush size:       4 KiB, 8 KiB, 16 KiB, 32 KiB, 64 KiB, 128 KiB negative-control only
workers:          1, 2, 4, 8, 16, 32, core-count, hardware-thread-count
payload size:     160 B, 512 B, 2 KiB, 8 KiB
queue depth:      1, 4, 16, 64
ack group size:   1, 2, 4, 8
log stealing:     disabled, topology-local enabled
force commit:     disabled negative-control only, enabled
load:             steady, bursty, idle trickle, open-loop
replication:      local, async replica, quorum
storage backend:  buffered filesystem (dev fallback), filesystem io_uring, io_uring_cmd NVMe passthrough
storage class:    enterprise NVMe with PLP, non-PLP SSD negative-control only
```

Metrics:

```text
ack latency p50/p90/p99/p999
READY-to-HARDENED latency
HARDENED-to-COMMITTED latency
journal write latency
commit queue waiting time
autonomous acknowledgement time
force-commit rate
log-steal attempt/success rate
stolen bytes per frame
out-of-order steal completion stalls
throughput per core
journal replay speed
safe-retry reconstruction time
HEF publication lag
manifest publish lag
CPU per million events
bytes written per event
write amplification
```

### Query benchmarks

Required query classes:

```text
time range count
time bucket count
source/type filtered count
source/type/entity sparse cube count
entity timeline
account timeline
opportunity timeline
metadata-only revenue aggregate
PreparedView revenue metric query
HEF-plus-LiveOverlay dashboard aggregate
HEF-plus-LiveOverlay entity timeline
payload predicate fallback scan
shredded payload-field filter and projection
residual variant single-path extraction
late event query with old occurred_at and new epoch/sequence
deletion-vector-heavy query
correction/latest-view query
chat/investigation context packet retrieval
text token search over selected summary fields
internal semantic retrieval using optional embedding/vector blocks
large payload late materialization
```

Reference paths:

```text
HEF + QueryEngine warm cache
HEF + QueryEngine cold-cache/NVMe-durable
HEF + QueryEngine cold-cache/S3-durable
HEF aggregate shortcut path
HEF + LiveOverlay fresh query path
PreparedView materialized path
```

Metrics:

```text
files pruned
footers opened
stripes pruned
granules pruned
pages pruned
marks range reads avoided
LiveOverlay segments pruned
LiveOverlay rows scanned
bytes read
payload bytes avoided
context payload scans avoided
metadata-only hit rate
HEF aggregate shortcut hit rate
HEF-plus-LiveOverlay aggregate shortcut hit rate
PreparedView hit rate
probabilistic filter false-positive rate
range filter false-positive rate and granule-prune improvement over min/max
Ribbon/split-block Bloom/binary fuse bytes per key
local HEF cache hit rate (memory tier, disk tier)
PreparedView incremental-maintenance lag behind manifest generation
manifest publication CAS retry rate
bitmap intersection time
rows/sec/core
p95 dashboard query latency
p95 entity timeline latency
p95 fresh query latency
p95 chat context packet latency
p95 investigation evidence retrieval latency
p99 LiveOverlay publish latency
p99 visibility_watermark lag behind commit_watermark
CPU per query
memory peak per query
```

### Acceptance gates

Latency-paper alignment gates:

```text
The HEJ acknowledgement hot path must not use a centralized group-commit writer.
The HEJ acknowledgement hot path must not depend on a single global background acknowledgement thread.
The implementation must benchmark 4 KiB and 16 KiB autonomous flush targets separately.
The 16 KiB target must be the default only if it preserves low p99 acknowledgement latency while sustaining required throughput.
The 4 KiB target must be available for latency-critical and low-load force-commit paths.
A 128 KiB or larger normal flush unit may be benchmarked only as a negative control and must not be selected for normal HEJ frames.
Topology-local log stealing must either improve p90/p99 acknowledgement latency under small-record workloads or remain disabled.
Force commit must keep idle-trickle p99 acknowledgement latency within the freshness target without creating periodic write bursts.
A single stalled or crashed ingest worker must not stall a tenant's commit_watermark beyond the reservation-lease deadline; the void-record path must close an abandoned reservation range within that bound.
Append-only event ingest must not pay dependency-tracking overhead for nonexistent page-level dependencies.
Routes with real transactional dependencies must prove autonomous acknowledgement correctness before enabling dependent acknowledgements.
Buffered-filesystem fallback must be reported separately from NVMe-passthrough/direct-I/O latency results.
AWUPF atomicity claims must be enabled only for shards where probing proves support.
RWF_ATOMIC untorn-write claims must be enabled only when the startup NVMe Identify Namespace query confirms AWUN >= 1 and the kernel accepts the atomic submission; rejected atomic submissions must fall back without losing the frame.
FDP/ZNS placement must be enabled by default only when measured device write amplification or p99 write latency improves on the benchmark workload; correctness must hold when placement is absent.
Range-filter SkipIndexes must be written only where measured granule-prune improvement over min/max pruning exceeds their metadata cost.
Incremental PreparedView maintenance must produce byte-identical or value-identical output to full recomputation on the equivalence test suite before a view class is marked ready.
Segment recycling must not advance before HEF coverage, recovery safety, and segment_chain_blake3 retention rules are satisfied.
```

Target numbers:

```text
HEJ commit p99 latency, single-node PCIe Gen4 NVMe, 4 KiB frame: < 50 microseconds.
HEJ throughput per core at 16 KiB frame: > 250k events/sec.
HEF numeric-column scan speed on AVX-512-capable host: > 10 GB/s/core decompressed.
HEF entity_id point lookup, warm (footer + marks resident in local HEF cache): < 5 ms when granule + marks pruning applies.
HEF entity_id point lookup, cold-cache/NVMe-durable (footer + marks served from durable node-local NVMe metadata tier): < 5 ms when granule + marks pruning applies.
HEF entity_id point lookup, cold-cache/S3-durable (footer + marks fetched from object-store durable tier): at most 2 dependent round trips plus a budgeted per-request millisecond allowance; no sub-5 ms figure is claimed on this profile.
HEF size ratio versus Parquet+Zstd reference: 0.6x-0.7x for representative event workloads.
BLAKE3 verify throughput: > 5 GB/s/core on AVX-512-capable host.
Compact layout overhead for < 10 MiB files: 30%-50% smaller than equivalent Wide layout.
```

Do not mark a query class ready unless:

```text
HEF + QueryEngine meets the acceptance target for that query class
under the warm, cold-cache/NVMe-durable, and cold-cache/S3-durable profiles that apply,
each measured separately, or the query class is explicitly excluded from the ready set.
```

Write-amplification and metadata-economics gates:

```text
Write-amplification factor (durable bytes written / logical bytes produced) at the fresh-publish stage must meet the ceiling recorded as its committed baseline; an FDP/ZNS placement path is not marked ready otherwise, and a regressed baseline drops it from the ready set. Backing benchmark: fresh-publish WAF benchmark.
Write-amplification factor at the rewrite stage must meet the ceiling recorded as its committed baseline; a splice-based rewrite path is not marked ready otherwise, and a regressed baseline drops it from the ready set. Backing benchmark: rewrite WAF benchmark.
Requests per cold open on the cold-cache/S3-durable profile must target 1; a path issuing more than the gated count without a recorded justification is not marked ready on that profile. Backing benchmark: cold-cache/S3-durable requests-per-cold-open benchmark.
Footer bytes decoded per surviving granule must stay proportional to the granules that survive pruning, not the whole footer. Backing benchmark: footer-bytes-per-surviving-granule benchmark.
Planner metadata bytes read per pruned-out file must target ~0 under lazy per-stripe marks; a build claiming lazy-marks economics that still costs its full granule-level marks share fails the gate. Backing benchmark: planner-bytes-per-pruned-file benchmark.
```

Real-time query acceptance gates:

```text
LiveOverlay size:
  p95 LiveOverlay rows scanned per dashboard query must stay below the benchmark target.

Visibility lag:
  p99 visibility_watermark lag behind commit_watermark must stay below the freshness target.

HEF publication lag:
  p99 durable-to-HEF-published lag must stay below the HEF publication target.

Duplicate safety:
  no event_id/(epoch, sequence) may be returned from both HEF and LiveOverlay in the same query snapshot.

Granule pruning:
  granule directory and marks pruning must reduce object-store range reads for selective predicates versus stripe-only pruning.

Late-event safety:
  occurred_at queries must include LiveOverlay rows by snapshot_watermark, not by recency-only heuristics.

Aggregate correctness:
  exact aggregate = exact HEF aggregate + exact LiveOverlay delta - exact deletion-vector/correction effects.

Freshness fallback:
  if LiveOverlay is missing after restart, the system must rebuild from HEJ before serving fresh queries,
  or serve only an explicit bounded-staleness/manifest-only query mode.

Public-output safety:
  blocked internal fields must not appear in public route, chat, investigation, export, or support outputs.
```

### Accelerator gates

An accelerator backend may be used only when:

```text
provider detection succeeds;
self-test passes;
software parity tests pass;
input encoding is compatible;
batch size exceeds measured threshold;
observed runtime benefit is positive;
fallback is available.
```

If any condition fails, the software path is used.

---

