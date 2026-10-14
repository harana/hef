use super::*;
use std::collections::{BTreeMap, BTreeSet};

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

#[test]
fn index_encode_matches_hand_built_bytes() {
    let mut index = BitmapIndex::new();
    index.insert(0, [0, 1, 2, 9]);
    index.insert(u64::MAX, [5]);
    let mut expected = Writer::new();
    expected.put_u32(2);
    for value in [0u64, u64::MAX] {
        let block = index.bitmap(value).unwrap().encode();
        expected.put_u64(value);
        expected.put_u32(block.len() as u32);
        expected.put_slice(&block);
    }
    assert_eq!(index.encode(), expected.into_bytes());
    assert_eq!(BitmapIndex::new().encode(), vec![0, 0, 0, 0]);
}

#[test]
fn from_ranges_merges_overlapping_touching_and_drops_empty() {
    let r = |start, end| RowRange { end, start };
    let bitmap = RoaringRangeBitmap::from_ranges([
        r(10, 12),
        r(0, 3),
        r(3, 5),
        r(7, 7),
        r(11, 20),
        r(u64::MAX - 1, u64::MAX),
        r(0, 1),
    ]);
    assert_eq!(bitmap.ranges(), &[r(0, 5), r(10, 20), r(u64::MAX - 1, u64::MAX)]);
    assert!(RoaringRangeBitmap::from_ranges([r(4, 4), r(9, 2)]).is_empty());
}

/// A run list written as `(start, end)` pairs.
type Runs<'a> = &'a [(u64, u64)];

fn bitmap_of(runs: &[(u64, u64)]) -> RoaringRangeBitmap {
    RoaringRangeBitmap::from_ranges(runs.iter().map(|&(start, end)| RowRange { end, start }))
}

/// How a repeated insert merged a value's rows before the linear union: concatenate both run lists and normalise them
/// with a general sort.
fn reference_union(a: &RoaringRangeBitmap, b: &RoaringRangeBitmap) -> RoaringRangeBitmap {
    RoaringRangeBitmap::from_ranges(a.ranges().iter().copied().chain(b.ranges().iter().copied()))
}

#[test]
fn union_matches_the_sorting_reference_and_set_union() {
    let cases: &[(Runs<'_>, Runs<'_>)] = &[
        // Disjoint.
        (&[(0, 2)], &[(5, 7)]),
        // Interleaved.
        (&[(0, 2), (10, 12), (20, 22)], &[(4, 6), (14, 16)]),
        // Equal starts.
        (&[(0, 5), (10, 12)], &[(0, 3), (10, 20)]),
        // Contained.
        (&[(0, 100)], &[(10, 20), (30, 40)]),
        // Identical.
        (&[(1, 4), (8, 9)], &[(1, 4), (8, 9)]),
        // A touching chain that collapses into one run.
        (&[(0, 2), (4, 6), (8, 10)], &[(2, 4), (6, 8)]),
        // One run on one side bridging several on the other.
        (&[(3, 5), (7, 9), (11, 13)], &[(4, 12)]),
        // Empty sides.
        (&[], &[(1, 2)]),
        (&[], &[]),
        // Runs ending at the top of the row-id space.
        (&[(u64::MAX - 5, u64::MAX)], &[(u64::MAX - 10, u64::MAX - 5)]),
        (&[(0, 1), (u64::MAX - 1, u64::MAX)], &[(u64::MAX - 3, u64::MAX)]),
    ];
    for &(a, b) in cases {
        let (a, b) = (bitmap_of(a), bitmap_of(b));
        let expected = reference_union(&a, &b);
        assert_eq!(a.union(&b), expected, "{a:?} | {b:?}");
        assert_eq!(b.union(&a), expected, "{b:?} | {a:?}");
        let rows: BTreeSet<u64> = rows_of(&a).union(&rows_of(&b)).copied().collect();
        assert_eq!(rows_of(&expected), rows, "{a:?} | {b:?}");
    }
}

