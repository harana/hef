# HEF Physical Artifacts — HEJ, HEF, LiveOverlay, and PreparedView Formats

Companion artifact for the `hef-physical-artifacts` capability — the concrete formats, schemas, byte-layouts, and tables referenced by `spec.md`.


### HEJ: Harana Event Journal

HEJ is the durable commit log, protected ingest durability record, and freshness source. It replaces full-payload pre-ack analytical buffers for the event path. Minimal safe-retry indexes may exist, but the event bytes themselves must not be double-written to KV before acknowledgement.

HEJ is optimized for:

```text
small aligned writes
autonomous log flush
autonomous acknowledgement
per-core ownership
lock-free per-worker commit queues
topology-local log stealing where safe
low-load force commit
io_uring submission
io_uring_cmd NVMe passthrough on the journal char device
direct LBA offsets on raw NVMe namespaces
fast replay
header-only CRC-64/NVME fast precheck
BLAKE3 authoritative integrity
segment-level BLAKE3 hash chain
AWUPF-aware atomic frame policy
segment recycling
idempotent safe-retry reconstruction
deterministic conversion into LiveOverlay Arrow RecordBatch segments
```

HEJ is **not** optimized for analytical queries. Normal QueryEngine queries read HEF and LiveOverlay, not raw HEJ frames. HEJ may be used to rebuild missing LiveOverlay ranges for strict fresh queries.

#### HEJ v1 encoding rule

```text
HEJ is not Arrow IPC.
HEJ is not an Arrow file.
HEJ is not an Arrow stream.
HEJ is not a serialized Arrow RecordBatch.
HEJ is not Parquet, JSON Lines, MessagePack stream, Avro container, or any other external analytical file format.

HEJ v1 uses exactly one required payload encoding:

  harana_hej_compact_batch_v1

No other HEJ payload encoding is valid in HEJ v1.

payload_encoding values:
  1 = harana_hej_compact_batch_v1

A HEJ reader must reject any frame whose payload_encoding is not 1.

Arrow is used only after replay, when a valid HEJ frame is decoded into node-local LiveOverlay Arrow RecordBatch segments. Arrow is not the HEJ durability format and is not the HEJ commit protocol.
```

#### HEJ physical layout

```text
A HEJ journal is an ordered sequence of HEJ segments.

A HEJ segment is a fixed-size recyclable allocation containing an ordered byte stream of HEJ frames plus segment metadata.

All integers are little-endian.

All HEJ frames are aligned to 4096 bytes.

A normal HEJ frame length must be exactly one of:

  4096, 8192, 16384, 32768, 65536 bytes.

The default normal HEJ flush target is 16 KiB.

The latency-critical and low-load force-commit target is 4 KiB.

The maximum normal HEJ frame size is 64 KiB.

Normal HEJ frames larger than 64 KiB are forbidden because they recreate the large-write latency spikes that autonomous commit is designed to avoid.

A single event whose encoded payload cannot fit inside a 64 KiB normal frame must be written as a large-event HEJ frame.

The maximum large-event HEJ frame size is 1 MiB.

An ingest event whose encoded event record cannot fit inside a 1 MiB HEJ frame is rejected with payload_too_large unless the owning ingest route has already converted the body into an external immutable payload reference before HEJ append.

A HEJ frame is durable only when the entire aligned frame has been written and verified according to the selected journal durability policy.

A HEJ frame must contain events for exactly one tenant and exactly one epoch.

Event sequences inside a frame are contiguous.

For event ordinal i in the frame:

  sequence = first_sequence + i

The frame is invalid if:

  last_sequence != first_sequence + event_count - 1
```

#### HEJ AWUN/AWUPF, raw-NVMe LBA layout, segment recycling, and hash chain

HEJ records the atomic-write capability of every journal shard.

```text
journal_shard.awupf_bytes
  read from the NVMe Identify AWUPF field (Atomic Write Unit Power Fail) in the
  same startup query that reads AWUN; zero when unknown or unavailable.

journal_shard.atomic_frame_multiple
  max(4096, awupf_bytes) rounded to an allowed HEJ frame size.

frame_atomicity_claim
  valid only when frame_len is a multiple of journal_shard.atomic_frame_multiple
  and the selected backend proves the device honours that AWUPF value.
```

In addition to AWUPF, untorn-write capability comes from the device, queried once at startup:

