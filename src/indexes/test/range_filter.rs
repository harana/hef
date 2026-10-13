use super::*;

/// A tiny deterministic pseudo-random source for generating test keys.
fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
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
fn every_inserted_key_reports_its_own_range_nonempty() {
    let keys = make_keys(1, 5_000);
    let filter = RangeFilter::build(&keys);
    for &key in &keys {
        assert!(
            filter.range_nonempty(key, key),
            "key {key} must report its own point range non-empty"
        );
    }
}

#[test]
fn never_under_reports_against_ground_truth() {
    // The defining property: whenever the real key set has a key in [lo, hi], the filter must say non-empty. (It may
    // also say non-empty when the real set does not — that is an allowed false positive.)
    let keys = make_keys(2, 3_000);
    let filter = RangeFilter::build(&keys);
    let mut sorted = keys.clone();
    sorted.sort_unstable();

    let mut bounds = lcg(0xabcd_1234);
    for _ in 0..10_000 {
        let a = bounds();
        let b = bounds();
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let truly_nonempty = sorted
            .binary_search(&lo)
            .map_or_else(|pos| sorted.get(pos).is_some_and(|&k| k <= hi), |_| true);
        if truly_nonempty {
            assert!(
                filter.range_nonempty(lo, hi),
                "range [{lo}, {hi}] holds a key but filter reported empty (false negative)"
            );
        }
    }
}

#[test]
fn empty_and_inverted_ranges_report_empty() {
    let filter = RangeFilter::build(&[]);
    assert!(!filter.range_nonempty(0, u64::MAX));
    // Inverted range is empty by definition.
    let filter = RangeFilter::build(&[100, 200, 300]);
    assert!(!filter.range_nonempty(500, 100));
}

#[test]
fn full_range_over_nonempty_filter_reports_nonempty() {
    let keys = make_keys(3, 100);
    let filter = RangeFilter::build(&keys);
    assert!(filter.range_nonempty(0, u64::MAX));
}

#[test]
fn round_trips_and_is_deterministic() {
    let keys = make_keys(4, 2_000);
    let a = RangeFilter::build(&keys);
    let b = RangeFilter::build(&keys);
    assert_eq!(a.encode(), b.encode(), "same keys -> identical bytes");

    let decoded = RangeFilter::decode(&a.encode()).expect("decode");
    assert_eq!(a, decoded);
    for &key in &keys {
        assert!(decoded.range_nonempty(key, key));
    }
}

#[test]
fn custom_bucket_count_is_honoured_and_round_trips() {
    let keys = make_keys(5, 1_000);
    let filter = RangeFilter::build_with_buckets(&keys, 64);
    let decoded = RangeFilter::decode(&filter.encode()).expect("decode");
    assert_eq!(filter, decoded);
    for &key in &keys {
        assert!(decoded.range_nonempty(key, key));
    }
}

#[test]
fn exactness_is_always_inexact() {
    let filter = RangeFilter::build(&[1, 2, 3]);
    assert_eq!(filter.exactness(), Exactness::InexactNoFalseNegative);
}

#[test]
fn decode_rejects_bad_magic_and_truncation() {
    let bytes = RangeFilter::build(&[1, 2, 3, 4, 5]).encode();
    let mut bad = bytes.clone();
    bad[0] ^= 0xff;
    assert!(RangeFilter::decode(&bad).is_err());
    assert!(RangeFilter::decode(&bytes[..bytes.len() - 1]).is_err());
}

#[test]
fn decode_rejects_out_of_range_and_unsorted_buckets() {
    // Hand-build a body with a bucket id >= bucket_count.
    let mut out = Vec::new();
    out.extend_from_slice(&0x5247_4631u32.to_le_bytes()); // magic
    out.extend_from_slice(&4u32.to_le_bytes()); // bucket_count = 4
    out.extend_from_slice(&1u32.to_le_bytes()); // occupied count
    out.extend_from_slice(&9u32.to_le_bytes()); // bucket id 9 >= 4
    assert!(RangeFilter::decode(&out).is_err());

    // Non-ascending buckets must be rejected (would break the binary search).
    let mut out = Vec::new();
    out.extend_from_slice(&0x5247_4631u32.to_le_bytes());
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&5u32.to_le_bytes());
    out.extend_from_slice(&5u32.to_le_bytes()); // equal -> not strictly ascending
    assert!(RangeFilter::decode(&out).is_err());
}

/// The original collect, sort and dedup builder, kept as the reference the bitmap builder must match byte for byte.
fn reference_build(keys: &[u64], bucket_count: u32) -> RangeFilter {
    let bucket_count = bucket_count.max(1);
    let mut occupied: Vec<u32> = keys
        .iter()
        .map(|&key| RangeFilter::bucket_of(key, bucket_count))
        .collect();
    occupied.sort_unstable();
    occupied.dedup();
    RangeFilter { bucket_count, occupied }
}

#[test]
fn bitmap_builder_matches_the_sort_and_dedup_reference() {
    let mut boundary_keys = vec![0, 1, u64::MAX, u64::MAX - 1];
    for bucket in [1u64, 63, 64, 65, 4095] {
        let edge = bucket.wrapping_mul(u64::MAX / 4096);
        boundary_keys.extend([edge.wrapping_sub(1), edge, edge.wrapping_add(1)]);
    }
    let every_bucket: Vec<u64> = (0..4096u64).map(|bucket| bucket << 52).collect();
    let duplicates: Vec<u64> = make_keys(7, 8).into_iter().cycle().take(5000).collect();
    let mut permuted = make_keys(11, 2000);
    permuted.reverse();
    let inputs = [
        Vec::new(),
        vec![42],
        vec![u64::MIN],
        vec![u64::MAX],
        boundary_keys,
        every_bucket.clone(),
        duplicates,
        make_keys(3, 9000),
        permuted,
    ];
    for bucket_count in [0, 1, 63, 64, 65, 4095, 4096, 4097, u32::MAX] {
        for keys in &inputs {
            let built = RangeFilter::build_with_buckets(keys, bucket_count);
            let reference = reference_build(keys, bucket_count);
            assert_eq!(built.encode(), reference.encode(), "bucket_count {bucket_count}");
            for (lo, hi) in [(0, u64::MAX), (0, 0), (u64::MAX, u64::MAX), (1 << 40, 1 << 62)] {
                assert_eq!(built.range_nonempty(lo, hi), reference.range_nonempty(lo, hi));
            }
        }
    }
    assert_eq!(RangeFilter::build(&every_bucket).occupied.len(), 4096);
}
