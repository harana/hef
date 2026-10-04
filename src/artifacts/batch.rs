//! Packs a group of events into the compact byte layout that goes inside one journal frame, and reads it back out.
//!
//! This batch is the only payload encoding version 1 of the journal accepts. It is built from several side-by-side
//! sections — fixed-size per-event records, variable-size records, a shared string table, the payload key dictionary,
//! and the payload bytes themselves. Every section offset is measured from the payload start and 8-byte aligned, all
//! unused padding is zero, and the batch is rejected if any sections overlap, run past the declared length, or leave
//! non-zero padding.

use super::{BATCH_HEADER_LEN, FIXED_RECORD_LEN, MAGIC_LEN, PROVENANCE_RECORD_LEN, REF_ABSENT, VARIABLE_RECORD_LEN};
use crate::error::FormatError;
use crate::events::provenance::{SignatureScheme, SignedEventProvenance};
use crate::events::relationships::{
    EVENT_ID_TARGET_LEN, EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace,
};
use crate::events::variant::{
    EncodeScratch, KeyDictionary, VariantRef, VariantValue, encode_value_into_with_scratch,
    transcode_value_into_with_scratch,
};
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::file::bytes::{Reader, Writer, slice};
use hashbrown::HashMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

/// Variable-record payload flag bit 0: the payload bytes are a UTF-8 internal immutable payload reference, not an
/// inline variant value.
pub const PAYLOAD_FLAG_EXTERNAL_REF: u32 = 1;

/// Batch flag bit 0: this batch carries a provenance table, because at least one of its events arrived over a signed
/// protocol. Clear for every stream without signatures, which materializes no provenance bytes at all.
pub const BATCH_FLAG_SIGNED_PROVENANCE: u32 = 1;

/// Batch flag bit 1: this batch carries a relationship table, because at least one of its events declares references
/// to other events. Clear for every stream without relationships, which materializes no relationship bytes at all.
pub const BATCH_FLAG_RELATIONSHIPS: u32 = 2;

/// Stored code for the BIP-340 Schnorr scheme in a provenance record.
const SCHEME_CODE_BIP340: u32 = 1;

/// Stored code for the Ed25519 scheme in a provenance record.
const SCHEME_CODE_ED25519: u32 = 2;

/// Byte width of one stored offset entry in a string table or the variant dictionary's key table: each offset is a
/// plain `u32`.
const OFFSET_ENTRY_LEN: usize = 4;

/// Batch sections (fixed table, variable table, string table, dictionary, provenance table, relationship table,
/// payload arena) are aligned to this many bytes from the payload start.
const SECTION_ALIGN: usize = 8;

/// Byte length of the author's BIP-340 x-only public key stored in a provenance record.
const AUTHOR_PUBKEY_LEN: usize = 32;

/// Byte length of the protocol's own sha256 event id stored in a provenance record.
const PROTOCOL_EVENT_ID_LEN: usize = 32;

/// Byte length of a BIP-340 Schnorr signature stored in a provenance record.
const SIGNATURE_LEN: usize = 64;

fn scheme_code(scheme: SignatureScheme) -> u32 {
    match scheme {
        SignatureScheme::Bip340SchnorrSecp256k1 => SCHEME_CODE_BIP340,
        SignatureScheme::Ed25519 => SCHEME_CODE_ED25519,
    }
}

fn scheme_from_code(code: u32) -> Result<SignatureScheme, FormatError> {
    match code {
        SCHEME_CODE_BIP340 => Ok(SignatureScheme::Bip340SchnorrSecp256k1),
        SCHEME_CODE_ED25519 => Ok(SignatureScheme::Ed25519),
        _ => Err(FormatError::Structural {
            rule: "unknown signature scheme code in a provenance record",
        }),
    }
}

fn relationship_kind_code(kind: RelationshipKind) -> u32 {
    match kind {
        RelationshipKind::Auth => 5,
        RelationshipKind::Link => 1,
        RelationshipKind::Parent => 2,
        RelationshipKind::Prev => 6,
        RelationshipKind::Related => 3,
        RelationshipKind::Root => 4,
    }
}

fn relationship_kind_from_code(code: u32) -> Result<RelationshipKind, FormatError> {
    match code {
        1 => Ok(RelationshipKind::Link),
        2 => Ok(RelationshipKind::Parent),
        3 => Ok(RelationshipKind::Related),
        4 => Ok(RelationshipKind::Root),
        5 => Ok(RelationshipKind::Auth),
        6 => Ok(RelationshipKind::Prev),
        _ => Err(FormatError::Structural {
            rule: "unknown relationship kind code in a relationship record",
        }),
    }
}

fn target_space_code(space: TargetIdSpace) -> u32 {
    match space {
        TargetIdSpace::EventId => 1,
        TargetIdSpace::ExternalId => 3,
        TargetIdSpace::ProtocolEventId => 2,
    }
}

fn target_space_from_code(code: u32) -> Result<TargetIdSpace, FormatError> {
    match code {
        1 => Ok(TargetIdSpace::EventId),
        2 => Ok(TargetIdSpace::ProtocolEventId),
        3 => Ok(TargetIdSpace::ExternalId),
        _ => Err(FormatError::Structural {
            rule: "unknown target identifier space code in a relationship record",
        }),
    }
}

/// One canonical `harana_variant_v1` value that is already encoded, together with the key dictionary its field ids
/// resolve against.
///
/// A payload only moving between two encoded homes — out of a worker's commit queue and into the frame it is written
/// to — travels in this form, so it is never inflated into an owned value tree along the way.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedPayload {
    pub bytes: Vec<u8>,
    pub dictionary: KeyDictionary,
}

/// The payload of one event entering a batch.
#[derive(Debug, Clone, PartialEq)]
pub enum PayloadInput {
    /// One canonical value already encoded against its own dictionary, re-encoded against the frame dictionary at
    /// build time.
    Encoded(EncodedPayload),
    /// UTF-8 internal immutable payload reference (large-body rule). Internal only; never public output.
    ExternalRef(String),
    None,
    /// One canonical `harana_variant_v1` value (encoded against the frame dictionary at build time).
    Variant(VariantValue),
}