#[test]
fn repeated_inserts_match_the_sorting_reference() {
    // A fixed-seed linear congruential generator, so a failure reproduces.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    for case in 0..300 {
        let mut index = BitmapIndex::new();
        let mut reference: BTreeMap<u64, RoaringRangeBitmap> = BTreeMap::new();
        for _ in 0..next(12) {
            let value = next(4);
            let rows: Vec<u64> = (0..next(10)).map(|_| next(64)).collect();
            index.insert(value, rows.iter().copied());
            let added = RoaringRangeBitmap::from_rows(rows);
            let merged = match reference.get(&value) {
                Some(existing) => reference_union(existing, &added),
                None => added,
            };
            reference.insert(value, merged);
        }
        let expected = BitmapIndex {
            entries: reference.into_iter().collect(),
        };
        assert_eq!(index, expected, "case {case}");
        assert_eq!(index.encode(), expected.encode(), "case {case}");
        assert_eq!(index.len(), expected.len(), "case {case}");
        for value in 0..4 {
            let (got, want) = (index.bitmap(value), expected.bitmap(value));
            assert_eq!(got.map(|b| b.count()), want.map(|b| b.count()), "case {case}");
            for row in 0..70 {
                assert_eq!(
                    got.is_some_and(|b| b.contains(row)),
                    want.is_some_and(|b| b.contains(row)),
                    "case {case}: value {value}, row {row}"
                );
            }
        }
    }
}

#[test]
fn empty_insert_leaves_an_existing_bitmap_untouched() {
    let mut index = BitmapIndex::new();
    index.insert(3, [1, 2, 3, 10]);
    let before = index.bitmap(3).unwrap().clone();
    let storage = index.bitmap(3).unwrap().ranges().as_ptr();
    index.insert(3, Vec::<u64>::new());
    assert_eq!(index.bitmap(3), Some(&before));
    // Same backing storage: the bitmap was not rebuilt.
    assert_eq!(index.bitmap(3).unwrap().ranges().as_ptr(), storage);
    assert_eq!(index.len(), 1);
}

#[test]
fn value_first_recorded_with_no_rows_takes_later_rows() {
    let mut index = BitmapIndex::new();
    index.insert(4, Vec::<u64>::new());
    assert_eq!(index.len(), 1);
    assert!(index.bitmap(4).unwrap().is_empty());
    index.insert(4, [6, 5, 5]);
    assert_eq!(index.bitmap(4).unwrap().ranges(), &[RowRange { end: 7, start: 5 }]);
    assert_eq!(index.len(), 1);
}

/// The runs a bit-by-bit walk of `bits` finds, the reference the word-at-a-time scan must reproduce.
fn runs_bit_by_bit(bits: &[u8]) -> Vec<RowRange> {
    let mut runs: Vec<RowRange> = Vec::new();
    for row in 0..bits.len() as u64 * 8 {
        if bits
            .get(row as usize / 8)
            .is_some_and(|byte| byte & (1 << (row % 8)) != 0)
        {
            match runs.last_mut() {
                Some(last) if last.end == row => last.end = row + 1,
                _ => runs.push(RowRange {
                    start: row,
                    end: row + 1,
                }),
            }
        }
    }
    runs
}

