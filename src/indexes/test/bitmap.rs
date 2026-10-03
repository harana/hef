use super::*;
use std::collections::BTreeSet;

/// The naive, fully-materialized intersection, used as an oracle to check that the compressed-form intersection agrees
/// with the true set intersection.
fn naive_intersection(a: &[u64], b: &[u64]) -> BTreeSet<u64> {
    let sa: BTreeSet<u64> = a.iter().copied().collect();
    let sb: BTreeSet<u64> = b.iter().copied().collect();
    sa.intersection(&sb).copied().collect()
}

fn rows_of(bitmap: &RoaringRangeBitmap) -> BTreeSet<u64> {
    bitmap.ranges().iter().flat_map(|r| r.start..r.end).collect()
}

#[test]
fn from_rows_collapses_into_runs() {
    let bitmap = RoaringRangeBitmap::from_rows([3, 1, 2, 2, 0, 7, 8, 9]);
    // 0..4 and 7..10 — adjacency merged, duplicates removed.
    assert_eq!(
        bitmap.ranges(),
        &[RowRange { start: 0, end: 4 }, RowRange { start: 7, end: 10 },]
    );
    assert_eq!(bitmap.count(), 7);
}

#[test]
fn from_packed_bits_equals_from_rows_over_the_same_bits() {
    for bits in [
        vec![],
        vec![0x00u8],
        vec![0xFF],
        vec![0x01, 0x80],
        vec![0xF0, 0x0F],
        vec![0xFF, 0x00, 0xFF],
        vec![0xAA, 0x55, 0xC3, 0x00, 0xFF, 0x81],
    ] {
        let set_rows: Vec<u64> = bits
            .iter()
            .enumerate()
            .flat_map(|(byte_index, &byte)| {
                (0..8u64).filter_map(move |bit| (byte & (1 << bit) != 0).then_some(byte_index as u64 * 8 + bit))
            })
            .collect();
        let from_bits = RoaringRangeBitmap::from_packed_bits(&bits);
        let from_rows = RoaringRangeBitmap::from_rows(set_rows);
        assert_eq!(from_bits, from_rows, "over {bits:02x?}");
    }
}

#[test]
fn from_packed_bits_merges_runs_across_byte_boundaries() {
    // Bits 4..12 set: one run spanning the byte boundary, not two touching runs.
    let bitmap = RoaringRangeBitmap::from_packed_bits(&[0xF0, 0x0F]);
    assert_eq!(bitmap.ranges(), &[RowRange { start: 4, end: 12 }]);
}

#[test]
fn from_ranges_merges_overlapping_and_touching() {
    let bitmap = RoaringRangeBitmap::from_ranges([
        RowRange { start: 10, end: 15 },
        RowRange { start: 0, end: 5 },
        RowRange { start: 5, end: 8 },   // touches 0..5
        RowRange { start: 12, end: 20 }, // overlaps 10..15
        RowRange { start: 3, end: 3 },   // empty, dropped
    ]);
    assert_eq!(
        bitmap.ranges(),
        &[RowRange { start: 0, end: 8 }, RowRange { start: 10, end: 20 },]
    );
}

#[test]
fn contains_matches_membership() {
    let bitmap = RoaringRangeBitmap::from_rows([1, 2, 3, 10, 11]);
    for present in [1, 2, 3, 10, 11] {
        assert!(bitmap.contains(present), "{present} should be present");
    }
    for absent in [0, 4, 5, 9, 12, 100] {
        assert!(!bitmap.contains(absent), "{absent} should be absent");
    }
}

#[test]
fn intersect_equals_true_set_intersection() {
    let cases: &[(&[u64], &[u64])] = &[
        (&[1, 2, 3, 4, 5], &[3, 4, 5, 6, 7]),
        (&[0, 1, 2], &[5, 6, 7]),
        (&[10, 11, 12, 13], &[11, 13]),
        (&[], &[1, 2, 3]),
        (&[1, 5, 9, 13], &[1, 2, 5, 6, 9, 10, 13]),
        (&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], &[2, 4, 6, 8, 10]),
    ];
    for (a, b) in cases {
        let ba = RoaringRangeBitmap::from_rows(a.iter().copied());
        let bb = RoaringRangeBitmap::from_rows(b.iter().copied());
        let intersected = ba.intersect(&bb);
        assert_eq!(
            rows_of(&intersected),
            naive_intersection(a, b),
            "intersection of {a:?} and {b:?}"
        );
        // Intersection is commutative on the compressed form too.
        assert_eq!(bb.intersect(&ba), intersected);
        // Count matches the materialized size.
        assert_eq!(intersected.count() as usize, naive_intersection(a, b).len());
    }
}