/// One event entering a batch: the logical envelope plus payload and connector lineage.
#[derive(Debug, Clone, PartialEq)]
pub struct EventInput {
    pub connector_delivery_hash_high: u64,
    pub connector_delivery_hash_low: u64,
    pub envelope: EventEnvelope,
    pub payload: PayloadInput,
    /// Present only for events that arrived over a signed protocol; stored byte-exact so the signature re-verifies
    /// from the archive alone.
    pub provenance: Option<SignedEventProvenance>,
    /// Present only for events that declare references to other events; stored exactly as declared, with no lookup
    /// of any target on the append path.
    pub relationships: Option<EventRelationships>,
    /// Optional connector delivery identity string.
    pub source_delivery: Option<String>,
    /// Optional source-format lineage (e.g. `protobuf:sha256:...`).
    pub source_schema: Option<String>,
}

/// The decoded batch header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HejCompactBatchHeaderV1 {
    pub batch_dictionary_generation_hint: u64,
    pub batch_schema_generation: u64,
    pub event_count: u32,
    pub fixed_table_len: u32,
    pub fixed_table_offset: u32,
    pub flags: u32,
    pub max_ingested_at_physical: i64,
    pub max_occurred_at_physical: i64,
    pub min_ingested_at_physical: i64,
    pub min_occurred_at_physical: i64,
    pub payload_arena_len: u32,
    pub payload_arena_offset: u32,
    pub provenance_table_len: u32,
    pub provenance_table_offset: u32,
    pub relationship_table_len: u32,
    pub relationship_table_offset: u32,
    pub string_table_len: u32,
    pub string_table_offset: u32,
    pub variable_table_len: u32,
    pub variable_table_offset: u32,
    pub variant_dictionary_len: u32,
    pub variant_dictionary_offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFixedRecordV1 {
    pub account_id_hash_low: u64,
    pub actor_id_hash_low: u64,
    pub dedupe_hash_high: u64,
    pub dedupe_hash_low: u64,
    pub entity_id_hash_high: u64,
    pub entity_id_hash_low: u64,
    pub entity_type_string_ref: u32,
    pub event_id: u128,
    pub event_type_string_ref: u32,
    pub flags: u32,
    pub ingested_at_physical: i64,
    pub occurred_at_physical: i64,
    pub schema_version: u32,
    pub source_string_ref: u32,
    pub stream_id: u64,
    pub stream_sequence: u64,
    pub trace_id_hash_low: u64,
    pub variable_record_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventVariableRecordV1 {
    pub account_id_string_ref: u32,
    pub actor_id_string_ref: u32,
    pub connector_delivery_hash_high: u64,
    pub connector_delivery_hash_low: u64,
    pub entity_id_string_ref: u32,
    pub payload_flags: u32,
    pub payload_len: u32,
    pub payload_offset: u32,
    /// Index into the batch's provenance table, or [`REF_ABSENT`] when this event carries no signature.
    pub provenance_ref: u32,
    /// Byte offset of this event's entry in the batch's relationship table, or [`REF_ABSENT`] when this event
    /// declares no relationships. Zero (and treated as absent) when the batch carries no relationship table.
    pub relationships_ref: u32,
    pub source_delivery_ref: u32,
    pub source_schema_ref: u32,
}

/// One decoded event with resolved strings and its payload slice.
#[derive(Debug, Clone)]
pub struct DecodedEvent<'a> {
    pub account_id: Option<&'a str>,
    pub actor_id: Option<&'a str>,
    pub entity_id: Option<&'a str>,
    pub entity_type: &'a str,
    pub event_type: &'a str,
    pub fixed: EventFixedRecordV1,
    /// Inline payload bytes inside the arena (`None` when `payload_len == 0`). When `PAYLOAD_FLAG_EXTERNAL_REF` is set
    /// these bytes are the UTF-8 reference, otherwise one `harana_variant_v1` value.
    pub payload: Option<&'a [u8]>,
    /// Decoded signed-event provenance, or `None` when this event carries no signature.
    pub provenance: Option<SignedEventProvenance>,
    /// Decoded relationship references, or `None` when this event declares none.
    pub relationships: Option<EventRelationships>,
    pub source: &'a str,
    pub source_delivery: Option<&'a str>,
    pub source_schema: Option<&'a str>,
    pub variable: EventVariableRecordV1,
}

/// A fully decoded and validated batch.
#[derive(Debug)]
pub struct DecodedBatch<'a> {
    pub dictionary: KeyDictionary,
    pub dictionary_bytes: &'a [u8],
    pub events: Vec<DecodedEvent<'a>>,
    pub header: HejCompactBatchHeaderV1,
    pub payload_arena: &'a [u8],
    pub strings: Vec<&'a str>,
}

/// String interner preserving first-insertion refs.
///
/// The lookup table and the ordered value list share one allocation per distinct value — an `Arc<str>` cloned into
/// both — rather than each holding its own copy of the text.
#[derive(Default)]
struct StringTableBuilder {
    refs: HashMap<Arc<str>, u32>,
    strings: Vec<Arc<str>>,
}

impl StringTableBuilder {
    fn intern(&mut self, value: &str) -> u32 {
        if let Some(existing) = self.refs.get(value) {
            return *existing;
        }
        let next = self.strings.len() as u32;
        let value: Arc<str> = Arc::from(value);
        self.strings.push(Arc::clone(&value));
        self.refs.insert(value, next);
        next
    }

    fn intern_optional(&mut self, value: Option<&str>) -> u32 {
        match value {
            Some(value) => self.intern(value),
            None => REF_ABSENT,
        }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_u32(self.strings.len() as u32);
        let offsets_len = OFFSET_ENTRY_LEN as u32 * (self.strings.len() as u32 + 1);
        out.put_u32(offsets_len);
        let mut offset = 0u32;
        for value in &self.strings {
            out.put_u32(offset);
            offset += value.len() as u32;
        }
        out.put_u32(offset);
        for value in &self.strings {
            out.put_slice(value.as_bytes());
        }
        out.into_bytes()
    }
}

pub(crate) fn encode_variant_dictionary(dictionary: &KeyDictionary) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u32(dictionary.key_count());
    out.put_u32(1); // flags bit 0 = sorted_keys, mandatory in HEJ v1
    let mut offset = 0u32;
    for key in dictionary.keys() {
        out.put_u32(offset);
        offset += key.len() as u32;
    }
    out.put_u32(offset);
    for key in dictionary.keys() {
        out.put_slice(key.as_bytes());
    }
    out.into_bytes()
}

