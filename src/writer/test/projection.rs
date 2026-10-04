use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan, column_ids};
use crate::encoding::ColumnData;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, SequencePoint, StreamId, TenantId, TimestampValue};
use crate::invariants::sim::SerialEncodeExecutor;
use crate::layout::LayoutTargets;
use crate::layout::footer::GranuleEntry;
use crate::layout::reader::HefFile;
use crate::security::FooterEncryption;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefRow, build_hef_file};
use std::collections::BTreeMap;

const ROOMS: u64 = 4;
const EVENTS_PER_ROOM: u64 = 600;
const ROOM_HASH_BASE: u64 = 1_000;
const ROOM_HASH_HIGH: u64 = 9;

fn tenant() -> TenantId {
    TenantId::new_test_id(3)
}

/// The sequence of room `room`'s `index`-th event: the rooms' events arrive interleaved round-robin, the way a busy
/// tenant's ingest spreads every room across every file.
fn sequence_of(room: u64, index: u64) -> u64 {
    1 + index * ROOMS + room
}

fn row(sequence: u64) -> HefRow {
    let room = (sequence - 1) % ROOMS;
    HefRow {
        epoch: 1,
        event: EventInput {
            connector_delivery_hash_high: 0,
            connector_delivery_hash_low: sequence,
            envelope: EventEnvelope {
                account_id: None,
                account_id_hash_low: 4,
                actor_id: None,
                actor_id_hash_low: 3,
                dedupe_hash_high: 6,
                dedupe_hash_low: sequence,
                entity_id: Some(format!("room-{room}")),
                entity_id_hash_high: ROOM_HASH_HIGH,
                entity_id_hash_low: ROOM_HASH_BASE + room,
                entity_type: "room".to_owned(),
                event_id: EventId::new_test_id(0xE000 + u128::from(sequence)),
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
            payload: PayloadInput::Variant(VariantValue::Object(BTreeMap::from([(
                "n".to_owned(),
                VariantValue::Int(sequence as i64),
            )]))),
            provenance: None,
            relationships: None,
            source_delivery: None,
            source_schema: None,
        },
        sequence,
    }
}

/// Every room's events in ingest order.
fn ingest_rows() -> Vec<HefRow> {
    (1..=ROOMS * EVENTS_PER_ROOM).map(row).collect()
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
        lifecycle: BuildLifecycle::RewriteOrCompaction,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 128,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 64 * 1024,
        },
        tenant_id: tenant(),
    }
}

fn open(built: BuiltHef) -> HefFile {
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

fn u64s(file: &HefFile, column_id: u32, granule: &GranuleEntry) -> Vec<u64> {
    match file.read_column(column_id, granule.granule_id).unwrap().data {
        ColumnData::U64(values) => values,
        other => panic!("expected a u64 column, got {other:?}"),
    }
}

fn point(sequence: u64) -> SequencePoint {
    SequencePoint { epoch: 1, sequence }
}

/// After compaction into the entity projection, a run of 100 events of one room is read from at most two granules,
/// and those granules hold all 100; the ingest file spreads the same run over more.
#[test]
fn a_hundred_event_range_of_one_entity_reads_at_most_two_granules() {
    let ingest = open(build_hef_file(ingest_rows(), &config(1)).unwrap());
    let projection = open(build_entity_projection(ingest_rows(), &config(2), &SerialEncodeExecutor).unwrap());
    let room = 2;
    let from = point(sequence_of(room, 250));
    let to = point(sequence_of(room, 349));

    let granules = projection.entity_granules(ROOM_HASH_BASE + room, ROOM_HASH_HIGH, from, to);
    assert!(
        granules.len() <= 2,
        "read {} granules for a 100-event range",
        granules.len()
    );
    let held: usize = granules
        .iter()
        .map(|granule| {
            let entities = u64s(&projection, column_ids::ENTITY_ID_HASH_LOW, granule);
            let sequences = u64s(&projection, column_ids::SEQUENCE, granule);
            entities
                .iter()
                .zip(&sequences)
                .filter(|&(&entity, &sequence)| {
                    entity == ROOM_HASH_BASE + room && (from.sequence..=to.sequence).contains(&sequence)
                })
                .count()
        })
        .sum();
    assert_eq!(held, 100);

    let spread = ingest.entity_granules(ROOM_HASH_BASE + room, ROOM_HASH_HIGH, from, to);
    assert!(
        spread.len() > 2,
        "the ingest file spreads the range over {} granules",
        spread.len()
    );
}

/// In the entity projection, an entity's min/max skips every granule that holds none of its rows.
#[test]
fn entity_min_max_skips_every_granule_without_the_entity() {
    let projection = open(build_entity_projection(ingest_rows(), &config(2), &SerialEncodeExecutor).unwrap());
    let granules = &projection.footer().granules;
    let everything = (point(0), point(u64::MAX));

    for room in 0..ROOMS {
        let entity = ROOM_HASH_BASE + room;
        let holding: Vec<u32> = granules
            .iter()
            .filter(|granule| u64s(&projection, column_ids::ENTITY_ID_HASH_LOW, granule).contains(&entity))
            .map(|granule| granule.granule_id)
            .collect();
        let read: Vec<u32> = projection
            .entity_granules(entity, ROOM_HASH_HIGH, everything.0, everything.1)
            .iter()
            .map(|granule| granule.granule_id)
            .collect();
        assert_eq!(read, holding, "room {room}");
        assert!(read.len() < granules.len(), "room {room} skips other rooms' granules");
    }
}

/// The entity order is accepted only when building the entity projection: the ordinary build still refuses rows out
/// of `(epoch, sequence)` order, and the projection refuses a point given twice.
#[test]
fn only_the_entity_projection_accepts_entity_order() {
    let mut entity_ordered = ingest_rows();
    entity_ordered.sort_by_key(|row| (row.event.envelope.entity_id_hash_low, row.epoch, row.sequence));
    assert!(build_hef_file(entity_ordered.clone(), &config(1)).is_err());

    let projection = open(build_entity_projection(entity_ordered, &config(2), &SerialEncodeExecutor).unwrap());
    assert_eq!(projection.header().row_count, ROOMS * EVENTS_PER_ROOM);
    assert_eq!(projection.header().min_sequence, 1);
    assert_eq!(projection.header().max_sequence, ROOMS * EVENTS_PER_ROOM);

    let mut repeated = ingest_rows();
    repeated.push(row(1));
    assert!(build_entity_projection(repeated, &config(2), &SerialEncodeExecutor).is_err());
}

/// Every granule of the entity projection records that it is sorted by entity, then `(epoch, sequence)` - never the
/// primary order it does not have - and still carries its true sequence bounds.
#[test]
fn the_entity_projection_records_its_sort_order_and_sequence_bounds() {
    let projection = open(build_entity_projection(ingest_rows(), &config(2), &SerialEncodeExecutor).unwrap());
    let footer = projection.footer();
    assert!(footer.all_granules_sorted_for(
        0,
        &["entity_id_hash_low", "entity_id_hash_high", "epoch", "sequence"],
        SortDirection::Ascending,
    ));
    assert!(!footer.all_granules_sorted_for(0, &["epoch", "sequence"], SortDirection::Ascending));

    for granule in &footer.granules {
        let sequences = u64s(&projection, column_ids::SEQUENCE, granule);
        assert_eq!(granule.first_sequence, *sequences.iter().min().unwrap());
        assert_eq!(granule.last_sequence, *sequences.iter().max().unwrap());
    }
}
