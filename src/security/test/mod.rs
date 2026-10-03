use super::*;
use crate::typed_id::TypedIdTestExt;

const FILE_ID: u128 = 0xF11E_D00D;

#[test]
fn a_destroyed_key_stays_a_tombstone_across_re_registration() {
    // Erasure must be terminal: once a subject's content key is destroyed, a late or replayed registration of the same
    // id must not resurrect it. The key stays a tombstone and its ciphertext stays unrecoverable.
    let scheme = AeadScheme::default();
    let tenant_dek = TenantDek::new([3u8; 32]);
    let id = ContentKeyId::new_test_id(1001);
    let content_key = ContentKey::new([9u8; 32]);
    let wrapped = tenant_dek.wrap_content_key(&content_key, id, scheme).unwrap();
    let sealed = content_key.seal(1, FILE_ID, 0, b"pii payload", scheme).unwrap();

    let mut store = ContentKeyStore::new(tenant_dek);
    store.register(id, wrapped.clone());
    store.destroy(id);
    assert!(store.is_destroyed(id));

    // A replayed registration with the original wrapped key must be ignored.
    store.register(id, wrapped);
    assert!(
        store.is_destroyed(id),
        "a destroyed key must stay destroyed after a re-registration"
    );
    assert_eq!(
        store.decrypt_or_tombstone(id, 1, FILE_ID, &sealed),
        DecryptOutcome::Tombstone,
        "an erased subject stays a tombstone even after their id is registered again"
    );
}

#[test]
fn erasing_before_registration_stays_terminal() {
    // Erasure must win even when it arrives before the key is ever registered: destroy leaves a tombstone for an id the
    // store has never seen, so a late or replayed registration of that id cannot resurrect decryptability.
    let scheme = AeadScheme::default();
    let tenant_dek = TenantDek::new([3u8; 32]);
    let id = ContentKeyId::new_test_id(2002);
    let content_key = ContentKey::new([9u8; 32]);
    let wrapped = tenant_dek.wrap_content_key(&content_key, id, scheme).unwrap();
    let sealed = content_key.seal(1, FILE_ID, 0, b"pii payload", scheme).unwrap();

    let mut store = ContentKeyStore::new(tenant_dek);
    // The erasure request lands first, before any registration.
    store.destroy(id);
    assert!(store.is_destroyed(id), "erasing an unseen id must leave a tombstone");

    // The registration that arrives afterwards must not resurrect the key.
    store.register(id, wrapped);
    assert!(
        store.is_destroyed(id),
        "a registration after an erase-before-register must not resurrect the key"
    );
    assert_eq!(
        store.decrypt_or_tombstone(id, 1, FILE_ID, &sealed),
        DecryptOutcome::Tombstone,
        "an erased subject stays a tombstone even when their registration was replayed after the erasure"
    );
}

#[test]
fn a_sealing_key_never_rolls_past_its_reserved_epoch_range() {
    // Each keystore checkout owns a bounded range of epochs. Re-seals of one block id roll inside that range, and once
    // it is spent the key refuses to seal — it must never wander into epochs another checkout (a concurrent node, or
    // the next restart) owns, because that is exactly how a nonce would repeat.
    let scheme = AeadScheme::default();
    let mut first = SealedContentKey::with_epoch_range(ContentKey::new([7u8; 32]), 0, 2, scheme);
    let sealed_a = first.encrypt(9, FILE_ID, b"a").unwrap();
    let sealed_b = first.encrypt(9, FILE_ID, b"b").unwrap();
    assert!(
        first.encrypt(9, FILE_ID, b"c").is_none(),
        "a spent epoch range must refuse to seal, never roll into another checkout's epochs"
    );

    // The next checkout's range starts where the first ended, so even the same block id seals under fresh nonces.
    let mut second = SealedContentKey::with_epoch_range(ContentKey::new([7u8; 32]), 2, 4, scheme);
    let sealed_c = second.encrypt(9, FILE_ID, b"c").unwrap();

    let nonce_len = scheme.nonce_len();
    let nonces: HashSet<_> = [&sealed_a, &sealed_b, &sealed_c]
        .iter()
        .map(|blob| blob.get(..nonce_len))
        .collect();
    assert_eq!(nonces.len(), 3, "no nonce may repeat within or across checkouts");
}

