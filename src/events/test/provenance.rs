use super::*;

use crate::events::sim::{SimulatedEventAuthor, signed_event_payload};
use crate::events::variant::VariantValue;
use std::collections::BTreeMap;

fn sign_event(seed: u8, claimed_at_seconds: i64, kind: u32, payload: &VariantValue) -> SignedEventProvenance {
    SimulatedEventAuthor::from_seed(seed)
        .sign(kind, claimed_at_seconds, payload)
        .expect("the payload is well-shaped")
}

#[test]
fn canonical_bytes_reconstruct_the_protocol_serialization_and_the_id_recomputes() {
    let payload = signed_event_payload("hello", &[&["e", "abc"], &["p", "def"]]);
    let provenance = sign_event(7, 1_700_000_000, 1, &payload);

    let canonical = provenance.canonical_bytes(&payload).expect("well-shaped payload");
    let expected = format!(
        "[0,\"{}\",1700000000,1,[[\"e\",\"abc\"],[\"p\",\"def\"]],\"hello\"]",
        hex_lower(&provenance.author_pubkey)
    );
    assert_eq!(String::from_utf8(canonical.clone()).expect("ascii-safe"), expected);
    assert_eq!(
        SignedEventProvenance::recompute_protocol_event_id(&canonical),
        provenance.protocol_event_id
    );
    provenance.verify(&payload).expect("a freshly signed event verifies");
}

#[test]
fn control_characters_and_quotes_escape_exactly_as_the_protocol_hashes_them() {
    let payload = signed_event_payload("a\"b\\c\nd\te\u{1}f", &[]);
    let provenance = sign_event(9, 1_700_000_001, 42, &payload);
    let canonical = String::from_utf8(provenance.canonical_bytes(&payload).expect("well-shaped")).expect("utf-8");
    assert!(
        canonical.ends_with("[],\"a\\\"b\\\\c\\nd\\te\\u0001f\"]"),
        "{canonical}"
    );
    provenance.verify(&payload).expect("escaped content still verifies");
}

#[test]
fn a_tampered_payload_fails_the_id_check_and_a_swapped_signature_fails_verification() {
    let payload = signed_event_payload("hello", &[]);
    let provenance = sign_event(3, 1_700_000_002, 1, &payload);

    let tampered = signed_event_payload("hello!", &[]);
    assert_eq!(provenance.verify(&tampered), Err(ProvenanceError::EventIdMismatch));

    // Same id, someone else's signature: the id check passes and the signature check is what refuses.
    let other = sign_event(11, 1_700_000_002, 1, &payload);
    let forged = SignedEventProvenance {
        signature: other.signature,
        ..provenance.clone()
    };
    assert_eq!(forged.verify(&payload), Err(ProvenanceError::SignatureRejected));
}

#[test]
fn a_payload_without_content_or_tags_is_refused_rather_than_guessed() {
    let provenance = sign_event(5, 1_700_000_003, 1, &signed_event_payload("x", &[]));
    let mut fields = BTreeMap::new();
    fields.insert("content".to_owned(), VariantValue::String("x".to_owned()));
    assert_eq!(
        provenance.canonical_bytes(&VariantValue::Object(fields)),
        Err(ProvenanceError::PayloadShape { field: TAGS_FIELD })
    );
    assert_eq!(
        provenance.canonical_bytes(&VariantValue::Null),
        Err(ProvenanceError::PayloadShape { field: CONTENT_FIELD })
    );
}

#[test]
fn hex_fields_round_trip_and_uppercase_is_refused() {
    let bytes = [0xabu8; 32];
    let text = hex_lower(&bytes);
    assert_eq!(hex_bytes::<32>(&text), Ok(bytes));
    assert_eq!(
        hex_bytes::<32>(&text.to_uppercase()),
        Err(ProvenanceError::MalformedHex)
    );
    assert_eq!(hex_bytes::<32>("ab"), Err(ProvenanceError::MalformedHex));
}

#[test]
fn the_scheme_tag_is_registry_controlled() {
    assert_eq!(
        SignatureScheme::from_str(SignatureScheme::Bip340SchnorrSecp256k1.as_str()),
        Some(SignatureScheme::Bip340SchnorrSecp256k1)
    );
    assert_eq!(SignatureScheme::from_str("made-up-scheme"), None);
}