```text
journal_shard.untorn_write_bytes
  derived from the NVMe Identify Namespace AWUN field (Atomic Write Unit Normal),
  queried via NVMe Admin command at startup;
  zero when AWUN is unreported or the query is unavailable.

untorn write path
  untorn-write claims require AWUN >= 1 confirmed at startup;
  on the io_uring_cmd passthrough path, frame-atomicity claims derive directly from
  AWUN/AWUPF for frames whose frame_len is within the confirmed atomic unit;
  on the filesystem dev fallback, qualifying frames are submitted with RWF_ATOMIC
  where the kernel accepts the atomic submission.

frame_atomicity_claim (io_uring backend)
  valid when AWUN >= 1 was confirmed and the frame is within the atomic unit
  (passthrough path), when the frame was submitted with RWF_ATOMIC and the kernel
  accepted it (dev fallback), OR when the AWUPF-derived rule below holds.
```

The AWUN query is authoritative at the device level, which is the only level that exists on the character-device passthrough path; a filesystem-layer probe such as statx answers for a file, not for the journal device, and is redundant on an NVMe-only target.

If AWUN/AWUPF and RWF_ATOMIC are all unavailable or the backend cannot prove the atomicity contract, HEJ remains correct through BLAKE3 validation and replay truncation. Atomic-write support only reduces torn-write recovery cost; it is not a correctness dependency.

Raw-NVMe direct-LBA journals (written through io_uring_cmd passthrough) use this on-device layout:

```text
JournalDeviceRegion {
  superblock_lba_range
  active_segment_table_lba_range
  recyclable_segment_table_lba_range
  frame_data_lba_ranges[]
}

SegmentDescriptor {
  segment_id
  generation
  state: active | sealed | recyclable | retired
  first_lba
  lba_count
  write_cursor_lba
  first_frame_sequence
  last_frame_sequence
  first_frame_blake3
  last_frame_blake3
  segment_chain_blake3
}
```

The active segment table is the only persistent allocator metadata required for replay. Segment recycling is mandatory for long-running HEJ deployments:

```text
A sealed segment becomes recyclable only after:
  - every frame in the segment is covered by manifest-published HEF files, or by retained replicated journal state;
  - the configured recovery safety window has elapsed;
  - no node-local LiveOverlay rebuild can require the segment;
  - the segment_chain_blake3 has been recorded in journal retention metadata.

A recycled segment increments generation and must zero or overwrite its header before reuse.

Replay must reject a segment when the descriptor generation disagrees with the on-segment generation.
```

Segment-level BLAKE3 hash chain:

```text
frame_blake3[n] = BLAKE3(aligned frame n with frame_blake3 set to zero)
segment_chain_blake3[0] = BLAKE3(segment_id || generation || frame_blake3[0])
segment_chain_blake3[n] = BLAKE3(segment_chain_blake3[n-1] || frame_blake3[n])
```

Replay verifies every frame_blake3 and the final segment_chain_blake3 before advancing the replay cursor beyond the segment.

When an epoch is sealed by EventManager change or crash recovery, any (epoch, sequence) value below the epoch's highest durable frame sequence that is not covered by a durable event frame or void record is a permanently abandoned reservation and is treated as a void range. The sealed epoch's commit_watermark is therefore its highest durable sequence, and replay reconstructs coverage with interior void ranges rather than stalling at the first gap. A sealed epoch does not require runtime void records to be re-derived, because the highest durable sequence already proves the lower ranges were allocated.

#### HEJFrameHeaderV1

Every HEJ frame starts with a fixed 192-byte header.

```text
Offset  Size  Field
0       4     magic = "HEJ1"
4       2     version = 1
6       2     header_len = 192
8       4     frame_len
12      4     payload_len
16      4     page_count
20      4     flags
24      16    tenant_id
40      4     writer_id
44      4     reserved_zero_0
48      8     epoch
56      8     first_sequence
64      8     last_sequence
72      4     event_count
76      4     payload_encoding = 1
80      8     durable_batch_id
88      8     writer_local_batch_id
96      8     schema_generation
104     8     dictionary_generation_hint
112     8     created_at_physical
120     8     committed_at_physical
128     8     header_crc64
136     32    frame_blake3
168     24    reserved_zero_1
```

`created_at_physical` and `committed_at_physical` are internal signed `i64` physical timestamps encoded as nanoseconds since Unix epoch UTC. They are not public schema names.

Flags:

```text
bit 0 = large_event_frame
bits 1..31 = reserved; must be zero in HEJ v1
```

A normal frame is valid only when `frame_len <= 65536` and bit 0 is clear. A large-event frame is valid only when `frame_len <= 1048576` and bit 0 is set.

