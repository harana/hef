use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::benchmarks::{InboxReadMeasurement, inbox_read_gate_passes};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::deletes::CorrectionMetadata;
use crate::error::EntityScanError;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::layout::footer::encode_footer;
use crate::layout::{LayoutTargets, Profile};
use crate::security::FooterEncryption;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use hashbrown::HashMap;
use std::time::{Duration, Instant};

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

/// `count` queue rows in arrival order: the messages to `recipients` recipients interleave round-robin.
fn rows(recipients: u64, count: u64) -> Vec<HefRow> {
    (1..=count)
        .map(|sequence| {
            let to = (sequence - 1) % recipients;
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

fn config(profile: Profile, index_granularity: usize) -> HefBuildConfig {
    HefBuildConfig {
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
            index_granularity,
            ..LayoutTargets::default()
        },
        tenant_id: tenant(),
        time_series_bucket_nanos: None,
    }
}

fn open(built: BuiltHef) -> HefFile {
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

fn build(profile: Profile) -> HefFile {
    open(build_hef_file(rows(RECIPIENTS, ROWS), &config(profile, 4)).unwrap())
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

/// Whatever rows of a recipient survive deletion - none, all, only the last, all but the leading two, every other one,
/// or a sparse few - each comes back with exactly the envelope and payload stored for it, still intact once the file
/// is dropped. Covers granules holding one recipient's whole queue beside others' and queues split across granules.
#[test]
fn a_queue_read_returns_each_kept_rows_own_envelope_and_payload() {
    let kept_patterns: [&[u64]; 6] = [
        &[],
        &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
        &[9],
        &[2, 3, 4, 5, 6, 7, 8, 9],
        &[1, 3, 5, 7, 9],
        &[2, 5, 6],
    ];
    for granularity in [4, 64] {
        for index in 0..RECIPIENTS {
            for kept in kept_patterns {
                let file = open(build_hef_file(rows(RECIPIENTS, ROWS), &config(Profile::Inbox, granularity)).unwrap());
                let entry = file.inbox_recipients().unwrap()[index as usize];
                let mut vector = DeletionVector::new();
                for offset in (0..entry.row_count).filter(|offset| !kept.contains(offset)) {
                    vector.mark_deleted(entry.first_row_ordinal + offset);
                }
                let expected: Vec<EntityEvent> = kept
                    .iter()
                    .map(|offset| {
                        let row = entry.first_row_ordinal + offset;
                        let granule = file
                            .footer()
                            .granules
                            .iter()
                            .find(|granule| row < granule.first_row_ordinal + u64::from(granule.row_count))
                            .unwrap();
                        let envelopes = file.granule_envelopes(granule.granule_id).unwrap();
                        let envelope = envelopes[(row - granule.first_row_ordinal) as usize].clone();
                        let at = point(envelope.stream_sequence);
                        EntityEvent {
                            envelope,
                            payload: file.payload(row).unwrap(),
                            point: at,
                        }
                    })
                    .collect();
                let deletes = Deletes(HashMap::from([(file.header().file_id, vector)]));

                let events = read_inbox(&[&file], tenant(), recipient(index), &Acks::default(), &deletes).unwrap();
                drop(file);
                assert_eq!(
                    events, expected,
                    "granularity {granularity}, recipient {index}, kept {kept:?}"
                );
            }
        }
    }
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

/// On a file of 1,000 recipients with 20 rows each in 16-row granules, the recipient directory costs 48 bytes per
/// recipient and each recipient's queue reads only the granules holding its rows, where the same rows in an event
/// file, which carries no directory, read more.
#[test]
fn a_queue_read_touches_only_the_granules_holding_the_recipients_rows() {
    let recipients = 1_000;
    let count = recipients * 20;
    let inbox = open(build_hef_file(rows(recipients, count), &config(Profile::Inbox, 16)).unwrap());
    let event = open(build_hef_file(rows(recipients, count), &config(Profile::Event, 16)).unwrap());

    let mut without_directory = inbox.footer().clone();
    without_directory.inbox_recipients.clear();
    let directory_bytes = (encode_footer(inbox.footer()).len() - encode_footer(&without_directory).len()) as u64;

    // Every granule holding a row of each recipient, by its hash, read from the rows themselves.
    let mut covering: HashMap<u64, Vec<u32>> = HashMap::new();
    for granule in &inbox.footer().granules {
        for envelope in inbox.granule_envelopes(granule.granule_id).unwrap() {
            let granules = covering.entry(envelope.entity_id_hash_low).or_default();
            if granules.last() != Some(&granule.granule_id) {
                granules.push(granule.granule_id);
            }
        }
    }

    let mut read_with_directory = 0;
    let mut read_without_directory = 0;
    for index in 0..recipients {
        let measurement = InboxReadMeasurement {
            covering_granules: covering[&(HASH_BASE + index)].len() as u64,
            directory_bytes,
            granules_read: recipient_granules(&inbox, recipient(index), None).len() as u64,
            recipients,
        };
        assert!(inbox_read_gate_passes(&measurement), "recipient {index}");
        read_with_directory += measurement.granules_read;
        read_without_directory += recipient_granules(&event, recipient(index), None).len() as u64;
    }
    let (deletes, acks) = (Deletes::default(), Acks::default());
    assert_eq!(
        read_inbox(&[&inbox], tenant(), recipient(500), &acks, &deletes).unwrap(),
        read_inbox(&[&event], tenant(), recipient(500), &acks, &deletes).unwrap()
    );
    println!(
        "inbox directory: {directory_bytes} B for {recipients} recipients ({:.2} B each); granules read per queue: \
         {:.2} with the directory, {:.2} without, of {} granules",
        directory_bytes as f64 / recipients as f64,
        read_with_directory as f64 / recipients as f64,
        read_without_directory as f64 / recipients as f64,
        inbox.footer().granules.len()
    );
    assert!(read_with_directory < read_without_directory);
}

/// Finding a recipient's granules by binary search over the granule directory picks the same granules, in the same
/// order, as checking every granule against the recipient's row range. Recipients of 20 rows in 16-row granules start
/// and end both at granule bounds and between them; a recipient with no rows gets none.
#[test]
fn a_recipients_granules_are_the_ones_a_full_directory_scan_finds() {
    let recipients = 50;
    let file = open(build_hef_file(rows(recipients, recipients * 20), &config(Profile::Inbox, 16)).unwrap());
    let directory = file.inbox_recipients().unwrap();
    for index in 0..=recipients {
        let scanned: Vec<u32> = directory
            .get(index as usize)
            .map(|entry| {
                let end = entry.first_row_ordinal + entry.row_count;
                file.footer()
                    .granules
                    .iter()
                    .filter(|granule| {
                        granule.first_row_ordinal < end
                            && entry.first_row_ordinal < granule.first_row_ordinal + u64::from(granule.row_count)
                    })
                    .map(|granule| granule.granule_id)
                    .collect()
            })
            .unwrap_or_default();
        let searched: Vec<u32> = recipient_granules(&file, recipient(index), None)
            .iter()
            .map(|granule| granule.granule_id)
            .collect();
        assert_eq!(searched, scanned, "recipient {index}");
        assert_eq!(index == recipients, searched.is_empty(), "recipient {index}");
    }
}

/// Times an inbox file of 100,000 rows addressed round-robin to 10,000 recipients, in the default granule size:
/// building it, reading one recipient's queue from a fresh reader with the recipient directory (the inbox file) and
/// without it (an event file of the same rows), and checking that every recipient has acknowledged the whole file.
/// Prints the median of seven samples of each. Run by hand:
/// `cargo test --release --all-features --lib inbox_read_benchmark -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode inbox read benchmark"]
fn inbox_read_benchmark() {
    let recipients = 10_000;
    let count = 100_000;
    let granularity = LayoutTargets::default().index_granularity;
    let samples = 7;
    let median = |mut times: Vec<Duration>| {
        times.sort_unstable();
        times[times.len() / 2]
    };

    let mut builds = Vec::new();
    let mut built = None;
    for _ in 0..samples {
        let input = rows(recipients, count);
        let started = Instant::now();
        built = Some(build_hef_file(input, &config(Profile::Inbox, granularity)).unwrap());
        builds.push(started.elapsed());
    }
    let built = built.unwrap();
    let file_bytes = built.bytes.len();
    let inbox = open(built);
    let event = open(build_hef_file(rows(recipients, count), &config(Profile::Event, granularity)).unwrap());

    let probes: Vec<Recipient> = (0..recipients).step_by(101).map(recipient).collect();
    let (deletes, mut acks) = (Deletes::default(), Acks::default());
    let read = |file: &HefFile| {
        let readers: Vec<HefFile> = probes.iter().map(|_| file.fresh_reader().unwrap()).collect();
        let started = Instant::now();
        for (reader, &to) in readers.iter().zip(&probes) {
            let events = read_inbox(&[reader], tenant(), to, &acks, &deletes).unwrap();
            assert_eq!(events.len() as u64, count / recipients);
        }
        started.elapsed() / probes.len() as u32
    };
    let with_directory = median((0..samples).map(|_| read(&inbox)).collect());
    let without_directory = median((0..samples).map(|_| read(&event)).collect());

    acks.0 = (0..recipients).map(|index| (recipient(index), point(count))).collect();
    let fully_acked = median(
        (0..samples)
            .map(|_| {
                let started = Instant::now();
                assert!(inbox.fully_acked(&acks).unwrap());
                started.elapsed()
            })
            .collect(),
    );

    println!(
        "inbox: {count} rows, {recipients} recipients, {file_bytes} B file, {} granules",
        inbox.footer().granules.len()
    );
    println!(
        "median build {:?}; one recipient's queue from a fresh reader: {with_directory:?} with the directory, \
         {without_directory:?} without; fully_acked over every recipient {fully_acked:?}",
        median(builds)
    );
}
