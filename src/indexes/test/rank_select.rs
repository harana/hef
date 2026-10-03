use super::super::bitmap::RoaringRangeBitmap;
use super::super::rank_select::RankSelect;

fn bitmap(rows: impl IntoIterator<Item = u64>) -> RoaringRangeBitmap {
    RoaringRangeBitmap::from_rows(rows)
}

/// Oracle: naive rank over materialized rows.
fn naive_rank(rows: &[u64], position: u64) -> u64 {
    rows.iter().filter(|&&r| r < position).count() as u64
}

/// Oracle: naive select over materialized rows.
fn naive_select(rows: &[u64], k: u64) -> Option<u64> {
    rows.get(k as usize).copied()
}

#[test]
fn rank_matches_naive_for_sparse_bitmap() {
    let rows: Vec<u64> = vec![0, 2, 5, 10, 11, 20];
    let bm = bitmap(rows.iter().copied());
    let rs = RankSelect::from_bitmap(&bm);
    for pos in 0u64..=22 {
        assert_eq!(rs.rank(pos), naive_rank(&rows, pos), "rank({pos})");
    }
}

#[test]
fn rank_matches_naive_for_dense_bitmap() {
    let rows: Vec<u64> = (0u64..100).collect();
    let bm = bitmap(rows.iter().copied());
    let rs = RankSelect::from_bitmap(&bm);
    for pos in 0u64..=101 {
        assert_eq!(rs.rank(pos), naive_rank(&rows, pos), "rank({pos})");
    }
}

#[test]
fn rank_at_zero_is_always_zero() {
    let bm = bitmap([5, 10, 15]);
    let rs = RankSelect::from_bitmap(&bm);
    assert_eq!(rs.rank(0), 0);
}

#[test]
fn rank_on_empty_bitmap_is_zero() {
    let rs = RankSelect::from_bitmap(&RoaringRangeBitmap::default());
    assert_eq!(rs.rank(0), 0);
    assert_eq!(rs.rank(100), 0);
}

#[test]
fn select_matches_naive_for_sparse_bitmap() {
    let rows: Vec<u64> = vec![1, 3, 7, 8, 100, 200];
    let bm = bitmap(rows.iter().copied());
    let rs = RankSelect::from_bitmap(&bm);
    for k in 0u64..=(rows.len() as u64 + 1) {
        assert_eq!(rs.select(k), naive_select(&rows, k), "select({k})");
    }
}

#[test]
fn select_matches_naive_for_dense_bitmap() {
    let rows: Vec<u64> = (50u64..60).collect();
    let bm = bitmap(rows.iter().copied());
    let rs = RankSelect::from_bitmap(&bm);
    for k in 0u64..=10 {
        assert_eq!(rs.select(k), naive_select(&rows, k), "select({k})");
    }
}

#[test]
fn select_returns_none_out_of_range() {
    let bm = bitmap([1, 2, 3]);
    let rs = RankSelect::from_bitmap(&bm);
    assert_eq!(rs.total(), 3);
    assert!(rs.select(3).is_none());
    assert!(rs.select(100).is_none());
}

#[test]
fn select_on_empty_bitmap_is_none() {
    let rs = RankSelect::from_bitmap(&RoaringRangeBitmap::default());
    assert!(rs.select(0).is_none());
}

#[test]
fn rank_and_select_are_inverses() {
    // For any k in [0, total): select(k) = pos => rank(pos) = k and rank(pos) = k => select(k) is the first position
    // with rank k.
    let rows: Vec<u64> = vec![0, 1, 5, 6, 7, 20, 21, 50];
    let bm = bitmap(rows.iter().copied());
    let rs = RankSelect::from_bitmap(&bm);
    for k in 0..rows.len() as u64 {
        let pos = rs.select(k).unwrap();
        assert_eq!(rs.rank(pos), k, "rank(select({k})) should be {k}");
        // rank(pos + 1) = k + 1 (each position holds exactly one set bit).
        assert_eq!(rs.rank(pos + 1), k + 1, "rank(select({k}) + 1) should be {}", k + 1);
    }
}

/// Oracle for the compressed-form constructor: the raw path that materializes one row id per set bit.
fn raw_path(bits: &[u8]) -> RankSelect {
    let rows = bits.iter().enumerate().flat_map(|(byte_index, &byte)| {
        (0..8u64).filter_map(move |bit| (byte & (1 << bit) != 0).then_some(byte_index as u64 * 8 + bit))
    });
    RankSelect::from_bitmap(&bitmap(rows))
}