Checksum and integrity rules:

```text
header_crc64 is computed over bytes 0..191 with header_crc64 and frame_blake3 set to zero, using the CRC-64/NVME algorithm.

HEJ v1 has no payload CRC; the header CRC-64/NVME covers the header only.

frame_blake3 is computed over the entire aligned frame with frame_blake3 set to zero.

segment_chain_blake3 is computed from the ordered frame_blake3 values in the segment descriptor.

CRC-64/NVME is a header-only fast precheck. BLAKE3 is authoritative for header, payload, padding, and segment replay integrity.
```

A frame is valid only when:

```text
magic matches;
version equals 1;
header_len equals 192;
frame_len is a multiple of 4096;
payload_len <= frame_len - 192;
page_count == frame_len / 4096;
payload_encoding == 1;
reserved bytes are zero;
reserved flag bits are zero;
header_crc64 matches;
frame_blake3 matches;
event_count > 0;
last_sequence == first_sequence + event_count - 1.
```

#### harana_hej_compact_batch_v1

The frame payload starts immediately after `HEJFrameHeaderV1`.

```text
The payload consists of:

  HEJCompactBatchHeaderV1
  EventFixedTable
  EventVariableTable
  StringTable
  VariantDictionary
  PayloadArena
  Padding

All section offsets are relative to the start of the payload.

All section offsets must be 8-byte aligned.

All unused padding bytes must be zero.

The payload is invalid if any section overlaps another section, extends beyond payload_len, or has non-zero padding.
```

#### HEJCompactBatchHeaderV1

```text
Offset  Size  Field
0       4     magic = "HCB1"
4       2     version = 1
6       2     header_len = 128
8       4     event_count
12      4     fixed_table_offset
16      4     fixed_table_len
20      4     variable_table_offset
24      4     variable_table_len
28      4     string_table_offset
32      4     string_table_len
36      4     payload_arena_offset
40      4     payload_arena_len
44      4     fixed_record_len = 128
48      4     variable_record_len = 64
52      4     flags
56      8     min_occurred_at_physical
64      8     max_occurred_at_physical
72      8     min_ingested_at_physical
80      8     max_ingested_at_physical
88      8     batch_schema_generation
96      8     batch_dictionary_generation_hint
104     4     variant_dictionary_offset
108     4     variant_dictionary_len
112     16    reserved_zero
```

The batch is invalid if:

```text
event_count != HEJFrameHeaderV1.event_count;
fixed_record_len != 128;
variable_record_len != 64;
magic != "HCB1";
version != 1;
header_len != 128;
reserved_zero contains non-zero bytes.
```

#### EventFixedRecordV1

There is exactly one `EventFixedRecordV1` per event. The record length is exactly 128 bytes.

Record `i` corresponds to:

```text
sequence = HEJFrameHeaderV1.first_sequence + i
```

```text
Offset  Size  Field
0       16    event_id
16      8     stream_id
24      8     stream_sequence
32      8     occurred_at_physical
40      8     ingested_at_physical
48      8     entity_id_hash_low
56      8     entity_id_hash_high
64      8     actor_id_hash_low
72      8     account_id_hash_low
80      8     trace_id_hash_low
88      8     dedupe_hash_low
96      8     dedupe_hash_high
104     4     source_string_ref
108     4     event_type_string_ref
112     4     entity_type_string_ref
116     4     schema_version
120     4     flags
124     4     variable_record_index
```

`occurred_at_physical` and `ingested_at_physical` are internal signed `i64` physical timestamps encoded as nanoseconds since Unix epoch UTC. They are not public schema names.

`source_string_ref`, `event_type_string_ref`, and `entity_type_string_ref` are indexes into `StringTableV1`.

`variable_record_index` must equal the event ordinal `i` in HEJ v1.

A frame is invalid if any string ref is out of range.

#### EventVariableRecordV1

There is exactly one `EventVariableRecordV1` per event. The record length is exactly 64 bytes.

Record `i` contains payload and optional display/entity references for `EventFixedRecordV1 i`.

```text
Offset  Size  Field
0       4     payload_flags
4       4     payload_offset
8       4     payload_len
12      4     entity_id_string_ref
16      4     actor_id_string_ref
20      4     account_id_string_ref
24      4     source_schema_ref
28      4     source_delivery_ref
32      8     connector_delivery_hash_low
40      8     connector_delivery_hash_high
48      8     reserved_zero_0
56      8     reserved_zero_1
```

