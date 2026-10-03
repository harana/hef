//! Each worker's hand-off buffer between accepting an event and writing it to disk: a bounded ring of pending records
//! that one nearby worker may steal from.
//!
//! Entries are variable-sized serialized pending records, cache-line aligned to avoid false sharing, in a bounded
//! circular byte buffer. Storage is reclaimed by advancing the head, not by freeing each event. The queue exposes a
//! clean cursor and an owner-private dirty cursor; topology-local stealing copies the bytes between them and then
//! claims the region by advancing the clean cursor — a stale claim (the cursor moved since the copy) fails and the
//! copied bytes are discarded. Every queue in a steal group is driven from one thread, so claims are serialized and
//! need no atomics; the compare on the clean cursor is what rejects a stale claim. Durability cursors advance only
//! contiguously, so a later completion cannot make an earlier still-in-flight region look durable.
//!
//! It is a deterministic state machine driven through the simulation interfaces.

use super::error::QueueError;
use crate::artifacts::batch::{EncodedPayload, EventInput, PayloadInput};
use crate::error::FormatError;
use crate::events::provenance::{SignatureScheme, SignedEventProvenance};
use crate::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef};
use crate::events::variant::{EncodeScratch, KeyDictionary, VariantRef, encode_value_with_scratch};
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::file::bytes::{Reader, Writer};
use uuid::Uuid;

/// Records are aligned to the cache line.
pub const RECORD_ALIGN: u64 = 64;

/// Starting buffer size for one serialized pending record: enough for the fixed descriptor plus typical identifier
/// strings, so encoding a record rarely regrows its buffer.
const PENDING_RECORD_CAPACITY_HINT: usize = 256;

/// A queue's buffer never shrinks below this, so a caller-requested capacity of near zero still gets a usable buffer.
const MIN_QUEUE_CAPACITY_BYTES: u64 = 1024;

/// The byte length of a provenance public key or protocol event id, once decoded from hex.
const PROTOCOL_ID_BYTES: usize = 32;

/// The byte length of a BIP-340 schnorr signature.
const PROTOCOL_SIGNATURE_BYTES: usize = 64;

/// Lower bound on one encoded relationship reference: two length-prefixed strings (kind, value), at least their 4-byte
/// length header each even when both are empty. Used only to cap a hostile/corrupt count from over-reserving.
const RELATIONSHIP_REF_MIN_BYTES: usize = 8;

/// One pending record: the worker-owned serialized descriptor between READY and frame claim. `tenant_id`/`epoch` ride
/// along so stealing can enforce the same-tenant/same-epoch rule without decoding payloads.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRecord {
    pub epoch: u64,
    pub event: EventInput,
    pub tenant_id: TenantId,
}

fn put_optional_string(out: &mut Writer, value: Option<&str>) {
    match value {
        None => out.put_u32(u32::MAX),
        Some(value) => {
            out.put_u32(value.len() as u32);
            out.put_slice(value.as_bytes());
        }
    }
}

fn read_optional_string(reader: &mut Reader<'_>) -> Result<Option<String>, FormatError> {
    let len = reader.u32("optional string length")?;
    if len == u32::MAX {
        return Ok(None);
    }
    let bytes = reader.take(len as usize, "optional string")?;
    let text = std::str::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
        what: "pending record string",
    })?;
    Ok(Some(text.to_owned()))
}

fn read_required_string(reader: &mut Reader<'_>) -> Result<String, FormatError> {
    read_optional_string(reader)?.ok_or(FormatError::Structural {
        rule: "required string absent in pending record",
    })
}

/// Writes one payload: the dictionary its field ids resolve against, then the value bytes governed by it.
fn put_encoded_payload(out: &mut Writer, dictionary: &KeyDictionary, bytes: &[u8]) {
    out.put_u32(dictionary.key_count());
    for key in dictionary.keys() {
        put_optional_string(out, Some(key));
    }
    out.put_u32(bytes.len() as u32);
    out.put_slice(bytes);
}

/// Serializes one pending record (internal `PendingRecordV1` descriptor). The payload variant value is encoded against
/// a record-local dictionary so the descriptor is self-contained; the frame-level dictionary is rebuilt at claim time.
pub fn encode_pending(record: &PendingRecord) -> Result<Vec<u8>, FormatError> {
    let mut out = Writer::with_capacity(PENDING_RECORD_CAPACITY_HINT);
    encode_pending_into(record, &mut EncodeScratch::new(), &mut out)?;
    Ok(out.into_bytes())
}

