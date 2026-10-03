use super::*;

fn descriptor(kind: ExternalPayloadKind, uri: Option<&str>, body: &[u8]) -> ExternalPayloadRef {
    ExternalPayloadRef {
        blake3: *blake3::hash(body).as_bytes(),
        kind,
        position: 4_096,
        size: body.len() as u64,
        uri: uri.map(str::to_owned),
    }
}

#[test]
fn every_placement_kind_round_trips() {
    let body = b"an oversized payload body";
    for (kind, uri) in [
        (ExternalPayloadKind::DedicatedObject, None),
        (
            ExternalPayloadKind::ExternalUri,
            Some("internal://archive/conn-7/part-3"),
        ),
        (ExternalPayloadKind::PackedSidecar, None),
    ] {
        let reference = descriptor(kind, uri, body);
        let decoded = ExternalPayloadRef::decode(&reference.encode()).unwrap();
        assert_eq!(decoded, reference);
    }
}

#[test]
fn fetched_bytes_verify_against_the_descriptor_blake3() {
    let body = b"referenced range bytes";
    let reference = descriptor(ExternalPayloadKind::ExternalUri, Some("internal://a"), body);
    assert!(reference.verify(body).is_ok());
    assert!(
        reference.verify(b"referenced range byteX").is_err(),
        "altered bytes refuse"
    );
    assert!(
        reference.verify(&body[..10]).is_err(),
        "short bytes refuse before hashing"
    );
}

#[test]
fn an_unknown_placement_kind_refuses() {
    let mut bytes = descriptor(ExternalPayloadKind::PackedSidecar, None, b"x").encode();
    bytes[0] = 9;
    assert!(ExternalPayloadRef::decode(&bytes).is_err());
}

#[test]
fn a_hostile_uri_length_refuses_instead_of_allocating() {
    let reference = descriptor(ExternalPayloadKind::ExternalUri, Some("internal://a"), b"x");
    let mut bytes = reference.encode();
    // The uri length word sits after tag + position + size + blake3.
    let at = 1 + 8 + 8 + 32;
    bytes[at..at + 4].copy_from_slice(&(u32::MAX).to_le_bytes());
    assert!(ExternalPayloadRef::decode(&bytes).is_err());
}