```text
payload_flags:
  bit 0 = external_immutable_payload_ref
  bits 1..31 = reserved; must be zero in HEJ v1
```

There is no payload format field. `harana_variant_v1` is the only payload format; presence and the external-reference case are derived:

`payload_offset` and `payload_len` select bytes inside `PayloadArenaV1`.

`payload_len == 0` means no payload; `payload_offset` and `payload_flags` must then be zero.

When `payload_len > 0` and bit 0 is clear, the selected bytes are one `harana_variant_v1` value whose field ids resolve against the frame `VariantDictionaryV1`.

When bit 0 is set, the selected bytes must contain a UTF-8 encoded internal immutable payload reference. The reference is internal and must not be public output.

`entity_id_string_ref`, `actor_id_string_ref`, `account_id_string_ref`, `source_schema_ref`, and `source_delivery_ref` use `StringTableV1` indexes. `source_schema_ref` optionally records source-format lineage (e.g. `protobuf:sha256:...`) for a transcoded payload.

The value `0xFFFFFFFF` means absent.

`reserved_zero_0` and `reserved_zero_1` must be zero.

#### StringTableV1

`StringTableV1` is a sequence of UTF-8 strings.

```text
Offset  Size  Field
0       4     string_count
4       4     offsets_len
8       N     offsets: u32[string_count + 1]
8+N     M     utf8_data
```

```text
offsets[0] must be 0.
offsets[string_count] must equal len(utf8_data).

For string k:

  start = offsets[k]
  end   = offsets[k + 1]

The string is utf8_data[start..end].

Strings must be valid UTF-8.

String refs are zero-based indexes into this table.

The table must contain the exact source, event type, and entity type strings needed to replay every event in the frame.

Dictionary IDs are not authoritative in HEJ v1. Replay correctness must use the string table values, not dictionary_generation_hint.
```

#### VariantDictionaryV1

`VariantDictionaryV1` is the frame-level shared Variant key dictionary. Every `harana_variant_v1` value in the frame resolves field ids against this single dictionary.

```text
Offset  Size  Field
0       4     key_count
4       4     flags
8       N     offsets: u32[key_count + 1]
8+N     M     utf8_keys
```

```text
flags bit 0 = sorted_keys; must be set in HEJ v1.
flags bits 1..31 = reserved; must be zero in HEJ v1.

offsets[0] must be 0.
offsets[key_count] must equal len(utf8_keys).

Keys must be valid UTF-8, unique, and sorted ascending bytewise.

field_id k denotes key utf8_keys[offsets[k]..offsets[k + 1]].

Key-to-field_id resolution uses binary search over the sorted keys.

The dictionary is self-contained per frame. Decoding a frame's payload values must not
require any other frame, segment, or external dictionary generation.
```

#### harana_variant_v1 value encoding

```text
harana_variant_v1 follows the Parquet Variant value encoding for value bytes, with this
deviation: no metadata dictionary is embedded in the value. Field ids reference the
external shared dictionary (VariantDictionaryV1 in HEJ; variant dictionary blocks in HEF).

Objects store a sorted field_id array plus a value offset array; field lookup is a binary
search over field ids followed by one offset jump. Arrays store an offset array; element
lookup is O(1) by index. Scalars store a type tag plus native bytes.

Extracting one path must not require decoding, allocating, or validating sibling fields.

Per-object offset widths (1, 2, or 4 bytes) follow the Parquet Variant encoding rules.

A value is invalid if any field id is out of range for the governing dictionary, if any
offset escapes the value bounds, or if object field ids are not strictly ascending.
```

#### PayloadArenaV1

```text
PayloadArenaV1 is a byte array selected by EventVariableRecordV1 payload_offset and payload_len.

Unless the external_immutable_payload_ref flag is set, the selected bytes are one
canonical harana_variant_v1 value produced by ingest transcoding (see the
hef-logical-event-model capability).

PayloadArenaV1 does not contain nested HEJ frames.

PayloadArenaV1 does not contain Arrow IPC.

PayloadArenaV1 does not contain HEF blocks.

PayloadArenaV1 does not contain source-format bytes. The canonical transcoded value is
authoritative; frame BLAKE3 covers it like every other frame byte.

Payload bytes are not public output. Public output may include only owner-approved decoded or redacted fields.
```

#### Mandatory HEJ-to-LiveOverlay conversion

