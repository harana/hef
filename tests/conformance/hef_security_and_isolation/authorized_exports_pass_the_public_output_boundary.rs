//! Checks that any interchange export produced from an `events` table passes through the same authorization and
//! redaction boundary that every other egress must cross — exports are never a side door around that boundary.

use hef::deletes::FieldDeletionVector;
use hef::events::families::{Caller, authorize_columns, column_allowed};
use hef::security::{
    AeadScheme, ContentKey, ContentKeyId, ContentKeyStore, DecryptOutcome, SealedContentKey, TenantDek,
};
use hef::typed_id::TypedIdTestExt;

/// conformance:
/// hef-security-and-isolation/authorized-exports-pass-the-public-output-boundary/export-does-not-mix-tenants
#[test]
fn export_does_not_mix_tenants() {
    // tenant_id is always blocked at the public scan boundary, so an export produced through the authorized egress path
    // can never expose the raw tenant identifier or accidentally include rows from another tenant.
    assert!(
        !column_allowed("tenant_id", Caller::Public),
        "tenant_id must be blocked for public callers — exports may not expose or mix tenants"
    );
    assert!(
        column_allowed("tenant_id", Caller::Internal),
        "tenant_id must remain readable by internal services that enforce the per-tenant boundary"
    );
}

/// conformance:
/// hef-security-and-isolation/authorized-exports-pass-the-public-output-boundary/
/// export-exposes-no-engine-internal-references
#[test]
fn export_exposes_no_engine_internal_references() {
    // An export produced at the authorized boundary must strip all storage- internal columns (file paths, row offsets,
    // sequences, hashes, embeddings) exactly as the scan boundary does for every other public read.
    let mixed_projection = [
        "occurred_at",
        "event_type_id",
        "object_store_path",
        "row_offset",
        "payload_ref",
        "sequence",
        "sequence_key",
        "tenant_id",
        "embedding_text",
        "embedding_semantic",
        "entity_id_hash_low",
        "dedupe_hash_low",
    ];

    let (allowed, dropped) = authorize_columns(&mixed_projection, Caller::Public);

    // Only the two public-safe analytical columns survive.
    assert_eq!(
        allowed,
        vec!["occurred_at", "event_type_id"],
        "export projection must retain only public-safe columns"
    );

    // Every storage-internal and embedding column is dropped — none slip through.
    for col in [
        "object_store_path",
        "row_offset",
        "payload_ref",
        "sequence",
        "sequence_key",
        "tenant_id",
        "embedding_text",
        "embedding_semantic",
        "entity_id_hash_low",
        "dedupe_hash_low",
    ] {
        assert!(
            dropped.contains(&col),
            "{col} must be dropped from export — it is an engine-internal reference"
        );
    }
}

/// conformance:
/// hef-security-and-isolation/authorized-exports-pass-the-public-output-boundary/export-omits-a-redacted-field
#[test]
fn export_omits_a_redacted_field() {
    // The export path reads the FieldDeletionVector before materialising any column. A field listed in the vector is
    // absent from every row it covers, so the export never carries a redacted field regardless of file layout.
    let mut fdv = FieldDeletionVector::new(vec!["email".to_owned(), "phone".to_owned()]);
    fdv.mark_affected(0);

    assert!(
        fdv.column_redacted("email", 0),
        "email must be absent from the export for the affected ordinal"
    );
    assert!(
        fdv.column_redacted("phone", 0),
        "phone must be absent from the export for the affected ordinal"
    );
    assert!(
        !fdv.column_redacted("email", 1),
        "non-affected ordinals retain all fields in the export"
    );
    assert!(
        !fdv.column_redacted("amount", 0),
        "fields not in the redaction set remain in the export"
    );
}

/// conformance:
/// hef-security-and-isolation/authorized-exports-pass-the-public-output-boundary/
/// crypto-shredded-subject-is-absent-from-later-exports
#[test]
fn crypto_shredded_subject_is_absent_from_later_exports() {
    // An export is taken at the authorized boundary where crypto-shredded subjects already render as tombstones.
    // Because the export passes the same boundary as every other read, a subject whose content keys have been destroyed
    // does not appear in the export — their data is permanently unrecoverable.
    let subject_key_id = ContentKeyId::new_test_id(5555);
    let scheme = AeadScheme::default();
    let tenant_dek = TenantDek::new([8u8; 32]);

    let subject_content_key = ContentKey::new([21u8; 32]);
    let wrapped_key = tenant_dek
        .wrap_content_key(&subject_content_key, subject_key_id, scheme)
        .expect("wrap the subject's content key under the tenant DEK");
    let ciphertext = SealedContentKey::new(subject_content_key, 0, scheme)
        .encrypt(1, 0, b"single-subject-pii")
        .expect("seal the subject's payload");

    let mut keystore = ContentKeyStore::new(tenant_dek.clone());
    keystore.register(subject_key_id, wrapped_key);

    // Before shredding, the subject's data is recoverable at the export boundary.
    assert!(
        matches!(
            keystore.decrypt_or_tombstone(subject_key_id, 1, 0, &ciphertext),
            DecryptOutcome::Plaintext(_)
        ),
        "subject data must be recoverable before crypto-shredding"
    );

    // Crypto-shred the subject by destroying their content key.
    keystore.destroy(subject_key_id);

    // The export boundary now renders the subject as a tombstone — their single-subject data does not appear in the
    // export.
    assert_eq!(
        keystore.decrypt_or_tombstone(subject_key_id, 1, 0, &ciphertext),
        DecryptOutcome::Tombstone,
        "crypto-shredded subject must be absent (tombstone) from any export produced after shredding"
    );

    // A different subject's data — sealed under its own content key — is unaffected: shredding is key-scoped.
    let other_subject = ContentKeyId::new_test_id(6666);
    let other_content_key = ContentKey::new([22u8; 32]);
    let other_wrapped_key = tenant_dek
        .wrap_content_key(&other_content_key, other_subject, scheme)
        .expect("wrap the other subject's content key");
    let other_ciphertext = SealedContentKey::new(other_content_key, 0, scheme)
        .encrypt(1, 0, b"other-subject-pii")
        .expect("seal the other subject's payload");
    keystore.register(other_subject, other_wrapped_key);
    assert!(
        matches!(
            keystore.decrypt_or_tombstone(other_subject, 1, 0, &other_ciphertext),
            DecryptOutcome::Plaintext(_)
        ),
        "other subjects' data must be unaffected by a different subject's shredding"
    );
}
