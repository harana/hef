use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput, build_batch, decode_batch};
use crate::artifacts::frame::{FrameBuildInput, build_frame, decode_frame};
use crate::artifacts::overlay::convert_frame;
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::{EventFlags, StreamId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::security::FooterEncryption;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};
use std::collections::BTreeMap;

/// Even sequences belong to room A, odd ones to room B.
const ROOM_A: u64 = 100;
const ROOM_B: u64 = 200;
const ROOM_HASH_HIGH: u64 = 9;

fn tenant() -> TenantId {
    TenantId::new_test_id(3)
}

fn room_of(sequence: u64) -> u64 {
    if sequence.is_multiple_of(2) { ROOM_A } else { ROOM_B }
}

fn event_id(sequence: u64) -> EventId {
    EventId::new_test_id(0xE000 + u128::from(sequence))
}

fn payload(sequence: u64) -> BTreeMap<String, VariantValue> {
    BTreeMap::from([
        ("n".to_owned(), VariantValue::Int(sequence as i64)),
        ("text".to_owned(), VariantValue::String(format!("message {sequence}"))),
    ])
}

fn event(sequence: u64, payload: BTreeMap<String, VariantValue>) -> EventInput {
    EventInput {
        connector_delivery_hash_high: 0,
        connector_delivery_hash_low: sequence,
        envelope: EventEnvelope {
            account_id: None,
            account_id_hash_low: 4,
            actor_id: None,
            actor_id_hash_low: 3,
            dedupe_hash_high: 6,
            dedupe_hash_low: sequence,
            entity_id: Some(format!("room-{}", room_of(sequence))),
            entity_id_hash_high: ROOM_HASH_HIGH,
            entity_id_hash_low: room_of(sequence),
            entity_type: "room".to_owned(),
            event_id: event_id(sequence),
            event_type: "message.sent".to_owned(),
            flags: EventFlags(0),
            ingested_at: TimestampValue::from_physical_nanos(2_000 + sequence as i64),
            occurred_at: TimestampValue::from_physical_nanos(1_000 + sequence as i64),
            schema_version: 1,
            source: "chat".to_owned(),
            stream_id: StreamId(1),
            stream_sequence: sequence,
            tenant_id: tenant(),
            trace_id_hash_low: 5,
        },
        payload: PayloadInput::Variant(VariantValue::Object(payload)),
        provenance: None,
        relationships: None,
        source_delivery: None,
        source_schema: None,
    }
}

fn config(generation_id: u64) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: FooterEncryption::Plaintext,
        freetext: FreetextDeclaration { fields: Vec::new() },
        freetext_row_offset_index: false,
        generation_id,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 4,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 4096,
        },
        tenant_id: tenant(),
    }
}

