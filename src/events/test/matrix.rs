use super::*;
use crate::events::provenance::SignatureScheme;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::json;

/// A deterministic server signing key, so every run signs the same bytes the same way.
pub(crate) fn server_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

/// Hashes and signs `event` the way a homeserver does before sending it over federation: the content hash goes in
/// first, then every server signs the redacted canonical form. Returns the event's wire bytes and the signatures as
/// the store keeps them.
pub(crate) fn federate(
    room_version: &str,
    mut event: Value,
    servers: &[(&str, &str, &SigningKey)],
) -> (Vec<u8>, Vec<SignerSignature>) {
    let hash = content_hash(&serde_json::to_vec(&event).unwrap()).unwrap();
    event["hashes"] = json!({ "sha256": hash });
    let message = signing_bytes(room_version, &serde_json::to_vec(&event).unwrap()).unwrap();
    let mut on_wire = Map::new();
    let mut stored = Vec::new();
    for (server, key_id, key) in servers {
        let signature = key.sign(&message).to_bytes();
        let mut by_key = Map::new();
        by_key.insert(
            (*key_id).to_owned(),
            Value::String(Base64Unpadded::encode_string(&signature)),
        );
        on_wire.insert((*server).to_owned(), Value::Object(by_key));
        stored.push(
            SignerSignature::new(
                server,
                key_id,
                SignatureScheme::Ed25519,
                key.verifying_key().to_bytes(),
                signature,
            )
            .unwrap(),
        );
    }
    event["signatures"] = Value::Object(on_wire);
    (serde_json::to_vec(&event).unwrap(), stored)
}

pub(crate) fn message_event() -> Value {
    json!({
        "auth_events": ["$create", "$power", "$member"],
        "content": { "body": "hello", "msgtype": "m.text" },
        "depth": 12,
        "origin": "origin.example.org",
        "origin_server_ts": 1_700_000_000_000_i64,
        "prev_events": ["$previous"],
        "room_id": "!room:origin.example.org",
        "sender": "@alice:origin.example.org",
        "type": "m.room.message",
        "unsigned": { "age": 5 }
    })
}

#[test]
fn canonical_json_sorts_keys_unescapes_unicode_and_keeps_safe_integers() {
    let raw = br#"{ "b" : 1, "a" : { "z": "caf\u00e9 \ud83d\ude00", "y": [9007199254740991, -9007199254740991] } }"#;
    let canonical = String::from_utf8(canonical_json(raw).unwrap()).unwrap();
    assert_eq!(
        canonical,
        "{\"a\":{\"y\":[9007199254740991,-9007199254740991],\"z\":\"caf\u{e9} \u{1f600}\"},\"b\":1}"
    );
}

#[test]
fn canonical_json_refuses_what_it_cannot_hold() {
    for raw in [&b"{\"a\":9007199254740992}"[..], &b"{\"a\":1.5}"[..], &b"not json"[..]] {
        assert!(
            matches!(canonical_json(raw), Err(ProvenanceError::MalformedEvent { .. })),
            "{}",
            String::from_utf8_lossy(raw)
        );
    }
}

#[test]
fn a_pdu_signed_by_two_servers_re_verifies_and_its_id_recomputes() {
    let origin = server_key(1);
    let relay = server_key(2);
    let (raw, signatures) = federate(
        "10",
        message_event(),
        &[
            ("origin.example.org", "ed25519:a1", &origin),
            ("relay.example.net", "ed25519:b2", &relay),
        ],
    );
    let provenance = MatrixProvenance::new("10", signatures).unwrap();
    let id = event_id("10", &raw).unwrap();
    assert!(id.starts_with('$'));
    assert!(
        !id.contains('+') && !id.contains('/'),
        "room version 4+ ids use the URL-safe alphabet: {id}"
    );
    let hash = reference_hash("10", &raw).unwrap();
    assert_eq!(id, format!("${}", Base64UrlUnpadded::encode_string(&hash)));
    provenance.verify(&raw, id.as_bytes()).unwrap();

    assert_eq!(
        provenance.verify(&raw, b"$not-the-id"),
        Err(ProvenanceError::EventIdMismatch)
    );
}