/// Decodes `VariantDictionaryV1` bytes into a `KeyDictionary`.
pub fn decode_variant_dictionary(bytes: &[u8]) -> Result<KeyDictionary, FormatError> {
    let mut reader = Reader::new(bytes);
    let key_count = reader.u32("dictionary key_count")? as usize;
    let flags = reader.u32("dictionary flags")?;
    if flags & 1 == 0 {
        return Err(FormatError::Structural {
            rule: "VariantDictionaryV1 sorted_keys flag must be set in HEJ v1",
        });
    }
    if flags & !1 != 0 {
        return Err(FormatError::ReservedNotZero {
            field: "VariantDictionaryV1.flags",
        });
    }
    let mut offsets = Vec::with_capacity(reader.capacity_hint(key_count.saturating_add(1), OFFSET_ENTRY_LEN));
    for _ in 0..=key_count {
        offsets.push(reader.u32("dictionary offset")? as usize);
    }
    let data = reader.take(reader.remaining(), "dictionary keys")?;
    if offsets.first() != Some(&0) || offsets.last() != Some(&data.len()) {
        return Err(FormatError::Structural {
            rule: "dictionary offsets must start at 0 and end at the data length",
        });
    }
    let mut keys = Vec::with_capacity(key_count);
    for pair in offsets.windows(2) {
        let (start, end) = match pair {
            [start, end] => (*start, *end),
            _ => continue,
        };
        if end < start {
            return Err(FormatError::Structural {
                rule: "dictionary offsets must be non-decreasing",
            });
        }
        let bytes = slice(data, start, end - start, "dictionary key")?;
        let key = std::str::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 { what: "dictionary key" })?;
        keys.push(key.to_owned());
    }
    KeyDictionary::from_sorted_unique(keys)
}

fn decode_string_table(bytes: &[u8]) -> Result<Vec<&str>, FormatError> {
    let mut reader = Reader::new(bytes);
    let string_count = reader.u32("string_count")? as usize;
    let offsets_len = reader.u32("offsets_len")? as usize;
    if offsets_len != OFFSET_ENTRY_LEN * (string_count + 1) {
        return Err(FormatError::Structural {
            rule: "StringTableV1 offsets_len must equal 4 * (string_count + 1)",
        });
    }
    let mut offsets = Vec::with_capacity(reader.capacity_hint(string_count.saturating_add(1), OFFSET_ENTRY_LEN));
    for _ in 0..=string_count {
        offsets.push(reader.u32("string offset")? as usize);
    }
    let data = reader.take(reader.remaining(), "string data")?;
    if offsets.first() != Some(&0) || offsets.last() != Some(&data.len()) {
        return Err(FormatError::Structural {
            rule: "string offsets must start at 0 and end at the data length",
        });
    }
    let mut strings = Vec::with_capacity(string_count);
    for pair in offsets.windows(2) {
        let (start, end) = match pair {
            [start, end] => (*start, *end),
            _ => continue,
        };
        if end < start {
            return Err(FormatError::Structural {
                rule: "string offsets must be non-decreasing",
            });
        }
        let bytes = slice(data, start, end - start, "string")?;
        let value = std::str::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
            what: "string table entry",
        })?;
        strings.push(value);
    }
    Ok(strings)
}