/// Appends [`encode_pending`]'s bytes to `out` instead of returning them, so a caller serializing a whole batch pays
/// one buffer for the batch rather than one per record. `scratch` supplies the payload encoder's container buffers,
/// reused (not reallocated) across every record a caller serializing many records passes the same scratch to.
fn encode_pending_into(
    record: &PendingRecord,
    scratch: &mut EncodeScratch,
    out: &mut Writer,
) -> Result<(), FormatError> {
    out.put_u128(record.tenant_id.uuid().as_u128());
    out.put_u64(record.epoch);
    let envelope = &record.event.envelope;
    out.put_u128(envelope.event_id.uuid().as_u128());
    out.put_u64(envelope.stream_id.0);
    out.put_u64(envelope.stream_sequence);
    out.put_i64(envelope.occurred_at.physical_nanos());
    out.put_i64(envelope.ingested_at.physical_nanos());
    out.put_u64(envelope.entity_id_hash_low);
    out.put_u64(envelope.entity_id_hash_high);
    out.put_u64(envelope.actor_id_hash_low);
    out.put_u64(envelope.account_id_hash_low);
    out.put_u64(envelope.trace_id_hash_low);
    out.put_u64(envelope.dedupe_hash_low);
    out.put_u64(envelope.dedupe_hash_high);
    out.put_u32(envelope.schema_version);
    out.put_u32(envelope.flags.0);
    out.put_u64(record.event.connector_delivery_hash_low);
    out.put_u64(record.event.connector_delivery_hash_high);
    put_optional_string(out, Some(&envelope.source));
    put_optional_string(out, Some(&envelope.event_type));
    put_optional_string(out, Some(&envelope.entity_type));
    put_optional_string(out, envelope.entity_id.as_deref());
    put_optional_string(out, envelope.actor_id.as_deref());
    put_optional_string(out, envelope.account_id.as_deref());
    put_optional_string(out, record.event.source_schema.as_deref());
    put_optional_string(out, record.event.source_delivery.as_deref());
    match &record.event.payload {
        PayloadInput::Encoded(payload) => {
            out.put_u8(1);
            put_encoded_payload(out, &payload.dictionary, &payload.bytes);
        }
        PayloadInput::None => out.put_u8(0),
        PayloadInput::Variant(value) => {
            out.put_u8(1);
            let mut keys = std::collections::BTreeSet::new();
            value.collect_keys(&mut keys);
            let dictionary = KeyDictionary::build(keys.into_iter().map(str::to_owned));
            let encoded = encode_value_with_scratch(value, &dictionary, scratch)?;
            put_encoded_payload(out, &dictionary, &encoded);
        }
        PayloadInput::ExternalRef(reference) => {
            out.put_u8(2);
            put_optional_string(out, Some(reference));
        }
    }
    match &record.event.provenance {
        None => out.put_u8(0),
        Some(provenance) => {
            out.put_u8(1);
            out.put_slice(&provenance.author_pubkey);
            out.put_slice(&provenance.protocol_event_id);
            out.put_slice(&provenance.signature);
            out.put_i64(provenance.claimed_at.physical_nanos());
            out.put_u32(provenance.protocol_kind);
            put_optional_string(out, Some(provenance.scheme.as_str()));
        }
    }
    match &record.event.relationships {
        None => out.put_u8(0),
        Some(relationships) => {
            out.put_u8(1);
            out.put_u32(relationships.refs().len() as u32);
            let mut scratch = String::new();
            for reference in relationships.refs() {
                put_optional_string(out, Some(reference.kind.as_str()));
                scratch.clear();
                reference.column_value_into(&mut scratch);
                put_optional_string(out, Some(&scratch));
            }
        }
    }
    Ok(())
}

/// Reads just the tenant id and epoch from the front of an encoded pending record — the fixed `tenant_id: u128`
/// then `epoch: u64` layout [`encode_pending_into`] always writes first — without decoding the rest of the record.
/// The steal path uses this to check the same-tenant/same-epoch rule on raw bytes, so a record that turns out not to
/// match is never fully decoded for nothing.
fn peek_tenant_epoch(bytes: &[u8]) -> Option<(TenantId, u64)> {
    let tenant_id = TenantId::from_uuid(Uuid::from_u128(u128::from_le_bytes(bytes.get(0..16)?.try_into().ok()?)));
    let epoch = u64::from_le_bytes(bytes.get(16..24)?.try_into().ok()?);
    Some((tenant_id, epoch))
}

