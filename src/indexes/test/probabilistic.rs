use super::*;

/// A tiny deterministic pseudo-random source for generating test keys, so every run exercises the same key sets and
/// failures reproduce.
fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        // SplitMix64 step; good enough spread for test keys.
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

fn make_keys(seed: u64, count: usize) -> Vec<u64> {
    let mut next = lcg(seed);
    (0..count).map(|_| next()).collect()
}

#[test]
fn split_block_bloom_has_no_false_negatives() {
    let keys = make_keys(1, 5_000);
    let filter = SplitBlockBloomFilter::build(&keys, 8);
    for &key in &keys {
        assert!(filter.contains(key), "inserted key {key} must be present");
    }
}

#[test]
fn split_block_bloom_false_positive_rate_is_bounded() {
    let keys = make_keys(2, 4_000);
    let filter = SplitBlockBloomFilter::build(&keys, 12);
    let key_set: hashbrown::HashSet<u64> = keys.iter().copied().collect();

    let mut probes = lcg(0xdead_beef);
    let trials = 20_000;
    let mut false_positives = 0;
    let mut checked = 0;
    for _ in 0..trials {
        let candidate = probes();
        if key_set.contains(&candidate) {
            continue;
        }
        checked += 1;
        if filter.contains(candidate) {
            false_positives += 1;
        }
    }
    // 12 bits/key split-block Bloom is well under 5% FPR; allow generous slack.
    let rate = false_positives as f64 / checked as f64;
    assert!(rate < 0.05, "false-positive rate {rate} too high");
}

#[test]
fn split_block_bloom_round_trips() {
    let keys = make_keys(3, 1_000);
    let filter = SplitBlockBloomFilter::build(&keys, 10);
    let bytes = filter.encode();
    let decoded = SplitBlockBloomFilter::decode(&bytes).expect("decode");
    assert_eq!(filter, decoded);
    for &key in &keys {
        assert!(decoded.contains(key));
    }
}

#[test]
fn split_block_bloom_is_deterministic() {
    let keys = make_keys(4, 2_000);
    let a = SplitBlockBloomFilter::build(&keys, 9);
    let b = SplitBlockBloomFilter::build(&keys, 9);
    assert_eq!(a.encode(), b.encode());
}

#[test]
fn split_block_bloom_handles_empty_key_set() {
    let filter = SplitBlockBloomFilter::build(&[], 8);
    assert_eq!(filter.exactness(), Exactness::InexactNoFalseNegative);
    // An empty filter never claims membership.
    assert!(!filter.contains(42));
    let decoded = SplitBlockBloomFilter::decode(&filter.encode()).expect("decode empty");
    assert_eq!(filter, decoded);
}

#[test]
fn split_block_bloom_decode_rejects_bad_magic() {
    let mut bytes = SplitBlockBloomFilter::build(&[1, 2, 3], 8).encode();
    bytes[0] ^= 0xff;
    assert!(SplitBlockBloomFilter::decode(&bytes).is_err());
}