/// Builds the complete `harana_hej_compact_batch_v1` payload for one frame. The caller guarantees one tenant, one
/// epoch, and contiguous sequences; the batch stores the per-event records in frame order.
pub fn build_batch(
    events: &[EventInput],
    schema_generation: u64,
    dictionary_generation_hint: u64,
) -> Result<Vec<u8>, FormatError> {
    if events.is_empty() {
        return Err(FormatError::Structural {
            rule: "a compact batch must carry at least one event",
        });
    }

    let mut strings = StringTableBuilder::default();
    let mut keys = BTreeSet::new();
    for event in events {
        match &event.payload {
            // An encoded payload's own dictionary already names its keys, so they join the frame's without the value
            // being walked.
            PayloadInput::Encoded(payload) => keys.extend(payload.dictionary.keys()),
            PayloadInput::Variant(value) => value.collect_keys(&mut keys),
            PayloadInput::ExternalRef(_) | PayloadInput::None => {}
        }
    }
    let dictionary = KeyDictionary::build(keys.into_iter().map(str::to_owned));

    // Payload arena + per-event (flags, offset, len).
    let mut arena = Writer::new();
    let mut payload_slots: Vec<(u32, u32, u32)> = Vec::with_capacity(events.len());
    // Shared across every event's payload so the encode/transcode recursion's container buffers are cleared and
    // reused instead of allocated fresh per container per event.
    let mut scratch = EncodeScratch::new();
    for event in events {
        match &event.payload {
            PayloadInput::Encoded(payload) => {
                let offset = arena.len() as u32;
                transcode_value_into_with_scratch(
                    VariantRef::new(&payload.bytes),
                    &payload.dictionary,
                    &dictionary,
                    &mut scratch,
                    &mut arena,
                )?;
                payload_slots.push((0, offset, arena.len() as u32 - offset));
            }
            PayloadInput::None => payload_slots.push((0, 0, 0)),
            PayloadInput::Variant(value) => {
                let offset = arena.len() as u32;
                encode_value_into_with_scratch(value, &dictionary, &mut scratch, &mut arena)?;
                payload_slots.push((0, offset, arena.len() as u32 - offset));
            }
            PayloadInput::ExternalRef(reference) => {
                // An empty reference would decode as payload_len == 0 with a nonzero flag, which the batch decoder
                // rejects; refuse it here so the write path never journals a batch its own decoder cannot read.
                if reference.is_empty() {
                    return Err(FormatError::Structural {
                        rule: "an external payload reference must be non-empty",
                    });
                }
                let offset = arena.len() as u32;
                arena.put_slice(reference.as_bytes());
                payload_slots.push((PAYLOAD_FLAG_EXTERNAL_REF, offset, reference.len() as u32));
            }
        }
    }
    let arena_bytes = arena.into_bytes();

    // Provenance table: one record per signed event, in event order, referenced from the variable records.
    let mut provenance = Writer::new();
    let mut provenance_refs: Vec<u32> = Vec::with_capacity(events.len());
    for event in events {
        match &event.provenance {
            None => provenance_refs.push(REF_ABSENT),
            Some(signed) => {
                provenance_refs.push((provenance.len() as u32) / PROVENANCE_RECORD_LEN);
                provenance.put_slice(&signed.author_pubkey);
                provenance.put_slice(&signed.protocol_event_id);
                provenance.put_slice(&signed.signature);
                provenance.put_i64(signed.claimed_at.physical_nanos());
                provenance.put_u32(signed.protocol_kind);
                provenance.put_u32(scheme_code(signed.scheme));
            }
        }
    }
    let provenance_bytes = provenance.into_bytes();

    // Relationship table: one variable-length entry per event that declares references, in event order, addressed by
    // byte offset from the variable records. References are stored exactly as declared — no target is looked up.
    let mut relationship_table = Writer::new();
    let mut relationship_refs: Vec<u32> = Vec::with_capacity(events.len());
    for event in events {
        match &event.relationships {
            None => relationship_refs.push(REF_ABSENT),
            Some(relationships) => {
                relationship_refs.push(relationship_table.len() as u32);
                relationship_table.put_u32(relationships.refs().len() as u32);
                for reference in relationships.refs() {
                    relationship_table.put_u32(relationship_kind_code(reference.kind));
                    relationship_table.put_u32(target_space_code(reference.space));
                    // A variable-length space records its target's length ahead of the bytes.
                    if reference.space.fixed_len().is_none() {
                        relationship_table.put_u32(reference.target_ref.len() as u32);
                    }
                    relationship_table.put_slice(&reference.target_ref);
                }
            }
        }
    }
    let relationship_bytes = relationship_table.into_bytes();

    // Fixed and variable tables.
    let mut fixed = Writer::with_capacity(events.len() * FIXED_RECORD_LEN as usize);
    let mut variable = Writer::with_capacity(events.len() * VARIABLE_RECORD_LEN as usize);
    let mut min_occurred = i64::MAX;
    let mut max_occurred = i64::MIN;
    let mut min_ingested = i64::MAX;
    let mut max_ingested = i64::MIN;
    for (index, event) in events.iter().enumerate() {
        let envelope = &event.envelope;
        min_occurred = min_occurred.min(envelope.occurred_at.physical_nanos());
        max_occurred = max_occurred.max(envelope.occurred_at.physical_nanos());
        min_ingested = min_ingested.min(envelope.ingested_at.physical_nanos());
        max_ingested = max_ingested.max(envelope.ingested_at.physical_nanos());

        fixed.put_u128(envelope.event_id.uuid().as_u128());
        fixed.put_u64(envelope.stream_id.0);
        fixed.put_u64(envelope.stream_sequence);
        fixed.put_i64(envelope.occurred_at.physical_nanos());
        fixed.put_i64(envelope.ingested_at.physical_nanos());
        fixed.put_u64(envelope.entity_id_hash_low);
        fixed.put_u64(envelope.entity_id_hash_high);
        fixed.put_u64(envelope.actor_id_hash_low);
        fixed.put_u64(envelope.account_id_hash_low);
        fixed.put_u64(envelope.trace_id_hash_low);
        fixed.put_u64(envelope.dedupe_hash_low);
        fixed.put_u64(envelope.dedupe_hash_high);
        fixed.put_u32(strings.intern(&envelope.source));
        fixed.put_u32(strings.intern(&envelope.event_type));
        fixed.put_u32(strings.intern(&envelope.entity_type));
        fixed.put_u32(envelope.schema_version);
        fixed.put_u32(envelope.flags.0);
        fixed.put_u32(index as u32);

        let (payload_flags, payload_offset, payload_len) = payload_slots.get(index).copied().unwrap_or((0, 0, 0));
        variable.put_u32(payload_flags);
        variable.put_u32(payload_offset);
        variable.put_u32(payload_len);
        variable.put_u32(strings.intern_optional(envelope.entity_id.as_deref()));
        variable.put_u32(strings.intern_optional(envelope.actor_id.as_deref()));
        variable.put_u32(strings.intern_optional(envelope.account_id.as_deref()));
        variable.put_u32(strings.intern_optional(event.source_schema.as_deref()));
        variable.put_u32(strings.intern_optional(event.source_delivery.as_deref()));
        variable.put_u64(event.connector_delivery_hash_low);
        variable.put_u64(event.connector_delivery_hash_high);
        variable.put_u32(provenance_refs.get(index).copied().unwrap_or(REF_ABSENT));
        // A batch without a relationship table writes zero here so the field stays reserved-zero on the wire.
        variable.put_u32(if relationship_bytes.is_empty() {
            0
        } else {
            relationship_refs.get(index).copied().unwrap_or(REF_ABSENT)
        });
        variable.put_u64(0); // reserved_zero_1
    }

    let string_table = strings.encode();
    let dictionary_bytes = encode_variant_dictionary(&dictionary);

    // Lay out the sections (all 8-byte aligned, offsets relative to payload start).
    let mut out = Writer::new();
    let mut header = HejCompactBatchHeaderV1 {
        event_count: events.len() as u32,
        fixed_table_offset: 0,
        fixed_table_len: fixed.len() as u32,
        variable_table_offset: 0,
        variable_table_len: variable.len() as u32,
        string_table_offset: 0,
        string_table_len: string_table.len() as u32,
        payload_arena_offset: 0,
        payload_arena_len: arena_bytes.len() as u32,
        provenance_table_offset: 0,
        provenance_table_len: provenance_bytes.len() as u32,
        relationship_table_offset: 0,
        relationship_table_len: relationship_bytes.len() as u32,
        flags: {
            let mut flags = 0;
            if !provenance_bytes.is_empty() {
                flags |= BATCH_FLAG_SIGNED_PROVENANCE;
            }
            if !relationship_bytes.is_empty() {
                flags |= BATCH_FLAG_RELATIONSHIPS;
            }
            flags
        },
        min_occurred_at_physical: min_occurred,
        max_occurred_at_physical: max_occurred,
        min_ingested_at_physical: min_ingested,
        max_ingested_at_physical: max_ingested,
        batch_schema_generation: schema_generation,
        batch_dictionary_generation_hint: dictionary_generation_hint,
        variant_dictionary_offset: 0,
        variant_dictionary_len: dictionary_bytes.len() as u32,
    };

    // Header placeholder; offsets are patched after layout.
    out.put_slice(&[0u8; BATCH_HEADER_LEN as usize]);
    out.pad_to(SECTION_ALIGN);
    header.fixed_table_offset = out.len() as u32;
    out.put_slice(fixed.bytes());
    out.pad_to(SECTION_ALIGN);
    header.variable_table_offset = out.len() as u32;
    out.put_slice(variable.bytes());
    out.pad_to(SECTION_ALIGN);
    header.string_table_offset = out.len() as u32;
    out.put_slice(&string_table);
    out.pad_to(SECTION_ALIGN);
    header.variant_dictionary_offset = out.len() as u32;
    out.put_slice(&dictionary_bytes);
    out.pad_to(SECTION_ALIGN);
    if !provenance_bytes.is_empty() {
        header.provenance_table_offset = out.len() as u32;
        out.put_slice(&provenance_bytes);
        out.pad_to(SECTION_ALIGN);
    }
    if !relationship_bytes.is_empty() {
        header.relationship_table_offset = out.len() as u32;
        out.put_slice(&relationship_bytes);
        out.pad_to(SECTION_ALIGN);
    }
    header.payload_arena_offset = out.len() as u32;
    out.put_slice(&arena_bytes);

    let mut payload = out.into_bytes();
    let mut header_bytes = Writer::with_capacity(BATCH_HEADER_LEN as usize);
    encode_batch_header(&header, &mut header_bytes);
    if let Some(target) = payload.get_mut(..BATCH_HEADER_LEN as usize) {
        target.copy_from_slice(header_bytes.bytes());
    }
    Ok(payload)
}

