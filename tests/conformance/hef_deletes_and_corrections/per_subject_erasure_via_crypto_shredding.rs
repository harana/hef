//! Requirement: Per-subject erasure via crypto-shredding.

use hef::deletes::{DeletionVector, FieldDeletionVector, RebuiltPayload, SubjectContentKey, rebuild_payload};

/// conformance:
/// hef-deletes-and-corrections/per-subject-erasure-via-crypto-shredding/erased-subject-after-key-destruction
#[test]
fn erased_subject_after_key_destruction() {
    let mut key = SubjectContentKey::generate([7u8; 32]);
    let payload = b"jane.doe@example.com order history".to_vec();
    let ciphertext = key.encrypt(1, 0, &payload).expect("key is live before erasure");

    // Before erasure, rebuild recovers the exact payload bytes.
    assert_eq!(
        rebuild_payload(&key, 1, 0, &ciphertext),
        RebuiltPayload::Payload(payload.clone())
    );

    // Erasing the subject destroys their content key.
    key.destroy();
    assert!(key.is_destroyed());

    // After key destruction the bytes are unrecoverable everywhere they persist (HEJ, HEF, backups); rebuild
    // produces a tombstone rather than serving the payload or blocking replay.
    assert_eq!(rebuild_payload(&key, 1, 0, &ciphertext), RebuiltPayload::Tombstone);
    assert_eq!(key.encrypt(1, 0, &payload), None);
}

/// conformance:
/// hef-deletes-and-corrections/per-subject-erasure-via-crypto-shredding/multi-subject-event-keeps-co-mentioned-data
#[test]
fn multi_subject_event_keeps_co_mentioned_data() {
    // A multi-subject event at ordinal 5 carries PII for two subjects, each under its own content key.
    let mut forgotten_key = SubjectContentKey::generate([1u8; 32]);
    let mut co_mentioned_key = SubjectContentKey::generate([2u8; 32]);
    let forgotten_email = b"forgotten@example.com".to_vec();
    let co_mentioned_email = b"co-mentioned@example.com".to_vec();
    let forgotten_ciphertext = forgotten_key.encrypt(5, 0, &forgotten_email).unwrap();
    let co_mentioned_ciphertext = co_mentioned_key.encrypt(5, 0, &co_mentioned_email).unwrap();

    // Multi-subject erasure uses field-level redaction, not row deletion: the event row is not row-deleted.
    let row_dv = DeletionVector::new();
    let mut field_dv = FieldDeletionVector::new(vec!["forgotten_subject_email".to_owned()]);
    field_dv.mark_affected(5);
    assert!(!row_dv.is_deleted(5));
    assert!(field_dv.column_redacted("forgotten_subject_email", 5));
    assert!(!field_dv.column_redacted("co_mentioned_subject_email", 5));

    // Forgetting the subject crypto-shreds only their content key.
    forgotten_key.destroy();

    // Only the forgotten subject's field is rendered unrecoverable; the co-mentioned subject's field survives.
    assert_eq!(
        rebuild_payload(&forgotten_key, 5, 0, &forgotten_ciphertext),
        RebuiltPayload::Tombstone
    );
    assert_eq!(
        rebuild_payload(&co_mentioned_key, 5, 0, &co_mentioned_ciphertext),
        RebuiltPayload::Payload(co_mentioned_email)
    );
}