The following conversion contract is part of the public HEJ v1 specification. An external reader that validates HEJ frames and implements this mapping must produce byte-equivalent Arrow logical values and row order. Implementations may use different internal builders, but they must not change field names, Arrow types, nullability, sequence derivation, payload_ref derivation, or ordering.

A valid HEJ frame must decode into exactly one LiveOverlay Arrow `RecordBatch` unless the frame is skipped because its complete sequence range is already covered by the manifest-published HEF snapshot selected for the reader.

The LiveOverlay segment may be materialized either as this Arrow `RecordBatch` or as an equivalent Vortex compressed array that is logically equal to it under the fixed v1 schema — the same rows, values, and row order. The choice between the two representations is a deterministic function of the HEJ sequence range (never of hardware, workload, or feature flags), so any node that rebuilds a given range produces byte-identical segment bytes for the representation it selects, and the two are interchangeable for queries.

The LiveOverlay RecordBatch schema is fixed in v1:

```text
Column name              Arrow type
tenant_id                FixedSizeBinary(16)
epoch                    UInt64
sequence                 UInt64
event_id                 FixedSizeBinary(16)
stream_id                UInt64
stream_sequence          UInt64
occurred_at              Timestamp(Nanosecond, UTC)
ingested_at              Timestamp(Nanosecond, UTC)
source                   Utf8
event_type               Utf8
entity_type              Utf8
entity_id_hash_low       UInt64
entity_id_hash_high      UInt64
entity_id                Utf8 nullable
actor_id_hash_low        UInt64
actor_id                 Utf8 nullable
account_id_hash_low      UInt64
account_id               Utf8 nullable
trace_id_hash_low        UInt64
schema_version           UInt32
payload_flags            UInt32
payload_ref              UInt64
flags                    UInt32
dedupe_hash_low          UInt64
dedupe_hash_high         UInt64
```

Conversion rules:

```text
sequence = first_sequence + row_index.

source, event_type, and entity_type are decoded from StringTableV1.

entity_id, actor_id, and account_id are null when their string ref is 0xFFFFFFFF.

payload_flags is copied verbatim from EventVariableRecordV1. Bit 0 = external_immutable_payload_ref. payload_len == 0 in the source record means no payload. All other payloads are harana_variant_v1.

payload_ref = (payload_len as u64 << 32) | payload_offset. A payload-less row is exactly 0; a payload at arena offset 0 is distinguished by its non-zero length in the high half.

payload_ref is valid only inside the LiveOverlay segment created from this HEJ frame.

The LiveOverlay segment metadata must include frame_blake3, segment_chain_blake3, durable_batch_id, epoch, first_sequence, last_sequence, source HEJ segment id/generation, payload arena bytes, and VariantDictionaryV1 bytes.

The RecordBatch must preserve row order exactly as HEJ sequence order.

A reader must reject a LiveOverlay segment if the Arrow row count differs from HEJFrameHeaderV1.event_count.
```

No implementation may choose a different HEJ encoding based on convenience, deployment profile, hardware, workload, feature flag, or benchmark result.

#### HEJ and KV boundary

```text
HEJ is the durable event replay source.

HEJ is not a KV table.

HEJ must not store mutable service lifecycle state, rule definitions, prepared-view readiness, dashboard state, memory lifecycle, chat session state, notification delivery state, insight state, or access/session state.

Safe-retry KV rows may point to HEJ positions.

Safe-retry KV rows are mutable indexes.

HEJ event records are immutable replay records.

If a safe-retry KV row is missing after recovery, it may be rebuilt from HEJ dedupe_hash fields and sequence positions.

If a safe-retry KV row disagrees with HEJ event bytes, HEJ wins for event replay and the safe-retry row must be repaired or discarded according to the owner recovery rule.
```

#### HEJ autonomous-commit alignment

HEJ v1 adopts the autonomous-commit write discipline from the latency paper, adapted to Harana's append-only event log.

Required state model:

```text
READY
  The ingest route has validated the event and serialized it into a per-worker pending HEJ record.
  The pending record is not durable and has no public visibility.

HARDENED
  A worker has claimed the pending record, assigned its final (epoch, sequence), written the containing HEJ frame,
  and verified the selected durability policy.

COMMITTED
  For ordinary append-only event ingest, HARDENED and COMMITTED are the same state.
  For routes that also mutate dependent transactional state, COMMITTED additionally requires the route's dependency
  condition to be satisfied by the autonomous acknowledgement logic below.
```

Hot-path requirements:

```text
There is no global group-commit writer for HEJ.
There is no single global commit-acknowledgement thread for HEJ.
Each ingest worker owns a local serialized commit queue.
Each queue is single-producer/single-consumer for its normal owner and must be implemented as a bounded circular queue.
Serialized pending records must be cache-line aligned to avoid false sharing.
Queue entries are variable-sized serialized descriptors, not heap-allocated transaction objects.
Normal queue release is by advancing the queue head, not by per-event deallocation.
```

Flush-unit policy:

```text
Allowed normal flush units: 4 KiB, 8 KiB, 16 KiB, 32 KiB, 64 KiB.
Default all-round flush target: 16 KiB.
Latency-critical target: 4 KiB.
Low-load force-commit target: 4 KiB.
Normal frames above 64 KiB are invalid.
The writer must not accumulate multi-megabyte commit batches on the acknowledgement path.
```

Sequence assignment rule:

```text
Pending records do not receive final epoch/sequence values until a worker successfully claims them for an HEJ frame.
The claiming worker reserves one contiguous (epoch, sequence) range for the frame.
Rows are written in frame order and receive sequence = first_sequence + row_index.
This preserves the HEJ invariant that every normal frame contains one tenant, one epoch, and a contiguous sequence range.
A reservation is a lease, not a permanent claim (see reservation leases and void records below), so a stalled or crashed worker cannot stall the contiguous watermark beyond its lease.
```

Reservation leases and void records:

```text
A sequence reservation is a lease, not a permanent claim. A reserved (epoch, sequence) range that is not hardened into a durable HEJ frame before its lease deadline is abandoned, and the system commits an internal HEJ void record covering that exact range so the contiguous commit_watermark can advance past it. The lease deadline is an internal multiple of the low-load force-commit interval, selected by benchmark gates, never an operator knob.

A void record is an internal HEJ frame carrying zero events, a void flag, and the abandoned (epoch, first_sequence, last_sequence) range. It participates in the segment BLAKE3 hash chain exactly like an event frame.
A void record is not a user event, is not a HEF row, is not visible in QueryEngine, and is never public output, the same boundary as barrier transactions.
For one (epoch, sequence) range a void record and an event frame are mutually exclusive; durable commit order arbitrates and the loser is rejected on detection. A worker whose reservation lease has expired must re-reserve a fresh range rather than harden a frame under the stale range.
Voided sequences are permanently skipped. Because sequence is internal-only and never public, a skipped range is observable only as empty coverage to cursors, pruning, and disjointness checks.
```

Topology-local log stealing is allowed only under these rules:

```text
A worker may steal only pending records belonging to the same tenant and epoch as the frame being assembled.
A worker may steal only within its CPU-topology steal group by default, for example cores sharing the same L3 cache.
The target queue exposes an atomic clean cursor and a non-atomic dirty cursor protected by the target queue owner.
The stealing worker copies bytes from clean_cursor..dirty_cursor, then CASes clean_cursor to dirty_cursor.
If the CAS fails, the copied bytes are discarded and must not be written.
If the CAS succeeds, the stealing worker owns those pending records and may include them in its HEJ frame.
writer_id in HEJFrameHeaderV1 identifies the frame-flushing worker, not necessarily the original ingest worker.
Out-of-order stolen-frame completions must publish durability through per-source-queue hardened cursors so a later stolen range cannot make an earlier unstolen range appear durable.
```

Low-load force commit is mandatory:

```text
A worker must not wait indefinitely for a full 16 KiB frame during trickle traffic.
When the worker predicts an idle period, it must force a 4 KiB-aligned HEJ frame if pending records exist.
The internal maximum idle target is 50 microseconds unless benchmark gates prove a better value on the deployment class.
The trigger must be probabilistic or jittered so workers do not synchronize into periodic write bursts.
This is an internal runtime policy, not an operator tuning knob.
```

Acknowledgement logic:

```text
Append-only event ingest has no page-level read/write dependency graph; after HEJ durability, the event is committed.
Routes that also perform dependent mutable state changes must use the autonomous acknowledgement variant:
  - workers acknowledge their own queues or a small acknowledgement group;
  - acknowledgement group size is selected automatically from benchmarked values 1, 2, 4, or 8;
  - group size 2 or 4 is the default safe choice unless workload benchmarks select otherwise;
  - dependency checking must not be centralized in one background thread.
```

Dependency-tracking boundary:

```text
Do not add GSN, RFA, dependency-vector, or barrier metadata to ordinary append-only event records.
For pure event append, the dependency condition is trivially true after HEJ durability.
For routes with real transactional dependencies, use a volatile commit_order_number equivalent to the paper's GSN.
Remote Flush Avoidance may be used when all dependencies are local to the worker's already-hardened range.
Barrier transactions may be generated only to advance a stalled dependency watermark.
Barrier transactions are not user events, are not HEF rows, are not visible in QueryEngine, and are not public output.
If dependency metadata is persisted for recovery, it must be internal-only and must not be stored in the KV store as a duplicate event payload.
```

#### HEJ write and acknowledgement rule

An event may be acknowledged only after the HEJ frame containing that event is durable.

For local-only NVMe passthrough or direct-I/O mode:

```text
durable = full aligned HEJ frame write completed to the selected direct journal target
        + storage completion received
        + header_crc64 verified
        + frame_blake3 verified
        + segment hash-chain position verified
```

For buffered-filesystem fallback mode:

```text
durable = full aligned HEJ frame write completed
        + fdatasync/fsync or equivalent durability barrier completed
        + header_crc64 verified
        + frame_blake3 verified
        + segment hash-chain position verified
```

Buffered-filesystem fallback mode (non-NVMe dev environments only) is correct but outside the microsecond-latency claim.

For replicated mode:

```text
durable = local durable
        + required replica or quorum durable acknowledgement for the same frame_blake3 and segment_chain_blake3 position
```

Publishing the decoded row into LiveOverlay may happen before or after client acknowledgement depending on the route's read-after-ack visibility mode.

If strict read-after-ack query visibility is required, the acknowledgement path must also wait until the decoded row is published into node-visible LiveOverlay or until the query path is guaranteed to block until that sequence is replayed.

HEF publication is never required for acknowledgement.

For ordinary append-only event ingest, the event may be acknowledged as soon as the HEJ frame is durable and the route's safe-retry receipt has been recorded or made reconstructable. For routes with transactional dependencies, durability only moves the event to HARDENED; acknowledgement waits for autonomous dependency acknowledgement as defined in `HEJ autonomous-commit alignment`.

io_uring_cmd NVMe passthrough fits this design because it exposes asynchronous NVMe submission and completion through per-core rings, with workers polling completions rather than blocking in the kernel. A given ring is used by exactly one worker thread at a time, which matches the per-core writer model, and the device stays bound to the kernel NVMe driver (no unbind, no second driver stack).

### HEF: Harana Event File

HEF is the immutable query file selected by the manifest.

It is:

```text
immutable
self-describing
range-readable
footer-indexed
columnar for envelope and promoted fields
payload-complete: every event row resolves to its canonical harana_variant_v1 value
metadata-rich for pruning and aggregation
QueryEngine-oriented
chat/investigation-context friendly
```

HEF has one file format. Files may differ in size, physical layout kind, optional index blocks, optional aggregate blocks, optional context blocks, and optional internal acceleration blocks, but these differences are declared in the file feature directory and do not create separate HEF formats. A reader must not infer behavior from human lifecycle labels. The planner must inspect the HEF footer and feature directory.

HEF uses a trailing footer because data can be written in a single pass and readers can start by loading metadata to locate only the needed chunks. This mirrors the proven layout used by Parquet, where file metadata is written after data and readers first read metadata to locate relevant column chunks.

Reference: <https://parquet.apache.org/docs/file-format/>

### LiveOverlay: queryable durable HEJ ranges not yet covered by HEF

LiveOverlay is the queryable representation of events that are:

```text
durable in HEJ;
validated and replayable;
not yet covered by a manifest-published HEF file;
not removed by a visible HEF-native deletion vector;
needed for fresh committed-event queries.
```

LiveOverlay is normally in memory and Arrow-native:

```text
immutable Arrow RecordBatch segments
string columns use Arrow Utf8View/BinaryView (German-style strings) so predicate
  evaluation can compare inline 4-byte prefixes without buffer chasing and decode
  can be zero-copy from validated frame buffers
low-cardinality promoted columns may use Arrow run-end encoded (REE) arrays
per-segment epoch/sequence ranges
per-segment occurred_at / ingested_at min-max
source/type/entity dictionaries
optional Ribbon or split-block Bloom filters for entity_id, account_id, actor_id, event_id
optional Memento-style dynamic range filters for entity_id_hash and occurred_at buckets
optional bitmap summaries for low-cardinality promoted columns
optional exact live aggregate deltas
optional context projection batches for chat/investigation tools
```