#[test]
fn split_block_bloom_decode_rejects_truncation() {
    let bytes = SplitBlockBloomFilter::build(&[1, 2, 3], 8).encode();
    assert!(SplitBlockBloomFilter::decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn binary_fuse_has_no_false_negatives() {
    for seed in 0..8u64 {
        let keys = make_keys(100 + seed, 3_000);
        let filter = BinaryFuseFilter::build(&keys).expect("build");
        for &key in &keys {
            assert!(filter.contains(key), "inserted key {key} must be present");
        }
    }
}

#[test]
fn binary_fuse_false_positive_rate_is_bounded() {
    let keys = make_keys(7, 4_000);
    let filter = BinaryFuseFilter::build(&keys).expect("build");
    let key_set: hashbrown::HashSet<u64> = keys.iter().copied().collect();

    let mut probes = lcg(0xfeed_face);
    let trials = 40_000;
    let mut false_positives = 0;
    let mut checked = 0;
    for _ in 0..trials {
        let candidate = probes();
        if key_set.contains(&candidate) {
            continue;
        }
        checked += 1;
        if filter.contains(candidate) {
            false_positives += 1;
        }
    }
    // 8-bit fingerprints give ~1/256 ≈ 0.4% FPR; allow up to 2% slack.
    let rate = false_positives as f64 / checked as f64;
    assert!(rate < 0.02, "false-positive rate {rate} too high");
}

#[test]
fn binary_fuse_round_trips_and_is_deterministic() {
    let keys = make_keys(8, 2_500);
    let a = BinaryFuseFilter::build(&keys).expect("build a");
    let b = BinaryFuseFilter::build(&keys).expect("build b");
    assert_eq!(a.encode(), b.encode(), "same keys -> identical bytes");

    let decoded = BinaryFuseFilter::decode(&a.encode()).expect("decode");
    assert_eq!(a, decoded);
    for &key in &keys {
        assert!(decoded.contains(key));
    }
}

#[test]
fn binary_fuse_handles_small_and_duplicate_key_sets() {
    for keys in [vec![], vec![1], vec![1, 1, 1], vec![1, 2, 3, 3, 2, 1]] {
        let filter = BinaryFuseFilter::build(&keys).expect("build");
        for &key in &keys {
            assert!(filter.contains(key), "key {key} must be present");
        }
    }
}

#[test]
fn binary_fuse_decode_rejects_bad_magic_and_truncation() {
    let bytes = BinaryFuseFilter::build(&[1, 2, 3, 4, 5]).expect("build").encode();
    let mut bad = bytes.clone();
    bad[0] ^= 0xff;
    assert!(BinaryFuseFilter::decode(&bad).is_err());
    assert!(BinaryFuseFilter::decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn low_cardinality_skips_probabilistic_filter() {
    // A handful of distinct values over a million rows: low cardinality.
    assert!(!should_build_probabilistic_filter(8, 1_000_000));
    // Below the absolute distinct floor even when every row is distinct.
    assert!(!should_build_probabilistic_filter(100, 100));
    // No rows: nothing to filter.
    assert!(!should_build_probabilistic_filter(0, 0));
    assert!(!should_build_probabilistic_filter(5, 0));
}

#[test]
fn high_cardinality_builds_probabilistic_filter() {
    // Many distinct values and a healthy distinct/row ratio: high cardinality.
    assert!(should_build_probabilistic_filter(900_000, 1_000_000));
    assert!(should_build_probabilistic_filter(1_000, 1_000));
    // Clears the floor and exactly meets the 10% ratio.
    assert!(should_build_probabilistic_filter(1_000, 10_000));
    // Clears the floor but under the ratio (mostly repeats): not worth it.
    assert!(!should_build_probabilistic_filter(300, 1_000_000));
}

#[test]
fn membership_choice_prefers_binary_fuse_for_immutable_known_keys() {
    assert_eq!(choose_membership_filter(true), MembershipFilterChoice::BinaryFuse);
}

#[test]
fn membership_choice_falls_back_to_bloom_for_unknown_key_set() {
    // No genuine ribbon is built, so a key set that is not an immutable build-time-known set falls straight back to the
    // split-block Bloom filter — never a ribbon substitute.
    assert_eq!(choose_membership_filter(false), MembershipFilterChoice::SplitBlockBloom);
}

#[test]
fn recommend_filter_routes_low_and_high_cardinality() {
    assert_eq!(
        recommend_filter(8, 1_000_000, true),
        FilterRecommendation::UseBitmapOrValueSet
    );
    assert_eq!(
        recommend_filter(900_000, 1_000_000, true),
        FilterRecommendation::UseMembershipFilter(MembershipFilterChoice::BinaryFuse)
    );
    assert_eq!(
        recommend_filter(900_000, 1_000_000, false),
        FilterRecommendation::UseMembershipFilter(MembershipFilterChoice::SplitBlockBloom)
    );
}

/// A self-contained checksum over encoded bytes, so one line can pin the exact bits of a large filter.
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0u64, |acc, b| acc.rotate_left(7) ^ u64::from(*b))
}

#[test]
fn split_block_bloom_bits_are_frozen() {
    // A split-block Bloom filter's bytes are persisted, so a build over the same keys must keep producing the same
    // bits. Any edit that moves these numbers is a format change, not an optimisation.
    let small = SplitBlockBloomFilter::build(&[1, 2, 3, 4, 5], 8);
    assert_eq!(checksum(&small.encode()), 0x3266_9d08_7033_ba81);
    let wide = SplitBlockBloomFilter::build(&make_keys(11, 1_000), 10);
    assert_eq!(checksum(&wide.encode()), 0xdb73_a910_963f_0f1a);
}

/// Probing encoded bytes must answer exactly what decoding them and probing the filter answers — the whole point of
/// the in-place probe is that it is a cheaper route to the same verdict, not a looser one. Checked over the inserted
/// keys and a large sweep of absent ones, so a divergence in either direction shows up.
#[test]
fn probing_encoded_bytes_answers_exactly_as_the_decoded_filter_does() {
    let keys = make_keys(29, 500);
    let filter = SplitBlockBloomFilter::build(&keys, 10);
    let encoded = filter.encode();

    for key in keys.iter().chain((900_000..902_000).collect::<Vec<u64>>().iter()) {
        assert_eq!(
            SplitBlockBloomFilter::contains_encoded(&encoded, *key).unwrap(),
            filter.contains(*key),
            "key {key}",
        );
    }
}

/// Bytes that are not a filter must be refused rather than answered from, so a caller can tell "this filter says no"
/// apart from "these are not filter bytes" and fall back to reading instead of skipping data.
#[test]
fn probing_encoded_bytes_refuses_anything_that_is_not_a_filter() {
    let encoded = SplitBlockBloomFilter::build(&[1, 2, 3], 10).encode();
    assert!(SplitBlockBloomFilter::contains_encoded(&[], 1).is_err());
    assert!(SplitBlockBloomFilter::contains_encoded(&encoded[..encoded.len() - 1], 1).is_err());
    let mut wrong_magic = encoded.clone();
    wrong_magic[0] ^= 0xff;
    assert!(SplitBlockBloomFilter::contains_encoded(&wrong_magic, 1).is_err());
}
