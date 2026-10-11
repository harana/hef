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
HEF SHALL be the immutable, footer-indexed, range-readable query file selected by the manifest, columnar for envelope/promoted fields and payload-complete (every event row resolves to its canonical `harana_variant_v1` value). There SHALL be exactly one HEF file format; size, layout kind, file profile (event, graph, snapshot, audit, log, inbox), and optional index/aggregate/context/acceleration blocks SHALL be declared in the file header, footer, and feature directory rather than creating separate formats. A reader SHALL NOT infer behavior from human lifecycle labels and SHALL inspect the HEF footer and feature directory.

#### Scenario: Planner inspects feature directory
- **WHEN** the planner selects an HEF file for a query
- **THEN** it inspects the footer and feature directory to determine available blocks rather than inferring behavior from a lifecycle label

### Requirement: Log objects for small write-ahead logs
A small append-only write-ahead log object (`log/{shard}/{seq}`, a few KiB) SHALL use compact log framing instead of the HEF container's 4 KiB header block and footer index, and SHALL declare the `log` profile code (4) up front. Its bytes, every integer little-endian, SHALL be a 6-byte header (`"HEFL"`, format version 1, profile code 4), then each record as a `u32` payload length, the payload, and a CRC-64/NVME of the length and payload, then a 12-byte trailer (`u32::MAX` in the length position, the `u32` record count, `"HEFL"`). A record payload SHALL be shorter than `u32::MAX` bytes. The magic SHALL differ from HEF's `"HEF1"` at both ends, so a HEF reader refuses a log object and a log reader refuses a HEF file. The object's root SHALL be the BLAKE3 of those bytes; an object larger than one 1 MiB chunk group SHALL carry the same outboard proof tree and `"HEFT"` trailer HEF stripes use under `stripe_verified_streaming`, after its log trailer. A reader SHALL read records forward. Opened with a root the caller pinned when the object was committed, it SHALL check every byte against that root, one chunk group at a time, before yielding it, and SHALL treat any mismatch as corruption. Opened without a root, it SHALL check each record's CRC and SHALL report bytes that stop partway through the header, a record, or the trailer, or at a record boundary with no trailer, as a torn tail carrying the intact record count and byte length; a CRC mismatch, a wrong trailer record count, or bytes after the trailer SHALL be corruption.

#### Scenario: Torn tail after a crash mid-write
- **WHEN** a log object without a pinned root is any strict prefix of a sealed object
- **THEN** the reader yields every complete record and then reports a torn tail with their count and byte length, never corruption

#### Scenario: Damage before the end
- **WHEN** a complete record's CRC does not match, or a byte of an object with a pinned root differs from what the root commits to
- **THEN** the reader reports corruption rather than a torn tail

#### Scenario: Log object and HEF file are never confused
- **WHEN** a log object is handed to the HEF header decoder or file opener, or a HEF file to the log reader
- **THEN** it is refused on its magic

#### Scenario: Log framing overhead
- **WHEN** a log object is written
- **THEN** its header and trailer add 18 bytes in total and each record adds 12 bytes to its payload

### Requirement: State chunks hold a subject's state as a content-addressed tree
A subject's state (for a chat room: which event last set each `(event type, state key)`) SHALL be stored as a prolly tree of immutable state chunks, so successive states share every chunk a change does not touch. A state map SHALL take a key of two interned ids, type id `u32` and state key id `u32`, ordered by type id and then state key id, to the `u64` sequence number of the event that set it. A chunk's plaintext SHALL be, every integer little-endian: the magic `"HSC1"`, a level byte (0 for a leaf), three zero bytes, an entry count `u32`, the entries strictly ascending by key, then a CRC-64/NVME of everything before it. A leaf entry SHALL be the type id, state key id and sequence number (16 bytes); an interior entry SHALL be its child's first type id and state key id, then the child's 32-byte address (40 bytes). The writer SHALL cut each level into chunks of 4 to 16 KiB of entries at content-defined boundaries: a chunk ends after an entry once it holds at least 4 KiB of entries and `stable_hash` of the level byte and the entry's key (type id then state key id, little-endian) is a multiple of 4096 divided by the entry length, and it ends before any entry that would take it past 16 KiB; only the last chunk of a level may hold less than 4 KiB. The boundary hash covers the key and not the value, so a changed value never moves a boundary. Levels SHALL be built until one chunk remains, whose address is the state root; the empty map SHALL be one empty leaf. The same map under the same tenant key SHALL always give the same tree and root. Each chunk SHALL be sealed and addressed as the hef-security-and-isolation requirement "State chunks are sealed convergently per tenant" prescribes, and uploaded under `state/{tenant}/{address in lowercase hex}.hsc` with a create-only put in which an object already at the key counts as stored, children before their parents. A lookup SHALL read one chunk per level. A diff of two roots SHALL return every changed key in key order and SHALL NOT read a subtree whose address both trees share. A reader SHALL refuse a chunk that is missing, fails to open, breaks the framing, the CRC or the key order, or sits at any level but one below its parent. Chunks are read through the `StateChunkStore` interface, which the application backs with durable storage (in production, the object store the uploads write to).

#### Scenario: Identical maps give identical roots
- **WHEN** the same state map is built twice under one tenant's key
- **THEN** both builds give the same chunks and the same root, and changing any one value gives a different root

#### Scenario: A one-key change rewrites few chunks
- **WHEN** one key of a 100,000-member room's state is changed, inserted or removed
- **THEN** the new tree holds 1 to 3 chunks the old tree lacks

#### Scenario: A diff skips shared subtrees
- **WHEN** two roots of a 100,000-member room's state that differ in one key are diffed
- **THEN** the diff returns exactly that key after reading at most 6 chunks, out of about 200 in each tree

#### Scenario: Measured against checkpointed delta chains
- **WHEN** a 100,000-member room takes 1,024 state events after its initial state (70% changes to existing memberships, 20% new joins, 10% power-level changes), and the same events are also stored as a delta chain that writes the whole map every 64 frames and only the changed keys between
- **THEN** the prolly tree writes fewer bytes and a cold one-member lookup fetches fewer objects (measured: 13,878 B and 3.00 new chunks per event against 25,076 B per event; 3.00 fetches and 16 KB per lookup against 32.4 fetches and 1.6 MB), while rebuilding the whole state at one frame costs 200 chunk fetches against 32.5 frame fetches on average

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

