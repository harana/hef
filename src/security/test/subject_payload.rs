use super::*;
use crate::events::variant::{KeyDictionary, encode_value};
use crate::typed_id::TypedIdTestExt;
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

fn message(text: &str) -> (VariantValue, EncodedPayload) {
    let mut fields = BTreeMap::new();
    fields.insert("body".to_owned(), VariantValue::String(text.to_owned()));
    let value = VariantValue::Object(fields);
    let dictionary = KeyDictionary::build(["body".to_owned()]);
    let bytes = encode_value(&value, &dictionary).unwrap();
    (value, EncodedPayload { bytes, dictionary })
}

fn open(stored: &[u8], event_id: EventId, keys: &MemorySubjectKeys) -> SubjectPayloadRead {
    let (subject, sealed) = split_sealed(stored).unwrap();
    open_sealed(subject, event_id.uuid().as_u128(), sealed, keys).unwrap()
}

#[test]
fn a_sealed_payload_opens_to_what_was_written_while_the_subject_key_is_live() {
    let keys = MemorySubjectKeys::default();
    let subject = SubjectId::new_test_id(1);
    let event_id = EventId::new_test_id(0xE1);
    let (value, encoded) = message("hello");
    let stored = seal_subject_payload(subject, event_id, &encoded, &keys).unwrap();

    assert!(stored.starts_with(SEALED_PAYLOAD_MAGIC));
    assert!(
        !stored.windows(5).any(|window| window == b"hello"),
        "the payload is not stored in the clear"
    );
    assert_eq!(open(&stored, event_id, &keys), SubjectPayloadRead::Opened(value));
}

#[test]
fn destroying_the_subject_key_turns_its_payloads_into_tombstones() {
    let keys = MemorySubjectKeys::default();
    let subject = SubjectId::new_test_id(1);
    let event_id = EventId::new_test_id(0xE1);
    let stored = seal_subject_payload(subject, event_id, &message("hello").1, &keys).unwrap();

    keys.destroy(subject);

    assert_eq!(open(&stored, event_id, &keys), SubjectPayloadRead::Tombstone);
}

#[test]
fn a_sealed_payload_copied_onto_another_event_is_rejected() {
    let keys = MemorySubjectKeys::default();
    let subject = SubjectId::new_test_id(1);
    let stored = seal_subject_payload(subject, EventId::new_test_id(0xE1), &message("hello").1, &keys).unwrap();

    assert_eq!(
        open(&stored, EventId::new_test_id(0xE2), &keys),
        SubjectPayloadRead::Rejected
    );
}

#[test]
fn nothing_new_is_sealed_for_an_erased_subject() {
    let keys = MemorySubjectKeys::default();
    let subject = SubjectId::new_test_id(1);
    seal_subject_payload(subject, EventId::new_test_id(0xE1), &message("first").1, &keys).unwrap();
    keys.destroy(subject);

    assert_eq!(
        seal_subject_payload(subject, EventId::new_test_id(0xE2), &message("second").1, &keys),
        Err(SubjectPayloadError::SubjectErased)
    );
}

#[test]
fn an_ordinary_binary_payload_is_not_mistaken_for_a_sealed_one() {
    assert_eq!(split_sealed(b"just some bytes"), None);
}