/// A published file holding sequences `first..=last` of epoch 1, each event's payload chosen by `payload_of`.
fn file(
    first: u64,
    last: u64,
    generation_id: u64,
    payload_of: impl Fn(u64) -> BTreeMap<String, VariantValue>,
) -> HefFile {
    let rows: Vec<HefRow> = (first..=last)
        .map(|sequence| HefRow {
            epoch: 1,
            event: event(sequence, payload_of(sequence)),
            sequence,
        })
        .collect();
    let built = build_hef_file(rows, &config(generation_id)).unwrap();
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

/// An overlay holding one not-yet-published segment of sequences `first..=last` of epoch 1.
fn overlay(first: u64, last: u64) -> LiveOverlayStore {
    let events: Vec<EventInput> = (first..=last)
        .map(|sequence| event(sequence, payload(sequence)))
        .collect();
    let batch = build_batch(&events, 1, 0).unwrap();
    let frame = build_frame(
        &FrameBuildInput {
            committed_at_physical: 2,
            created_at_physical: 1,
            dictionary_generation_hint: 0,
            durable_batch_id: 1,
            epoch: 1,
            event_count: events.len() as u32,
            first_sequence: first,
            flags: 0,
            last_sequence: last,
            schema_generation: 1,
            tenant_id: tenant(),
            writer_id: 1,
            writer_local_batch_id: 1,
        },
        &batch,
    )
    .unwrap();
    let (header, body) = decode_frame(&frame).unwrap();
    let decoded = decode_batch(body, header.event_count).unwrap();
    let segment = convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .unwrap();
    let mut store = LiveOverlayStore::new();
    store.publish(segment);
    store
}

fn scan(direction: SortDirection, limit: Option<usize>) -> EntityScan {
    EntityScan {
        direction,
        entity_id_hash_high: ROOM_HASH_HIGH,
        entity_id_hash_low: ROOM_A,
        limit,
        sequence_range: SequencePoint { epoch: 1, sequence: 0 }..=SequencePoint {
            epoch: 1,
            sequence: u64::MAX,
        },
        tenant_id: tenant(),
    }
}

fn sequences(events: &[EntityEvent]) -> Vec<u64> {
    events.iter().map(|event| event.point.sequence).collect()
}

/// The application's recorded deletes and corrections, held in memory.
#[derive(Default)]
struct RecordedDeletes {
    corrections: Vec<CorrectionMetadata>,
    deleted: HashMap<u128, DeletionVector>,
}

impl DeletesAndCorrections for RecordedDeletes {
    fn deletion_vector(&self, _tenant_id: TenantId, file_id: u128) -> Result<Option<DeletionVector>, EntityScanError> {
        Ok(self.deleted.get(&file_id).cloned())
    }

    fn is_correction(&self, _tenant_id: TenantId, point: SequencePoint) -> Result<bool, EntityScanError> {
        Ok(self.corrections.iter().any(|correction| {
            correction.correction_epoch == point.epoch && correction.correction_sequence == point.sequence
        }))
    }

    fn latest_correction(
        &self,
        _tenant_id: TenantId,
        event_id: EventId,
    ) -> Result<Option<CorrectionMetadata>, EntityScanError> {
        Ok(self
            .corrections
            .iter()
            .filter(|correction| correction.corrects_event_id == event_id)
            .max_by_key(|correction| correction.correction_generation)
            .cloned())
    }
}

fn correction(original: u64, correcting: u64, correction_type: CorrectionType) -> CorrectionMetadata {
    CorrectionMetadata {
        correction_epoch: 1,
        correction_generation: 1,
        correction_sequence: correcting,
        correction_type,
        corrects_event_id: event_id(original),
    }
}

/// One room's events come back in sequence order from two published files and the overlay together, each exactly as
/// it was written, and none of the other room's.
#[test]
fn a_scan_spans_two_files_and_the_overlay() {
    let older = file(1, 20, 1, payload);
    let newer = file(21, 40, 2, payload);
    let fresh = overlay(41, 50);

    let events = scan(SortDirection::Ascending, None)
        .run(&[&older, &newer], &fresh, &RecordedDeletes::default())
        .unwrap();

    assert_eq!(sequences(&events), (2..=50).step_by(2).collect::<Vec<_>>());
    for event in &events {
        let sequence = event.point.sequence;
        assert_eq!(event.envelope, self::event(sequence, payload(sequence)).envelope);
        assert_eq!(
            event.payload,
            PayloadRead::Value(VariantValue::Object(payload(sequence)))
        );
    }
}

/// Where two files hold the same point, the copy from the newer generation is served, once.
#[test]
fn the_newest_generation_wins_a_repeated_point() {
    let ingest = file(1, 20, 1, payload);
    let rewritten = file(1, 20, 2, |sequence| {
        BTreeMap::from([("n".to_owned(), VariantValue::Int(1_000 + sequence as i64))])
    });

    let events = scan(SortDirection::Ascending, None)
        .run(
            &[&rewritten, &ingest],
            &LiveOverlayStore::new(),
            &RecordedDeletes::default(),
        )
        .unwrap();

    assert_eq!(sequences(&events), (2..=20).step_by(2).collect::<Vec<_>>());
    for event in &events {
        let rewritten_payload =
            BTreeMap::from([("n".to_owned(), VariantValue::Int(1_000 + event.point.sequence as i64))]);
        assert_eq!(
            event.payload,
            PayloadRead::Value(VariantValue::Object(rewritten_payload))
        );
    }
}

/// Read backwards with a limit, a scan returns the newest events first, crossing from the overlay into the files.
#[test]
fn a_backwards_scan_with_a_limit_returns_the_newest_events() {
    let older = file(1, 20, 1, payload);
    let newer = file(21, 40, 2, payload);
    let fresh = overlay(41, 50);

    let events = scan(SortDirection::Descending, Some(7))
        .run(&[&older, &newer], &fresh, &RecordedDeletes::default())
        .unwrap();

    assert_eq!(sequences(&events), vec![50, 48, 46, 44, 42, 40, 38]);
}

/// A row the application deleted is never served; its neighbours are.
#[test]
fn a_deleted_row_is_skipped() {
    let older = file(1, 20, 1, payload);
    let mut deletes = RecordedDeletes::default();
    let mut vector = DeletionVector::new();
    // Sequences 1..=20 sit at row ordinals 0..=19, so sequence 4 is ordinal 3.
    vector.mark_deleted(3);
    deletes.deleted.insert(older.header().file_id, vector);

    let events = scan(SortDirection::Ascending, None)
        .run(&[&older], &LiveOverlayStore::new(), &deletes)
        .unwrap();

    assert_eq!(sequences(&events), vec![2, 6, 8, 10, 12, 14, 16, 18, 20]);
}

/// A replaced event is served as its replacement, in the original's place, and the replacement is not served a second
/// time at its own place.
#[test]
fn a_replacement_correction_is_returned_instead_of_the_original() {
    let older = file(1, 20, 1, payload);
    let newer = file(21, 40, 2, payload);
    let deletes = RecordedDeletes {
        corrections: vec![correction(6, 30, CorrectionType::Replacement)],
        ..RecordedDeletes::default()
    };

    let events = scan(SortDirection::Ascending, None)
        .run(&[&older, &newer], &LiveOverlayStore::new(), &deletes)
        .unwrap();

    let expected: Vec<u64> = (2..=40).step_by(2).filter(|sequence| *sequence != 30).collect();
    assert_eq!(sequences(&events), expected);
    let replaced = events.iter().find(|event| event.point.sequence == 6).unwrap();
    assert_eq!(replaced.envelope.event_id, event_id(30));
    assert_eq!(replaced.payload, PayloadRead::Value(VariantValue::Object(payload(30))));
    assert!(events.iter().all(|event| event.envelope.event_id != event_id(6)));
}

/// An amended event keeps its own envelope and the fields the amendment does not carry, taking the ones it does.
#[test]
fn an_amendment_lays_its_fields_over_the_original() {
    let older = file(1, 20, 1, |sequence| {
        if sequence == 8 {
            BTreeMap::from([("text".to_owned(), VariantValue::String("edited".to_owned()))])
        } else {
            payload(sequence)
        }
    });
    let deletes = RecordedDeletes {
        corrections: vec![correction(4, 8, CorrectionType::Amendment)],
        ..RecordedDeletes::default()
    };

    let events = scan(SortDirection::Ascending, None)
        .run(&[&older], &LiveOverlayStore::new(), &deletes)
        .unwrap();

    assert_eq!(sequences(&events), vec![2, 4, 6, 10, 12, 14, 16, 18, 20]);
    let amended = events.iter().find(|event| event.point.sequence == 4).unwrap();
    assert_eq!(amended.envelope.event_id, event_id(4));
    let mut expected = payload(4);
    expected.insert("text".to_owned(), VariantValue::String("edited".to_owned()));
    assert_eq!(amended.payload, PayloadRead::Value(VariantValue::Object(expected)));
}

/// A retracted event is not served at all.
#[test]
fn a_retraction_drops_the_event() {
    let older = file(1, 20, 1, payload);
    let deletes = RecordedDeletes {
        corrections: vec![correction(4, 9, CorrectionType::Retraction)],
        ..RecordedDeletes::default()
    };

    let events = scan(SortDirection::Ascending, None)
        .run(&[&older], &LiveOverlayStore::new(), &deletes)
        .unwrap();

    assert_eq!(sequences(&events), vec![2, 6, 8, 10, 12, 14, 16, 18, 20]);
}

/// A correction whose correcting event is in none of the scanned sources is refused rather than serving the stale
/// original.
#[test]
fn a_correction_outside_the_scanned_sources_is_refused() {
    let older = file(1, 20, 1, payload);
    let deletes = RecordedDeletes {
        corrections: vec![correction(4, 99, CorrectionType::Replacement)],
        ..RecordedDeletes::default()
    };

    let refused = scan(SortDirection::Ascending, None).run(&[&older], &LiveOverlayStore::new(), &deletes);

    assert_eq!(
        refused,
        Err(EntityScanError::CorrectionNotFound { epoch: 1, sequence: 99 })
    );
}
