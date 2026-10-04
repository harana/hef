use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::deletes::{SubjectContentKey, SubjectId};
use crate::error::{FormatError, StorageError};
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::layout::reader::{HefFile, PayloadRead};
use crate::security::{FooterEncryption, SubjectPayloadRead, read_subject_payload};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, SharedStrings, build_hef_file};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

/// An in-memory subject key store: one key per subject, created on first seal, destroyable by the test.
#[derive(Default)]
struct MemorySubjectKeys {
    keys: RefCell<HashMap<SubjectId, SubjectContentKey>>,
}

impl MemorySubjectKeys {
    fn destroy(&self, subject: SubjectId) {
        if let Some(key) = self.keys.borrow_mut().get_mut(&subject) {
            key.destroy();
        }
    }
}

impl SubjectKeyStore for MemorySubjectKeys {
    fn opening_key(&self, subject: SubjectId) -> Result<Option<SubjectContentKey>, StorageError> {
        Ok(self
            .keys
            .borrow()
            .get(&subject)
            .filter(|key| !key.is_destroyed())
            .cloned())
    }

    fn sealing_key(&self, subject: SubjectId) -> Result<Option<SubjectContentKey>, StorageError> {
        let mut keys = self.keys.borrow_mut();
        let key = keys
            .entry(subject)
            .or_insert_with(|| SubjectContentKey::generate([subject.uuid().as_u128() as u8; 32]));
        Ok((!key.is_destroyed()).then(|| key.clone()))
    }
}

fn tenant() -> TenantId {
    TenantId::new_test_id(7)
}

fn config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets::default(),
        tenant_id: tenant(),
    }
}

fn payload(i: u64) -> VariantValue {
    let mut fields = BTreeMap::new();
    fields.insert("body".to_owned(), VariantValue::String(format!("message {i}")));
    fields.insert("n".to_owned(), VariantValue::Int(i as i64));
    VariantValue::Object(fields)
}

fn row(i: u64, subject: Option<SubjectId>) -> BuildRow {
    let mut row = SharedStrings::default().build_row(HefRow {
        epoch: 1,
        event: EventInput {
            connector_delivery_hash_high: 0,
            connector_delivery_hash_low: i,
            envelope: EventEnvelope {
                account_id: None,
                account_id_hash_low: 0,
                actor_id: None,
                actor_id_hash_low: 0,
                dedupe_hash_high: 0,
                dedupe_hash_low: i,
                entity_id: Some(format!("room-{i}")),
                entity_id_hash_high: 1,
                entity_id_hash_low: i,
                entity_type: "room".to_owned(),
                event_id: EventId::new_test_id(0xBEEF_0000 + u128::from(i)),
                event_type: "m.room.message".to_owned(),
                flags: EventFlags(0),
                ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64),
                occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64),
                schema_version: 1,
                source: "matrix".to_owned(),
                stream_id: StreamId(1),
                stream_sequence: i,
                tenant_id: tenant(),
                trace_id_hash_low: 0,
            },
            payload: PayloadInput::Variant(payload(i)),
            provenance: None,
            relationships: None,
            source_delivery: None,
            source_schema: None,
        },
        sequence: i + 1,
    });
    row.subject = subject;
    row
}

#[test]
fn destroying_a_subject_key_tombstones_its_rows_while_other_rows_in_the_granule_still_read() {
    let alice = SubjectId::new_test_id(1);
    let bob = SubjectId::new_test_id(2);
    let subjects = [Some(alice), Some(bob), Some(alice), Some(bob), Some(alice), None];
    let mut rows: Vec<BuildRow> = (0..subjects.len() as u64)
        .zip(subjects)
        .map(|(i, subject)| row(i, subject))
        .collect();
    let keys = MemorySubjectKeys::default();
    seal_subject_rows(&mut rows, &keys).unwrap();
    assert!(rows.iter().all(|row| row.subject.is_none()));

    let built = build_hef_file(rows, &config()).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_eq!(file.footer().granules.len(), 1, "every row shares one granule");

    let read = |i: u64| read_subject_payload(&file, i, &keys).unwrap();
    for (i, subject) in (0..).zip(subjects) {
        let expected = match subject {
            Some(_) => SubjectPayloadRead::Opened(payload(i)),
            None => SubjectPayloadRead::Plain(PayloadRead::Value(payload(i))),
        };
        assert_eq!(read(i), expected, "row {i} reads while every key is live");
    }

    keys.destroy(alice);

    for (i, subject) in (0..).zip(subjects) {
        let expected = match subject {
            Some(subject) if subject == alice => SubjectPayloadRead::Tombstone,
            Some(_) => SubjectPayloadRead::Opened(payload(i)),
            None => SubjectPayloadRead::Plain(PayloadRead::Value(payload(i))),
        };
        assert_eq!(read(i), expected, "row {i} after alice's key is destroyed");
    }
}

#[test]
fn a_sealed_payload_is_not_stored_in_the_clear() {
    let mut rows = vec![row(0, Some(SubjectId::new_test_id(1)))];
    seal_subject_rows(&mut rows, &MemorySubjectKeys::default()).unwrap();

    let built = build_hef_file(rows, &config()).unwrap();
    assert!(!built.bytes.windows(9).any(|window| window == b"message 0"));
}

#[test]
fn the_build_refuses_a_row_that_still_names_a_subject() {
    let rows = vec![row(0, Some(SubjectId::new_test_id(1)))];

    assert_eq!(
        build_hef_file(rows, &config()).err(),
        Some(FormatError::Structural {
            rule: "a row naming a subject is sealed with seal_subject_rows before the build",
        })
    );
}

#[test]
fn sealing_for_an_erased_subject_fails_and_leaves_the_row_untouched() {
    let alice = SubjectId::new_test_id(1);
    let keys = MemorySubjectKeys::default();
    seal_subject_rows(&mut [row(0, Some(alice))], &keys).unwrap();
    keys.destroy(alice);

    let mut rows = vec![row(1, Some(alice))];
    let before = rows.clone();

    assert_eq!(
        seal_subject_rows(&mut rows, &keys),
        Err(SubjectPayloadError::SubjectErased)
    );
    assert_eq!(rows, before);
}