/// Deserializes one pending record.
pub fn decode_pending(bytes: &[u8]) -> Result<PendingRecord, FormatError> {
    let mut reader = Reader::new(bytes);
    let tenant_id = TenantId::from_uuid(Uuid::from_u128(reader.u128("tenant_id")?));
    let epoch = reader.u64("epoch")?;
    let event_id = EventId::from_uuid(Uuid::from_u128(reader.u128("event_id")?));
    let stream_id = StreamId(reader.u64("stream_id")?);
    let stream_sequence = reader.u64("stream_sequence")?;
    let occurred_at = TimestampValue::from_physical_nanos(reader.i64("occurred_at")?);
    let ingested_at = TimestampValue::from_physical_nanos(reader.i64("ingested_at")?);
    let entity_id_hash_low = reader.u64("entity_id_hash_low")?;
    let entity_id_hash_high = reader.u64("entity_id_hash_high")?;
    let actor_id_hash_low = reader.u64("actor_id_hash_low")?;
    let account_id_hash_low = reader.u64("account_id_hash_low")?;
    let trace_id_hash_low = reader.u64("trace_id_hash_low")?;
    let dedupe_hash_low = reader.u64("dedupe_hash_low")?;
    let dedupe_hash_high = reader.u64("dedupe_hash_high")?;
    let schema_version = reader.u32("schema_version")?;
    let flags = EventFlags(reader.u32("flags")?);
    let connector_delivery_hash_low = reader.u64("connector_delivery_hash_low")?;
    let connector_delivery_hash_high = reader.u64("connector_delivery_hash_high")?;
    let source = read_required_string(&mut reader)?;
    let event_type = read_required_string(&mut reader)?;
    let entity_type = read_required_string(&mut reader)?;
    let entity_id = read_optional_string(&mut reader)?;
    let actor_id = read_optional_string(&mut reader)?;
    let account_id = read_optional_string(&mut reader)?;
    let source_schema = read_optional_string(&mut reader)?;
    let source_delivery = read_optional_string(&mut reader)?;
    let payload = match reader.u8("payload kind")? {
        0 => PayloadInput::None,
        1 => {
            let key_count = reader.u32("dictionary key count")?;
            let mut keys = Vec::with_capacity(key_count as usize);
            for _ in 0..key_count {
                keys.push(read_required_string(&mut reader)?);
            }
            let dictionary = KeyDictionary::from_sorted_unique(keys)?;
            let len = reader.u32("payload length")? as usize;
            let bytes = reader.take(len, "payload value")?;
            // The payload is carried on in the form it was stored in — the frame it is bound for re-encodes it against
            // the frame dictionary — so it is checked here rather than proven decodable by being decoded.
            VariantRef::new(bytes).validate(&dictionary)?;
            PayloadInput::Encoded(EncodedPayload {
                bytes: bytes.to_vec(),
                dictionary,
            })
        }
        2 => PayloadInput::ExternalRef(read_required_string(&mut reader)?),
        _ => {
            return Err(FormatError::Structural {
                rule: "unknown pending payload kind",
            });
        }
    };
    let provenance = match reader.u8("provenance presence")? {
        0 => None,
        1 => {
            let author_pubkey = reader
                .take(PROTOCOL_ID_BYTES, "author_pubkey")?
                .try_into()
                .map_err(|_| FormatError::Truncated { what: "author_pubkey" })?;
            let protocol_event_id = reader
                .take(PROTOCOL_ID_BYTES, "protocol_event_id")?
                .try_into()
                .map_err(|_| FormatError::Truncated {
                    what: "protocol_event_id",
                })?;
            let signature = reader
                .take(PROTOCOL_SIGNATURE_BYTES, "signature")?
                .try_into()
                .map_err(|_| FormatError::Truncated { what: "signature" })?;
            let claimed_at = TimestampValue::from_physical_nanos(reader.i64("claimed_at")?);
            let protocol_kind = reader.u32("protocol_kind")?;
            let tag = read_required_string(&mut reader)?;
            let scheme = SignatureScheme::from_str(&tag).ok_or(FormatError::Structural {
                rule: "unknown signature scheme in a pending record",
            })?;
            Some(SignedEventProvenance {
                author_pubkey,
                claimed_at,
                protocol_event_id,
                protocol_kind,
                scheme,
                signature,
            })
        }
        _ => {
            return Err(FormatError::Structural {
                rule: "unknown pending provenance presence byte",
            });
        }
    };
    let relationships = match reader.u8("relationships presence")? {
        0 => None,
        1 => {
            let count = reader.u32("relationship count")? as usize;
            let mut refs = Vec::with_capacity(reader.capacity_hint(count, RELATIONSHIP_REF_MIN_BYTES));
            for _ in 0..count {
                let kind_tag = read_required_string(&mut reader)?;
                let kind = RelationshipKind::from_str(&kind_tag).ok_or(FormatError::Structural {
                    rule: "unknown relationship kind in a pending record",
                })?;
                let value = read_required_string(&mut reader)?;
                refs.push(
                    RelationshipRef::parse_column_value(kind, &value).map_err(|_| FormatError::Structural {
                        rule: "malformed relationship reference in a pending record",
                    })?,
                );
            }
            Some(EventRelationships::new(refs).map_err(|_| FormatError::Structural {
                rule: "an event may declare at most one parent and one root reference",
            })?)
        }
        _ => {
            return Err(FormatError::Structural {
                rule: "unknown pending relationships presence byte",
            });
        }
    };
    Ok(PendingRecord {
        tenant_id,
        epoch,
        event: EventInput {
            envelope: EventEnvelope {
                event_id,
                tenant_id,
                stream_id,
                stream_sequence,
                occurred_at,
                ingested_at,
                source,
                event_type,
                entity_type,
                entity_id_hash_low,
                entity_id_hash_high,
                entity_id,
                actor_id_hash_low,
                actor_id,
                account_id_hash_low,
                account_id,
                trace_id_hash_low,
                dedupe_hash_low,
                dedupe_hash_high,
                schema_version,
                flags,
            },
            payload,
            source_schema,
            source_delivery,
            connector_delivery_hash_low,
            connector_delivery_hash_high,
            provenance,
            relationships,
        },
    })
}