fn encode_batch_header(header: &HejCompactBatchHeaderV1, out: &mut Writer) {
    out.put_slice(b"HCB1");
    out.put_u16(1); // version
    out.put_u16(BATCH_HEADER_LEN as u16);
    out.put_u32(header.event_count);
    out.put_u32(header.fixed_table_offset);
    out.put_u32(header.fixed_table_len);
    out.put_u32(header.variable_table_offset);
    out.put_u32(header.variable_table_len);
    out.put_u32(header.string_table_offset);
    out.put_u32(header.string_table_len);
    out.put_u32(header.payload_arena_offset);
    out.put_u32(header.payload_arena_len);
    out.put_u32(FIXED_RECORD_LEN);
    out.put_u32(VARIABLE_RECORD_LEN);
    out.put_u32(header.flags);
    out.put_i64(header.min_occurred_at_physical);
    out.put_i64(header.max_occurred_at_physical);
    out.put_i64(header.min_ingested_at_physical);
    out.put_i64(header.max_ingested_at_physical);
    out.put_u64(header.batch_schema_generation);
    out.put_u64(header.batch_dictionary_generation_hint);
    out.put_u32(header.variant_dictionary_offset);
    out.put_u32(header.variant_dictionary_len);
    out.put_u32(header.provenance_table_offset);
    out.put_u32(header.provenance_table_len);
    out.put_u32(header.relationship_table_offset);
    out.put_u32(header.relationship_table_len);
}

fn decode_batch_header(bytes: &[u8]) -> Result<HejCompactBatchHeaderV1, FormatError> {
    let header_bytes = slice(bytes, 0, BATCH_HEADER_LEN as usize, "batch header")?;
    let mut reader = Reader::new(header_bytes);
    let magic = reader.take(MAGIC_LEN, "batch magic")?;
    if magic != b"HCB1" {
        return Err(FormatError::BadMagic { expected: "HCB1" });
    }
    let version = reader.u16("batch version")?;
    if version != 1 {
        return Err(FormatError::UnsupportedVersion {
            field: "HEJCompactBatchHeaderV1.version",
            found: u32::from(version),
        });
    }
    let header_len = reader.u16("batch header_len")?;
    if u32::from(header_len) != BATCH_HEADER_LEN {
        return Err(FormatError::Structural {
            rule: "batch header_len must be 128",
        });
    }
    let event_count = reader.u32("event_count")?;
    let fixed_table_offset = reader.u32("fixed_table_offset")?;
    let fixed_table_len = reader.u32("fixed_table_len")?;
    let variable_table_offset = reader.u32("variable_table_offset")?;
    let variable_table_len = reader.u32("variable_table_len")?;
    let string_table_offset = reader.u32("string_table_offset")?;
    let string_table_len = reader.u32("string_table_len")?;
    let payload_arena_offset = reader.u32("payload_arena_offset")?;
    let payload_arena_len = reader.u32("payload_arena_len")?;
    let fixed_record_len = reader.u32("fixed_record_len")?;
    let variable_record_len = reader.u32("variable_record_len")?;
    let flags = reader.u32("batch flags")?;
    if flags & !(BATCH_FLAG_SIGNED_PROVENANCE | BATCH_FLAG_RELATIONSHIPS) != 0 {
        return Err(FormatError::ReservedNotZero {
            field: "HEJCompactBatchHeaderV1.flags",
        });
    }
    let min_occurred_at_physical = reader.i64("min_occurred_at")?;
    let max_occurred_at_physical = reader.i64("max_occurred_at")?;
    let min_ingested_at_physical = reader.i64("min_ingested_at")?;
    let max_ingested_at_physical = reader.i64("max_ingested_at")?;
    let batch_schema_generation = reader.u64("batch_schema_generation")?;
    let batch_dictionary_generation_hint = reader.u64("batch_dictionary_generation_hint")?;
    let variant_dictionary_offset = reader.u32("variant_dictionary_offset")?;
    let variant_dictionary_len = reader.u32("variant_dictionary_len")?;
    let provenance_table_offset = reader.u32("provenance_table_offset")?;
    let provenance_table_len = reader.u32("provenance_table_len")?;
    let relationship_table_offset = reader.u32("relationship_table_offset")?;
    let relationship_table_len = reader.u32("relationship_table_len")?;
    if fixed_record_len != FIXED_RECORD_LEN {
        return Err(FormatError::Structural {
            rule: "fixed_record_len must be 128",
        });
    }
    if variable_record_len != VARIABLE_RECORD_LEN {
        return Err(FormatError::Structural {
            rule: "variable_record_len must be 64",
        });
    }
    Ok(HejCompactBatchHeaderV1 {
        event_count,
        fixed_table_offset,
        fixed_table_len,
        variable_table_offset,
        variable_table_len,
        string_table_offset,
        string_table_len,
        payload_arena_offset,
        payload_arena_len,
        flags,
        min_occurred_at_physical,
        max_occurred_at_physical,
        min_ingested_at_physical,
        max_ingested_at_physical,
        batch_schema_generation,
        batch_dictionary_generation_hint,
        variant_dictionary_offset,
        variant_dictionary_len,
        provenance_table_offset,
        provenance_table_len,
        relationship_table_offset,
        relationship_table_len,
    })
}

