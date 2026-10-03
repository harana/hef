## Purpose

Defines the correctness rules every part of the HEF/HEJ engine must always uphold, whatever the layout, encoding, query path, or hardware:

- Which source is authoritative for ordering between the HEJ journal, the LiveOverlay, and published HEF files.
- Consistent query snapshots, pruning granularity, exact aggregates, payload and internal-column handling, crash recovery, and the HEJ acknowledgement hot path.
- These are the consolidated guarantees that the layout, encoding, query, write, and lifecycle behaviour must never break.

The concrete design stance, performance targets, alignment rules, payload/metadata scope, goals, non-goals, the verbatim numbered invariant list, the implementation phases, and the verification and testing strategy are embedded in [design-goals-and-phases.md](design-goals-and-phases.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-core-invariants/spec.md).
## Requirements
### Requirement: Source-of-truth and visibility ordering
HEJ SHALL be the source of truth until a manifest-published HEF file covers the journal range. LiveOverlay SHALL be queryable but not authoritative and SHALL be rebuildable from HEJ. Nodes SHALL use node-local LiveOverlay, not EventManager-forwarded live event reads. HEF files SHALL be immutable after footer finalization. Public queries SHALL read only manifest-published Active HEF files plus the approved node-local LiveOverlay snapshot. The manifest, not file existence, SHALL define HEF visibility.

#### Scenario: Unpublished HEF is not visible
- **WHEN** an HEF file exists on storage but is not referenced by the manifest
- **THEN** queries do not read it, and the journal range it would cover is still served from HEJ/LiveOverlay until the manifest publishes the HEF

### Requirement: Payload, pushdown, internal columns, and refusing features
Raw payload reads SHALL be late-materialized and authorized. Payloads SHALL have exactly one canonical format, `harana_variant_v1`. Structured payload-field pushdown SHALL use `variant_shredded_field_blocks`, while unshredded paths remain in residual variant values in the payload arena. Internal columns such as tenant_id, epoch, sequence, payload_ref, row offsets, local paths, and embeddings SHALL NOT be public output. Unknown required format features SHALL refuse.

#### Scenario: Unknown required feature refuses
- **WHEN** a reader encounters a required format feature flag it does not understand
- **THEN** it refuses the file rather than serving partial or incorrect data

#### Scenario: Single payload path extracted without sibling decode
- **WHEN** a query needs one unshredded payload path from a final matching row
- **THEN** the path is read as a deterministic merge of shredded typed values and the residual variant value, using offset-based navigation against the governing variant dictionary without decoding sibling fields

### Requirement: Crash recovery and hardware are correctness-neutral
Crash recovery SHALL be idempotent and SHALL rebuild missing LiveOverlay state from HEJ before serving fresh queries. Hardware accelerators and NVMe device capabilities (FDP placement, AWUN/AWUPF untorn-write support, DSM TRIM) SHALL be optimizations, not correctness dependencies. BLAKE3 SHALL be the authoritative integrity check; CRC-64/NVME SHALL be a header-only fast precheck.

#### Scenario: Fresh query after restart waits for rebuild
- **WHEN** LiveOverlay state is missing after a restart
- **THEN** the system rebuilds it from HEJ before serving a fresh query, or serves only an explicit bounded-staleness/manifest-only mode

### Requirement: HEJ frame sizing and acknowledgement hot path
HEJ v1 normal frame sizes SHALL be exactly 4 KiB, 8 KiB, 16 KiB, 32 KiB, or 64 KiB. HEJ hot-path acknowledgement SHALL use autonomous worker flush and SHALL NOT depend on a centralized group-commit writer or a single global acknowledgement thread. Buffered-filesystem HEJ writes SHALL be correctness fallbacks outside the microsecond-latency claim unless fdatasync/fsync-equivalent durability meets the same benchmark gate. Ordinary append-only event ingest SHALL NOT pay GSN/RFA/barrier dependency overhead; that machinery is permitted only for routes with real transactional dependencies.

#### Scenario: Append-only ingest avoids dependency machinery
- **WHEN** an ordinary append-only event is ingested with no page-level transactional dependency
- **THEN** it is acknowledged via autonomous worker flush without GSN/RFA/barrier overhead

### Requirement: Conditional manifest publication, filter exactness, incremental views, and rebuildable caches
Manifest publication on object storage SHALL use conditional writes (create-only generation objects, If-Match CAS pointer advance); a lost CAS race SHALL rebase and retry, never overwrite. RWF_ATOMIC, AWUPF, FDP, and ZNS SHALL be torn-write-cost and write-amplification optimizations only; HEJ correctness SHALL NOT depend on them. Range-filter and point-filter SkipIndexes SHALL be `inexact_no_false_negative` pruning structures, and point filters SHALL NOT answer range predicates. Incrementally maintained PreparedView output SHALL equal full recomputation from the same committed event snapshot; views that cannot satisfy this SHALL fall back to recompute. Caches (LiveOverlay, local HEF cache, vector indexes, trained compression dictionaries) SHALL be rebuildable acceleration state whose loss or erasure never loses authoritative data, and subject-encrypted blocks SHALL remain encrypted in every cache tier.

#### Scenario: Lost manifest CAS rebases instead of overwriting
- **WHEN** two publishers race to advance the manifest pointer and one loses the If-Match CAS
- **THEN** the loser re-reads the latest generation, rebases, and retries rather than overwriting the winner

### Requirement: Storage-core code runs unmodified under deterministic simulation
HEJ, HEF, LiveOverlay, and publication production code SHALL access wall-clock time, randomness, scheduling, storage I/O, and manifest publication only through injectable interfaces, so that identical production code runs under deterministic simulation. Test harnesses SHALL be able to inject storage and publication faults — torn frame tails, reordered or failed I/O completions, crash points, and publication CAS races — through those interfaces without patching or recompiling production code paths, and every simulated run SHALL be reproducible from its seed.