LiveOverlay is **not** the durability source. HEJ remains the source of truth until a manifest-published HEF file covers the same journal range. If the process crashes, in-memory Arrow batches can be lost and must be rebuilt from HEJ replay or replicated journal fragments.

Required lifecycle:

```text
HEJ durable frame
  -> validate header CRC-64/NVME, frame BLAKE3, and segment BLAKE3 chain
  -> decode harana_hej_compact_batch_v1 into LiveOverlay Arrow RecordBatch using the public deterministic mapping
  -> publish immutable Arrow RecordBatch segment into EventManager LiveOverlay
  -> replicate or expose committed journal ranges for node-local rebuild
  -> publish node-local LiveOverlay segment after validation
  -> expose segment through QueryEngine LiveOverlay scan
  -> later publish covered HEJ range into HEF
  -> atomically publish HEF manifest entry, coverage, and watermarks
  -> evict LiveOverlay segments only after published HEF coverage is visible
```

Node-local LiveOverlay is required. Nodes must not forward event reads to the tenant's EventManager to obtain LiveOverlay rows. A node may serve strict fresh queries only for ranges it has rebuilt, validated, and published into its own LiveOverlay snapshot. If node-local LiveOverlay is behind, the query must wait, rebuild missing ranges, or serve only an explicit bounded-staleness mode.

LiveOverlay may have a temporary overflow tier for memory pressure or HEF publication lag:

```text
memory LiveOverlay:     in-memory Arrow RecordBatch segments
overflow LiveOverlay:   Arrow IPC / mmap / local temporary columnar segments
durable source:         HEJ
HEF source:             manifest-published HEF
```

The overflow tier is a performance and memory-pressure mechanism only. It is not authoritative and must be reconstructable from HEJ.

Required eviction rule:

```text
A LiveOverlay segment may be evicted only when its complete (epoch, sequence) range
is covered by a manifest-published HEF file visible to future query snapshots.
```

The implementation must track three explicit watermarks:

```text
commit_watermark
  highest (epoch, sequence) such that every lower sequence in the epoch is durably covered by an event frame or a committed void range. A reserved-but-abandoned range — closed at runtime by a void record on lease expiry, or sealed as an interior gap on epoch seal — counts as covered, so a single stalled or crashed worker cannot stall the watermark beyond its reservation lease.

visibility_watermark
  highest (epoch, sequence) published into node-local LiveOverlay or manifest-published HEF and safe for the selected query mode, with void ranges counting as covered for contiguity (they publish zero rows).

snapshot_watermark
  stable (epoch, sequence) upper bound captured by a QuerySnapshot. It is <= visibility_watermark.
```

Derived compatibility cursors:

```text
durable_journal_cursor compatibility alias = commit_watermark
live_queryable_cursor compatibility alias  = LiveOverlay component of visibility_watermark
```

If the public API requires read-after-ack query visibility, acknowledgement must either wait until the event is included in visibility_watermark, or strict fresh queries must wait for LiveOverlay to catch up to the selected commit_watermark. The system must never silently omit acknowledged HEJ events not yet HEF-covered from a fresh query.

### PreparedView and materialized aggregate outputs

HEF physical aggregate metadata accelerates generic event queries. PreparedView outputs accelerate semantic product metrics and repeated Revenue Intelligence queries.

```text
HEF aggregate metadata:
  file-local physical aggregates over event columns and promoted fields.

PreparedView output:
  product-level materialization with metric definition version, semantic lineage,
  input coverage, quality status, data-status, and readiness.
```

PreparedView outputs are trusted only when the owning catalogue row says the output is ready and its input coverage, checksum, schema version, and replay anchor match. The committed event snapshot plus catalogue row remains the rebuild source.

PreparedView maintenance is incremental by default:

```text
maintenance model
  DBSP-style incremental view maintenance over Z-set deltas;
  inputs are signed row deltas: +1 for newly HEF-published or LiveOverlay-visible
  rows, -1 for deletion-vector effects, paired -1/+1 for correction effects.

delta sources
  manifest generation transitions (new HEF coverage),
  deletion_vector_generation transitions,
  correction_overlay_generation transitions,
  LiveOverlay segment publication when the view opts into fresh inputs.

equivalence rule
  incremental output must equal full recomputation from the same committed event
  snapshot; this equivalence is benchmark- and test-gated, and any view whose
  operators are not incrementalizable falls back to full recompute.

catalogue fields
  each PreparedView records its last applied (manifest_generation,
  deletion_vector_generation, correction_overlay_generation, snapshot_watermark)
  so maintenance is idempotent and restartable.
```

---
