use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::clock::FixedClock;
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::error::InboxError;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, SequencePoint, StreamId, TimestampValue};
use crate::invariants::sim::SimulatedPublishedSet;
use crate::layout::LayoutTargets;
use crate::layout::inbox::{AckWatermarks, Recipient};
use crate::layout::reader::HefFile;
use crate::object_store::sim::SimObjectStore;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefRow, build_hef_file};
use crate::writer::publish::NoopPeerNotices;
use hashbrown::HashMap;

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
        audit: None,
        created_at_physical: 1,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 0,
        graph: None,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile: crate::layout::Profile::Event,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets::default(),
        tenant_id: tenant(),
        time_series_bucket_nanos: None,
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
        profile: crate::layout::Profile::Event,
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
fn a_compaction_of_an_audit_file_is_refused_before_anything_is_written() {
    let mut audit_input = entry(102, 3, 4, PartState::Active);
    audit_input.profile = crate::layout::Profile::Audit;
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Active), audit_input],
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
    assert!(matches!(
        result,
        Err(PublishFailure::Verification(AUDIT_COMPACTION_REFUSED))
    ));

    // An audit file as the output is refused the same way.
    let rows = (1..=4)
        .map(crate::writer::source_form::tests::plain_row)
        .collect::<Vec<_>>();
    let audit_output = build_hef_file(
        rows,
        &HefBuildConfig {
            audit: Some(crate::writer::audit::AuditSettings {
                chain_start: crate::layout::constant::AUDIT_GENESIS,
                legal_hold: false,
                retention: None,
            }),
            profile: crate::layout::Profile::Audit,
            ..crate::writer::source_form::tests::source_form_config(4)
        },
    )
    .unwrap();
    let result = HefPublisher::new().publish_compaction(
        audit_output,
        &build_config(),
        &[101],
        &mut set,
        &objects,
        &mut NoopPeerNotices::default(),
        &FixedClock::at(START_NANOS),
    );
    assert!(matches!(
        result,
        Err(PublishFailure::Verification(AUDIT_COMPACTION_REFUSED))
    ));
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

/// Acknowledgement watermarks held in memory.
#[derive(Default)]
struct Acks(HashMap<Recipient, SequencePoint>);

impl AckWatermarks for Acks {
    fn acked_through(&self, _tenant_id: TenantId, recipient: Recipient) -> Result<Option<SequencePoint>, InboxError> {
        Ok(self.0.get(&recipient).copied())
    }
}

fn recipient(entity_id_hash_low: u64) -> Recipient {
    Recipient {
        entity_id_hash_high: 0,
        entity_id_hash_low,
    }
}

/// An inbox file of sequences 1 through 4 for two recipients: 1 and 3 to recipient 1, 2 and 4 to recipient 0.
fn inbox_file() -> HefFile {
    let rows = (1..=4)
        .map(|sequence| {
            let mut event = event(sequence);
            event.envelope.entity_id_hash_low = sequence % 2;
            HefRow {
                epoch: 1,
                event,
                sequence,
            }
        })
        .collect();
    let config = HefBuildConfig {
        profile: Profile::Inbox,
        ..build_config()
    };
    let built = build_hef_file(rows, &config).unwrap();
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

#[test]
fn an_inbox_file_is_retired_once_every_recipient_acks_it_and_then_swept_like_any_replaced_file() {
    let file = inbox_file();
    let file_id = file.header().file_id;
    let inbox = HefFileEntry {
        file_id,
        profile: Profile::Inbox,
        ..entry(file_id, 1, 4, PartState::Active)
    };
    let objects = SimObjectStore::new();
    objects
        .put_if_absent(&hef_object_key(tenant(), file_id), b"hef")
        .unwrap();
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![inbox.clone()],
            generation: 1,
            ..Default::default()
        },
    );
    let clock = FixedClock::at(START_NANOS);

    let mut acks = Acks::default();
    acks.0.insert(recipient(1), SequencePoint { epoch: 1, sequence: 3 });
    assert!(!file.fully_acked(&acks).unwrap(), "recipient 0 has not acknowledged");
    acks.0.insert(recipient(0), SequencePoint { epoch: 1, sequence: 2 });
    assert!(
        !file.fully_acked(&acks).unwrap(),
        "recipient 0's sequence 4 is unacknowledged"
    );
    acks.0.insert(recipient(0), SequencePoint { epoch: 1, sequence: 4 });
    assert!(file.fully_acked(&acks).unwrap());

    assert_eq!(retire_acked_inbox_files(&mut set, &[file_id], &clock).unwrap(), Some(2));
    let (_, head) = set.head().unwrap();
    assert_eq!(
        head.files,
        vec![HefFileEntry {
            part_state: PartState::Outdated,
            ..inbox
        }]
    );
    assert_eq!(head.snapshot_files().count(), 0);
    assert_eq!(
        head.retirements,
        vec![Retirement {
            file_id,
            generation: 2,
            since_nanos: START_NANOS
        }]
    );
    assert_eq!(
        retire_acked_inbox_files(&mut set, &[file_id], &clock).unwrap(),
        None,
        "an already retired file is skipped"
    );

    let policy = SweepPolicy {
        in_flight_query_horizon_nanos: 100,
        safety_window_nanos: 1_000,
    };
    clock.advance(100);
    let report = sweep_retired_files(&mut set, &objects, &policy, 2, &clock).unwrap();
    assert_eq!(report.unreferenced, vec![file_id]);
    clock.advance(1_000);
    let report = sweep_retired_files(&mut set, &objects, &policy, 3, &clock).unwrap();
    assert_eq!(report.deleted, vec![file_id]);
    assert!(objects.keys().is_empty());
    assert!(set.head().unwrap().1.files.is_empty());
}

