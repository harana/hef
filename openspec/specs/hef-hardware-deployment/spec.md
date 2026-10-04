## Purpose

Defines how the HEF journal is stored on hardware and the deployment constraints. The deployment target is local NVMe driven entirely through io_uring:

- The io_uring storage backend with explicit durability barriers: `io_uring_cmd` NVMe passthrough via the NVMe character device as the primary write path, with filesystem io_uring retained only as a non-NVMe development-environment fallback.
- The deployment constraints the passthrough path creates (device-node access, capability grants).
- Optional data-path accelerators that may speed things up but must never change results.

The concrete io_uring/NVMe/accelerator deployment detail is embedded in [hardware-detail.md](hardware-detail.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-hardware-deployment/spec.md).
## Requirements
### Requirement: Default io_uring backend with explicit durability
The journal backend SHALL be io_uring, with `io_uring_cmd` NVMe passthrough via the NVMe character device (`/dev/ngXnY`) as the primary write path for journal devices. Filesystem io_uring SHALL be retained only as the non-NVMe development-environment fallback, and filesystem HEF output SHALL exist only on that fallback path; production HEF output is staged for object-store upload after finalization. The backend SHALL use: untorn HEJ frame writes claimed only when the startup NVMe Identify Namespace query confirms `AWUN >= 1` (AWUN/AWUPF-derived frame-atomicity claims on the passthrough path; RWF_ATOMIC submission on the dev fallback where the kernel accepts it); registered (fixed) buffers for HEJ frame buffers and registered files for journal fds; registered ring buffers (`IORING_REGISTER_PBUF_RING`) for completion buffer selection across the 4 KiB–64 KiB HEJ frame-size range; SQPOLL and IOPOLL enabled by default on the NVMe path, disabled only where profiling shows the pinned polling thread costs more than the interrupt-latency savings; linked SQEs (`IOSQE_IO_LINK`) chaining NVMe write → NVMe flush → sequence-watermark update, with the kernel auto-cancelling the remainder of the chain when an earlier link fails; DSM TRIM (deallocate) issued via `io_uring_cmd` passthrough as asynchronous fire-and-forget after segment GC confirmation (segment sealed and HEF coverage confirmed) on non-FDP namespaces; 4 KiB-aligned buffers and offsets; worker-polled completions; and object-store upload after HEF finalization. The backend SHALL be correct only with an explicit durability barrier: on the passthrough path acknowledgement SHALL NOT precede completion of the linked NVMe flush; on the dev fallback, when direct write semantics are unavailable, HEJ MAY use buffered filesystem writes but client acknowledgement SHALL wait for fdatasync/fsync or an equivalent barrier (outside the low-latency benchmark claim). On consumer SSDs without power-loss protection, local completion alone SHALL NOT be a sufficient durable acknowledgement.

#### Scenario: Buffered fallback waits for fsync
- **WHEN** a non-NVMe development environment uses the filesystem fallback and HEJ uses buffered writes
- **THEN** client acknowledgement waits for fdatasync/fsync or an equivalent durability barrier

#### Scenario: Consumer SSD without power-loss protection
- **WHEN** the journal device lacks power-loss protection
- **THEN** local write completion alone is not treated as durable acknowledgement (replication or PLP storage is required)

#### Scenario: Linked chain cancelled on write failure
- **WHEN** the NVMe write at the head of a linked write → flush → sequence-watermark chain fails
- **THEN** the kernel auto-cancels the remaining links, the sequence watermark does not advance, and the frame is not acknowledged

#### Scenario: TRIM only after segment GC confirmation
- **WHEN** segment GC confirms a recyclable segment is sealed with every frame HEF-covered on a non-FDP namespace
- **THEN** a DSM TRIM for the segment's range is issued via `io_uring_cmd` passthrough fire-and-forget, and on FDP namespaces no TRIM is issued because placement handles already scope the GC signal

### Requirement: NVMe character-device deployment constraint
The primary journal write path SHALL target the NVMe character device (`/dev/ngXnY`) via `io_uring_cmd` passthrough, which requires `CAP_SYS_RAWIO` or an equivalent capability grant for the product process. Container deployments SHALL explicitly pass the device node through to the container. When the configured deployment is NVMe and the device node or the required capability is missing, startup SHALL refuse with a diagnostic naming the missing device node or capability; the filesystem dev fallback SHALL NOT be silently substituted for the NVMe path.

