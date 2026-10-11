use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::deletes::CorrectionMetadata;
use crate::error::EntityScanError;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::layout::{LayoutTargets, Profile};
use crate::security::FooterEncryption;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};
use hashbrown::HashMap;

const RECIPIENTS: u64 = 3;
const ROWS: u64 = 30;
const HASH_BASE: u64 = 100;
const HASH_HIGH: u64 = 7;

fn tenant() -> TenantId {
    TenantId::new_test_id(4)
}

fn recipient(index: u64) -> Recipient {
    Recipient {
        entity_id_hash_high: HASH_HIGH,
        entity_id_hash_low: HASH_BASE + index,
    }
}

fn point(sequence: u64) -> SequencePoint {
    SequencePoint { epoch: 1, sequence }
}

/// The queue rows in arrival order: the recipients' messages interleave round-robin.
fn rows() -> Vec<HefRow> {
    (1..=ROWS)
        .map(|sequence| {
            let to = (sequence - 1) % RECIPIENTS;
            HefRow {
                epoch: 1,
                event: EventInput {
                    connector_delivery_hash_high: 0,
                    connector_delivery_hash_low: sequence,
                    envelope: EventEnvelope {
                        account_id: None,
                        account_id_hash_low: 0,
                        actor_id: None,
                        actor_id_hash_low: 0,
                        dedupe_hash_high: 1,
                        dedupe_hash_low: sequence,
                        entity_id: Some(format!("device-{to}")),
                        entity_id_hash_high: HASH_HIGH,
                        entity_id_hash_low: HASH_BASE + to,
                        entity_type: "device".to_owned(),
                        event_id: EventId::new_test_id(0x1_0000 + u128::from(sequence)),
                        event_type: "to_device".to_owned(),
                        flags: EventFlags(0),
                        ingested_at: TimestampValue::from_physical_nanos(2_000 + sequence as i64),
                        occurred_at: TimestampValue::from_physical_nanos(1_000 + sequence as i64),
                        schema_version: 1,
                        source: "inbox".to_owned(),
                        stream_id: StreamId(1),
                        stream_sequence: sequence,
                        tenant_id: tenant(),
                        trace_id_hash_low: 0,
                    },
                    payload: PayloadInput::Variant(VariantValue::Int(sequence as i64)),
                    provenance: None,
                    relationships: None,
                    source_delivery: None,
                    source_schema: None,
                },
                sequence,
            }
        })
        .collect()
}

fn build(profile: Profile) -> HefFile {
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        audit: None,
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        graph: None,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 4,
            ..LayoutTargets::default()
        },
        tenant_id: tenant(),
    };
    let built = build_hef_file(rows(), &config).unwrap();
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

/// Every sequence addressed to recipient `index`, in queue order.
fn queue_of(index: u64) -> Vec<u64> {
    (1..=ROWS)
        .filter(|sequence| (sequence - 1) % RECIPIENTS == index)
        .collect()
}

fn sequences(events: &[EntityEvent]) -> Vec<u64> {
    events.iter().map(|event| event.point.sequence).collect()
}

/// Acknowledgement watermarks held in memory.
#[derive(Default)]
struct Acks(HashMap<Recipient, SequencePoint>);

impl AckWatermarks for Acks {
    fn acked_through(&self, _tenant_id: TenantId, recipient: Recipient) -> Result<Option<SequencePoint>, InboxError> {
        Ok(self.0.get(&recipient).copied())
    }
}

/// Deleted rows held in memory, per file.
#[derive(Default)]
struct Deletes(HashMap<u128, DeletionVector>);

impl DeletesAndCorrections for Deletes {
    fn deletion_vector(&self, _tenant_id: TenantId, file_id: u128) -> Result<Option<DeletionVector>, EntityScanError> {
        Ok(self.0.get(&file_id).cloned())
    }

    fn is_correction(&self, _tenant_id: TenantId, _point: SequencePoint) -> Result<bool, EntityScanError> {
        Ok(false)
    }

    fn latest_correction(
        &self,
        _tenant_id: TenantId,
        _event_id: EventId,
    ) -> Result<Option<CorrectionMetadata>, EntityScanError> {
        Ok(None)
    }
}

#[test]
fn an_inbox_file_groups_rows_by_recipient_and_records_each_recipients_range() {
    let file = build(Profile::Inbox);
    assert_eq!(file.header().profile, Profile::Inbox);
    assert_ne!(
        file.footer().optional_feature_flags & optional_features::INBOX_RECIPIENT_DIRECTORY,
        0
    );
    let directory = file.inbox_recipients().unwrap();
    let expected: Vec<InboxRecipientEntry> = (0..RECIPIENTS)
        .map(|index| InboxRecipientEntry {
            entity_id_hash_high: HASH_HIGH,
            entity_id_hash_low: HASH_BASE + index,
            first_row_ordinal: index * (ROWS / RECIPIENTS),
            last_epoch: 1,
            last_sequence: *queue_of(index).last().unwrap(),
            row_count: ROWS / RECIPIENTS,
        })
        .collect();
    assert_eq!(directory, expected.as_slice());

    // The directory's ranges are where the rows really are.
    for (index, entry) in directory.iter().enumerate() {
        for ordinal in entry.first_row_ordinal..entry.first_row_ordinal + entry.row_count {
            assert_eq!(
                file.envelope(ordinal).unwrap().entity_id_hash_low,
                HASH_BASE + index as u64
            );
        }
    }
}