/// A batch of pending records serialized once and confirmed to fit: what [`CommitQueue::admit`] returns and
/// [`CommitQueue::push_admitted`] appends. Carrying the bytes between the two steps is what keeps an admitted batch
/// from being serialized a second time on its way into the queue.
#[derive(Debug)]
pub struct QueueAdmission {
    bytes: Vec<u8>,
    /// Each record's `[start, end)` byte range in `bytes`, in submission order.
    spans: Vec<(usize, usize)>,
}

/// A claimed region of the queue: `[start, end)` in absolute cursor space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueueRegion {
    pub end: u64,
    pub start: u64,
}

/// The bounded circular commit queue. Cursors are absolute (monotonic); buffer offsets are cursor mod capacity. Records
/// never wrap: a record that would cross the buffer end is preceded by a skip marker to the next wrap boundary.
#[derive(Debug)]
pub struct CommitQueue {
    buffer: Vec<u8>,
    capacity: u64,
    /// Steal/claim boundary: records in `clean..dirty` are pending and stealable; a successful claim advances it to
    /// the region end and transfers ownership. Plain (non-atomic) because every queue in a steal group is driven from
    /// one thread; a stale claim still fails the compare against the current value.
    clean_cursor: u64,
    /// Owner-private end of fully serialized records.
    dirty_cursor: u64,
    /// Contiguous boundary below which every record is resolved: hardened by this queue, stolen (the thief queue owns
    /// its durability), or aborted (its range closed by a void record).
    hardened_cursor: u64,
    /// Hardened regions not yet absorbed into `hardened_cursor`, blocked by an earlier still-in-flight region.
    hardened_pending: Vec<QueueRegion>,
    /// Release cursor: storage before `head` is reusable.
    head: u64,
    /// Regions this queue claimed for its own flush and has not yet hardened. Everything else between `head` and
    /// `clean_cursor` has been stolen by a peer (whose thief queue now owns its durability), so `head` may advance over
    /// it; these regions are the barrier that stops the release.
    in_flight: Vec<QueueRegion>,
    /// Reused variant-encoder container buffers for [`push`](Self::push), alongside `push_scratch`.
    push_encode_scratch: EncodeScratch,
    /// Reused encode buffer for [`push`](Self::push), so submitting one record does not allocate a fresh buffer on
    /// every call.
    push_scratch: Vec<u8>,
}

