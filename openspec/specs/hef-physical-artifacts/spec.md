## Purpose

Defines the physical pieces of the event-storage system and how they fit together:

- HEJ — the durable commit log and source of freshness; HEF — the immutable file queries read from; LiveOverlay — durable recent HEJ data not yet rolled into HEF; and PreparedView outputs.
- The write/acknowledgement discipline and the watermark model that ties all of these together.

The concrete HEJ/HEF/LiveOverlay/PreparedView byte-layouts and struct definitions are embedded in [artifact-formats.md](artifact-formats.md).

Requirements that Pulse's query engine or services implement are specified in the harana/pulse repository under the same capability name (openspec/specs/hef-physical-artifacts/spec.md).
## Requirements
### Requirement: HEJ is the durable event replay source
HEJ SHALL be the durable commit log, protected pre-ack durability record, and freshness source for the event path. HEJ SHALL NOT be a KV table and SHALL NOT store mutable service lifecycle state (rules, prepared-view readiness, dashboards, memory/chat/session/notification/insight/access state). Event records in HEJ SHALL be immutable replay records. Safe-retry KV rows MAY point to HEJ positions and are mutable indexes; if a safe-retry row disagrees with HEJ event bytes, HEJ SHALL win for event replay and the safe-retry row SHALL be repaired or discarded.

#### Scenario: Safe-retry row disagrees with HEJ
- **WHEN** a safe-retry KV row disagrees with the HEJ event bytes for the same position
- **THEN** HEJ is authoritative for replay and the safe-retry row is repaired or discarded per the owner recovery rule

#### Scenario: Missing safe-retry row after recovery
- **WHEN** a safe-retry KV row is missing after recovery
- **THEN** it is rebuilt from HEJ `dedupe_hash` fields and sequence positions

### Requirement: Deterministic HEJ-to-LiveOverlay conversion
A valid HEJ frame SHALL decode into exactly one LiveOverlay segment (unless its full sequence range is already covered by the selected manifest-published HEF). That segment SHALL be either the v1 Arrow `RecordBatch` or an equivalent Vortex compressed array that is logically equal to that `RecordBatch` — same fixed v1 schema semantics and the same rows, values, and row order. The conversion SHALL produce byte-equivalent Arrow logical values and row order using the fixed v1 schema and rules: `sequence = first_sequence + row_index`, string fields decoded from `StringTableV1`, null refs for `0xFFFFFFFF`, and `payload_ref = (row_index << 32) | payload_offset`. Implementations SHALL NOT change field names, Arrow types, nullability, sequence/payload_ref derivation, or ordering, and SHALL NOT select a different HEJ encoding based on convenience, hardware, workload, feature flag, or benchmark. The choice between the Arrow `RecordBatch` and the equivalent Vortex compressed array SHALL be a deterministic function of the HEJ sequence range, so any node that rebuilds a given range produces byte-identical segment bytes for the representation it selects, and the two representations SHALL be logically interchangeable for queries. HEJ SHALL remain the sole durability source; the LiveOverlay segment — in either representation — SHALL NOT be a durability or correctness source, and on crash it SHALL be rebuilt from HEJ replay or replicated fragments. A reader SHALL reject the segment whenever its decoded row count differs from `HEJFrameHeaderV1.event_count`, regardless of which representation is used.

#### Scenario: Row count mismatch
- **WHEN** a decoded LiveOverlay segment's Arrow row count differs from `HEJFrameHeaderV1.event_count`
- **THEN** the reader rejects the segment

#### Scenario: Range already HEF-covered
- **WHEN** an HEJ frame's complete sequence range is covered by the manifest-published HEF snapshot selected for the reader
- **THEN** the frame is skipped rather than decoded into LiveOverlay

#### Scenario: Compressed-array segment yields identical query results
- **WHEN** the same HEJ frame is decoded once into the Arrow `RecordBatch` segment and once into the equivalent Vortex compressed-array segment, and an identical query reads each
- **THEN** the two segments return identical rows, values, and order, because the compressed array is logically equal to the `RecordBatch` under the fixed v1 schema

