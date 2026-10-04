use super::super::frame::{self, FrameBuildInput};
use super::*;
use crate::events::sim::{SimulatedEventAuthor, signed_event_payload};
use crate::events::variant::encode_value;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::typed_id::TypedIdTestExt;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) fn sample_events(count: usize) -> Vec<EventInput> {
    (0..count)
        .map(|i| {
            let mut payload_fields = BTreeMap::new();
            payload_fields.insert("kind".to_owned(), VariantValue::String(format!("k{i}")));
            payload_fields.insert("amount".to_owned(), VariantValue::Int(1000 + i as i64));
            EventInput {
                envelope: EventEnvelope {
                    event_id: EventId::new_test_id(0x1000 + i as u128),
                    tenant_id: TenantId::new_test_id(7),
                    stream_id: StreamId(42),
                    stream_sequence: 100 + i as u64,
                    occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64),
                    ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64),
                    source: "crm".to_owned(),
                    event_type: "deal.updated".to_owned(),
                    entity_type: "opportunity".to_owned(),
                    entity_id_hash_low: 11 + i as u64,
                    entity_id_hash_high: 12,
                    entity_id: (i % 2 == 0).then(|| format!("opp-{i}")),
                    actor_id_hash_low: 13,
                    actor_id: None,
                    account_id_hash_low: 14,
                    account_id: Some("acct-1".to_owned()),
                    trace_id_hash_low: 15,
                    dedupe_hash_low: 16 + i as u64,
                    dedupe_hash_high: 17,
                    schema_version: 3,
                    flags: EventFlags(0),
                },
                payload: PayloadInput::Variant(VariantValue::Object(payload_fields)),
                source_schema: None,
                source_delivery: Some(format!("delivery-{i}")),
                connector_delivery_hash_low: 21,
                connector_delivery_hash_high: 22,
                provenance: None,
                relationships: None,
            }
        })
        .collect()
}

pub(crate) fn sample_frame(count: usize, epoch: u64, first_sequence: u64) -> Vec<u8> {
    let events = sample_events(count);
    let payload = build_batch(&events, 1, 0).unwrap();
    frame::build_frame(
        &FrameBuildInput {
            flags: 0,
            tenant_id: TenantId::new_test_id(7),
            writer_id: 3,
            epoch,
            first_sequence,
            last_sequence: first_sequence + count as u64 - 1,
            event_count: count as u32,
            durable_batch_id: 900,
            writer_local_batch_id: 1,
            schema_generation: 1,
            dictionary_generation_hint: 0,
            created_at_physical: 1,
            committed_at_physical: 2,
        },
        &payload,
    )
    .unwrap()
}

#[test]
fn documented_sizes_and_magics() {
    // HEJFrameHeaderV1: fixed 192-byte header, magic "HEJ1", LE fields at their documented offsets.
    let frame_bytes = sample_frame(3, 5, 101);
    assert_eq!(&frame_bytes[0..4], b"HEJ1");
    assert_eq!(u16::from_le_bytes([frame_bytes[4], frame_bytes[5]]), 1);
    assert_eq!(u16::from_le_bytes([frame_bytes[6], frame_bytes[7]]), 192);
    // frame_len at offset 8 is one of the allowed normal sizes.
    let frame_len = u32::from_le_bytes(frame_bytes[8..12].try_into().unwrap());
    assert!(super::super::NORMAL_FRAME_SIZES.contains(&frame_len));
    assert_eq!(frame_bytes.len() as u32, frame_len);
    // epoch at offset 48, first_sequence at 56, last_sequence at 64.
    assert_eq!(u64::from_le_bytes(frame_bytes[48..56].try_into().unwrap()), 5);
    assert_eq!(u64::from_le_bytes(frame_bytes[56..64].try_into().unwrap()), 101);
    assert_eq!(u64::from_le_bytes(frame_bytes[64..72].try_into().unwrap()), 103);
    // event_count at 72, payload_encoding at 76 must be 1.
    assert_eq!(u32::from_le_bytes(frame_bytes[72..76].try_into().unwrap()), 3);
    assert_eq!(u32::from_le_bytes(frame_bytes[76..80].try_into().unwrap()), 1);

    // HEJCompactBatchHeaderV1: magic "HCB1", header_len 128, fixed_record_len 128 at offset 44, variable_record_len 64
    // at 48.
    let payload = &frame_bytes[192..];
    assert_eq!(&payload[0..4], b"HCB1");
    assert_eq!(u16::from_le_bytes(payload[6..8].try_into().unwrap()), 128);
    assert_eq!(u32::from_le_bytes(payload[44..48].try_into().unwrap()), 128);
    assert_eq!(u32::from_le_bytes(payload[48..52].try_into().unwrap()), 64);
}