const LEN_HEADER: u64 = 8;
/// Marker length value: skip to the next wrap boundary.
const SKIP_MARKER: u64 = u64::MAX;

impl CommitQueue {
    /// An empty queue whose buffer is at least `capacity` bytes, rounded up to a cache-line multiple (and never below 1
    /// KiB).
    pub fn new(capacity: usize) -> Self {
        let capacity = (capacity as u64)
            .next_multiple_of(RECORD_ALIGN)
            .max(MIN_QUEUE_CAPACITY_BYTES);
        Self {
            buffer: vec![0; capacity as usize],
            capacity,
            head: 0,
            dirty_cursor: 0,
            clean_cursor: 0,
            hardened_cursor: 0,
            hardened_pending: Vec::new(),
            in_flight: Vec::new(),
            push_encode_scratch: EncodeScratch::new(),
            push_scratch: Vec::with_capacity(PENDING_RECORD_CAPACITY_HINT),
        }
    }

    fn offset_of(&self, cursor: u64) -> usize {
        (cursor % self.capacity) as usize
    }

    fn write_at(&mut self, cursor: u64, bytes: &[u8]) {
        let offset = self.offset_of(cursor);
        if let Some(target) = self.buffer.get_mut(offset..offset + bytes.len()) {
            target.copy_from_slice(bytes);
        }
    }

    fn read_u64_at(&self, cursor: u64) -> u64 {
        let offset = self.offset_of(cursor);
        self.buffer
            .get(offset..offset + 8)
            .map(|bytes| u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
            .unwrap_or(0)
    }

    /// Serialized pending bytes not yet claimed.
    pub fn pending_bytes(&self) -> u64 {
        self.dirty_cursor - self.clean_cursor
    }

    /// Absolute cursor just past the last fully serialized record.
    pub fn dirty_cursor(&self) -> u64 {
        self.dirty_cursor
    }

    /// Absolute cursor marking the start of the stealable/claimable region.
    pub fn clean_cursor(&self) -> u64 {
        self.clean_cursor
    }

    /// Absolute cursor below which every record is resolved — hardened by this queue, stolen (the thief queue owns
    /// its durability), or aborted (its range closed by a void record). Advances only contiguously.
    pub fn hardened_cursor(&self) -> u64 {
        self.hardened_cursor
    }

    /// Where a record of `payload_len` bytes appended at `cursor` would sit, as `[start, end)` in cursor space, or
    /// `None` when the queue has no room for it. Records never wrap, so `start` is past the wrap boundary — and past a
    /// skip marker the writer lays down at `cursor` — when the record would otherwise cross it.
    fn placement(&self, cursor: u64, payload_len: usize) -> Option<(u64, u64)> {
        let record_len = (LEN_HEADER + payload_len as u64).next_multiple_of(RECORD_ALIGN);
        if record_len > self.capacity {
            return None;
        }
        let to_wrap = self.capacity - (cursor % self.capacity);
        let start = if record_len > to_wrap { cursor + to_wrap } else { cursor };
        if start + record_len - self.head > self.capacity {
            return None;
        }
        Some((start, start + record_len))
    }

    /// Appends one already-serialized record (cache-line aligned).
    fn push_encoded(&mut self, payload: &[u8]) -> Result<(), QueueError> {
        // Reclaim any storage peers have stolen from this queue before measuring free space, so a queue whose records
        // are being drained by stealers keeps making room and never stalls even when it never flushes itself.
        self.advance_head();
        let cursor = self.dirty_cursor;
        let Some((start, end)) = self.placement(cursor, payload.len()) else {
            return Err(QueueError::Full);
        };
        if start != cursor {
            self.write_at(cursor, &SKIP_MARKER.to_le_bytes());
        }
        self.write_at(start, &(payload.len() as u64).to_le_bytes());
        self.write_at(start + LEN_HEADER, payload);
        self.dirty_cursor = end;
        Ok(())
    }

    /// READY: appends one serialized pending record (cache-line aligned).
    pub fn push(&mut self, record: &PendingRecord) -> Result<(), QueueError> {
        // Reuses `push_scratch`'s allocated capacity instead of `encode_pending` allocating a fresh buffer for
        // every submitted event.
        let mut scratch = std::mem::take(&mut self.push_scratch);
        scratch.clear();
        let mut out = Writer::from_vec(scratch);
        let encoded =
            encode_pending_into(record, &mut self.push_encode_scratch, &mut out).map_err(|_| QueueError::Codec);
        let payload = out.into_bytes();
        let result = encoded.and_then(|()| self.push_encoded(&payload));
        self.push_scratch = payload;
        result
    }

    /// Copies the stealable region `clean..dirty` (step 1 of the steal protocol, also used by the owner's own claim).
    /// Returns the region and its decoded records.
    pub fn copy_pending(&self) -> Result<(QueueRegion, Vec<PendingRecord>), QueueError> {
        let start = self.clean_cursor;
        let end = self.dirty_cursor;
        let mut records = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let len = self.read_u64_at(cursor);
            if len == SKIP_MARKER {
                cursor += self.capacity - (cursor % self.capacity);
                continue;
            }
            let offset = self.offset_of(cursor + LEN_HEADER);
            let bytes = self
                .buffer
                .get(offset..offset + len as usize)
                .ok_or(QueueError::Codec)?;
            records.push(decode_pending(bytes).map_err(|_| QueueError::Codec)?);
            cursor += (LEN_HEADER + len).next_multiple_of(RECORD_ALIGN);
        }
        Ok((QueueRegion { start, end }, records))
    }