#[test]
fn tampering_with_content_or_signatures_is_caught() {
    let origin = server_key(3);
    let (raw, signatures) = federate("10", message_event(), &[("origin.example.org", "ed25519:a1", &origin)]);
    let id = event_id("10", &raw).unwrap();

    let mut edited: Value = serde_json::from_slice(&raw).unwrap();
    edited["content"]["body"] = json!("goodbye");
    let edited = serde_json::to_vec(&edited).unwrap();
    let provenance = MatrixProvenance::new("10", signatures.clone()).unwrap();
    assert_eq!(
        provenance.verify(&edited, id.as_bytes()),
        Err(ProvenanceError::ContentHashMismatch)
    );

    let mut forged = signatures;
    forged[0].public_key = server_key(4).verifying_key().to_bytes();
    let forged = MatrixProvenance::new("10", forged).unwrap();
    assert_eq!(
        forged.verify(&raw, id.as_bytes()),
        Err(ProvenanceError::SignatureRejected)
    );
}

#[test]
fn unsigned_data_and_key_order_do_not_change_what_was_signed() {
    let origin = server_key(5);
    let (raw, signatures) = federate("10", message_event(), &[("origin.example.org", "ed25519:a1", &origin)]);
    let id = event_id("10", &raw).unwrap();
    let mut relayed: Value = serde_json::from_slice(&raw).unwrap();
    relayed["unsigned"] = json!({ "age": 99_999 });
    let relayed = serde_json::to_vec_pretty(&relayed).unwrap();
    let provenance = MatrixProvenance::new("10", signatures).unwrap();
    provenance.verify(&relayed, id.as_bytes()).unwrap();
}

#[test]
fn room_version_three_ids_use_the_standard_alphabet() {
    let origin = server_key(6);
    let (raw, _) = federate("3", message_event(), &[("origin.example.org", "ed25519:a1", &origin)]);
    let hash = reference_hash("3", &raw).unwrap();
    assert_eq!(
        event_id("3", &raw).unwrap(),
        format!("${}", Base64Unpadded::encode_string(&hash))
    );
}

#[test]
fn room_version_one_takes_the_event_id_the_event_carries() {
    let origin = server_key(7);
    let mut event = message_event();
    event["event_id"] = json!("$abc:example.org");
    let (raw, signatures) = federate("1", event, &[("origin.example.org", "ed25519:a1", &origin)]);
    let provenance = MatrixProvenance::new("1", signatures).unwrap();
    provenance.verify(&raw, b"$abc:example.org").unwrap();
    assert_eq!(
        provenance.verify(&raw, b"$other:example.org"),
        Err(ProvenanceError::EventIdMismatch)
    );
}

#[test]
fn redaction_follows_the_room_version() {
    let member = json!({
        "content": { "displayname": "Alice", "membership": "join" },
        "origin": "origin.example.org",
        "type": "m.room.member",
        "hashes": { "sha256": "x" }
    });
    let raw = serde_json::to_vec(&member).unwrap();
    let v10 = String::from_utf8(signing_bytes("10", &raw).unwrap()).unwrap();
    assert_eq!(
        v10,
        "{\"content\":{\"membership\":\"join\"},\"hashes\":{\"sha256\":\"x\"},\"origin\":\"origin.example.org\",\"type\":\"m.room.member\"}"
    );
    let v11 = String::from_utf8(signing_bytes("11", &raw).unwrap()).unwrap();
    assert!(
        !v11.contains("origin"),
        "room version 11 no longer protects origin: {v11}"
    );
}

#[test]
fn unknown_room_versions_and_empty_signature_lists_are_refused() {
    assert_eq!(
        MatrixProvenance::new("org.example.custom", Vec::new()),
        Err(ProvenanceError::UnknownProtocolVersion)
    );
    assert_eq!(
        MatrixProvenance::new("10", Vec::new()),
        Err(ProvenanceError::NoSignatures)
    );
}