#[test]
fn cloning_a_sealing_key_shares_its_nonce_history() {
    // #3924: nonce history lived in a per-instance set, so two clones of one sealing key at the same state could each
    // seal a different plaintext under the same (block_id, epoch) nonce — catastrophic AES-GCM reuse. Clones must guard
    // against the same history, so the second seal of a block id rolls to a fresh nonce no matter which clone made it.
    let scheme = AeadScheme::default();
    let key = SealedContentKey::new(ContentKey::new([4u8; 32]), 0, scheme);
    let mut original = key.clone();
    let mut clone = key;

    let sealed_a = original.encrypt(9, FILE_ID, b"plaintext-a").unwrap();
    let sealed_b = clone.encrypt(9, FILE_ID, b"plaintext-b").unwrap();

    let nonce_len = scheme.nonce_len();
    assert_ne!(
        sealed_a.get(..nonce_len),
        sealed_b.get(..nonce_len),
        "two clones of one sealing key must never seal different plaintexts under the same nonce"
    );
}

#[test]
fn re_sealing_one_block_id_never_repeats_a_nonce() {
    // #2746: the deterministic (block_id, epoch) nonce means sealing one block id twice under one content key would
    // reuse a nonce; the guard rolls the epoch so the second seal draws a fresh nonce for its differing plaintext.
    let scheme = AeadScheme::default();
    let mut key = SealedContentKey::new(ContentKey::new([8u8; 32]), 0, scheme);
    let first = key.encrypt(1, FILE_ID, b"v1").unwrap();
    let second = key.encrypt(1, FILE_ID, b"v2").unwrap();

    let nonce_len = scheme.nonce_len();
    assert_ne!(
        first.get(..nonce_len),
        second.get(..nonce_len),
        "a re-seal of one block id must roll to a fresh nonce, never repeat the deterministic one"
    );
}

#[test]
fn a_subject_keys_job_requires_the_tenant_dek_lease_too() {
    // Content keys are unwrapped via the tenant DEK, so a stale subject-key lease alone must never make a node
    // eligible for a decrypting job once its DEK lease is revoked (e.g. by erasure).
    let tenant = TenantId::new_test_id(7);
    let region = KeyResidencyRegion("eu-west".to_owned());
    let subject_lease = KeyLease {
        residency: region.clone(),
        scope: KeyScope::SubjectKeys,
        tenant_id: tenant,
    };
    let dek_lease = KeyLease {
        residency: region.clone(),
        scope: KeyScope::TenantDek,
        tenant_id: tenant,
    };

    assert!(
        !is_eligible_for_job(tenant, KeyScope::SubjectKeys, &region, &[subject_lease.clone()]),
        "a subject-key lease alone must not satisfy a SubjectKeys job"
    );
    assert!(
        is_eligible_for_job(
            tenant,
            KeyScope::SubjectKeys,
            &region,
            &[subject_lease.clone(), dek_lease.clone()]
        ),
        "holding both the subject-key and tenant-DEK leases makes the node eligible"
    );

    let other_tenant_dek = KeyLease {
        residency: region.clone(),
        scope: KeyScope::TenantDek,
        tenant_id: TenantId::new_test_id(8),
    };
    assert!(
        !is_eligible_for_job(
            tenant,
            KeyScope::SubjectKeys,
            &region,
            &[subject_lease.clone(), other_tenant_dek]
        ),
        "the tenant-DEK lease must be for the same tenant as the job"
    );

    // Both leases are for eu-west, so a job that must run in us-east finds neither in its region and is ineligible: a
    // lease never lets a node process a tenant's keys outside their residency.
    let other_region = KeyResidencyRegion("us-east".to_owned());
    assert!(
        !is_eligible_for_job(
            tenant,
            KeyScope::SubjectKeys,
            &other_region,
            &[subject_lease, dek_lease]
        ),
        "leases for one region must not satisfy a job that must run in another"
    );
}

