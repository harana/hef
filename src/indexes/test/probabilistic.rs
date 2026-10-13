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

#[test]
fn binary_fuse_scratch_reuse_after_failed_attempt_matches_fresh_build() {
    let keys = {
        let mut keys = make_keys(85, 400);
        keys.sort_unstable();
        keys.dedup();
        keys
    };
    let mut scratch = FuseScratch::default();
    assert!(BinaryFuseFilter::try_build(&keys, 4, 7, &mut scratch).is_none());
    let segment_len = 200;
    let reused = BinaryFuseFilter::try_build(&keys, segment_len, 9, &mut scratch).expect("peels");
    let fresh = BinaryFuseFilter::try_build(&keys, segment_len, 9, &mut FuseScratch::default()).expect("peels");
    assert_eq!(reused.encode(), fresh.encode());
    for &key in &keys {
        assert!(reused.contains(key));
    }
}

#[test]
fn binary_fuse_build_matches_per_seed_fresh_scratch() {
    for count in [0, 1, 50, 5_000] {
        let mut keys = make_keys(count as u64 + 3, count);
        keys.extend_from_slice(&keys.clone());
        let built = BinaryFuseFilter::build(&keys).expect("builds");
        let mut distinct = keys.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let expected = (0..FUSE_MAX_SEEDS)
            .find_map(|attempt| {
                let seed = 0x51_2b_9f_01_u64.wrapping_mul(u64::from(attempt) + 1);
                BinaryFuseFilter::try_build(&distinct, built.segment_len, seed, &mut FuseScratch::default())
            })
            .expect("builds");
        assert_eq!(built.encode(), expected.encode());
    }
}

#[test]
fn binary_fuse_encoded_probe_matches_owned_probe() {
    for count in [0, 1, 100, 10_000] {
        let keys = make_keys(count as u64 + 35, count);
        let filter = BinaryFuseFilter::build(&keys).expect("builds");
        let bytes = filter.encode();
        for key in keys.iter().copied().chain(make_keys(9_999, 2_000)) {
            assert_eq!(
                BinaryFuseFilter::contains_encoded(&bytes, key).unwrap(),
                filter.contains(key)
            );
        }
    }
}

#[test]
fn binary_fuse_encoded_probe_rejects_what_decode_rejects() {
    let bytes = BinaryFuseFilter::build(&make_keys(5, 64)).expect("builds").encode();
    for cut in 0..bytes.len() {
        assert!(BinaryFuseFilter::contains_encoded(&bytes[..cut], 1).is_err());
        assert!(BinaryFuseFilter::decode(&bytes[..cut]).is_err());
    }
    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 0xff;
    assert!(BinaryFuseFilter::contains_encoded(&bad_magic, 1).is_err());
    let mut zero_segment = bytes.clone();
    zero_segment[4..8].copy_from_slice(&0u32.to_le_bytes());
    assert!(BinaryFuseFilter::contains_encoded(&zero_segment, 1).is_err());
}

#[test]
fn split_block_bloom_encode_into_preserves_prefix() {
    let filter = SplitBlockBloomFilter::build(&make_keys(67, 1_000), 10);
    let mut out = Writer::with_capacity(3 + filter.encoded_len());
    out.put_slice(&[7, 8, 9]);
    filter.encode_into(&mut out);
    let bytes = out.into_bytes();
    assert_eq!(&bytes[..3], &[7, 8, 9]);
    assert_eq!(&bytes[3..], filter.encode().as_slice());
    assert_eq!(filter.encode().len(), filter.encoded_len());
}