/// Random bitmaps of every length from empty to several words, built from runs of random lengths (short runs that
/// change inside a word, long ones that cross whole words and end exactly at a word or at the end), plus uniform and
/// random-byte inputs, all scan to exactly the runs a bit-by-bit walk finds and to `from_rows` over the set rows.
#[test]
fn from_packed_bits_matches_a_bit_by_bit_walk_on_random_bitmaps() {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for len in 0..=40usize {
        let mut shapes = vec![vec![0x00u8; len], vec![0xFFu8; len]];
        for max_run in [1u64, 3, 9, 70, 200] {
            for _ in 0..8 {
                let mut bits = vec![0u8; len];
                let mut row = 0usize;
                let mut set = next() & 1 == 1;
                while row < len * 8 {
                    let run = 1 + (next() % max_run) as usize;
                    for r in row..(row + run).min(len * 8) {
                        if set {
                            bits[r / 8] |= 1 << (r % 8);
                        }
                    }
                    row += run;
                    set = !set;
                }
                shapes.push(bits);
            }
        }
        shapes.push((0..len).map(|_| next() as u8).collect());
        for bits in shapes {
            let from_bits = RoaringRangeBitmap::from_packed_bits(&bits);
            assert_eq!(
                from_bits.ranges(),
                runs_bit_by_bit(&bits).as_slice(),
                "over {bits:02x?}"
            );
            assert_eq!(
                from_bits,
                RoaringRangeBitmap::from_rows(rows_of(&from_bits)),
                "over {bits:02x?}"
            );
        }
    }
}

/// How `intersect_values` worked before it reused buffers: clone the first value's bitmap, then replace it with a
/// freshly allocated intersection for each later value.
fn reference_intersect_values(index: &BitmapIndex, values: &[u64]) -> RoaringRangeBitmap {
    let mut iter = values.iter();
    let Some(first) = iter.next() else {
        return RoaringRangeBitmap::default();
    };
    let Some(acc) = index.bitmap(*first) else {
        return RoaringRangeBitmap::default();
    };
    let mut acc = acc.clone();
    for value in iter {
        match index.bitmap(*value) {
            Some(next) => acc = acc.intersect(next),
            None => return RoaringRangeBitmap::default(),
        }
        if acc.is_empty() {
            break;
        }
    }
    acc
}

/// Where each value's runs live in memory, to check a read left the index's storage in place.
fn storage_of(index: &BitmapIndex) -> Vec<(u64, *const RowRange)> {
    index
        .entries
        .iter()
        .map(|(value, bitmap)| (*value, bitmap.ranges().as_ptr()))
        .collect()
}

/// Checks `intersect_values` against the clone-and-intersect reference, its encoded bytes and a naive set oracle, and
/// that the index itself is left untouched.
fn assert_intersect_values_matches(index: &BitmapIndex, values: &[u64], context: &str) {
    let bytes = index.encode();
    let storage = storage_of(index);
    let got = index.intersect_values(values);
    let want = reference_intersect_values(index, values);
    assert_eq!(got, want, "{context}: values {values:?}");
    assert_eq!(got.encode(), want.encode(), "{context}: values {values:?}");
    let mut oracle: Option<BTreeSet<u64>> = None;
    for value in values {
        let rows = index.bitmap(*value).map(rows_of).unwrap_or_default();
        oracle = Some(match oracle {
            Some(acc) => acc.intersection(&rows).copied().collect(),
            None => rows,
        });
    }
    assert_eq!(
        rows_of(&got),
        oracle.unwrap_or_default(),
        "{context}: values {values:?}"
    );
    assert!(
        got.ranges().iter().all(|r| r.start < r.end) && got.ranges().windows(2).all(|w| w[0].end < w[1].start),
        "{context}: result not canonical: {got:?}"
    );
    assert_eq!(index.encode(), bytes, "{context}: index bytes changed");
    assert_eq!(storage_of(index), storage, "{context}: index storage moved");
}

