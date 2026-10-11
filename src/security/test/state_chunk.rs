use super::*;

fn key(byte: u8) -> StateChunkKey {
    StateChunkKey::derive(&TenantDek::new([byte; KEY_LEN]))
}

#[test]
fn the_same_chunk_seals_to_the_same_address_and_bytes_within_a_tenant() {
    let plaintext = b"a state chunk".to_vec();
    let first = key(1).seal(&plaintext).unwrap();
    assert_eq!(key(1).seal(&plaintext).unwrap(), first);
    assert_eq!(first.0, key(1).address(&plaintext));
    assert_eq!(key(1).open(&first.0, &first.1).unwrap(), plaintext);
    assert_ne!(first.1, plaintext, "the stored bytes are not the plaintext");
}

#[test]
fn tenants_share_no_addresses_and_cannot_open_each_others_chunks() {
    let plaintext = b"a state chunk";
    let (address_a, sealed_a) = key(1).seal(plaintext).unwrap();
    let (address_b, sealed_b) = key(2).seal(plaintext).unwrap();
    assert_ne!(address_a, address_b);
    assert_ne!(sealed_a, sealed_b);
    assert_ne!(address_a, *blake3::hash(plaintext).as_bytes());
    assert_eq!(key(2).open(&address_a, &sealed_a), None);
    assert_eq!(key(1).open(&address_b, &sealed_b), None);
}

#[test]
fn a_relocated_or_altered_chunk_is_refused() {
    let (address, sealed) = key(1).seal(b"one chunk").unwrap();
    let (other, _) = key(1).seal(b"another chunk").unwrap();
    assert_eq!(key(1).open(&other, &sealed), None);
    for index in 0..sealed.len() {
        let mut altered = sealed.clone();
        altered[index] ^= 1;
        assert_eq!(key(1).open(&address, &altered), None, "byte {index}");
    }
}