#[test]
fn only_inbox_files_retire_by_acknowledgement() {
    let mut set = SimulatedPublishedSet::new();
    publish_directly(
        &mut set,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Active)],
            generation: 1,
            ..Default::default()
        },
    );
    let result = retire_acked_inbox_files(&mut set, &[101], &FixedClock::at(START_NANOS));
    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    assert_eq!(set.head().unwrap().0, 1);
}

/// A snapshot file of `epoch`, keyed `opp-1` through `opp-{keys}`.
fn snapshot_file(epoch: u64, keys: u64) -> BuiltHef {
    let mut config = build_config();
    config.profile = Profile::Snapshot;
    let rows = (1..=keys)
        .map(|sequence| HefRow {
            epoch,
            sequence,
            event: event(sequence),
        })
        .collect();
    build_hef_file(rows, &config).unwrap()
}

fn publish_snapshot(
    set: &mut SimulatedPublishedSet,
    objects: &SimObjectStore,
    built: BuiltHef,
) -> Result<Published, PublishFailure> {
    let mut config = build_config();
    config.profile = Profile::Snapshot;
    HefPublisher::new().publish_snapshot(
        built,
        &config,
        set,
        objects,
        &mut NoopPeerNotices::default(),
        &FixedClock::at(START_NANOS),
    )
}

/// Each epoch's snapshot replaces the last in one generation: the new file is the only active one and the old one is
/// outdated with its retirement recorded, so the sweeper can delete it later.
#[test]
fn a_snapshot_replaces_the_previous_epoch_in_one_generation() {
    let mut set = SimulatedPublishedSet::new();
    let objects = SimObjectStore::new();
    let first = publish_snapshot(&mut set, &objects, snapshot_file(1, 4)).unwrap();
    assert_eq!(first.generation, 1);
    assert_eq!(first.entry.profile, Profile::Snapshot);
    assert_eq!(
        first.entry.coverage,
        SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 4
        }
    );

    let second = publish_snapshot(&mut set, &objects, snapshot_file(2, 4)).unwrap();
    assert_eq!(second.generation, 2);
    let (_, head) = set.head().unwrap();
    let mut outdated = first.entry.clone();
    outdated.part_state = PartState::Outdated;
    assert_eq!(head.files, vec![outdated, second.entry.clone()]);
    assert_eq!(
        head.retirements,
        vec![Retirement {
            file_id: first.entry.file_id,
            generation: 2,
            since_nanos: START_NANOS
        }]
    );
    let active: Vec<u128> = head.snapshot_files().map(|file| file.file_id).collect();
    assert_eq!(active, vec![second.entry.file_id]);
}

/// A stale writer cannot put an older epoch, or other content for the same epoch, over the current one; republishing
/// the current file changes nothing; and a snapshot never lands in a catalogue of event files or carries an event
/// file.
#[test]
fn a_snapshot_publication_refuses_an_older_epoch_a_mixed_catalogue_and_an_event_file() {
    let mut set = SimulatedPublishedSet::new();
    let objects = SimObjectStore::new();
    let published = publish_snapshot(&mut set, &objects, snapshot_file(2, 4)).unwrap();
    for (epoch, keys) in [(1, 4), (2, 3)] {
        let result = publish_snapshot(&mut set, &objects, snapshot_file(epoch, keys));
        assert!(matches!(result, Err(PublishFailure::Verification(_))), "epoch {epoch}");
    }
    assert_eq!(set.head().unwrap().0, 1);
    let again = publish_snapshot(&mut set, &objects, snapshot_file(2, 4)).unwrap();
    assert_eq!(
        (again.generation, again.entry),
        (1, published.entry),
        "republishing the same file is a no-op"
    );

    let mut events = SimulatedPublishedSet::new();
    publish_directly(
        &mut events,
        ManifestGeneration {
            files: vec![entry(101, 1, 2, PartState::Active)],
            generation: 1,
            ..Default::default()
        },
    );
    let result = publish_snapshot(&mut events, &objects, snapshot_file(3, 4));
    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    assert_eq!(events.head().unwrap().0, 1);

    let result = publish_snapshot(&mut SimulatedPublishedSet::new(), &objects, merged_file());
    assert!(matches!(result, Err(PublishFailure::Verification(_))));
}