#[test]
fn batch_round_trip_preserves_envelopes_and_payloads() {
    let events = sample_events(4);
    let payload = build_batch(&events, 9, 0).unwrap();
    let decoded = decode_batch(&payload, 4).unwrap();
    assert_eq!(decoded.header.batch_schema_generation, 9);
    for (input, output) in events.iter().zip(decoded.events.iter()) {
        assert_eq!(envelope_of(output, TenantId::new_test_id(7)), input.envelope);
        let PayloadInput::Variant(expected) = &input.payload else {
            panic!("sample uses variant payloads");
        };
        let bytes = output.payload.expect("payload present");
        let value = VariantRef::new(bytes).decode(&decoded.dictionary).unwrap();
        assert_eq!(&value, expected);
    }
}

/// The same payloads as [`sample_events`], pre-encoded against a per-event dictionary — the form a payload arrives in
/// when it comes back out of a worker's commit queue.
fn encoded_payload_events(count: usize) -> Vec<EventInput> {
    sample_events(count)
        .into_iter()
        .map(|mut event| {
            let PayloadInput::Variant(value) = &event.payload else {
                panic!("sample uses variant payloads");
            };
            let mut keys = BTreeSet::new();
            value.collect_keys(&mut keys);
            let dictionary = KeyDictionary::build(keys.into_iter().map(str::to_owned));
            event.payload = PayloadInput::Encoded(EncodedPayload {
                bytes: encode_value(value, &dictionary).unwrap(),
                dictionary,
            });
            event
        })
        .collect()
}

#[test]
fn an_already_encoded_payload_builds_the_same_batch_as_its_value_tree() {
    // A payload that reaches the batch already encoded is re-encoded against the frame dictionary in place of being
    // decoded into a value tree first. Both routes must produce the same batch, byte for byte, or which route an event
    // took would change what is journaled.
    let from_values = build_batch(&sample_events(4), 9, 0).unwrap();
    let from_encoded = build_batch(&encoded_payload_events(4), 9, 0).unwrap();
    assert_eq!(from_encoded, from_values);
}

#[test]
fn a_nested_already_encoded_payload_transcodes_to_the_same_bytes() {
    // Nesting is where the two routes could diverge: the frame dictionary's wider ids can change an object's id and
    // offset table widths, and arrays and long strings must survive the copy untouched.
    let mut nested = BTreeMap::new();
    nested.insert("zeta".to_owned(), VariantValue::String("z".repeat(300)));
    nested.insert(
        "alpha".to_owned(),
        VariantValue::Array(vec![
            VariantValue::Int(-9_000_000_000),
            VariantValue::Double(1.5),
            VariantValue::Object(BTreeMap::from([(
                "deep".to_owned(),
                VariantValue::Binary(vec![1, 2, 3]),
            )])),
        ]),
    );
    let payload = VariantValue::Object(nested);

    let mut with_value = sample_events(2);
    with_value[1].payload = PayloadInput::Variant(payload.clone());
    let mut with_encoded = encoded_payload_events(2);
    let mut keys = BTreeSet::new();
    payload.collect_keys(&mut keys);
    let dictionary = KeyDictionary::build(keys.into_iter().map(str::to_owned));
    with_encoded[1].payload = PayloadInput::Encoded(EncodedPayload {
        bytes: encode_value(&payload, &dictionary).unwrap(),
        dictionary,
    });

    assert_eq!(
        build_batch(&with_encoded, 9, 0).unwrap(),
        build_batch(&with_value, 9, 0).unwrap()
    );
}

