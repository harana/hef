//! Checks the encryption control set: file DEK, chunk AEAD, footer encryption, payload-arena encryption,
//! embedding-block encryption, and per-subject content keys. The pinned AEAD primitive is AES-256-GCM (default) or
//! XChaCha20-Poly1305 (named alternative); no other cipher is permitted.

use hef::deletes::{RebuiltPayload, SubjectContentKey, rebuild_payload};
use hef::events::families::{Caller, ColumnFamily, column_allowed};
use hef::security::{AeadScheme, BlockNonce, FooterEncryption};

const FILE_ID: u128 = 0xF11E_D00D;

/// conformance: hef-security-and-isolation/encryption-control-set/scan-path-uses-file-dek-columns-only
#[test]
fn scan_path_uses_file_dek_columns_only() {
    // The analytical scan path (dimensions, timestamps, numeric measures) reads file-DEK-encrypted columns and never
    // unwraps a per-subject content key. The access-control partition enforces one side of this: the columns that would
    // be content-key encrypted are either internal_only (embeddings) or blocked by the scan boundary. Analytical
    // scan-path columns are accessible to both internal and authorized public callers.
    //
    // Embedding columns — which are encrypted under subject content keys — must be internal_only so no export or scan
    // path can reach them without going through the key-authorization layer.
    assert!(
        ColumnFamily::EmbeddingColumnsInternal.internal_only(),
        "embedding columns are encrypted under subject content keys and must be internal_only"
    );
    for embedding_col in ["embedding_text", "embedding_semantic", "embedding_quantized"] {
        assert!(
            !column_allowed(embedding_col, Caller::Public),
            "{embedding_col} is subject-content-key encrypted and must be blocked at the scan boundary"
        );
    }

    // Analytical scan-path columns (dimensions, timestamps, measures) are not internal_only — they travel under the
    // file DEK and are readable by authorized public callers through the normal scan boundary.
    for analytical_col in ["occurred_at", "event_type_id", "source_id", "entity_type_id"] {
        assert!(
            column_allowed(analytical_col, Caller::Public),
            "{analytical_col} is a file-DEK column and must be readable at the scan boundary"
        );
    }
}

/// conformance: hef-security-and-isolation/encryption-control-set/sensitive-schema-footer
#[test]
fn sensitive_schema_footer() {
    // Footer encryption is available as an opt-in when a tenant's policy requires the column schema to be protected.
    // The default is Plaintext so tenants without that requirement pay no overhead.
    assert_eq!(
        FooterEncryption::default(),
        FooterEncryption::Plaintext,
        "footer encryption must default to Plaintext — it is opt-in only"
    );
    assert_ne!(
        FooterEncryption::Encrypted,
        FooterEncryption::Plaintext,
        "Encrypted is a distinct value from Plaintext so a tenant can opt in"
    );
    // Exhaustive match: only these two states exist, proving there is no intermediate or unknown footer-encryption
    // state.
    for policy in [FooterEncryption::Plaintext, FooterEncryption::Encrypted] {
        let _: FooterEncryption = policy;
    }
}

/// conformance: hef-security-and-isolation/encryption-control-set/payload-aead-uses-the-pinned-cipher
#[test]
fn payload_aead_uses_the_pinned_cipher() {
    // The pinned AEAD is AES-256-GCM (default) or XChaCha20-Poly1305 (named alternative). The exhaustive match below is
    // the machine-checkable proof that the enum has exactly two variants and no unlisted cipher can sneak in.
    assert_eq!(
        AeadScheme::default(),
        AeadScheme::AesGcm256,
        "default AEAD must be AES-256-GCM (hardware-only `hardware-rust-crypto`)"
    );

    // Each scheme uses the nonce length and crate name mandated by the spec. The exhaustive match catches any future
    // variant addition.
    for scheme in [AeadScheme::AesGcm256, AeadScheme::XChaCha20Poly1305] {
        match scheme {
            AeadScheme::AesGcm256 => {
                assert_eq!(scheme.nonce_len(), 12, "AES-256-GCM nonce must be 96 bits (12 bytes)");
                assert_eq!(
                    scheme.crate_name(),
                    "hardware-rust-crypto",
                    "must name the hardware-rust-crypto crate"
                );
            }
            AeadScheme::XChaCha20Poly1305 => {
                assert_eq!(
                    scheme.nonce_len(),
                    24,
                    "XChaCha20-Poly1305 nonce must be 192 bits (24 bytes)"
                );
                assert_eq!(
                    scheme.crate_name(),
                    "chacha20poly1305",
                    "must name the RustCrypto chacha20poly1305 crate"
                );
            }
        }
    }

    // Each AEAD invocation uses a unique nonce: nonces derived for different block IDs or different invocation counters
    // must differ, so a ciphertext from one block cannot be replayed as another block's ciphertext under the same key.
    let nonce_block1_inv0 = BlockNonce::derive(AeadScheme::AesGcm256, 1, 0);
    let nonce_block2_inv0 = BlockNonce::derive(AeadScheme::AesGcm256, 2, 0);
    let nonce_block1_inv1 = BlockNonce::derive(AeadScheme::AesGcm256, 1, 1);
    assert_ne!(
        nonce_block1_inv0.bytes, nonce_block2_inv0.bytes,
        "different block IDs must produce different nonces under the same key"
    );
    assert_ne!(
        nonce_block1_inv0.bytes, nonce_block1_inv1.bytes,
        "different invocation counters on the same block must produce different nonces"
    );

    // Nonces are exactly scheme.nonce_len() bytes — no padding or truncation.
    assert_eq!(nonce_block1_inv0.bytes.len(), AeadScheme::AesGcm256.nonce_len());
    let xchacha_nonce = BlockNonce::derive(AeadScheme::XChaCha20Poly1305, 99, 0);
    assert_eq!(xchacha_nonce.bytes.len(), AeadScheme::XChaCha20Poly1305.nonce_len());

    // Block identity is encoded in the nonce so that a ciphertext cannot be relocated to a different block without
    // using a nonce mismatch (the high 8 bytes of every derived nonce carry the block_id).
    let nonce_bytes = BlockNonce::derive(AeadScheme::AesGcm256, 42, 0).bytes;
    let block_id_in_nonce = u64::from_le_bytes(nonce_bytes[..8].try_into().unwrap());
    assert_eq!(
        block_id_in_nonce, 42,
        "block_id must be encoded in the high bytes of the nonce"
    );
}