#### Scenario: Representation is deterministic across nodes
- **WHEN** two nodes independently rebuild the same HEJ sequence range into LiveOverlay
- **THEN** each selects the same representation by the same deterministic function of the range and produces byte-identical segment bytes, with HEJ remaining the sole durability source and the segment never treated as durable

### Requirement: Autonomous per-worker commit discipline
HEJ ingest SHALL follow the autonomous-commit discipline: no global group-commit writer and no single global acknowledgement thread. Each ingest worker SHALL own a single-producer/single-consumer bounded circular commit queue with cache-line-aligned serialized records, released by advancing the queue head. Final `(epoch, sequence)` SHALL be assigned only when a worker claims pending records for a frame, reserving one contiguous range so every normal frame contains one tenant, one epoch, and a contiguous sequence range. A sequence reservation SHALL be a lease, not a permanent claim: a reserved range not hardened into a durable frame before its lease deadline SHALL be abandoned and closed by an internal HEJ void record (zero events, void flag, the abandoned range), participating in the segment BLAKE3 hash chain, so a stalled or crashed worker cannot stall the contiguous `commit_watermark` beyond its lease. Void records SHALL NOT be user events, HEF rows, QueryEngine-visible rows, or public output. Topology-local log stealing SHALL be limited to same-tenant/same-epoch records within the worker's CPU-topology steal group using the clean/dirty cursor CAS protocol.

#### Scenario: Log-steal CAS fails
- **WHEN** a stealing worker copies bytes from `clean_cursor..dirty_cursor` but its CAS to advance `clean_cursor` fails
- **THEN** the copied bytes are discarded and SHALL NOT be written into a frame

#### Scenario: Abandoned reservation closed by void record
- **WHEN** a worker reserves a contiguous `(epoch, sequence)` range but crashes or stalls past its lease deadline without hardening a frame
- **THEN** an internal void record covering that exact range is committed so `commit_watermark` advances past it, and the voided sequences are permanently skipped without ever being public output

#### Scenario: Low-load force commit
- **WHEN** trickle traffic means a full 16 KiB frame will not fill before the idle target (default 50 µs, jittered/probabilistic)
- **THEN** the worker force-commits a 4 KiB-aligned HEJ frame for the pending records rather than waiting indefinitely

### Requirement: Acknowledge only after HEJ durability
An event SHALL be acknowledged only after the HEJ frame containing it is durable, where durability is defined per mode (NVMe passthrough/direct-I/O, buffered-filesystem dev fallback, replicated) and always includes header CRC-64/NVME, frame BLAKE3, and segment hash-chain verification. HEF publication SHALL NEVER be required for acknowledgement. For append-only ingest, durability is commit; for routes with transactional dependencies, durability moves the event to HARDENED and acknowledgement waits for autonomous dependency acknowledgement. Normal flush units SHALL be 4/8/16/32/64 KiB (default 16 KiB) and frames above 64 KiB SHALL be invalid.

#### Scenario: Strict read-after-ack visibility
- **WHEN** a route requires strict read-after-ack query visibility
- **THEN** acknowledgement waits until the event is included in `visibility_watermark` (or fresh queries block until LiveOverlay replays that sequence), and acknowledged HEJ events not yet HEF-covered are never silently omitted from fresh queries

### Requirement: HEF is the immutable, self-describing query file
HEF SHALL be the immutable, footer-indexed, range-readable query file selected by the manifest, columnar for envelope/promoted fields and payload-complete (every event row resolves to its canonical `harana_variant_v1` value). There SHALL be exactly one HEF file format; size, layout kind, and optional index/aggregate/context/acceleration blocks SHALL be declared in the file feature directory rather than creating separate formats. A reader SHALL NOT infer behavior from human lifecycle labels and SHALL inspect the HEF footer and feature directory.

#### Scenario: Planner inspects feature directory
- **WHEN** the planner selects an HEF file for a query
- **THEN** it inspects the footer and feature directory to determine available blocks rather than inferring behavior from a lifecycle label