    /// Copies the stealable region `clean..dirty` as raw encoded record bytes, without decoding them. The steal path
    /// uses this instead of [`copy_pending`](Self::copy_pending): checking the same-tenant/same-epoch rule only needs
    /// each record's fixed-offset prefix (see [`peek_tenant_epoch`]), and a matching region is then admitted into the
    /// thief's queue by copying these same bytes — [`decode_pending`] followed by [`encode_pending_into`] would
    /// reproduce the identical bytes at the cost of decoding and re-encoding every field of every record.
    pub fn copy_pending_raw(&self) -> Result<(QueueRegion, Vec<&[u8]>), QueueError> {
        let start = self.clean_cursor;
        let end = self.dirty_cursor;
        let mut records = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let len = self.read_u64_at(cursor);
            if len == SKIP_MARKER {
                cursor += self.capacity - (cursor % self.capacity);
                continue;
            }
            let offset = self.offset_of(cursor + LEN_HEADER);
            let bytes = self
                .buffer
                .get(offset..offset + len as usize)
                .ok_or(QueueError::Codec)?;
            records.push(bytes);
            cursor += (LEN_HEADER + len).next_multiple_of(RECORD_ALIGN);
        }
        Ok((QueueRegion { start, end }, records))
    }

    /// Copies the longest prefix of the stealable region whose records all belong to `tenant_id` and `epoch`. The
    /// returned region ends exactly at the first non-matching record, so a flush over a mixed queue can claim just the
    /// flushable prefix and leave the rest pending — a claimed record is never dropped for belonging to another
    /// tenant or epoch. An empty prefix (the queue is empty, or its first record does not match) returns an empty
    /// region and no records.
    pub fn copy_matching_prefix(
        &self,
        tenant_id: TenantId,
        epoch: u64,
    ) -> Result<(QueueRegion, Vec<PendingRecord>), QueueError> {
        let start = self.clean_cursor;
        let end = self.dirty_cursor;
        let mut records = Vec::new();
        let mut cursor = start;
        while cursor < end {
            let len = self.read_u64_at(cursor);
            if len == SKIP_MARKER {
                cursor += self.capacity - (cursor % self.capacity);
                continue;
            }
            let offset = self.offset_of(cursor + LEN_HEADER);
            let bytes = self
                .buffer
                .get(offset..offset + len as usize)
                .ok_or(QueueError::Codec)?;
            let record = decode_pending(bytes).map_err(|_| QueueError::Codec)?;
            if record.tenant_id != tenant_id || record.epoch != epoch {
                break;
            }
            records.push(record);
            cursor += (LEN_HEADER + len).next_multiple_of(RECORD_ALIGN);
        }
        Ok((QueueRegion { start, end: cursor }, records))
    }

    /// A peer's steal claim: advances `clean_cursor` from `region.start` to `region.end`. Fails when the cursor has
    /// already moved since the copy — another claim won — and the caller must discard the copied bytes. On success the
    /// source reclaims the region's storage automatically: a stolen region is one the source never claimed for its own
    /// flush, so it is absent from the in-flight set and [`advance_head`](Self::advance_head) frees it as soon as
    /// `clean_cursor` passes it.
    pub fn commit_claim(&mut self, region: QueueRegion) -> bool {
        if self.clean_cursor != region.start {
            return false;
        }
        self.clean_cursor = region.end;
        true
    }

    /// The owner's own claim ahead of writing a frame: claims like [`commit_claim`](Self::commit_claim), but on
    /// success also records the region as in-flight so the release step keeps its storage pinned until the owner
    /// hardens it. Returns `false` if a stealer won the region first, in which case the owner writes nothing.
    pub fn claim_for_flush(&mut self, region: QueueRegion) -> bool {
        if self.commit_claim(region) {
            self.in_flight.push(region);
            true
        } else {
            false
        }
    }

    /// Serializes `records` once and hands back their bytes when every one of them fits in this queue's free space
    /// right now, or `None` when the batch does not fit whole. Nothing is mutated either way: the caller appends the
    /// batch with [`push_admitted`](Self::push_admitted), which reuses these bytes rather than serializing the events a
    /// second time.
    pub fn admit(&self, records: &[PendingRecord]) -> Result<Option<QueueAdmission>, QueueError> {
        // Shared across every record so the payload encoder's container buffers are cleared and reused instead of
        // allocated fresh per container per record.
        let mut scratch = EncodeScratch::new();
        self.admit_placed(
            records.len(),
            records.len() * PENDING_RECORD_CAPACITY_HINT,
            |index, out| encode_pending_into(&records[index], &mut scratch, out).map_err(|_| QueueError::Codec),
        )
    }

    /// Like [`admit`](Self::admit), but for record bytes already in this queue's own wire format (stolen from a
    /// peer queue's [`copy_pending_raw`](Self::copy_pending_raw)) — copies them in rather than decoding and
    /// re-encoding every field. A stealer admits a peer's records before claiming their region, so it never claims
    /// records it cannot hold — a claimed record is then guaranteed a slot here and can never be dropped between the
    /// two queues.
    pub fn admit_encoded(&self, records: &[&[u8]]) -> Result<Option<QueueAdmission>, QueueError> {
        self.admit_placed(records.len(), records.iter().map(|r| r.len()).sum(), |index, out| {
            out.put_slice(records[index]);
            Ok(())
        })
    }

    /// Shared placement loop behind [`admit`](Self::admit) and [`admit_encoded`](Self::admit_encoded): writes
    /// `count` records (via `write_one`, index by index) into one buffer and confirms each lands in this queue's
    /// free space in turn, or reports `None` the moment one does not fit whole.
    fn admit_placed(
        &self,
        count: usize,
        capacity_hint: usize,
        mut write_one: impl FnMut(usize, &mut Writer) -> Result<(), QueueError>,
    ) -> Result<Option<QueueAdmission>, QueueError> {
        let mut bytes = Writer::with_capacity(capacity_hint);
        let mut spans = Vec::with_capacity(count);
        let mut cursor = self.dirty_cursor;
        for index in 0..count {
            let start = bytes.len();
            write_one(index, &mut bytes)?;
            let end = bytes.len();
            let Some((_, next)) = self.placement(cursor, end - start) else {
                return Ok(None);
            };
            cursor = next;
            spans.push((start, end));
        }
        Ok(Some(QueueAdmission {
            bytes: bytes.into_bytes(),
            spans,
        }))
    }

    /// Appends every record of an admission returned by [`admit`](Self::admit), in order.
    pub fn push_admitted(&mut self, admission: &QueueAdmission) -> Result<(), QueueError> {
        for (start, end) in &admission.spans {
            let payload = admission.bytes.get(*start..*end).ok_or(QueueError::Codec)?;
            self.push_encoded(payload)?;
        }
        Ok(())
    }

    /// Whether every record in `records` would fit in this queue's free space right now, without mutating anything.
    pub fn can_admit(&self, records: &[PendingRecord]) -> Result<bool, QueueError> {
        Ok(self.admit(records)?.is_some())
    }

    /// Publishes durability for a claimed region and advances the contiguous durability boundary. The boundary moves
    /// over regions this queue hardened and over stolen or aborted gaps (a thief queue owns a stolen region's
    /// durability; an aborted region's range is closed by a void record), stopping only at the earliest region still
    /// in flight — so a later completion can never make an earlier still-in-flight region look durable. The region is
    /// also cleared from the in-flight set so the release step may reclaim its storage.
    pub fn mark_hardened(&mut self, region: QueueRegion) {
        self.in_flight.retain(|pending| *pending != region);
        self.hardened_pending.push(region);
        self.drain_hardened();
    }

    /// Normal queue release: advances the head over every region that is done — hardened by this queue or stolen by a
    /// peer.
    pub fn release_hardened(&mut self) {
        self.advance_head();
    }

    /// Abandons a region claimed by [`claim_for_flush`](Self::claim_for_flush) whose frame never reached durability:
    /// clears it from the in-flight set and advances the head so its storage is reclaimed. The region's records are
    /// dropped here; their reserved sequence range is closed by a void record so the contiguous watermark still
    /// advances past it. The durability boundary treats the aborted region as resolved — its records will never harden
    /// here — so later hardened regions are never pinned behind it. Call this on any flush failure so a claimed region
    /// is never pinned forever.
    pub fn abort_flush(&mut self, region: QueueRegion) {
        self.in_flight.retain(|pending| *pending != region);
        self.drain_hardened();
        self.advance_head();
    }

    /// Advances the contiguous durability boundary. Everything between the boundary and `clean_cursor` was claimed —
    /// by this queue's own flush (tracked in `in_flight` until it hardens or aborts) or by a peer's steal (absent from
    /// both sets; the thief owns its durability) — so the boundary advances to the earliest still-in-flight region, or
    /// all the way to `clean_cursor` when none remains, and drops the hardened regions it passes. Without the jump
    /// over stolen and aborted gaps, `hardened_pending` would grow without bound and the boundary would stall forever.
    fn drain_hardened(&mut self) {
        let limit = self
            .in_flight
            .iter()
            .map(|region| region.start)
            .min()
            .unwrap_or(self.clean_cursor);
        self.hardened_cursor = self.hardened_cursor.max(limit);
        self.hardened_pending.retain(|region| region.end > self.hardened_cursor);
    }

    /// Advances `head` over reclaimable storage. Everything between `head` and `clean_cursor` has been claimed; of those
    /// regions only the ones still in-flight for this queue (self-claimed, not yet hardened) must stay pinned, so `head`
    /// advances up to the earliest in-flight region, or all the way to `clean_cursor` when none remains. Stolen and
    /// hardened regions before that barrier are reclaimed.
    fn advance_head(&mut self) {
        let clean = self.clean_cursor;
        let barrier = self
            .in_flight
            .iter()
            .map(|region| region.start)
            .filter(|&start| start >= self.head)
            .min()
            .unwrap_or(clean);
        self.head = self.head.max(barrier.min(clean));
    }
}

