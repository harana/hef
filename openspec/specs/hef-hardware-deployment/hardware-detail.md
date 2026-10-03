# HEF io_uring NVMe Deployment Model

Companion artifact for the `hef-hardware-deployment` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


The format has exactly one journal backend: io_uring. It has two paths — the NVMe primary path and a development-environment fallback.

### Default backend

```text
io_uring
io_uring_cmd NVMe passthrough via /dev/ngXnY as the primary write path for journal devices
filesystem io_uring retained as the non-NVMe dev environment fallback only (includes filesystem HEF output before object-store upload)
untorn HEJ frame writes claimed only when the startup NVMe Identify Namespace query confirms AWUN >= 1
  (AWUN/AWUPF-derived frame-atomicity claims on the passthrough path; RWF_ATOMIC on the dev fallback where the kernel accepts it;
   see the hef-physical-artifacts capability)
registered (fixed) buffers for HEJ frame buffers to avoid per-IO pinning and mapping
registered files for journal fds
registered ring buffers (IORING_REGISTER_PBUF_RING) for completion buffer selection across the 4 KiB-64 KiB HEJ frame-size range
SQPOLL and IOPOLL enabled by default on the NVMe path; disabled only if profiling shows the pinned polling thread costs more than the interrupt latency savings
linked SQEs (IOSQE_IO_LINK): NVMe write linked to NVMe flush cmd linked to sequence watermark update; the kernel auto-cancels the remainder of the chain on failure
DSM TRIM via io_uring_cmd NVMe passthrough issued async fire-and-forget after segment GC confirmation (segment sealed + HEF coverage confirmed);
  non-FDP namespaces only — on FDP devices placement handles already scope the GC signal
4 KiB-aligned buffers and offsets
worker-polled completions
object-store upload after HEF finalization
```

The backend is correct only if its durability barrier is explicit. On the passthrough path, acknowledgement must not precede completion of the linked NVMe flush command. On the dev fallback, when direct write semantics are unavailable, HEJ may still use buffered filesystem writes, but client acknowledgement must wait for fdatasync/fsync or an equivalent durability barrier. The dev fallback is not part of the low-latency benchmark claim.

Enterprise NVMe latency assumptions require power-loss-protected storage or replicated durability. On consumer SSDs without power-loss protection, local completion alone is not a sufficient durable acknowledgement.

There is no separate high-performance backend: `io_uring_cmd` passthrough achieves the device control SPDK was historically considered for — per-core rings, polled completions, raw LBA journal regions described by `JournalDeviceRegion` (see the hef-physical-artifacts capability) — without unbinding the device from the kernel NVMe driver.

### Deployment constraint: NVMe character device access

```text
the journal write path targets /dev/ngXnY (the NVMe generic character device), not a block device or filesystem;
io_uring_cmd passthrough on that node requires CAP_SYS_RAWIO or an equivalent capability grant;
container deployments must explicitly pass the device node through to the container;
startup refuses with a diagnostic naming the missing device node or capability when the configured
  deployment is NVMe and either is absent — the filesystem dev fallback is never silently substituted.
```

### Optional data-path accelerators

The file format allows runtime-selected acceleration below QueryEngine:

```text
QPL/IAA:
  compatible compression/decompression and filter preselection;

DSA:
  large buffer copy/fill, scan output assembly, cache refill movement;

QAT:
  approved compression or crypto only with software parity;

SIMD:
  default local acceleration for bitmap, dictionary, hash, and predicate kernels.
```

Accelerators are optimizations only. They define no operator config keys. They never change saved-record correctness, public output, query results, BLAKE3 verification, commit boundaries, TLS/mTLS policy, object-store visibility, or route semantics.

### NVMe data placement: FDP and ZNS

Journal and local-cache write amplification is reduced with device-level data placement when the device supports it:

```text
FDP (NVMe Flexible Data Placement, TP4146)
  probed exactly once at startup from the NVMe Identify Namespace data;
  asserted present or absent for the deployment; no runtime fallback or re-probe path;
  placement handles separate streams with different lifetimes:
    handle A: active HEJ segments (short-lived, recycled in order)
    handle B: sealed-awaiting-HEF-coverage HEJ segments
    handle C: local HEF cache blocks (see Local HEF cache below)
    handle D: temporary rewrite/spill output
  supported through io_uring_cmd NVMe passthrough only.

ZNS (zoned namespaces)
  optional stricter alternative for dedicated journal devices;
  HEJ segments map naturally to zones because segments are append-only and
  recycled whole; zone reset replaces segment erase accounting.

rules
  placement is a hint layer: data must remain correct and readable when the
  namespace lacks FDP support (recorded absent at startup);
  placement configuration is recorded per journal shard alongside
  AWUN/AWUPF untorn-write capability;
  acceptance: FDP/ZNS may be enabled by default only when measured device WAF or
  p99 write latency improves on the benchmark workload.
```

### End-to-end CRC-64/NVME prechecks

The platform already computes CRC-64/NVME once per file for its own fast admission precheck. The same value is verifiable for free at both ends of the format — the storage provider on upload and the NVMe device on read — as two more non-authoritative precheck layers slotted into existing capability-probe frameworks:

```text
nvme_pi (NVMe end-to-end protection information, device guard tag)
  probed at startup as one more store-file-layer capability-descriptor entry;
  reported present only when the namespace is PI-formatted with a CRC-64/NVME guard tag
    and the running kernel exposes the Linux 6.14 per-IO integrity attributes on io_uring
    read and write;
  where present, the block-file path attaches a CRC-64/NVME guard tag on each write and
    asks the device to verify the guard tag on each read, so a sector corrupted on the
    media is caught by the device before the bytes reach the reader;
  where absent — no PI format, a pre-6.14 kernel, or a non-NVMe host — the layer uses
    its portable no-guard-tag path, which is today's behavior.

provider CRC-64/NVME upload and download prechecks
  the object-store provider capability descriptor names the server-side checksum
    algorithms the provider supports;
  where a provider supports CRC-64/NVME (Amazon S3 via CRC64NVME), the checksum computed
    during the file's single streaming BLAKE3 pass is sent as the upload checksum so the
    provider verifies the bytes server-side at commit and rejects a corrupted commit before
    the object becomes visible;
  on download, a provider-returned CRC-64/NVME checksum is evaluated as a precheck before
    the authoritative BLAKE3 check;
  a provider with a different native checksum (GCS CRC32C, Azure) or none (the
    local-directory default) degrades to that algorithm or sends none, exactly as today.

rules
  PI is hint-only, mirroring "NVMe data placement is a hint layer" above: it is never
    required for correctness, defines no operator config key, and a PI absence, format
    mismatch, or device verification failure falls back to the portable path with
    identical durable bytes;
  the provider checksum is likewise non-authoritative and keyless; it follows the
    provider descriptor, not an operator config key;
  both new prechecks may reject bytes early but never admit bytes BLAKE3 would reject —
    BLAKE3 remains the sole admission authority in both the device and provider
    directions — and no consumer serves a byte that has not passed BLAKE3 verification;
  neither precheck introduces a new on-disk HEF file shape: guard tags live in the
    device's protection-information area, not the file bytes, and the provider checksum
    is transport/commit metadata, not stored file content.
```

### Storage-path preallocation and large block size

Write-once storage paths materialise their extents ahead of the write where the host supports it:

```text
extent preallocation (fallocate write-zeroes mode, Linux 6.17+)
  capability-probed once at startup against a staging temp file; recorded in the shared file
  layer's capability descriptor beside atomic-write/direct-I/O/large-block, and mirrored as an
  object-store fast-path entry;
  where present, a write-once extent is preallocated and zeroed in one call before it is filled
  — a staged whole file of known size, or a journal segment ahead of the append head on the
  filesystem block path — so later writes land in already-materialised blocks;
  where absent, the portable fallback is plain allocation-on-write, byte-identical;
  preallocated-but-unwritten space is never written extent, never readable, never durable, so a
  zeroed preallocated tail never decodes as a frame and torn-tail recovery is unchanged;
  the NVMe character-device passthrough journal path has no file and is untouched.

large block size (filesystem block size above the page size)
  filesystem-neutral: the same statx-based probe covers any mounted filesystem reporting a block
  size above the page size — XFS on Linux 6.12+, EXT4 on Linux 6.19+ — so a capable EXT4 host
  takes the same fast path a capable XFS host does, with the same portable fallback and no new
  capability bit.
```

### Local HEF cache

The local HEF granule/footer cache is a hybrid memory-plus-disk cache:

```text
design
  foyer-style hybrid cache (in-memory tier + local NVMe disk tier) keyed by
  (file_id, block kind, block range);
  memory tier holds hot footers, marks, dictionaries, filters, and small granules;
  disk tier holds recently read granule ranges and rewrite inputs.

rules
  the cache is rebuildable acceleration state; object storage plus manifest is
  authoritative;
  cached blocks are verified by their HEF block checksums on admission;
  tenant isolation and data-class labels apply to cache keys and eviction;
  single-subject encrypted blocks are cached only in encrypted form so
  crypto-shredding remains effective;
  the disk tier may use FDP placement handle C, but placement is not required;
  an operator may additionally place the disk tier's filesystem — or any node-local
  cache-tier volume — on an operator-managed persistent block cache underneath the mount
  (a device-mapper target such as dm-pcache, Linux 6.18+); that substrate is deployment
  infrastructure, not an in-process accelerator: the platform never probes for it,
  configures it, or behaves differently because of it, and the cache contract is identical
  with or without it; a lost, degraded, or removed substrate loses nothing that cannot be
  rebuilt from object storage plus the manifest.
```

### Memory placement and host baseline (Linux 7.1+ target)

Memory placement is tuned within the RAM budget and never changes results; the 7.1+ line also brings free wins that are assumed, never depended on:

```text
mTHP (multi-size transparent huge pages, Linux 6.8+) and
weighted-interleave NUMA memory policy (Linux 6.16+)
  each capability-probed at startup via sysfs (enabled mTHP sizes; weighted-interleave
    support; NUMA node count), surfaced only through diagnostics/metrics — no operator config key;
  the engine may advise mTHP on its large sort/aggregate/hash working-set allocations and set a
    weighted-interleave policy on a multi-socket node where probed, falling back to base pages and
    the default policy otherwise, with byte-identical results;
  per-NUMA-node proactive reclaim (Linux 6.17+) is host-level operator tooling the platform
    tolerates but never requires, drives, or reacts to;
  placement changes where pages live and how fast they are reached, never how many bytes Automatic
    RAM Budgeting accounts or when pools shed or spill.

host-baseline free wins (assumptions, not dependencies)
  Intel FRED (default-on in 7.1 on FRED-capable CPUs) lowers syscall/interrupt/exception entry and
    return cost; the io_uring-heavy engine inherits it automatically;
  vDSO getrandom() (Linux 6.11+) serves random bytes without a per-call syscall; the live randomness
    interface obtains bytes through the standard getrandom path, so high-rate ULID, session-token,
    and nonce generation inherits the vDSO fast path automatically, while deterministic simulation
    keeps injecting its own randomness;
  neither feature is a probe, config key, health signal, or startup check: a pre-FRED CPU or an
    older kernel runs with identical results, identifiers, and durable bytes, only slower;
  (EEVDF scheduling and AccECN congestion signalling are recorded with the OS-scheduling and
    transport-networking work, not here.)
```

---