#[test]
fn intersect_result_is_canonical_runs() {
    // Two overlapping run sets whose intersection spans adjacent runs must come back merged, not as fragments.
    let a = RoaringRangeBitmap::from_ranges([RowRange { start: 0, end: 100 }]);
    let b = RoaringRangeBitmap::from_ranges([RowRange { start: 0, end: 40 }, RowRange { start: 40, end: 80 }]);
    let intersected = a.intersect(&b);
    assert_eq!(intersected.ranges(), &[RowRange { start: 0, end: 80 }]);
}

#[test]
fn bitmap_encode_decode_round_trip() {
    let bitmap = RoaringRangeBitmap::from_rows([1, 2, 3, 50, 51, 99]);
    let bytes = bitmap.encode();
    let decoded = RoaringRangeBitmap::decode(&bytes).unwrap();
    assert_eq!(decoded, bitmap);
}

#[test]
fn decode_rejects_unsorted_or_touching_runs() {
    // Two touching runs (end == next start) violate the non-adjacent invariant.
    let mut out = Vec::new();
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&5u64.to_le_bytes());
    out.extend_from_slice(&5u64.to_le_bytes());
    out.extend_from_slice(&8u64.to_le_bytes());
    assert!(RoaringRangeBitmap::decode(&out).is_err());
}

#[test]
fn decode_rejects_empty_run() {
    let mut out = Vec::new();
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&5u64.to_le_bytes());
    out.extend_from_slice(&5u64.to_le_bytes());
    assert!(RoaringRangeBitmap::decode(&out).is_err());
}

#[test]
fn decode_rejects_truncated_input() {
    assert!(RoaringRangeBitmap::decode(&[1, 0, 0, 0]).is_err());
}

#[test]
fn exactness_is_exact() {
    assert_eq!(RoaringRangeBitmap::exactness(), Exactness::Exact);
}

#[test]
fn bitmap_index_maps_values_to_rows() {
    let mut index = BitmapIndex::new();
    index.insert(7, [0, 1, 2, 10]);
    index.insert(9, [2, 3, 10, 11]);
    assert_eq!(index.len(), 2);
    assert_eq!(rows_of(index.bitmap(7).unwrap()), [0, 1, 2, 10].into());
    assert!(index.bitmap(42).is_none());
}

#[test]
fn bitmap_index_insert_accumulates_same_value() {
    let mut index = BitmapIndex::new();
    index.insert(1, [0, 1]);
    index.insert(1, [5, 6]);
    assert_eq!(rows_of(index.bitmap(1).unwrap()), [0, 1, 5, 6].into());
    assert_eq!(index.len(), 1);
}

#[test]
fn intersect_values_ands_across_values() {
    let mut index = BitmapIndex::new();
    index.insert(100, [0, 1, 2, 3, 4]); // e.g. status = active
    index.insert(200, [2, 3, 4, 5, 6]); // e.g. country = US
    let both = index.intersect_values(&[100, 200]);
    assert_eq!(rows_of(&both), [2, 3, 4].into());

    // A missing value yields the empty set (no rows carry it).
    assert!(index.intersect_values(&[100, 999]).is_empty());
    // No values -> empty.
    assert!(index.intersect_values(&[]).is_empty());
    // Single value -> that value's rows.
    assert_eq!(rows_of(&index.intersect_values(&[100])), [0, 1, 2, 3, 4].into());
}