#[test]
fn an_event_file_carries_no_recipient_directory() {
    let file = build(Profile::Event);
    assert!(file.inbox_recipients().is_none());
    assert!(!file.fully_acked(&Acks::default()).unwrap());
}

#[test]
fn reading_a_queue_never_returns_a_row_at_or_below_the_ack_watermark() {
    let file = build(Profile::Inbox);
    let deletes = Deletes::default();
    let mut acks = Acks::default();

    let all = read_inbox(&[&file], tenant(), recipient(1), &acks, &deletes).unwrap();
    assert_eq!(sequences(&all), queue_of(1));
    assert!(
        all.iter()
            .all(|event| event.envelope.entity_id_hash_low == HASH_BASE + 1)
    );
    assert_eq!(
        all[0].payload,
        crate::layout::reader::PayloadRead::Value(VariantValue::Int(2))
    );

    // Acknowledging up to and including sequence 14 hides it and everything before it; the other recipients' queues
    // are untouched.
    acks.0.insert(recipient(1), point(14));
    let rest = read_inbox(&[&file], tenant(), recipient(1), &acks, &deletes).unwrap();
    assert_eq!(sequences(&rest), vec![17, 20, 23, 26, 29]);
    assert!(rest.iter().all(|event| event.point > point(14)));
    let other = read_inbox(&[&file], tenant(), recipient(0), &acks, &deletes).unwrap();
    assert_eq!(sequences(&other), queue_of(0));

    // A watermark between two of the recipient's rows works the same.
    acks.0.insert(recipient(1), point(15));
    let rest = read_inbox(&[&file], tenant(), recipient(1), &acks, &deletes).unwrap();
    assert_eq!(sequences(&rest), vec![17, 20, 23, 26, 29]);

    // Acknowledging the newest row empties the queue.
    acks.0.insert(recipient(1), point(29));
    assert!(
        read_inbox(&[&file], tenant(), recipient(1), &acks, &deletes)
            .unwrap()
            .is_empty()
    );
    // A recipient with no rows has an empty queue.
    assert!(
        read_inbox(&[&file], tenant(), recipient(9), &acks, &deletes)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reading_a_queue_skips_deleted_rows() {
    let file = build(Profile::Inbox);
    let entry = file.inbox_recipients().unwrap()[2];
    let mut vector = DeletionVector::new();
    // The recipient's second and fourth rows.
    vector.mark_deleted(entry.first_row_ordinal + 1);
    vector.mark_deleted(entry.first_row_ordinal + 3);
    let deletes = Deletes(HashMap::from([(file.header().file_id, vector)]));
    let mut acks = Acks::default();
    acks.0.insert(recipient(2), point(3));

    let events = read_inbox(&[&file], tenant(), recipient(2), &acks, &deletes).unwrap();
    assert_eq!(sequences(&events), vec![9, 15, 18, 21, 24, 27, 30]);
}

#[test]
fn a_file_without_a_directory_returns_the_same_queue() {
    let inbox = build(Profile::Inbox);
    let event = build(Profile::Event);
    let deletes = Deletes::default();
    let mut acks = Acks::default();
    acks.0.insert(recipient(0), point(10));
    for index in 0..RECIPIENTS {
        let from_inbox = read_inbox(&[&inbox], tenant(), recipient(index), &acks, &deletes).unwrap();
        let from_event = read_inbox(&[&event], tenant(), recipient(index), &acks, &deletes).unwrap();
        assert_eq!(from_inbox, from_event);
    }
}

#[test]
fn a_file_is_fully_acked_only_once_every_recipient_acks_its_newest_row() {
    let file = build(Profile::Inbox);
    let mut acks = Acks::default();
    assert!(!file.fully_acked(&acks).unwrap());
    acks.0.insert(recipient(0), point(28));
    acks.0.insert(recipient(1), point(29));
    assert!(
        !file.fully_acked(&acks).unwrap(),
        "recipient 2 has acknowledged nothing"
    );
    acks.0.insert(recipient(2), point(29));
    assert!(!file.fully_acked(&acks).unwrap(), "recipient 2's newest row is 30");
    acks.0.insert(recipient(2), point(30));
    assert!(file.fully_acked(&acks).unwrap());
    // A watermark in a later epoch is past every row of this one.
    acks.0.insert(recipient(0), SequencePoint { epoch: 2, sequence: 1 });
    assert!(file.fully_acked(&acks).unwrap());
}