/// The topology-local steal rule, validation only: a worker may steal only same-tenant, same-epoch pending records, and
/// only within its CPU-topology steal group. Returns the stealable region and its raw encoded record bytes — for the
/// caller to admit into its own queue with [`CommitQueue::admit_encoded`] and only then claim — or `None` when the
/// rules do not permit a steal. The same-tenant/same-epoch check reads only each record's fixed-offset prefix (see
/// `peek_tenant_epoch`), so a peer's records are never decoded just to be re-encoded byte-identically into the
/// thief's queue; they are decoded only later, at flush. Claiming is a separate step (see
/// [`CommitQueue::can_admit`] and [`CommitQueue::commit_claim`]) so the thief first confirms it has room and only
/// then commits the claim; a claimed record is therefore never dropped for lack of space, nor left claimed on the
/// source with nowhere to go.
pub fn peek_steal(
    target: &CommitQueue,
    frame_tenant: TenantId,
    frame_epoch: u64,
    same_steal_group: bool,
) -> Option<(QueueRegion, Vec<&[u8]>)> {
    if !same_steal_group {
        return None;
    }
    let (region, records) = target.copy_pending_raw().ok()?;
    if region.start == region.end {
        return None;
    }
    if records
        .iter()
        .any(|record| peek_tenant_epoch(record) != Some((frame_tenant, frame_epoch)))
    {
        return None;
    }
    Some((region, records))
}

#[cfg(test)]
#[path = "test/queue.rs"]
mod tests;
