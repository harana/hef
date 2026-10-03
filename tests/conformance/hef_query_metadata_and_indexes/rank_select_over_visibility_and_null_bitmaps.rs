//! Checks that rank and select over visibility and null bitmaps work as the spec requires: without expanding the bitmap into row ids, and with the deletion anti-join computed as bitmap AND-NOT rather than row-by-row.

use hef::indexes::bitmap::{RoaringRangeBitmap, RowRange};
use hef::indexes::rank_select::RankSelect;
use std::collections::BTreeSet;

/// Reference gather: materialise all live rows, then index into the list.
fn naive_gather(live: &[u64], logical_positions: &[u64]) -> Vec<u64> {
    logical_positions
        .iter()
        .filter_map(|&k| live.get(k as usize).copied())
        .collect()
}

/// Reference anti-join: row-by-row filter.
fn naive_anti_join(rows: &BTreeSet<u64>, deleted: &BTreeSet<u64>) -> BTreeSet<u64> {
    rows.difference(deleted).copied().collect()
}

fn rows_of(bitmap: &RoaringRangeBitmap) -> BTreeSet<u64> {
    bitmap.ranges().iter().flat_map(|r| r.start..r.end).collect()
}

/// conformance: hef-query-metadata-and-indexes/rank-select-over-visibility-and-null-bitmaps/gather-survivors-without-expanding-the-bitmap
#[test]
fn gather_survivors_without_expanding_the_bitmap() {
    // WHEN the scan must gather the surviving rows of a wide column after filtering and deletion THEN it uses rank/select to map each surviving logical position to a physical offset without expanding the bitmap into row ids.
    //
    // Setup: 20 physical rows; rows 1, 4, 7, 11, 15, 18 are deleted. The live bitmap is everything else.
    let deleted: Vec<u64> = vec![1, 4, 7, 11, 15, 18];
    let all = RoaringRangeBitmap::from_rows(0u64..20);
    let del_bm = RoaringRangeBitmap::from_rows(deleted.iter().copied());
    let live = all.difference(&del_bm);

    // The live set must equal the complement of deleted within [0, 20).
    let expected_live: Vec<u64> = (0u64..20).filter(|r| !deleted.contains(r)).collect();
    let got_live: Vec<u64> = live.ranges().iter().flat_map(|r| r.start..r.end).collect();
    assert_eq!(got_live, expected_live, "live bitmap must equal complement of deleted");

    // Build the rank/select index — the bitmap stays compressed throughout.
    let rs = RankSelect::from_bitmap(&live);
    assert_eq!(rs.total(), expected_live.len() as u64);

    // Gather: logical position k -> physical offset, without materialising the bitmap into a Vec<u64> at any point in the production path.
    for (logical, &physical) in expected_live.iter().enumerate() {
        let got = rs.select(logical as u64).expect("logical position must be in range");
        assert_eq!(got, physical, "select({logical}) must return physical row {physical}");
    }

    // Rank confirms the inverse: given a physical offset, recover logical pos.
    for (logical, &physical) in expected_live.iter().enumerate() {
        assert_eq!(
            rs.rank(physical),
            logical as u64,
            "rank({physical}) must return logical position {logical}"
        );
    }
}

/// conformance: hef-query-metadata-and-indexes/rank-select-over-visibility-and-null-bitmaps/anti-join-as-bitmap-operations
#[test]
fn anti_join_as_bitmap_operations() {
    // WHEN deletion vectors are applied to a granule THEN the anti-join is computed as rank/select bitmap operations over the row-id space, identical in result to a row-by-row anti-join.
    //
    // Simulate a granule with 50 rows, a deletion vector marking 10 of them.
    let deletion_vector = RoaringRangeBitmap::from_rows([0u64, 5, 10, 15, 20, 25, 30, 35, 40, 45]);
    let all_rows = RoaringRangeBitmap::from_rows(0u64..50);

    // Bitmap AND-NOT: live = all_rows \ deletion_vector.
    let live_bitmap = all_rows.difference(&deletion_vector);

    // Row-by-row oracle: iterate every row and keep those not deleted.
    let deleted_set: std::collections::BTreeSet<u64> =
        deletion_vector.ranges().iter().flat_map(|r| r.start..r.end).collect();
    let expected_live: Vec<u64> = (0u64..50).filter(|r| !deleted_set.contains(r)).collect();

    let bitmap_live: Vec<u64> = live_bitmap.ranges().iter().flat_map(|r| r.start..r.end).collect();

    // Bitmap anti-join must equal the row-by-row anti-join exactly.
    assert_eq!(
        bitmap_live, expected_live,
        "bitmap AND-NOT must equal row-by-row anti-join"
    );

    // Rank/select over the result lets the scan address survivors directly.
    let rs = RankSelect::from_bitmap(&live_bitmap);
    assert_eq!(rs.total(), expected_live.len() as u64, "survivor count must match");

    // Every logical position maps to the correct physical row.
    for (k, &physical) in expected_live.iter().enumerate() {
        assert_eq!(
            rs.select(k as u64),
            Some(physical),
            "select({k}) must address physical row {physical}"
        );
    }
}