#[test]
fn key_material_lives_in_self_scrubbing_containers() {
    // Crypto-shredding relies on a destroyed key being gone: each key type must hold its 32 raw bytes inside
    // `Zeroizing`, so dropping the key scrubs the material instead of leaving it recoverable in freed memory.
    let master = MasterKey::new([1u8; 32]);
    let dek = TenantDek::new([2u8; 32]);
    let content = ContentKey::new([3u8; 32]);
    let _: &zeroize::Zeroizing<[u8; 32]> = &master.0;
    let _: &zeroize::Zeroizing<[u8; 32]> = &dek.0;
    let _: &zeroize::Zeroizing<[u8; 32]> = &content.0;

    // The unwrap path hands material back in the same container, so the transient copy scrubs itself too.
    let scheme = AeadScheme::default();
    let id = ContentKeyId::new_test_id(3003);
    let wrapped = dek.wrap_content_key(&content, id, scheme).unwrap();
    let unwrapped = dek.unwrap_content_key(&wrapped, id, scheme).unwrap();
    let _: &zeroize::Zeroizing<[u8; 32]> = &unwrapped.0;
}

#[test]
fn open_rejects_a_block_replayed_into_a_different_slot() {
    // A sealed block carries its own nonce, but the block identity is authenticated against the id the caller expects
    // for this slot, not the id recovered from the blob — so replaying an intact blob into another slot is rejected.
    let scheme = AeadScheme::default();
    let content_key = ContentKey::new([7u8; 32]);
    let sealed = content_key.seal(3, FILE_ID, 0, b"single-subject pii", scheme).unwrap();

    assert_eq!(
        content_key.open(3, FILE_ID, &sealed, scheme).as_deref(),
        Some(b"single-subject pii".as_slice()),
        "the block opens under the id it was sealed for"
    );
    assert!(
        content_key.open(4, FILE_ID, &sealed, scheme).is_none(),
        "the same intact blob opened under a different block id is rejected"
    );
}

#[test]
fn seal_binds_a_distinct_subkey_per_epoch_even_when_the_nonce_collides() {
    // AES-256-GCM keeps only the low 32 bits of the epoch in its 12-byte nonce, so block id 5 at epoch 0 and at epoch
    // 2^32 derive an identical nonce. Because each seal derives its AEAD subkey by HKDF from the full (block_id,
    // key_epoch), those two seals encrypt under different key material — no (key, nonce) pair is reused — and each still
    // round-trips through open, which recovers the epoch from the blob.
    let scheme = AeadScheme::AesGcm256;
    let content_key = ContentKey::new([5u8; 32]);
    let block_id = 5;
    let low = content_key.seal(block_id, FILE_ID, 0, b"secret", scheme).unwrap();
    let high = content_key
        .seal(block_id, FILE_ID, 1u64 << 32, b"secret", scheme)
        .unwrap();

    let nonce_len = scheme.nonce_len();
    assert_eq!(
        low.get(..nonce_len),
        high.get(..nonce_len),
        "the two epochs must share the low-32-bit nonce this test targets"
    );
    assert_ne!(
        low, high,
        "a colliding nonce must still seal under distinct subkeys, so the ciphertexts differ"
    );
    assert_eq!(
        content_key.open(block_id, FILE_ID, &low, scheme).as_deref(),
        Some(b"secret".as_slice()),
        "the epoch-0 blob round-trips"
    );
    assert_eq!(
        content_key.open(block_id, FILE_ID, &high, scheme).as_deref(),
        Some(b"secret".as_slice()),
        "the epoch-2^32 blob round-trips under its own subkey"
    );
}