#### Scenario: Container missing device node or capability
- **WHEN** a production NVMe deployment starts in a container without `/dev/ngXnY` passed through or without `CAP_SYS_RAWIO`
- **THEN** startup fails with a diagnostic naming the missing device node or capability and does not silently fall back to the filesystem path

### Requirement: Accelerators never change correctness
Optional data-path accelerators (QPL/IAA, DSA, QAT, SIMD) SHALL be optimizations only, SHALL define no operator config keys, and SHALL never change saved-record correctness, public output, query results, BLAKE3 verification, commit boundaries, TLS/mTLS policy, object-store visibility, or route semantics; accelerator absence, health failure, incompatibility, or slower-than-software selection SHALL fall back to the software path with identical results (INV-HARDWARE-ACCEL).

#### Scenario: Accelerator path matches software path
- **WHEN** an accelerator is used for compression or filtering
- **THEN** the produced records, query results, and BLAKE3 verification are identical to the software path

### Requirement: NVMe data placement is a hint layer
Journal and local-cache write amplification MAY be reduced with device-level data placement (NVMe FDP/TP4146 with separate placement handles per lifetime, or ZNS zoned namespaces for dedicated journal devices). FDP presence SHALL be probed exactly once at startup from the NVMe Identify Namespace data and asserted present or absent for the deployment; there SHALL be no runtime fallback or re-probe path. Placement on the NVMe character-device journal path SHALL be supported through `io_uring_cmd` NVMe passthrough only. On hosts where the kernel wires NVMe FDP to block-layer write streams (Linux 6.16+ — per-inode lifetime hints and per-IO write-stream hints through the io_uring SQE), placement MAY additionally be requested on the filesystem path for the foyer disk-cache files and staged HEF parts that live on a filesystem rather than the character device, using those write-stream hints; this filesystem write-stream path SHALL be probed independently of the character-device FDP probe, SHALL remain hint-only, and SHALL change no durable byte, checksum, ordering, visibility, or query result — it only groups physically like-lifetime writes for the device. Placement SHALL remain a hint layer: data SHALL remain correct and readable when the namespace lacks FDP support or the host lacks write-stream support (recorded absent at startup), placement configuration SHALL be recorded per journal shard alongside AWUN/AWUPF untorn-write capability, and FDP/ZNS SHALL be enabled by default only when measured device write amplification or p99 write latency improves on the benchmark workload. No placement path — character-device or filesystem write-stream — SHALL become a correctness or startup dependency.

#### Scenario: FDP absent at startup
- **WHEN** the startup Identify Namespace probe records FDP as absent
- **THEN** writes proceed without placement handles and remain correct and readable, and no runtime fallback or re-probe path executes

#### Scenario: Write-stream placement absent on the filesystem path
- **WHEN** the host does not expose block-layer write streams for the volume holding the foyer disk-cache files or staged HEF parts
- **THEN** those files are written without write-stream hints and remain correct and readable, with identical durable bytes, checksums, and query results

### Requirement: Rebuildable hybrid local HEF cache
The local HEF granule/footer cache SHALL be a foyer-style hybrid cache (in-memory tier plus local NVMe disk tier) keyed by `(file_id, block kind, block range)`. It SHALL be rebuildable acceleration state with object storage plus the manifest authoritative; cached blocks SHALL be verified by their HEF block checksums on admission; tenant isolation and data-class labels SHALL apply to cache keys and eviction; and single-subject encrypted blocks SHALL be cached only in encrypted form so crypto-shredding remains effective. The disk tier MAY use FDP placement, but placement SHALL NOT be required. An operator MAY additionally place the disk tier's filesystem — or any node-local cache-tier volume — on an operator-managed persistent block cache underneath it (a device-mapper target such as dm-pcache, Linux 6.18+). That substrate is deployment infrastructure, not an in-process accelerator: the platform SHALL NOT probe for it, configure it, or behave differently because of it, and the cache contract SHALL be identical with or without it — the cache remains rebuildable acceleration state with object storage plus the manifest authoritative, and admission verification, tenant isolation, data-class labels, encrypted-form caching, and eviction are unchanged. A lost, degraded, or removed substrate SHALL lose nothing that cannot be rebuilt from object storage plus the manifest.