#[test]
fn bitmap_index_encode_decode_round_trip() {
    let mut index = BitmapIndex::new();
    index.insert(1, [0, 1, 2]);
    index.insert(5, [10, 11]);
    index.insert(9, [100, 101, 102, 103]);
    let bytes = index.encode();
    let decoded = BitmapIndex::decode(&bytes).unwrap();
    assert_eq!(decoded, index);
}

#[test]
fn bitmap_index_decode_rejects_unsorted_values() {
    let mut out = Vec::new();
    out.extend_from_slice(&2u32.to_le_bytes());
    // value 5 first
    out.extend_from_slice(&5u64.to_le_bytes());
    let block = RoaringRangeBitmap::from_rows([0]).encode();
    out.extend_from_slice(&(block.len() as u32).to_le_bytes());
    out.extend_from_slice(&block);
    // value 5 again (not strictly increasing)
    out.extend_from_slice(&5u64.to_le_bytes());
    out.extend_from_slice(&(block.len() as u32).to_le_bytes());
    out.extend_from_slice(&block);
    assert!(BitmapIndex::decode(&out).is_err());
}

#[test]
fn can_use_bitmap_requires_all_three() {
    let good = BitmapBlockInfo {
        checksum_verified: true,
        directly_intersectable: true,
        feature_declared: true,
    };
    assert!(can_use_bitmap(&good));

    // Drop each condition in turn -> rejected.
    assert!(!can_use_bitmap(&BitmapBlockInfo {
        directly_intersectable: false,
        ..good
    }));
    assert!(!can_use_bitmap(&BitmapBlockInfo {
        feature_declared: false,
        ..good
    }));
    assert!(!can_use_bitmap(&BitmapBlockInfo {
        checksum_verified: false,
        ..good
    }));
}

#[test]
fn rank_counts_members_before_position() {
    use super::super::rank_select::RankSelect;
    // Bitmap: {0, 1, 2, 10, 11}
    let bitmap = RankSelect::from_bitmap(&RoaringRangeBitmap::from_rows([0, 1, 2, 10, 11]));
    // rank(0): nothing before 0 -> 0
    assert_eq!(bitmap.rank(0), 0);
    // rank(1): only row 0 -> 1
    assert_eq!(bitmap.rank(1), 1);
    // rank(3): rows 0,1,2 -> 3
    assert_eq!(bitmap.rank(3), 3);
    // rank(10): same 3 (gap 3..10 not in set)
    assert_eq!(bitmap.rank(10), 3);
    // rank(11): rows 0,1,2,10 -> 4
    assert_eq!(bitmap.rank(11), 4);
    // rank(100): all 5 members
    assert_eq!(bitmap.rank(100), 5);
    // Empty bitmap always ranks 0
    assert_eq!(RankSelect::from_bitmap(&RoaringRangeBitmap::default()).rank(999), 0);
}

#[test]
fn select_returns_kth_member() {
    use super::super::rank_select::RankSelect;
    let bitmap = RankSelect::from_bitmap(&RoaringRangeBitmap::from_rows([0, 1, 2, 10, 11]));
    assert_eq!(bitmap.select(0), Some(0));
    assert_eq!(bitmap.select(1), Some(1));
    assert_eq!(bitmap.select(2), Some(2));
    assert_eq!(bitmap.select(3), Some(10));
    assert_eq!(bitmap.select(4), Some(11));
    // k >= count() -> None
    assert_eq!(bitmap.select(5), None);
    assert_eq!(bitmap.select(100), None);
    // Empty bitmap
    assert_eq!(RankSelect::from_bitmap(&RoaringRangeBitmap::default()).select(0), None);
}

#[test]
fn rank_select_are_left_inverses() {
    use super::super::rank_select::RankSelect;
    // rank(select(k)) == k for every k in 0..count
    let bitmap = RankSelect::from_bitmap(&RoaringRangeBitmap::from_rows([3, 5, 7, 100, 101, 200]));
    for k in 0..bitmap.total() {
        let pos = bitmap.select(k).unwrap();
        assert_eq!(bitmap.rank(pos), k, "rank(select({k})) != {k}");
    }
}