#### Scenario: Fault injection requires no production-code changes
- **WHEN** a deterministic-simulation test injects a torn HEJ frame tail and a lost publication CAS race through the injected interfaces
- **THEN** the production storage-core code under test runs unmodified and the run is reproducible from its seed

### Requirement: Probed journal-storage capabilities with a portable fallback
The journal-storage backend SHALL discover, at startup and per shard, which kernel and filesystem fast paths the host supports — atomic-write unit, direct-I/O alignment, uncached reads, large block size, and zoned placement — and record them in a capability descriptor that extends the existing shard atomicity probe. A fast path SHALL be taken only when the probe confirms support on the running host (targeting Linux 7.1+), and every fast path SHALL have a portable fallback used otherwise. Taking or not taking a fast path SHALL be observably equivalent: the durable journal bytes, the frame offsets returned by append, the shard extent, the bytes returned by read, and the error taxonomy SHALL be identical either way, with only performance differing. This strengthens, and SHALL NOT weaken, the existing rules that hardware atomic-write, FDP, and ZNS support are optimizations and never correctness dependencies, and that BLAKE3 remains the authoritative integrity check; the microsecond-latency acknowledgement claim continues to require the durable (non-buffered-fallback) path per "HEJ frame sizing and acknowledgement hot path".

#### Scenario: Unsupported journal fast path falls back
- **WHEN** the host kernel or filesystem does not support a journal fast path the backend could use
- **THEN** the backend uses the portable fallback and the appended frame is durable, replayable, and BLAKE3-verified exactly as before

#### Scenario: Atomic append and torn-tail recovery reach the same result
- **WHEN** a multi-block frame is appended on a host that supports a large atomic-write unit, and again on a host that does not
- **THEN** on the first the frame is published untorn by the atomic write, on the second a torn tail is truncated to the last complete frame during recovery, and in neither case is a partial frame ever replayed or acknowledged

### Requirement: Uncached journal replay reads avoid double-buffering the page cache
The large sequential journal replay-and-validation scan MAY use uncached or direct reads so the kernel page cache does not retain a second copy of journal bytes the engine streams through once during recovery or validation. Choosing an uncached or direct read SHALL NOT change the CRC-64/NVME precheck, the authoritative BLAKE3 verification, or the recovery result, and SHALL fall back to ordinary buffered reads where the host does not support it.

#### Scenario: Replay scan does not evict hot pages
- **WHEN** the engine replays a long contiguous durable journal range through an uncached read
- **THEN** every frame is validated and verified exactly as a buffered read would, and the scan does not leave a duplicate copy of the range in the page cache

### Requirement: Batched journal submissions confined behind the synchronous interface
A journal-storage backend MAY drive a completion-based runtime internally to batch and link one worker's own append and sync submissions and to use registered aligned buffers, so a burst of frames plus its durability barrier costs fewer syscalls. This batching SHALL remain per worker and SHALL NOT reintroduce a centralized group-commit writer or a global acknowledgement thread, the `JournalStorage` interface SHALL stay synchronous and deterministic, the in-memory simulation backend SHALL NOT use a kernel runtime, and runtime failures SHALL surface as the same storage-error taxonomy as the portable path. Durability SHALL still be claimed only after the barrier completes.

#### Scenario: A worker batches frames without a global commit thread
- **WHEN** one worker links several frame appends and a sync into a single submission to its completion runtime
- **THEN** acknowledgement still waits for the barrier to complete, no centralized group-commit writer is involved, and the same sequence under simulation runs without a kernel runtime and is reproducible from its seed

### Requirement: Probed journal-device health watch is correctness-neutral
The journal-storage backend SHALL discover, per shard at startup, whether the host exposes proactive storage-health signals — file-I/O-error reporting and live filesystem-health monitoring — for the volumes holding the HEJ journal, recording them in the same shard capability descriptor that holds the existing journal fast-path probes (see "Probed journal-storage capabilities with a portable fallback"). Where supported, the backend MAY watch those volumes and raise the same early-warning storage-health signal the object store uses when the kernel reports an I/O error against, or a filesystem-health degradation under, a journal path, off the append and replay hot paths. This watch SHALL strengthen, and SHALL NOT weaken, "Crash recovery and hardware are correctness-neutral": it SHALL NOT gate append, acknowledgement, replay, or recovery; BLAKE3 SHALL remain the authoritative integrity check with CRC-64/NVME the header-only precheck; and the microsecond-latency acknowledgement hot path of "HEJ frame sizing and acknowledgement hot path" SHALL be unaffected. Where the host does not expose these signals, the backend SHALL keep its existing reactive behaviour — a bad frame is caught by CRC/BLAKE3 on replay and a torn tail is truncated during recovery — and the signal, when present, SHALL only inform health and prioritize droppable background maintenance, never admit an unverified frame or acknowledge a write whose durability barrier did not complete.

#### Scenario: Journal device error raises an early warning without gating the hot path
- **WHEN** the kernel reports an I/O error under a journal path on a host that exposes the signal
- **THEN** an early-warning storage-health signal is raised off the hot path while append, acknowledgement, and replay continue to be governed only by the durability barrier, CRC-64/NVME, and authoritative BLAKE3

#### Scenario: Unsupported host keeps the reactive path
- **WHEN** the host exposes no proactive storage-health signal for the journal volume
- **THEN** journal correctness is unchanged — torn tails are truncated on recovery and bad frames are caught by CRC/BLAKE3 — and no health signal is assumed