The cache SHALL sit between a remote reader's range source and the decoder and SHALL be keyed by `(tenant, file_id, block kind, offset, length)`, where block kind distinguishes a file's tail (footer region plus proof appendix) from a proof-aligned stripe range. A piece SHALL be admitted only after it is proven (a tail against the manifest seal, a stripe range against its authenticated stripe root), SHALL be proven again every time it is served from either tier, and a piece that fails SHALL be dropped from both tiers and fetched again from object storage. The cache SHALL hold only the object's stored bytes, never a decrypted form. The memory tier SHALL be byte-budgeted with least-recently-used eviction; the disk tier SHALL be an interface the deploying application backs, and a disk-tier failure SHALL be a cache miss, never a read failure. When a node loads a generation's footer mirror, it SHALL seed the cache with every mirrored tail first, so opening those files issues no tail request of their own.

#### Scenario: Second open is served from the cache
- **WHEN** a second reader on the node opens and reads a file another reader already read through the same cache
- **THEN** it issues no range request to object storage and returns the same results

#### Scenario: Corrupted cached piece is rejected and refetched
- **WHEN** a cached tail or stripe range no longer proves against the seal or its stripe root
- **THEN** the reader drops it from both tiers, fetches it again from object storage, serves the proven bytes, and re-admits them

#### Scenario: Eviction never changes results
- **WHEN** reads run through a cache whose memory budget is smaller than the file
- **THEN** every result equals the in-memory reader's and the memory tier stays within its budget

#### Scenario: Encrypted block stays encrypted in cache
- **WHEN** a single-subject encrypted block is admitted to the local HEF cache
- **THEN** it is cached only in encrypted form so destroying the subject's content key still renders it unrecoverable

#### Scenario: Cache contract is identical on an operator-managed block-cache substrate
- **WHEN** an operator places the disk tier's filesystem on a dm-pcache device-mapper target and the substrate is later lost or removed
- **THEN** cache admission verification, tenant isolation, encrypted-form caching, and eviction behaved identically while it was present, the platform never probed for or configured the substrate, and the cache is rebuilt from object storage plus the manifest with nothing lost

### Requirement: NVMe passthrough operation set is provided behind the synchronous interface
The journal write path's previously-blocked operations — `io_uring_cmd` NVMe passthrough append, the linked write→flush→sequence-watermark submission, polled completions, registered fixed buffers, and post-GC DSM TRIM — SHALL be provided through the sanctioned io_uring operations crate behind the unchanged synchronous `JournalStorage` interface, which keeps owning the asynchronous edge internally. Append SHALL write from a registered pooled buffer rather than allocating and copying a fresh buffer per frame. Taking these fast paths SHALL be observably equivalent to the buffered portable fallback: the durable journal bytes, the frame offsets returned by append, the shard extent, the bytes returned by read, and the error taxonomy SHALL be identical either way, with only performance differing, and BLAKE3 SHALL remain the authoritative integrity check on every path. On hosts without the NVMe character device — including non-NVMe Linux and macOS development hosts — the backend SHALL use the buffered filesystem fallback and SHALL NOT fail to build, start, or pass tests. Startup SHALL refuse only where the deployment declares NVMe-required and the character device or its capability grant (`CAP_SYS_RAWIO` or an equivalent, plus device-node passthrough in containers) is absent; it SHALL NOT silently fall back in that case. This requirement implements the operation set that `nvme-only-io-uring-backend` specified and left blocked at the interface (design D3); it does not change that change's device-probe or NVMe-only decisions.

#### Scenario: Accelerated and buffered journal paths reach the same result
- **WHEN** the same sequence of frames is appended, synced, and read back through the NVMe passthrough path on one host and through the buffered fallback on another
- **THEN** the durable bytes, frame offsets, shard extents, read results, and error taxonomy are identical, BLAKE3 verification is unchanged, and only latency differs

#### Scenario: Development host falls back without breaking the build
- **WHEN** the workspace is built and tested on a macOS aarch64 host or a Linux host with no NVMe character device
- **THEN** the passthrough operations are feature-gated off, the buffered filesystem fallback is used, and the build and test suite pass

#### Scenario: NVMe-required deployment refuses when the device is absent
- **WHEN** a deployment declares the NVMe passthrough path required but the character device or its capability grant is unavailable at startup
- **THEN** startup refuses with a diagnostic rather than silently falling back to the buffered path

