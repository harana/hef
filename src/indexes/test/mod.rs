use super::*;

/// The byte-at-a-time definition of [`stable_hash`]: an FNV-1a-seeded polynomial over the bytes, then the SplitMix64
/// finalizer. The shipped hash folds eight bytes per step; it must agree with this reference on every input.
fn reference_hash(bytes: &[u8]) -> u64 {
    let h = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |acc, b| {
        acc.wrapping_mul(0x0000_0100_0000_01b3).wrapping_add(u64::from(*b))
    });
    let mut z = h.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

fn sample(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(37).wrapping_add(11)).collect()
}

#[test]
fn stable_hash_matches_the_byte_at_a_time_definition() {
    for len in 0..=64 {
        let bytes = sample(len);
        assert_eq!(stable_hash(&bytes), reference_hash(&bytes), "length {len}");
    }
}

#[test]
fn stable_hash_ascii_lowercase_matches_lowercasing_first() {
    let mixed = b"AbC-Def_GHI/jkl.MNOPQRSTUVWXYZ0123456789/Zz";
    for len in 0..=mixed.len() {
        let bytes = &mixed[..len];
        let lowered: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
        assert_eq!(
            stable_hash_ascii_lowercase(bytes),
            reference_hash(&lowered),
            "length {len}"
        );
    }
}

#[test]
fn stable_hash_bits_are_frozen() {
    // These values reach persisted filter bits, so changing the hash would silently invalidate filters already on
    // disk. Any edit that moves these numbers is a format change, not an optimisation.
    assert_eq!(stable_hash(b""), 0xc381_7c01_6ba4_ff30);
    assert_eq!(stable_hash(b"entity"), 0x534f_1409_06a7_4233);
    assert_eq!(
        stable_hash(b"a-token-well-past-eight-bytes-long"),
        0x8ed0_c6b6_d621_af84
    );
    assert_eq!(stable_hash_ascii_lowercase(b"Entity-ID/Path"), 0x81a4_6167_8312_7e6e);
}