fn index_of(entries: &[(u64, Runs<'_>)]) -> BitmapIndex {
    BitmapIndex {
        entries: entries.iter().map(|&(value, runs)| (value, bitmap_of(runs))).collect(),
    }
}

#[test]
fn intersect_values_matches_the_reference_on_edge_cases() {
    let fragments: Vec<(u64, u64)> = (0..64u64).map(|i| (i * 4, i * 4 + 2)).collect();
    let entries: &[(u64, Runs<'_>)] = &[
        // Empty bitmap.
        (0, &[]),
        // One wide run.
        (1, &[(0, 1_000)]),
        // Many disjoint runs that split the wide run.
        (2, fragments.as_slice()),
        // Shifted fragments, alternating against value 2.
        (3, &[(1, 3), (5, 7), (9, 11), (13, 15), (100, 200)]),
        // Runs touching the bounds of value 1.
        (4, &[(0, 1), (999, 1_000), (1_000, 1_001)]),
        // Runs ending at the top of the row-id space.
        (5, &[(0, 10), (u64::MAX - 10, u64::MAX)]),
        (6, &[(5, 6), (u64::MAX - 3, u64::MAX)]),
        // Disjoint from value 1.
        (7, &[(2_000, 3_000)]),
        // A progressively shrinking chain.
        (8, &[(0, 500)]),
        (9, &[(100, 400)]),
        (10, &[(200, 300)]),
    ];
    let index = index_of(entries);
    // Value 11 is absent from the index.
    let requests: &[&[u64]] = &[
        &[],
        &[1],
        &[0],
        &[11],
        &[1, 1],
        &[1, 1, 1, 1],
        &[11, 1],
        &[1, 11],
        &[1, 2, 11],
        &[1, 7, 11],
        &[0, 1],
        &[1, 0],
        &[1, 2, 0],
        &[0, 11],
        &[1, 2],
        &[2, 1],
        &[1, 2, 3],
        &[2, 3, 2, 3],
        &[1, 4],
        &[4, 1, 4],
        &[5, 6],
        &[5, 6, 5],
        &[1, 7],
        &[1, 7, 2],
        &[1, 8, 9, 10],
        &[10, 9, 8, 1],
        &[1, 8, 9, 10, 2],
    ];
    for values in requests {
        assert_intersect_values_matches(&index, values, "edge case");
    }
}

#[test]
fn intersect_values_matches_the_reference_on_random_indexes() {
    // A fixed-seed linear congruential generator, so a failure reproduces.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    for case in 0..500 {
        let mut index = BitmapIndex::new();
        for value in 0..6 {
            // Leave some values absent and give the rest a mix of dense and fragmented rows.
            if next(5) == 0 {
                continue;
            }
            let density = next(4) + 1;
            let rows: Vec<u64> = (0..96).filter(|_| next(density) == 0).collect();
            index.insert(value, rows);
        }
        for _ in 0..8 {
            let values: Vec<u64> = (0..next(7)).map(|_| next(7)).collect();
            assert_intersect_values_matches(&index, &values, &format!("case {case}"));
        }
    }
}

/// Runs that cross word edges stay one run, and a run reaching the last bit closes at the end of the bitmap, whether
/// the bitmap ends on a whole word or partway through one.
#[test]
fn from_packed_bits_keeps_runs_whole_across_words_and_to_the_end() {
    let mut bits = vec![0u8; 24];
    bits[7] = 0x80;
    bits[8..16].fill(0xFF);
    bits[16] = 0x01;
    bits[23] = 0xC0;
    assert_eq!(
        RoaringRangeBitmap::from_packed_bits(&bits).ranges(),
        &[RowRange { start: 63, end: 129 }, RowRange { start: 190, end: 192 }]
    );
    assert_eq!(
        RoaringRangeBitmap::from_packed_bits(&[0xFF; 11]).ranges(),
        &[RowRange { start: 0, end: 88 }]
    );
    assert_eq!(
        RoaringRangeBitmap::from_packed_bits(&[0x00, 0, 0, 0, 0, 0, 0, 0, 0x80]).ranges(),
        &[RowRange { start: 71, end: 72 }]
    );
}

#[test]
fn intersect_runs_into_replaces_previous_contents() {
    let mut out = vec![RowRange { end: 9, start: 8 }; 4];
    intersect_runs_into(
        bitmap_of(&[(0, 10)]).ranges(),
        bitmap_of(&[(2, 3), (5, 7)]).ranges(),
        &mut out,
    );
    assert_eq!(out, vec![RowRange { end: 3, start: 2 }, RowRange { end: 7, start: 5 }]);
    intersect_runs_into(bitmap_of(&[(0, 10)]).ranges(), &[], &mut out);
    assert!(out.is_empty());
}