fn check_section(
    payload_len: usize,
    offset: u32,
    len: u32,
    spans: &mut Vec<(usize, usize)>,
) -> Result<(), FormatError> {
    if !offset.is_multiple_of(SECTION_ALIGN as u32) {
        return Err(FormatError::Structural {
            rule: "section offsets must be 8-byte aligned",
        });
    }
    let start = offset as usize;
    let end = start.checked_add(len as usize).ok_or(FormatError::Structural {
        rule: "section escapes payload",
    })?;
    if end > payload_len {
        return Err(FormatError::Structural {
            rule: "section escapes payload",
        });
    }
    for (other_start, other_end) in spans.iter() {
        if start < *other_end && *other_start < end {
            return Err(FormatError::Structural {
                rule: "sections must not overlap",
            });
        }
    }
    spans.push((start, end));
    Ok(())
}

fn resolve_required<'a>(strings: &[&'a str], reference: u32) -> Result<&'a str, FormatError> {
    strings
        .get(reference as usize)
        .copied()
        .ok_or(FormatError::RefOutOfRange {
            what: "string table reference",
        })
}

fn resolve_optional<'a>(strings: &[&'a str], reference: u32) -> Result<Option<&'a str>, FormatError> {
    if reference == REF_ABSENT {
        return Ok(None);
    }
    resolve_required(strings, reference).map(Some)
}

