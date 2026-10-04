use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::clock::FixedClock;
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::invariants::sim::SimulatedPublishedSet;
use crate::layout::LayoutTargets;
use crate::object_store::sim::SimObjectStore;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefRow, build_hef_file};
use crate::writer::publish::NoopPeerNotices;

const START_NANOS: i64 = 1_700_000_000_000_000_000;

fn tenant() -> TenantId {
    TenantId::new_test_id(9)
}

fn event(i: u64) -> EventInput {
    EventInput {
        envelope: EventEnvelope {
            event_id: EventId::new_test_id(0xCAFE + u128::from(i)),
            tenant_id: tenant(),
            stream_id: StreamId(1),
            stream_sequence: i,
            occurred_at: TimestampValue::from_physical_nanos(100 + i as i64),
            ingested_at: TimestampValue::from_physical_nanos(200 + i as i64),
            source: "crm".into(),
            event_type: "deal.updated".into(),
            entity_type: "opportunity".into(),
            entity_id_hash_low: i,
            entity_id_hash_high: 0,
            entity_id: Some(format!("opp-{i}")),
            actor_id_hash_low: 0,
            actor_id: None,
            account_id_hash_low: 0,
            account_id: None,
            trace_id_hash_low: 0,
            dedupe_hash_low: 1000 + i,
            dedupe_hash_high: 7,
            schema_version: 1,
            flags: EventFlags(0),
        },
        payload: PayloadInput::Variant(VariantValue::Int(i as i64)),
        source_schema: None,
        source_delivery: None,
        connector_delivery_hash_low: 5000 + i,
        connector_delivery_hash_high: 1,
        provenance: None,
        relationships: None,
    }
}

fn build_config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 1,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 0,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets::default(),
        tenant_id: tenant(),
    }
}

/// The compaction output: one file holding sequences 1 through 4.
fn merged_file() -> BuiltHef {
    let rows = (1..=4)
        .map(|sequence| HefRow {
            epoch: 1,
            sequence,
            event: event(sequence),
        })
        .collect();
    build_hef_file(rows, &build_config()).unwrap()
}

fn entry(file_id: u128, first_sequence: u64, last_sequence: u64, part_state: PartState) -> HefFileEntry {
    HefFileEntry {
        coverage: SequenceRange {
            epoch: 1,
            first_sequence,
            last_sequence,
        },
        feature_metadata: None,
        file_seal: [1; 32],
        file_id,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state,
        required_feature_flags: 0,
        size_bytes: 100,
        tenant_id: tenant(),
        tree_len: None,
    }
}

/// Writes `generation` and makes it the head.
fn publish_directly(set: &mut SimulatedPublishedSet, generation: ManifestGeneration) {
    let id = generation.generation;
    set.put_generation(generation).unwrap();
    set.advance_head(id - 1, id).unwrap();
}

#[test]
fn a_compaction_publishes_one_generation_that_adds_the_output_and_outdates_the_inputs() {
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Active), entry(102, 3, 4, PartState::Active)],
            generation: 1,
            ..Default::default()
        },
    );
    let objects = SimObjectStore::new();
    let mut notices = NoopPeerNotices::default();
    let clock = FixedClock::at(START_NANOS);
    let built = merged_file();
    let bytes = built.bytes.clone();

    let published = HefPublisher::new()
        .publish_compaction(
            built,
            &build_config(),
            &[102, 101],
            &mut set,
            &objects,
            &mut notices,
            &clock,
        )
        .unwrap();

    assert_eq!(published.generation, 2);
    assert_eq!(
        set.generation(3),
        Err(PublishError::UnknownGeneration),
        "exactly one generation"
    );
    let (head_id, head) = set.head().unwrap();
    assert_eq!(head_id, 2);
    assert_eq!(
        head.files,
        vec![
            entry(101, 1, 2, PartState::Outdated),
            entry(102, 3, 4, PartState::Outdated),
            published.entry.clone(),
        ]
    );
    assert_eq!(
        published.entry.coverage,
        SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 4
        }
    );
    assert_eq!(published.entry.part_state, PartState::Active);
    let snapshot: Vec<u128> = head.snapshot_files().map(|file| file.file_id).collect();
    assert_eq!(snapshot, vec![published.entry.file_id]);
    assert_eq!(
        head.retirements,
        vec![
            Retirement {
                file_id: 102,
                generation: 2,
                since_nanos: START_NANOS
            },
            Retirement {
                file_id: 101,
                generation: 2,
                since_nanos: START_NANOS
            },
        ]
    );
    assert_eq!(
        objects.object(&hef_object_key(tenant(), published.entry.file_id)),
        Some(bytes)
    );
    assert_eq!(notices.published, vec![published.entry.file_id]);
}