### Requirement: LiveOverlay serves fresh durable ranges and is reconstructable
LiveOverlay SHALL be the queryable Arrow-native representation of HEJ-durable, validated ranges not yet covered by a manifest-published HEF and not removed by a visible deletion vector. LiveOverlay SHALL NOT be a durability source; on crash its segments SHALL be rebuilt from HEJ replay or replicated fragments. Node-local LiveOverlay SHALL be required — nodes SHALL NOT forward event reads to the tenant's EventManager for LiveOverlay rows, and SHALL serve strict fresh queries only for ranges they have rebuilt, validated, and published locally. A LiveOverlay segment SHALL be evicted only when its complete `(epoch, sequence)` range is covered by a manifest-published HEF file visible to future query snapshots. Any overflow tier SHALL be non-authoritative and reconstructable from HEJ.

#### Scenario: Node behind on a range
- **WHEN** a node's local LiveOverlay does not yet cover a requested fresh range
- **THEN** the query waits, rebuilds the missing range, or serves only an explicit bounded-staleness mode — it does not forward the read to the tenant's EventManager

#### Scenario: Premature eviction prevented
- **WHEN** a LiveOverlay segment's range is not yet covered by a visible manifest-published HEF
- **THEN** the segment is not evicted

### Requirement: Explicit watermark model
The system SHALL track three explicit watermarks: `commit_watermark` (highest `(epoch, sequence)` such that every lower sequence in the epoch is durably covered by an event frame or a committed void range), `visibility_watermark` (highest published into node-local LiveOverlay or HEF and safe for the query mode, with void ranges counting as covered for contiguity since they publish zero rows), and `snapshot_watermark` (a stable upper bound captured by a QuerySnapshot, `<= visibility_watermark`). Compatibility aliases SHALL hold: `durable_journal_cursor = commit_watermark` and `live_queryable_cursor = LiveOverlay component of visibility_watermark`.

#### Scenario: Snapshot bound respected
- **WHEN** a QuerySnapshot is captured
- **THEN** its `snapshot_watermark` is at most the current `visibility_watermark`

### Requirement: Membership-hash width and collision semantics
HEF stores hashes of identifiers so it can answer membership questions — "does this file mention this actor, account, entity, or trace?" — without carrying the raw identifiers. Those hashes are not all the same width: `entity_id_hash` persists as a 128-bit hash, while the actor, account, and trace membership hashes persist as 64-bit low halves. Every membership hash, regardless of width, SHALL be inexact-no-false-negative acceleration state: a hash match MAY be a collision and SHALL be confirmed by an exact identifier comparison before a row is returned, counted, or presented as matching, and a hash mismatch SHALL be trusted (the filter never produces a false negative). The narrower 64-bit membership hashes carry a weaker collision bound than the 128-bit `entity_id_hash`, and each SHALL record its accepted collision bound so a reader knows the false-positive rate it must confirm away with the exact comparison. Because equality confirms every match, the narrower width SHALL never change a query result — only the number of exact confirmations. Widening any membership hash (for example moving an actor, account, or trace hash from 64-bit to 128-bit) SHALL be a feature-directory-gated format change under HEF's feature discipline, with older readers refusing on the unrecognized feature, and SHALL NOT be a silent reinterpretation of existing bytes.

#### Scenario: Hash match is confirmed by exact comparison
- **WHEN** a membership filter reports a hash match for a 64-bit actor, account, or trace hash
- **THEN** the reader confirms the candidate with an exact identifier comparison before returning or counting the row, so a hash collision never produces a wrong answer

#### Scenario: Hash mismatch is trusted with no false negative
- **WHEN** a membership hash does not match the queried identifier's hash
- **THEN** the reader trusts the mismatch and skips the row, because the filter is inexact-no-false-negative

#### Scenario: Widening a membership hash is feature-gated
- **WHEN** a producer widens an actor, account, or trace membership hash from 64-bit to 128-bit
- **THEN** it declares the change in the file feature directory and a reader that does not recognize the feature refuses rather than reinterpreting the existing 64-bit bytes

