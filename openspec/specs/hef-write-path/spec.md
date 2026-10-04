## Purpose

Defines the path an event takes from arrival to being safely stored:

- From ingest to a durable acknowledgement, with safe retries on the HEJ journal.
- Publishing HEF files from HEJ idempotently (re-runs don't duplicate data), and later rewriting/repacking HEF files.
- The publish boundary that gates when a HEF becomes visible, and how a service that derives state from those rows stages, promotes, or rolls back that state across the boundary.

The concrete write-path, safe-retry, publish, and rewrite detail are embedded in [write-path-detail.md](write-path-detail.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-write-path/spec.md).
## Requirements
### Requirement: Ingest-to-acknowledgement pipeline
The write path SHALL follow this pipeline: validate → serialize into a pending compact-batch descriptor on the worker's lock-free queue (READY) → reach the flush target (default 16 KiB; 4 KiB for latency-critical/low-load) → claim and reserve a contiguous `(epoch, sequence)` range → submit an aligned HEJ frame via io_uring (`io_uring_cmd` NVMe passthrough; filesystem io_uring in the non-NVMe dev fallback) → verify header CRC-64/NVME and frame BLAKE3 → HARDENED → record reconstructable safe-retry metadata → COMMITTED → decode into LiveOverlay → acknowledge per the API durability/freshness contract → become HEF-publication eligible. LiveOverlay publication SHALL NOT redefine durability (durability is HEJ completion); it defines query visibility for HEJ-backed rows not yet HEF-covered.

#### Scenario: Append-only commit
- **WHEN** an append-only event's HEJ frame is durable and its safe-retry receipt is recorded or reconstructable
- **THEN** HARDENED becomes COMMITTED immediately and the client may be acknowledged

### Requirement: Single protected payload record with safe retry
The system SHALL NOT require both a full KV buffer payload and an HEJ payload before acknowledgement; HEJ SHALL be the protected event payload record. Safe-retry state SHALL use client/connector delivery identity, scope, commit receipt, HEJ frame cursor, status class, expiry, and minimal dedupe metadata, and SHALL be reconstructable after restart/takeover from retained replay-guard rows plus HEJ coverage. Replaying a retained HEJ frame SHALL NOT enqueue duplicate events while the original acknowledgement is within the replay-guard window.

#### Scenario: Replay within guard window
- **WHEN** a retained HEJ frame is replayed while the original acknowledgement is still within the replay-guard window
- **THEN** no duplicate events are enqueued

### Requirement: Idempotent HEF publication
Publishing HEF from HEJ SHALL follow this sequence and atomically publish the manifest entry, projection metadata, deletion-vector generation, and coverage watermarks. The publisher SHALL be idempotent: re-publishing the same journal range SHALL produce either the same file identity or a safely replaceable file. LiveOverlay segments SHALL become evictable only after the new published HEF range is visible, and HEJ retention/segment recycling SHALL advance only after publication and the configured recovery safety window.

#### Scenario: Re-publish same range
- **WHEN** the publisher re-publishes a journal range it already published
- **THEN** the result is the same file identity or a safely replaceable file, with no double-counting

### Requirement: File roll boundary owned by publish policy
The HEF file boundary SHALL be owned by publish policy, not by the file-layout rules. The publisher SHALL pack a contiguous durable HEJ range into one HEF and SHALL roll (seal) the file on a dual trigger, whichever fires first: (a) a compressed byte target (default ~1 GiB), or (b) a maximum open-time window (default minutes-scale), so that low-volume tenants seal small compact files and HEJ/LiveOverlay retention can advance even when the byte target is never reached. The 512 MiB stripe-size clamp SHALL act only as a safety backstop and SHALL NOT be the primary roll trigger. Either trigger SHALL produce a `Sealed` file via the `OpenTmp → Sealed` edge.

#### Scenario: High-volume tenant rolls on bytes
- **WHEN** a tenant's open HEF reaches the compressed byte target before the time window elapses
- **THEN** the publisher seals the file at the byte target, producing a wide file with a handful of stripes

#### Scenario: Low-volume tenant rolls on time
- **WHEN** a tenant's open HEF has not reached the byte target when the maximum time window elapses
- **THEN** the publisher seals the small compact file so HEJ retention and LiveOverlay eviction can advance

### Requirement: HEF rewrite preserves one format and correctness
HEF rewrite/repacking SHALL select Active source files, mark them Outdated only after the replacement generation is published, select rewrite scope incrementally using per-granule `clustering_quality` rather than whole-file resorts by default, rebuild data/marks/indexes/aggregates (including `variant_shredded_field_blocks` and variant dictionary blocks), write replacement files in the same HEF format, verify BLAKE3/aggregate/marks/deletion-vector accounting, publish a new generation manifest atomically, and move superseded files to DeleteOnDestroy only after the safety window and in-flight query horizon expire. Rewrite SHALL NOT create a new file format or separate freshness/historical/cold/archive HEF classes.

#### Scenario: Source files retired safely
- **WHEN** an HEF rewrite produces a replacement generation
- **THEN** source files are marked Outdated only after the replacement is published and deleted only after the safety window and in-flight query horizon expire

### Requirement: Compaction folds sidecar files into the base
SuperHEF compaction SHALL fold sidecar files into the rewritten base: promotion-backfill vertical projections SHALL be folded in and dropped (they are transient bridges), and derived-columns sibling files SHALL be folded in once their columns are settled per their declared settle horizon, with a new sibling re-emitted for the unsettled tail. Folding SHALL preserve row alignment, deletion-vector and correction accounting, and BLAKE3 verification, and SHALL publish atomically in one manifest generation.

#### Scenario: Settled derived columns folded in
- **WHEN** compaction rewrites a range whose derived-columns sibling has columns past their settle horizon
- **THEN** the settled columns are embedded in the new base file and a sibling is re-emitted only for the unsettled tail

### Requirement: HEF publish boundary gates visibility
A HEF SHALL NOT be visible until the complete publish boundary succeeds (HefPublishRule): select a contiguous durable HEJ range, validate each frame by CRC-64/NVME precheck and authoritative BLAKE3, decode only `harana_hej_compact_batch_v1` (rejecting every other `payload_encoding`), stage the HEF object and manifest entry to tenant-qualified staging, verify size/schema/BLAKE3/tenant/range/feature-directory/quota, publish the manifest entry and HEFs only after all checks pass, and emit peer notices only after publication succeeds. Failed or losing attempts SHALL leak no peer notices or public reads, and HEJ coverage SHALL remain the recovery source until published HEFs plus the retention safety window allow journal retention to advance.

#### Scenario: Failed publish leaks nothing
- **WHEN** a HEF publish attempt fails a checksum or loses the commit race
- **THEN** no peer frame is sent, staged side effects are discarded, and no public read observes the attempt

#### Scenario: Object uploaded and verified before the manifest names it
- **WHEN** a publish attempt uploads its HEF object to the application's object store
- **THEN** the stored size and CRC-64/NVME are checked against the build before any manifest generation is written, a failed upload aborts any multipart upload and writes no generation, and multipart parts begin only on stripe boundaries

#### Scenario: Lost race after upload
- **WHEN** an attempt loses the range to a different file after its object was uploaded
- **THEN** the object is left in place, unreferenced by any generation, for the application's sweep of unreferenced keys

### Requirement: HEF-publish side-effect transaction discipline
When a service mutates derived state while observing rows during HEF publication, it SHALL record staged changes (or a rollback point) under the publish attempt id, promote staged state only at `after_hef_publish_before_peer_notice` if the manifest publication wins, otherwise roll back or rebuild from the last checkpoint plus UnifiedEvents, and SHALL finish rollback/rebuild before answering reads, emitting outputs, or accepting more publish-side mutations.

#### Scenario: Side effect rolled back on lost publish
- **WHEN** a service staged derived-state changes for a publish attempt that then loses
- **THEN** it rolls back or rebuilds from its last checkpoint plus UnifiedEvents before serving reads