/// Decodes and validates one batch payload. `expected_event_count` is the frame header's `event_count`; a mismatch
/// invalidates the batch.
pub fn decode_batch(payload: &[u8], expected_event_count: u32) -> Result<DecodedBatch<'_>, FormatError> {
    let header = decode_batch_header(payload)?;
    if header.event_count != expected_event_count {
        return Err(FormatError::Structural {
            rule: "batch event_count must equal frame event_count",
        });
    }
    if header.event_count == 0 {
        return Err(FormatError::Structural {
            rule: "a compact batch must carry at least one event",
        });
    }
    let expected_fixed_len = header
        .event_count
        .checked_mul(FIXED_RECORD_LEN)
        .ok_or(FormatError::Structural {
            rule: "fixed table length must be event_count * 128",
        })?;
    if header.fixed_table_len != expected_fixed_len {
        return Err(FormatError::Structural {
            rule: "fixed table length must be event_count * 128",
        });
    }
    let expected_variable_len = header
        .event_count
        .checked_mul(VARIABLE_RECORD_LEN)
        .ok_or(FormatError::Structural {
            rule: "variable table length must be event_count * 64",
        })?;
    if header.variable_table_len != expected_variable_len {
        return Err(FormatError::Structural {
            rule: "variable table length must be event_count * 64",
        });
    }

    let mut spans = vec![(0usize, BATCH_HEADER_LEN as usize)];
    check_section(
        payload.len(),
        header.fixed_table_offset,
        header.fixed_table_len,
        &mut spans,
    )?;
    check_section(
        payload.len(),
        header.variable_table_offset,
        header.variable_table_len,
        &mut spans,
    )?;
    check_section(
        payload.len(),
        header.string_table_offset,
        header.string_table_len,
        &mut spans,
    )?;
    check_section(
        payload.len(),
        header.variant_dictionary_offset,
        header.variant_dictionary_len,
        &mut spans,
    )?;
    check_section(
        payload.len(),
        header.payload_arena_offset,
        header.payload_arena_len,
        &mut spans,
    )?;
    // The provenance table is optional: its flag, its length, and its presence must agree, so a batch can never claim
    // signatures it does not carry or carry records nothing references.
    let carries_provenance = header.flags & BATCH_FLAG_SIGNED_PROVENANCE != 0;
    if carries_provenance != (header.provenance_table_len != 0) {
        return Err(FormatError::Structural {
            rule: "the signed-provenance flag must be set exactly when a provenance table is present",
        });
    }
    if !header.provenance_table_len.is_multiple_of(PROVENANCE_RECORD_LEN) {
        return Err(FormatError::Structural {
            rule: "provenance table length must be a multiple of 144",
        });
    }
    if carries_provenance {
        check_section(
            payload.len(),
            header.provenance_table_offset,
            header.provenance_table_len,
            &mut spans,
        )?;
    } else if header.provenance_table_offset != 0 {
        return Err(FormatError::Structural {
            rule: "an absent provenance table must declare a zero offset",
        });
    }
    let provenance_count = header.provenance_table_len / PROVENANCE_RECORD_LEN;

    // The relationship table follows the same optional-section discipline as the provenance table.
    let carries_relationships = header.flags & BATCH_FLAG_RELATIONSHIPS != 0;
    if carries_relationships != (header.relationship_table_len != 0) {
        return Err(FormatError::Structural {
            rule: "the relationships flag must be set exactly when a relationship table is present",
        });
    }
    if carries_relationships {
        check_section(
            payload.len(),
            header.relationship_table_offset,
            header.relationship_table_len,
            &mut spans,
        )?;
    } else if header.relationship_table_offset != 0 {
        return Err(FormatError::Structural {
            rule: "an absent relationship table must declare a zero offset",
        });
    }

    // Bytes not covered by any section must be zero padding.
    spans.sort();
    let mut cursor = 0usize;
    for (start, end) in &spans {
        let gap = slice(payload, cursor, start.saturating_sub(cursor), "padding")?;
        if gap.iter().any(|byte| *byte != 0) {
            return Err(FormatError::ReservedNotZero { field: "batch padding" });
        }
        cursor = (*end).max(cursor);
    }
    let tail = slice(payload, cursor, payload.len() - cursor, "padding")?;
    if tail.iter().any(|byte| *byte != 0) {
        return Err(FormatError::ReservedNotZero { field: "batch padding" });
    }

    let string_bytes = slice(
        payload,
        header.string_table_offset as usize,
        header.string_table_len as usize,
        "string table",
    )?;
    let strings = decode_string_table(string_bytes)?;

    let dictionary_bytes = slice(
        payload,
        header.variant_dictionary_offset as usize,
        header.variant_dictionary_len as usize,
        "variant dictionary",
    )?;
    let dictionary = decode_variant_dictionary(dictionary_bytes)?;

    let payload_arena = slice(
        payload,
        header.payload_arena_offset as usize,
        header.payload_arena_len as usize,
        "payload arena",
    )?;

    // Bound the preallocation by what the payload could actually hold — each event needs at least FIXED_RECORD_LEN
    // bytes — so a forged event_count cannot drive an unbounded allocation. The loop below still reads exactly
    // event_count records and refuses on truncation.
    let mut events = Vec::with_capacity((header.event_count as usize).min(payload.len() / FIXED_RECORD_LEN as usize));
    let mut min_occurred = i64::MAX;
    let mut max_occurred = i64::MIN;
    let mut min_ingested = i64::MAX;
    let mut max_ingested = i64::MIN;
    for index in 0..header.event_count {
        let fixed_bytes = slice(
            payload,
            header.fixed_table_offset as usize + (index * FIXED_RECORD_LEN) as usize,
            FIXED_RECORD_LEN as usize,
            "fixed record",
        )?;
        let mut reader = Reader::new(fixed_bytes);
        let fixed = EventFixedRecordV1 {
            event_id: reader.u128("event_id")?,
            stream_id: reader.u64("stream_id")?,
            stream_sequence: reader.u64("stream_sequence")?,
            occurred_at_physical: reader.i64("occurred_at")?,
            ingested_at_physical: reader.i64("ingested_at")?,
            entity_id_hash_low: reader.u64("entity_id_hash_low")?,
            entity_id_hash_high: reader.u64("entity_id_hash_high")?,
            actor_id_hash_low: reader.u64("actor_id_hash_low")?,
            account_id_hash_low: reader.u64("account_id_hash_low")?,
            trace_id_hash_low: reader.u64("trace_id_hash_low")?,
            dedupe_hash_low: reader.u64("dedupe_hash_low")?,
            dedupe_hash_high: reader.u64("dedupe_hash_high")?,
            source_string_ref: reader.u32("source_string_ref")?,
            event_type_string_ref: reader.u32("event_type_string_ref")?,
            entity_type_string_ref: reader.u32("entity_type_string_ref")?,
            schema_version: reader.u32("schema_version")?,
            flags: reader.u32("flags")?,
            variable_record_index: reader.u32("variable_record_index")?,
        };
        if fixed.variable_record_index != index {
            return Err(FormatError::Structural {
                rule: "variable_record_index must equal the event ordinal in HEJ v1",
            });
        }
        min_occurred = min_occurred.min(fixed.occurred_at_physical);
        max_occurred = max_occurred.max(fixed.occurred_at_physical);
        min_ingested = min_ingested.min(fixed.ingested_at_physical);
        max_ingested = max_ingested.max(fixed.ingested_at_physical);

        let variable_bytes = slice(
            payload,
            header.variable_table_offset as usize + (index * VARIABLE_RECORD_LEN) as usize,
            VARIABLE_RECORD_LEN as usize,
            "variable record",
        )?;
        let mut reader = Reader::new(variable_bytes);
        let variable = EventVariableRecordV1 {
            payload_flags: reader.u32("payload_flags")?,
            payload_offset: reader.u32("payload_offset")?,
            payload_len: reader.u32("payload_len")?,
            entity_id_string_ref: reader.u32("entity_id_string_ref")?,
            actor_id_string_ref: reader.u32("actor_id_string_ref")?,
            account_id_string_ref: reader.u32("account_id_string_ref")?,
            source_schema_ref: reader.u32("source_schema_ref")?,
            source_delivery_ref: reader.u32("source_delivery_ref")?,
            connector_delivery_hash_low: reader.u64("connector_delivery_hash_low")?,
            connector_delivery_hash_high: reader.u64("connector_delivery_hash_high")?,
            provenance_ref: reader.u32("provenance_ref")?,
            relationships_ref: reader.u32("relationships_ref")?,
        };
        // A batch without a relationship table keeps the field reserved-zero, exactly as it was before the table
        // existed.
        if !carries_relationships && variable.relationships_ref != 0 {
            return Err(FormatError::ReservedNotZero {
                field: "EventVariableRecordV1.relationships_ref",
            });
        }
        let reserved_1 = reader.u64("variable reserved_zero_1")?;
        if reserved_1 != 0 {
            return Err(FormatError::ReservedNotZero {
                field: "EventVariableRecordV1 reserved",
            });
        }
        if variable.payload_flags & !PAYLOAD_FLAG_EXTERNAL_REF != 0 {
            return Err(FormatError::ReservedNotZero {
                field: "EventVariableRecordV1.payload_flags",
            });
        }

        let payload_slice = if variable.payload_len == 0 {
            if variable.payload_offset != 0 || variable.payload_flags != 0 {
                return Err(FormatError::Structural {
                    rule: "payload_len == 0 requires zero payload_offset and payload_flags",
                });
            }
            None
        } else {
            let bytes = slice(
                payload_arena,
                variable.payload_offset as usize,
                variable.payload_len as usize,
                "event payload",
            )?;
            if variable.payload_flags & PAYLOAD_FLAG_EXTERNAL_REF != 0 {
                // External references must be UTF-8; they are internal-only.
                std::str::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
                    what: "external payload reference",
                })?;
            } else {
                VariantRef::new(bytes).validate(&dictionary)?;
            }
            Some(bytes)
        };

        let provenance = decode_provenance(payload, &header, provenance_count, variable.provenance_ref)?;
        let relationships = if carries_relationships {
            decode_relationships(payload, &header, variable.relationships_ref)?
        } else {
            None
        };

        events.push(DecodedEvent {
            provenance,
            relationships,
            source: resolve_required(&strings, fixed.source_string_ref)?,
            event_type: resolve_required(&strings, fixed.event_type_string_ref)?,
            entity_type: resolve_required(&strings, fixed.entity_type_string_ref)?,
            entity_id: resolve_optional(&strings, variable.entity_id_string_ref)?,
            actor_id: resolve_optional(&strings, variable.actor_id_string_ref)?,
            account_id: resolve_optional(&strings, variable.account_id_string_ref)?,
            source_schema: resolve_optional(&strings, variable.source_schema_ref)?,
            source_delivery: resolve_optional(&strings, variable.source_delivery_ref)?,
            payload: payload_slice,
            fixed,
            variable,
        });
    }

    // The header's declared occurred/ingested bounds are redundant with the events; a mismatch means a forged or
    // corrupt header, so reject it rather than trust the summary over the rows. `event_count >= 1` is enforced above,
    // so the accumulated bounds are always initialized.
    if min_occurred != header.min_occurred_at_physical
        || max_occurred != header.max_occurred_at_physical
        || min_ingested != header.min_ingested_at_physical
        || max_ingested != header.max_ingested_at_physical
    {
        return Err(FormatError::Structural {
            rule: "batch header timestamp bounds must match the decoded events",
        });
    }

    Ok(DecodedBatch {
        header,
        strings,
        dictionary,
        dictionary_bytes,
        payload_arena,
        events,
    })
}