### Requirement: Kernel transport baseline — AccECN assumed, BIG TCP provisioned and probed
Two kernel TCP improvements SHALL be treated as facts about the host, not as platform features. **AccECN** — accurate explicit congestion notification, which gives TCP congestion control a finer-grained signal and is on by default from kernel 7.0 — SHALL be a baseline assumption of the Linux 7.1+ deployment target: the platform SHALL ship no code and define no operator config key for it, and no saved record, query result, BLAKE3 verification, commit boundary, TLS/mTLS policy, or route semantic SHALL depend on it; a host with AccECN absent or disabled SHALL differ only in transport performance. **BIG TCP** — GSO/GRO super-segments larger than 64 KiB that cut per-packet CPU on high-throughput TCP paths, available for IPv6 and, from kernel 6.3, for IPv4 — SHALL be a host/NIC provisioning choice made outside the platform: deployment provisioning MAY raise a NIC's transmit and receive segment limits where the NIC supports it, the platform SHALL define no operator config key for it and SHALL never require it, and the object store's startup capability probe SHALL record its presence or absence in the capability descriptor like any other fast path (object-store, Requirement: "Capability-probed acceleration with a portable fallback"). Both improvements apply to every TCP path the deployment runs — the object-store cloud transport and the peer transport's libfabric sockets-provider fallback — and SHALL change no observable behaviour of either (INV-HARDWARE-ACCEL): byte streams, message and frame boundaries, error taxonomy, BLAKE3 outcomes, and the peer transport's deployment-network-boundary trust model (peer-service, Requirement: "libfabric peer transport on the compio runtime") are identical with them on or off.

#### Scenario: A host without BIG TCP runs identically
- **WHEN** a deployment host's kernel or NIC does not support super-segments larger than 64 KiB
- **THEN** every TCP transport runs with standard segment sizes, every committed byte, result, and verification outcome is identical, and only per-packet CPU and throughput differ

#### Scenario: AccECN is never a correctness input
- **WHEN** AccECN is disabled or absent on a host
- **THEN** no platform code path, config key, saved record, or result changes — only transport performance under congestion differs

#### Scenario: The peer transport's trust model is untouched
- **WHEN** BIG TCP or AccECN accelerates the peer transport's libfabric sockets-provider fallback
- **THEN** the peer-frame classes, their hint-only semantics, and the deployment-network-boundary security model are exactly as `peer-service` specifies, with no per-channel encryption added or required

### Requirement: In-use hardening controls at the deployment host
Beyond making storage fast, a deployment host can make a running node harder to attack, and none of that hardening is allowed to change what the node computes. Three optional host-level controls make up this layer. First, a deployment MAY run nodes as hardware-encrypted confidential VMs (Intel TDX or AMD SEV-SNP trust domains); trust-domain presence SHALL be probed once at startup and recorded in diagnostics beside the existing host-capability probes, and every storage path — NVMe passthrough, the filesystem dev fallback, and object-store upload — SHALL behave identically inside a trust domain, with the same durable bytes, frame offsets, checksums, and error taxonomy as outside one.

Second, on kernels that provide io_uring task-level ring restrictions and opcode filters (Linux 7.0/7.1), each io_uring the platform creates MAY be restricted at ring setup to exactly the opcode set that ring's path uses, so a compromised worker cannot issue arbitrary io_uring operations through it. The restriction set for the storage rings SHALL be derived from the sanctioned io_uring operation set (`implementation-toolchain`, Requirement: "io_uring operation set is owned by one sanctioned crate") — the owning location that already constructs every submission is the one place that knows each ring's opcodes — and SHALL NOT come from an operator config key. Restriction SHALL NOT change the durable bytes, frame offsets, read results, or error taxonomy of any storage path; on kernels without the feature, the rings SHALL run unrestricted with identical results.

Third, where an optional accelerator offload (IAA/QAT/DSA) or the NVMe passthrough path is active on silicon whose PCIe links support Integrity and Data Encryption (IDE, Linux 6.19+), the deployment MAY enable link encryption so DMA between the CPU and the accelerator or NVMe device is protected on the bus. IDE is relevant only where those offloads are enabled; it SHALL be probed and reported like the other host capabilities and SHALL NOT change any transferred byte, any (de)compression or query result, BLAKE3 verification, or accelerator selection — the rule that no operator config key selects an accelerator path (INV-HARDWARE-ACCEL) is unchanged.