#[test]
fn subtract_equals_set_difference() {
    fn naive_subtract(a: &[u64], b: &[u64]) -> BTreeSet<u64> {
        let sa: BTreeSet<u64> = a.iter().copied().collect();
        let sb: BTreeSet<u64> = b.iter().copied().collect();
        sa.difference(&sb).copied().collect()
    }

    let cases: &[(&[u64], &[u64])] = &[
        (&[0, 1, 2, 3, 4], &[1, 3]),
        (&[5, 6, 7, 8], &[5, 6, 7, 8]),
        (&[0, 1, 2], &[]),
        (&[], &[1, 2]),
        (&[0, 10, 20, 30], &[10, 30]),
        (&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], &[0, 2, 4, 6, 8]),
    ];
    for (a, b) in cases {
        let ba = RoaringRangeBitmap::from_rows(a.iter().copied());
        let bb = RoaringRangeBitmap::from_rows(b.iter().copied());
        let result = ba.difference(&bb);
        assert_eq!(rows_of(&result), naive_subtract(a, b), "subtract({a:?}, {b:?})");
    }

    // Subtracting self yields empty.
    let bm = RoaringRangeBitmap::from_rows([1, 2, 3, 10, 20]);
    assert!(bm.difference(&bm).is_empty());
    // Subtracting empty is identity.
    assert_eq!(bm.difference(&RoaringRangeBitmap::default()), bm);
}

#[test]
fn verify_bitmap_block_checks_real_checksum() {
    let bitmap = RoaringRangeBitmap::from_rows([1, 2, 3]);
    let block = bitmap.encode();
    let checksum = bitmap_block_checksum(&block);

    let info = verify_bitmap_block(&block, &checksum, true, true);
    assert!(info.checksum_verified);
    assert!(can_use_bitmap(&info));

    // A tampered block does not verify, so the planner must not use it.
    let mut tampered = block.clone();
    if let Some(byte) = tampered.last_mut() {
        *byte ^= 0xFF;
    }
    let bad = verify_bitmap_block(&tampered, &checksum, true, true);
    assert!(!bad.checksum_verified);
    assert!(!can_use_bitmap(&bad));
}

#[test]
fn difference_subtracts_deleted_rows() {
    let cases: &[(&[u64], &[u64], &[u64])] = &[
        // Subtracted middle portion.
        (&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], &[3, 4, 5, 6], &[0, 1, 2, 7, 8, 9]),
        // No overlap: nothing removed.
        (&[0, 1, 2], &[10, 11, 12], &[0, 1, 2]),
        // Full overlap: everything removed.
        (&[5, 6, 7], &[5, 6, 7], &[]),
        // Disjoint b runs that split a into several pieces.
        (&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9], &[2, 5, 8], &[0, 1, 3, 4, 6, 7, 9]),
        // b extends beyond a on both sides.
        (&[3, 4, 5], &[0, 1, 2, 3, 4, 5, 6, 7], &[]),
        // Empty self.
        (&[], &[1, 2, 3], &[]),
        // Empty other: self unchanged.
        (&[1, 2, 3], &[], &[1, 2, 3]),
    ];
    for (a_rows, b_rows, expected) in cases {
        let a = RoaringRangeBitmap::from_rows(a_rows.iter().copied());
        let b = RoaringRangeBitmap::from_rows(b_rows.iter().copied());
        let diff = a.difference(&b);
        let got: Vec<u64> = diff.ranges().iter().flat_map(|r| r.start..r.end).collect();
        assert_eq!(&got, expected, "a={a_rows:?} b={b_rows:?}");
        // Verify count matches.
        assert_eq!(diff.count() as usize, expected.len());
    }
}

#[test]
fn difference_result_is_canonical() {
    // When two adjacent gaps in b leave a-run pieces that touch, the result must still be merged into canonical
    // non-adjacent runs.
    let a = RoaringRangeBitmap::from_ranges([RowRange { start: 0, end: 20 }]);
    let b = RoaringRangeBitmap::from_ranges([RowRange { start: 10, end: 10 }]); // empty run — dropped
    let diff = a.difference(&b);
    // Empty b run has no effect; a should come back intact as one run.
    assert_eq!(diff.ranges(), &[RowRange { start: 0, end: 20 }]);
}