/// Reads one event's provenance record out of the batch's provenance table, or `None` when the event carries no
/// signature. A reference past the end of the table is a corrupt batch, not an absent signature.
fn decode_provenance(
    payload: &[u8],
    header: &HejCompactBatchHeaderV1,
    provenance_count: u32,
    provenance_ref: u32,
) -> Result<Option<SignedEventProvenance>, FormatError> {
    if provenance_ref == REF_ABSENT {
        return Ok(None);
    }
    if provenance_ref >= provenance_count {
        return Err(FormatError::RefOutOfRange {
            what: "provenance table reference",
        });
    }
    let record = slice(
        payload,
        header.provenance_table_offset as usize + (provenance_ref * PROVENANCE_RECORD_LEN) as usize,
        PROVENANCE_RECORD_LEN as usize,
        "provenance record",
    )?;
    let mut reader = Reader::new(record);
    let author_pubkey = fixed_bytes::<AUTHOR_PUBKEY_LEN>(reader.take(AUTHOR_PUBKEY_LEN, "author_pubkey")?)?;
    let protocol_event_id =
        fixed_bytes::<PROTOCOL_EVENT_ID_LEN>(reader.take(PROTOCOL_EVENT_ID_LEN, "protocol_event_id")?)?;
    let signature = fixed_bytes::<SIGNATURE_LEN>(reader.take(SIGNATURE_LEN, "signature")?)?;
    let claimed_at = reader.i64("claimed_at")?;
    let protocol_kind = reader.u32("protocol_kind")?;
    let scheme = scheme_from_code(reader.u32("signature_scheme")?)?;
    Ok(Some(SignedEventProvenance {
        author_pubkey,
        claimed_at: TimestampValue::from_physical_nanos(claimed_at),
        protocol_event_id,
        protocol_kind,
        scheme,
        signature,
    }))
}

fn fixed_bytes<const N: usize>(bytes: &[u8]) -> Result<[u8; N], FormatError> {
    bytes.try_into().map_err(|_| FormatError::Truncated {
        what: "provenance record field",
    })
}

/// Reads one event's relationship entry out of the batch's relationship table, or `None` when the event declares no
/// relationships. An offset past the end of the table is a corrupt batch, not an absent declaration.
fn decode_relationships(
    payload: &[u8],
    header: &HejCompactBatchHeaderV1,
    relationships_ref: u32,
) -> Result<Option<EventRelationships>, FormatError> {
    if relationships_ref == REF_ABSENT {
        return Ok(None);
    }
    if relationships_ref >= header.relationship_table_len {
        return Err(FormatError::RefOutOfRange {
            what: "relationship table reference",
        });
    }
    let table = slice(
        payload,
        header.relationship_table_offset as usize,
        header.relationship_table_len as usize,
        "relationship table",
    )?;
    let entry = slice(
        table,
        relationships_ref as usize,
        table.len() - relationships_ref as usize,
        "relationship entry",
    )?;
    let mut reader = Reader::new(entry);
    let ref_count = reader.u32("relationship ref_count")? as usize;
    let mut refs = Vec::with_capacity(reader.capacity_hint(ref_count, 8 + EVENT_ID_TARGET_LEN));
    for _ in 0..ref_count {
        let kind = relationship_kind_from_code(reader.u32("relationship kind")?)?;
        let space = target_space_from_code(reader.u32("relationship target space")?)?;
        let target_len = match space.fixed_len() {
            Some(len) => len,
            None => reader.u32("relationship target length")? as usize,
        };
        let target_ref = reader.take(target_len, "relationship target")?.to_vec();
        refs.push(
            RelationshipRef::new(kind, space, target_ref).map_err(|_| FormatError::Structural {
                rule: "relationship target length must match its identifier space",
            })?,
        );
    }
    EventRelationships::new(refs)
        .map(Some)
        .map_err(|_| FormatError::Structural {
            rule: "an event may declare at most one parent and one root reference",
        })
}

/// Reconstructs the logical envelope of decoded event `index` (tenant, epoch, and sequence come from the frame header).
pub fn envelope_of(event: &DecodedEvent<'_>, tenant_id: crate::events::TenantId) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::from_uuid(Uuid::from_u128(event.fixed.event_id)),
        tenant_id,
        stream_id: StreamId(event.fixed.stream_id),
        stream_sequence: event.fixed.stream_sequence,
        occurred_at: TimestampValue::from_physical_nanos(event.fixed.occurred_at_physical),
        ingested_at: TimestampValue::from_physical_nanos(event.fixed.ingested_at_physical),
        source: event.source.to_owned(),
        event_type: event.event_type.to_owned(),
        entity_type: event.entity_type.to_owned(),
        entity_id_hash_low: event.fixed.entity_id_hash_low,
        entity_id_hash_high: event.fixed.entity_id_hash_high,
        entity_id: event.entity_id.map(str::to_owned),
        actor_id_hash_low: event.fixed.actor_id_hash_low,
        actor_id: event.actor_id.map(str::to_owned),
        account_id_hash_low: event.fixed.account_id_hash_low,
        account_id: event.account_id.map(str::to_owned),
        trace_id_hash_low: event.fixed.trace_id_hash_low,
        dedupe_hash_low: event.fixed.dedupe_hash_low,
        dedupe_hash_high: event.fixed.dedupe_hash_high,
        schema_version: event.fixed.schema_version,
        flags: EventFlags(event.fixed.flags),
    }
}

#[cfg(test)]
#[path = "test/batch.rs"]
pub(crate) mod tests;