#[test]
fn forged_event_count_that_overflows_the_length_math_is_rejected() {
    // A batch header whose event_count would overflow `event_count * FIXED_RECORD_LEN` must refuse — not wrap the
    // length equality check (release) or panic (debug), and never drive an unbounded preallocation from the count.
    let events = sample_events(1);
    let mut payload = build_batch(&events, 9, 0).unwrap();
    // event_count is a u32 at offset 8 of the batch header; 2^26 * 128 overflows u32.
    let forged: u32 = 0x0400_0000;
    payload[8..12].copy_from_slice(&forged.to_le_bytes());
    assert!(decode_batch(&payload, forged).is_err());
}

#[test]
fn build_batch_rejects_a_payload_nested_beyond_the_decoder_depth() {
    // The decoder caps variant nesting at 128; a deeper payload must be refused at build time instead of being
    // journaled as a frame whose batch can never be decoded.
    let mut events = sample_events(1);
    let mut value = VariantValue::Int(1);
    for _ in 0..200 {
        value = VariantValue::Array(vec![value]);
    }
    events[0].payload = PayloadInput::Variant(value);
    assert!(build_batch(&events, 1, 0).is_err());
}

#[test]
fn build_batch_rejects_an_empty_external_ref() {
    // An empty reference would encode as payload_len == 0 with the external-ref flag set, which decode_batch rejects;
    // the build side must fail instead of producing a batch its own decoder cannot read.
    let mut events = sample_events(1);
    events[0].payload = PayloadInput::ExternalRef(String::new());
    assert!(build_batch(&events, 1, 0).is_err());

    // A non-empty reference still round-trips.
    events[0].payload = PayloadInput::ExternalRef("hej://tenant/7/blob/1".to_owned());
    let payload = build_batch(&events, 1, 0).unwrap();
    let decoded = decode_batch(&payload, 1).unwrap();
    assert_eq!(decoded.events[0].payload, Some("hej://tenant/7/blob/1".as_bytes()));
    assert_eq!(decoded.events[0].variable.payload_flags, PAYLOAD_FLAG_EXTERNAL_REF);
}

#[test]
fn frame_decode_validates_and_round_trips() {
    let frame_bytes = sample_frame(2, 1, 1);
    let (header, payload) = frame::decode_frame(&frame_bytes).unwrap();
    assert_eq!(header.event_count, 2);
    let decoded = decode_batch(payload, header.event_count).unwrap();
    assert_eq!(decoded.events.len(), 2);

    // Corrupting any byte breaks the authoritative BLAKE3.
    let mut corrupted = frame_bytes.clone();
    *corrupted.last_mut().unwrap() ^= 0xFF;
    assert!(frame::decode_frame(&corrupted).is_err());

    // A wrong payload_encoding is rejected.
    let mut wrong_encoding = frame_bytes.clone();
    wrong_encoding[76] = 2;
    assert!(frame::decode_frame(&wrong_encoding).is_err());
}

#[test]
fn live_overlay_conversion_is_deterministic_and_ordered() {
    use super::super::overlay;
    let frame_bytes = sample_frame(3, 2, 50);
    let (header, payload) = frame::decode_frame(&frame_bytes).unwrap();
    let decoded = decode_batch(payload, header.event_count).unwrap();
    let segment = overlay::convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("not covered");
    assert_eq!(segment.batch.num_rows(), 3);
    let sequences = segment
        .batch
        .column(2)
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    assert_eq!(sequences.values(), &[50, 51, 52]);
    // payload_ref = (payload_len << 32) | payload_offset.
    let refs = segment
        .batch
        .column(21)
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap();
    for (row, value) in refs.values().iter().enumerate() {
        let variable = &decoded.events[row].variable;
        assert_eq!(value >> 32, u64::from(variable.payload_len));
        assert_eq!(value & 0xFFFF_FFFF, u64::from(variable.payload_offset));
    }
    // Skip rule: a fully HEF-covered range converts to nothing.
    let skipped = overlay::convert_frame(&header, &decoded, 1, 1, None, |_, _| true).unwrap();
    assert!(skipped.is_none());
    // Determinism: converting twice yields byte-equivalent batches.
    let again = overlay::convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .unwrap();
    assert_eq!(format!("{:?}", segment.batch), format!("{:?}", again.batch));
}