#[test]
fn a_compaction_over_a_gap_is_refused_before_anything_is_written() {
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Active), entry(102, 4, 4, PartState::Active)],
            generation: 1,
            ..Default::default()
        },
    );
    let objects = SimObjectStore::new();

    let result = HefPublisher::new().publish_compaction(
        merged_file(),
        &build_config(),
        &[101, 102],
        &mut set,
        &objects,
        &mut NoopPeerNotices::default(),
        &FixedClock::at(START_NANOS),
    );

    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    assert_eq!(set.head().unwrap().0, 1);
    assert!(objects.keys().is_empty());
}

#[test]
fn the_sweeper_deletes_nothing_a_live_snapshot_may_still_read() {
    let policy = SweepPolicy {
        in_flight_query_horizon_nanos: 100,
        safety_window_nanos: 1_000,
    };
    let clock = FixedClock::at(START_NANOS);
    let objects = SimObjectStore::new();
    for file_id in [101, 102, 103] {
        objects
            .put_if_absent(&hef_object_key(tenant(), file_id), b"hef")
            .unwrap();
    }
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Active), entry(102, 3, 4, PartState::Active)],
            generation: 1,
            ..Default::default()
        },
    );
    // Generation 2 is the compaction: 101 and 102 replaced by 103.
    let retired = |file_id| Retirement {
        file_id,
        generation: 2,
        since_nanos: START_NANOS,
    };
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![
                entry(101, 1, 2, PartState::Outdated),
                entry(102, 3, 4, PartState::Outdated),
                entry(103, 1, 4, PartState::Active),
            ],
            generation: 2,
            retirements: vec![retired(101), retired(102)],
            ..Default::default()
        },
    );

    // A query still reads generation 1, where 101 and 102 are live: however long it runs, nothing moves.
    clock.advance(1_000_000);
    let report = sweep_retired_files(&mut set, &objects, &policy, 1, &clock).unwrap();
    assert_eq!(report, SweepReport::default());
    assert_eq!(set.head().unwrap().0, 2);

    // Every live snapshot is at generation 2 or later, so no query selected the inputs: they become DeleteOnDestroy,
    // but their objects stay for the safety window.
    let report = sweep_retired_files(&mut set, &objects, &policy, 2, &clock).unwrap();
    assert_eq!(report.unreferenced, vec![101, 102]);
    assert!(report.deleted.is_empty());
    assert_eq!(report.generation, Some(3));
    assert_eq!(objects.keys().len(), 3);

    clock.advance(999);
    let report = sweep_retired_files(&mut set, &objects, &policy, 3, &clock).unwrap();
    assert_eq!(report, SweepReport::default(), "the safety window has not passed");
    assert_eq!(objects.keys().len(), 3);

    // After the safety window the objects are deleted and the entries leave the catalogue.
    clock.advance(1);
    let report = sweep_retired_files(&mut set, &objects, &policy, 3, &clock).unwrap();
    assert_eq!(report.deleted, vec![101, 102]);
    assert_eq!(report.generation, Some(4));
    assert_eq!(objects.keys(), vec![hef_object_key(tenant(), 103)]);
    let (_, head) = set.head().unwrap();
    assert_eq!(head.files, vec![entry(103, 1, 4, PartState::Active)]);
    assert!(head.retirements.is_empty());
}

#[test]
fn an_outdated_file_waits_out_the_in_flight_query_horizon() {
    let policy = SweepPolicy {
        in_flight_query_horizon_nanos: 100,
        safety_window_nanos: 1_000,
    };
    let clock = FixedClock::at(START_NANOS);
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Outdated)],
            generation: 1,
            retirements: vec![Retirement {
                file_id: 101,
                generation: 1,
                since_nanos: START_NANOS,
            }],
            ..Default::default()
        },
    );
    let objects = SimObjectStore::new();

    clock.advance(99);
    let report = sweep_retired_files(&mut set, &objects, &policy, 1, &clock).unwrap();
    assert_eq!(report, SweepReport::default());

    clock.advance(1);
    let report = sweep_retired_files(&mut set, &objects, &policy, 1, &clock).unwrap();
    assert_eq!(report.unreferenced, vec![101]);
}

#[test]
fn a_retired_file_without_a_record_starts_its_clock_instead_of_being_swept() {
    let policy = SweepPolicy {
        in_flight_query_horizon_nanos: 100,
        safety_window_nanos: 1_000,
    };
    let clock = FixedClock::at(START_NANOS);
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::DeleteOnDestroy)],
            generation: 1,
            ..Default::default()
        },
    );
    let objects = SimObjectStore::new();
    objects.put_if_absent(&hef_object_key(tenant(), 101), b"hef").unwrap();

    let report = sweep_retired_files(&mut set, &objects, &policy, 1, &clock).unwrap();

    assert!(report.deleted.is_empty());
    assert_eq!(report.generation, Some(2));
    let (_, head) = set.head().unwrap();
    assert_eq!(
        head.retirements,
        vec![Retirement {
            file_id: 101,
            generation: 2,
            since_nanos: START_NANOS
        }]
    );
    assert_eq!(objects.keys().len(), 1);
}