/// Every rank and select answer of `from_packed_bits` must equal the raw materialized path's.
fn assert_equivalent_to_raw_path(bits: &[u8]) {
    let compressed = RankSelect::from_packed_bits(bits);
    let raw = raw_path(bits);
    assert_eq!(compressed.total(), raw.total(), "total over {bits:02x?}");
    for position in 0..=(bits.len() as u64 * 8 + 2) {
        assert_eq!(
            compressed.rank(position),
            raw.rank(position),
            "rank({position}) over {bits:02x?}"
        );
    }
    for k in 0..=(raw.total() + 2) {
        assert_eq!(compressed.select(k), raw.select(k), "select({k}) over {bits:02x?}");
    }
}

#[test]
fn from_packed_bits_matches_raw_path_on_edge_patterns() {
    assert_equivalent_to_raw_path(&[]);
    assert_equivalent_to_raw_path(&[0x00]);
    assert_equivalent_to_raw_path(&[0xFF]);
    assert_equivalent_to_raw_path(&[0x00, 0x00, 0x00]);
    assert_equivalent_to_raw_path(&[0xFF, 0xFF, 0xFF]);
    assert_equivalent_to_raw_path(&[0x01]);
    assert_equivalent_to_raw_path(&[0x80]);
    assert_equivalent_to_raw_path(&[0xAA, 0x55]);
    // Runs that cross byte boundaries in both directions, and isolated bits beside full bytes.
    assert_equivalent_to_raw_path(&[0xF0, 0x0F]);
    assert_equivalent_to_raw_path(&[0xFF, 0x01, 0x80, 0xFF]);
    assert_equivalent_to_raw_path(&[0x00, 0xFF, 0x00, 0xFF, 0x00]);
}

#[test]
fn from_packed_bits_matches_raw_path_on_generated_patterns() {
    // Deterministic xorshift so the byte patterns cover mixed bytes, long runs, and trailing partial content without
    // depending on an RNG crate.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for len in [1usize, 2, 7, 8, 9, 32, 129] {
        let bits: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        assert_equivalent_to_raw_path(&bits);
        // Bias toward long all-set and all-clear stretches, the shapes real presence bitmaps take.
        let clustered: Vec<u8> = (0..len)
            .map(|i| match (i / 4) % 3 {
                0 => 0xFF,
                1 => 0x00,
                _ => next() as u8,
            })
            .collect();
        assert_equivalent_to_raw_path(&clustered);
    }
}

#[test]
fn total_matches_bitmap_count() {
    let rows: Vec<u64> = vec![3, 7, 11, 15];
    let bm = bitmap(rows.iter().copied());
    let rs = RankSelect::from_bitmap(&bm);
    assert_eq!(rs.total(), bm.count());
    assert_eq!(rs.total(), 4);
}

#[test]
fn gather_survivors_via_select() {
    // Simulate late-materialisation gather: after filtering, logical positions [0, 1, 2] of the live-row stream map to
    // physical offsets via select.
    //
    // Live rows at physical positions 10, 20, 30, 40 (row 5, 15, 25, 35 deleted).
    let live = bitmap([10, 20, 30, 40]);
    let rs = RankSelect::from_bitmap(&live);
    // Logical position k -> physical position select(k).
    let physical: Vec<u64> = (0..4).map(|k| rs.select(k).unwrap()).collect();
    assert_eq!(physical, vec![10, 20, 30, 40]);
    // Rank confirms the reverse: physical 20 is logical 1.
    assert_eq!(rs.rank(20), 1);
    assert_eq!(rs.rank(21), 2);
}

#[test]
fn anti_join_via_difference_matches_select_gather() {
    // All rows 0..10, deletion vector marks rows 2, 5, 8.
    let all = bitmap(0..10u64);
    let deleted = bitmap([2u64, 5, 8]);
    let live = all.difference(&deleted);
    let expected_live: Vec<u64> = (0..10u64).filter(|r| ![2, 5, 8].contains(r)).collect();
    let got_live: Vec<u64> = live.ranges().iter().flat_map(|r| r.start..r.end).collect();
    assert_eq!(got_live, expected_live);

    // Rank/select over the live bitmap gives correct physical positions.
    let rs = RankSelect::from_bitmap(&live);
    assert_eq!(rs.total(), expected_live.len() as u64);
    for (k, &physical) in expected_live.iter().enumerate() {
        assert_eq!(rs.select(k as u64), Some(physical));
        assert_eq!(rs.rank(physical), k as u64);
    }
}