#[test]
fn header_timestamp_bounds_must_match_the_decoded_events() {
    // The batch header carries redundant min/max occurred/ingested bounds. A header whose bounds disagree with the
    // events it declares is forged or corrupt, and decode must reject it rather than trust the summary over the rows.
    let events = sample_events(4);
    let mut payload = build_batch(&events, 1, 0).unwrap();
    // min_occurred_at_physical is the i64 at header offset 56 (magic+version+header_len+event_count + 8 u32 table
    // fields + fixed_record_len + variable_record_len + flags). Shift it off the events' true minimum.
    payload[56..64].copy_from_slice(&i64::MIN.to_le_bytes());
    let err = decode_batch(&payload, 4).expect_err("mismatched header timestamp bounds must be rejected");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "batch header timestamp bounds must match the decoded events"
        }
    );
}

#[test]
fn a_signed_event_survives_the_batch_round_trip_byte_exactly() {
    // Two events, one signed and one not: the provenance table carries only the signed one, and the unsigned event
    // decodes with no provenance at all.
    let author = SimulatedEventAuthor::from_seed(4);
    let payload = signed_event_payload("hello", &[&["e", "root"]]);
    let provenance = author.sign(1, 1_700_000_000, &payload).expect("well-shaped payload");
    let mut events = sample_events(2);
    if let Some(event) = events.get_mut(1) {
        event.payload = PayloadInput::Variant(payload.clone());
        event.provenance = Some(provenance.clone());
    }

    let encoded = build_batch(&events, 1, 0).expect("batch builds");
    let decoded = decode_batch(&encoded, 2).expect("batch decodes");
    assert_eq!(decoded.events.first().and_then(|e| e.provenance.clone()), None);
    assert_eq!(
        decoded.events.get(1).and_then(|e| e.provenance.clone()),
        Some(provenance)
    );

    // The decoded event still re-verifies: the canonical bytes rebuild from the stored form alone.
    let stored = decoded.events.get(1).expect("two events");
    let stored_payload = VariantRef::new(stored.payload.expect("a payload"))
        .decode(&decoded.dictionary)
        .expect("payload decodes");
    stored
        .provenance
        .as_ref()
        .expect("provenance survived")
        .verify(&stored_payload)
        .expect("the stored event re-verifies");
}

#[test]
fn an_unsigned_batch_carries_no_provenance_bytes_at_all() {
    let encoded = build_batch(&sample_events(3), 1, 0).expect("batch builds");
    let decoded = decode_batch(&encoded, 3).expect("batch decodes");
    assert_eq!(decoded.header.flags & BATCH_FLAG_SIGNED_PROVENANCE, 0);
    assert_eq!(decoded.header.provenance_table_len, 0);
    assert_eq!(decoded.header.provenance_table_offset, 0);
    assert!(decoded.events.iter().all(|event| event.provenance.is_none()));
}

#[test]
fn prev_and_auth_references_to_external_ids_survive_the_journal() {
    let mut refs: Vec<RelationshipRef> = (0..20)
        .map(|i| RelationshipRef::to_external(RelationshipKind::Prev, format!("$prev-{i}").as_bytes()).unwrap())
        .collect();
    refs.extend(
        (0..10).map(|i| RelationshipRef::to_external(RelationshipKind::Auth, format!("$auth-{i}").as_bytes()).unwrap()),
    );
    refs.push(RelationshipRef::to_external(RelationshipKind::Prev, b"$abc:example.org").unwrap());
    refs.push(RelationshipRef::to_event(RelationshipKind::Parent, 7));
    let relationships = EventRelationships::new(refs).unwrap();
    let mut events = sample_events(2);
    events[1].relationships = Some(relationships.clone());
    let encoded = build_batch(&events, 1, 0).expect("batch builds");
    let decoded = decode_batch(&encoded, 2).expect("batch decodes");
    assert_eq!(
        decoded.events.get(1).and_then(|event| event.relationships.clone()),
        Some(relationships)
    );
}