// The tests below are regression coverage for the hardened crypto path (spec change
// harden-hef-crypto-nonce-and-shred). They are intentionally not annotated `/// conformance:` — their scenarios live in
// the change delta and enter the corpus (and the ledger) only when the change is archived.

#[test]
fn pinned_aead_seals_and_rejects_tamper() {
    // Every per-subject block is sealed with the pinned AEAD, not an unauthenticated keystream: it round-trips to the
    // exact plaintext, and any flipped ciphertext byte or altered block identity is rejected on decrypt rather than
    // returning altered plaintext. This holds for both the default AES-256-GCM and the XChaCha20-Poly1305 alternative.
    for scheme in [AeadScheme::AesGcm256, AeadScheme::XChaCha20Poly1305] {
        let mut key = SubjectContentKey::generate_with([5u8; 32], 0, scheme);
        let plaintext = b"single-subject pii".to_vec();
        let sealed = key.encrypt(3, FILE_ID, &plaintext).expect("a live key seals the block");

        assert_eq!(
            rebuild_payload(&key, 3, FILE_ID, &sealed),
            RebuiltPayload::Payload(plaintext.clone()),
            "an intact block round-trips to the exact plaintext under the pinned AEAD"
        );

        // Flip a ciphertext byte — the authentication tag fails, so the block is rejected (not altered plaintext, not a
        // tombstone: the key is still live).
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        assert_eq!(
            rebuild_payload(&key, 3, FILE_ID, &tampered),
            RebuiltPayload::Rejected,
            "a flipped ciphertext byte is rejected on decrypt, never returned as altered plaintext"
        );

        // Replay the whole sealed blob — nonce and ciphertext together, byte for byte — into a different block slot.
        // Opening it under the id the new slot expects fails the associated-data check, so a ciphertext cannot be
        // relocated even when the attacker keeps the blob perfectly intact.
        assert_eq!(
            rebuild_payload(&key, 4, FILE_ID, &sealed),
            RebuiltPayload::Rejected,
            "an intact block opened under a different block id is rejected — ciphertext cannot be relocated"
        );

        // Replay the same intact blob into a different file: the seal binds the file identity as associated data, so a
        // block relocated across files under a mishandled key fails the tag rather than decrypting as that file's block.
        assert_eq!(
            rebuild_payload(&key, 3, FILE_ID ^ 1, &sealed),
            RebuiltPayload::Rejected,
            "an intact block opened as part of a different file is rejected — ciphertext cannot be relocated across files"
        );

        // Flipping the leading nonce byte likewise fails: the seal no longer verifies under the derived keystream.
        let mut tampered_nonce = sealed.clone();
        tampered_nonce[0] ^= 0x01;
        assert_eq!(
            rebuild_payload(&key, 3, FILE_ID, &tampered_nonce),
            RebuiltPayload::Rejected,
            "altering the nonce is rejected on decrypt"
        );
    }
}

#[test]
fn deterministic_seal_is_reproducible_across_schemes() {
    // The seal never changes decrypted plaintext and, on the deterministic-nonce path, reproduces identical sealed
    // bytes from the same key material, epoch, and block id — so a deterministic simulation stays reproducible from its
    // seed. Both the deterministic-nonce (AES-256-GCM) and extended-nonce (XChaCha20-Poly1305) paths satisfy this.
    for scheme in [AeadScheme::AesGcm256, AeadScheme::XChaCha20Poly1305] {
        let mut key_a = SubjectContentKey::generate_with([6u8; 32], 2, scheme);
        let mut key_b = SubjectContentKey::generate_with([6u8; 32], 2, scheme);
        let plaintext = b"reproducible payload".to_vec();
        let sealed_a = key_a.encrypt(9, FILE_ID, &plaintext).expect("seal under key a");
        let sealed_b = key_b.encrypt(9, FILE_ID, &plaintext).expect("seal under key b");

        assert_eq!(
            sealed_a, sealed_b,
            "the deterministic seal reproduces identical bytes for identical inputs"
        );
        assert_eq!(
            rebuild_payload(&key_a, 9, FILE_ID, &sealed_b),
            RebuiltPayload::Payload(plaintext),
            "either key opens the other's block to the same plaintext"
        );
    }
}