All three controls are hardening, not acceleration and not correctness: absence, a failed probe, or an unsupported kernel SHALL leave the un-hardened path fully correct, and which controls are active SHALL be surfaced only through diagnostics/metrics. A deployment MAY declare any of these controls required, and startup SHALL then refuse with a diagnostic naming the missing control — exactly as the NVMe character-device deployment constraint refuses — rather than silently starting without the promised protection. This is the deployment-host half of the confidential-computing posture (`hef-security-and-isolation`, Requirement: "Confidential-computing and in-use hardening posture").

#### Scenario: Restricted ring refuses an out-of-set opcode
- **WHEN** an io_uring restricted to its storage path's opcode set receives a submission for an opcode outside that set, as a compromised worker would issue
- **THEN** the kernel refuses the submission, and every sanctioned storage operation on that ring keeps working because the restriction set was derived from the sanctioned operation set

#### Scenario: Older kernel runs unrestricted with identical results
- **WHEN** the same storage workload runs on a kernel without io_uring ring restrictions
- **THEN** the rings run unrestricted and the durable bytes, frame offsets, read results, and error taxonomy are identical, with only the hardening layer differing

#### Scenario: Link encryption leaves offload results unchanged
- **WHEN** PCIe IDE link encryption is active for an enabled IAA/QAT offload or the NVMe passthrough path
- **THEN** the transferred bytes, (de)compression and query results, BLAKE3 verification, and accelerator selection are unchanged, and the control is visible only in diagnostics

#### Scenario: Required confidential-VM profile refuses
- **WHEN** a deployment declares the confidential-VM profile required and a node starts on a host that cannot provide the trust domain
- **THEN** startup refuses with a diagnostic naming the missing control and does not silently start outside the domain

### Requirement: Host-baseline kernel improvements are assumptions, not dependencies
The Linux 7.1+ deployment target includes two kernel improvements that speed the platform up with no platform code, probe, or configuration, and they SHALL be recorded as host-baseline assumptions rather than dependencies. Intel FRED (Flexible Return and Event Delivery, default-on in 7.1 on FRED-capable CPUs) lowers the cost of every syscall, interrupt, and exception entry and return — a direct win the io_uring-heavy, interrupt-heavy engine inherits automatically. vDSO `getrandom()` (Linux 6.11+) delivers random bytes without a per-call syscall: the live implementation of the platform's injected randomness interface SHALL obtain randomness through the standard `getrandom` path so high-rate ULID, session-token, and nonce generation inherits the vDSO fast path automatically, while deterministic simulation keeps injecting its own randomness, unchanged. Neither feature SHALL be a correctness, startup, or diagnostic requirement: on a pre-FRED CPU or an older kernel the platform SHALL start and run with identical results, identifiers, and durable bytes, only slower, and no probe result, config key, or health signal SHALL depend on either. The two remaining free wins of the same kernel line — the EEVDF scheduler default and AccECN congestion signalling — are recorded with the OS-scheduling and transport-networking work, not here.

#### Scenario: Older host runs identically, only slower
- **WHEN** the platform runs on a pre-FRED CPU or a kernel without vDSO `getrandom()`
- **THEN** it starts and serves identical results, identifiers, and durable bytes, and nothing probes for, configures, or alerts on the missing baseline features

#### Scenario: High-rate identifier generation needs no per-call syscall on baseline hosts
- **WHEN** the live randomness implementation generates ULIDs, session tokens, or nonces on a Linux 7.1+ host
- **THEN** the bytes come through the standard `getrandom` interface, served from the vDSO without a per-call syscall, and identifier formats, token entropy, and simulation-injected randomness are unchanged

### Requirement: OS CPU-scheduling baseline and tuning posture
The node runs on the operating system's CPU scheduler, and the platform SHALL treat that scheduler as a substrate it tunes, never as a second decision-maker: every item below is performance-only and correctness-neutral, SHALL reduce to the un-tuned behaviour with identical results, and SHALL stay subordinate to the maintenance governor's priority order per `system-architecture` (Requirement: "CPU scheduling and core placement are performance-only and never a second priority authority"). **Baseline:** the platform SHALL assume EEVDF — the kernel's default scheduler since Linux 6.6 — as the baseline on the Linux 7.1+ production target, SHALL carry no CFS-era scheduler tuning, and SHALL require no scheduler configuration work for the baseline to hold; the assumption is about performance, never a correctness dependency or a startup check, so older development hosts keep working. **Cooperative preemption deferral:** the node MAY use the rseq time-slice extension (Linux 7.0) so a thread inside a short critical section on a genuinely shared structure (for example the RAM budgeter or the maintenance governor's counters) asks the scheduler for a brief reprieve from preemption; support SHALL be probed once at startup, and where the kernel lacks the extension or rseq registration fails, the node SHALL run without deferral hints with identical results. **Core partitioning:** a deployment MAY dedicate cores to the few latency-critical ingest and live-SLA threads through host-level isolation (`cpuset`, `isolcpus`, `nohz_full`) with `SCHED_FIFO`/`SCHED_RR` on those threads; the node SHALL discover and adapt to the host's core layout rather than require it, an un-partitioned host SHALL remain a fully valid deployment with identical results, and a real-time thread policy SHALL be used only with the kernel's deadline-server starvation safety net (Linux 6.8) in effect, so `SCHED_OTHER` work — materialization and background jobs among it — always keeps making progress and what work is admitted remains the maintenance governor's decision alone. Core partitioning SHALL become a recommended posture only after a measured spike shows it bounds tail jitter on a representative ingest/live workload while staying correctness-neutral. **PREEMPT_RT:** a real-time kernel SHALL be at most an optional operational deployment profile, considered only where measured core isolation still leaves the live-SLA path with unacceptable tail jitter; the same `pulse` binary SHALL run identically on standard and PREEMPT_RT kernels, and no build-time flag or dependency SHALL require the RT kernel. **`sched_ext`:** a BPF-programmable kernel scheduler SHALL NOT be adopted, per the second-priority-authority exclusion in `system-architecture`. **Confinement:** scheduling and affinity syscalls (`sched_setaffinity`, `sched_setscheduler`, and rseq registration and time-slice operations) SHALL be reached only through the sanctioned per-core-primitives/syscall wrapper — a dedicated location carrying the documented unsafe opt-out per `implementation-toolchain` (Requirement: "Unsafe code is confined to dedicated crates and documented") — exposing safe APIs, so application and engine crates stay `forbid(unsafe_code)`; the thread-per-core `compio` runtime is unchanged and no async runtime is introduced (INV-RUNTIME). Every kernel scheduling feature SHALL be probed on the running host and never assumed, matching the below-floor-kernel development posture of `platform-reference`; no operator-facing config key SHALL select any scheduling policy, and the active posture (baseline scheduler, deferral-hint availability, isolated-core layout, RT profile) SHALL be surfaced only through diagnostics/metrics.

#### Scenario: rseq time-slice absent falls back with identical results
- **WHEN** the running kernel lacks the rseq time-slice extension or rseq registration fails at startup
- **THEN** the node runs without preemption-deferral hints, every result, durable byte, and commit boundary is identical to a run with hints available, and the probe outcome is visible only through diagnostics/metrics

#### Scenario: Partitioned and un-partitioned hosts reach the same results
- **WHEN** the same workload runs on a host with isolated cores and `SCHED_FIFO` latency-critical threads and on a host with no core isolation
- **THEN** the durable journal bytes, query results, checksums, and commit boundaries are identical, and only the latency distribution differs

#### Scenario: Real-time threads cannot starve normal work
- **WHEN** latency-critical ingest/live-SLA threads run under `SCHED_FIFO` on isolated cores while materialization runs under `SCHED_OTHER`
- **THEN** the kernel's deadline-server safety net keeps the `SCHED_OTHER` work making progress, and the maintenance governor — not the OS scheduler — remains the authority over what work is admitted and at what priority

#### Scenario: The same binary runs with and without PREEMPT_RT
- **WHEN** one deployment runs the standard kernel and another runs a PREEMPT_RT kernel as an operational profile
- **THEN** both run the same `pulse` binary with identical results, and no build-time flag, dependency, or startup check requires the RT kernel

#### Scenario: Scheduling syscalls stay behind the sanctioned wrapper
- **WHEN** the node pins a thread, sets a scheduling policy, or registers an rseq time-slice hint
- **THEN** the syscall is issued through the sanctioned per-core-primitives/syscall wrapper, no application or engine crate opens an unsafe opt-out for it, and `forbid(unsafe_code)` continues to hold outside the wrapper

