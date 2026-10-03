//! Checks the bitmap acceleration contract: a planner uses a bitmap index only when its encoding can be intersected
//! directly in compressed form, the file declares the bitmap feature, and the block's checksum verifies — and the
//! intersection it produces equals the true set intersection.

use hef::indexes::Exactness;
use hef::indexes::bitmap::*;

/// conformance:
/// hef-query-metadata-and-indexes/exact-directly-intersectable-bitmap-indexes/non-intersectable-bitmap-rejected
#[test]
fn non_intersectable_bitmap_rejected() {
    // A bitmap encoding that cannot be intersected directly in compressed form -> can_use_bitmap(..) is false (planner
    // does not use the bitmap path).
    let not_intersectable = BitmapBlockInfo {
        checksum_verified: true,
        directly_intersectable: false,
        feature_declared: true,
    };
    assert!(!can_use_bitmap(&not_intersectable));

    // The same is true when the feature is undeclared or the checksum fails, even if the encoding itself is
    // intersectable.
    assert!(!can_use_bitmap(&BitmapBlockInfo {
        checksum_verified: true,
        directly_intersectable: true,
        feature_declared: false,
    }));
    assert!(!can_use_bitmap(&BitmapBlockInfo {
        checksum_verified: false,
        directly_intersectable: true,
        feature_declared: true,
    }));

    // Positive control: a directly-intersectable, declared, checksum-verified block -> can_use_bitmap(..) is true, and
    // intersect() equals the true set intersection.
    let mut index = BitmapIndex::new();
    index.insert(100, [0, 1, 2, 3, 4]); // e.g. status = active
    index.insert(200, [2, 3, 4, 5, 6]); // e.g. country = US

    let active = index.bitmap(100).unwrap();
    let us = index.bitmap(200).unwrap();
    let both = active.intersect(us);
    // True set intersection of {0,1,2,3,4} and {2,3,4,5,6} is {2,3,4}.
    let rows: Vec<u64> = both.ranges().iter().flat_map(|r| r.start..r.end).collect();
    assert_eq!(rows, vec![2, 3, 4]);
    // intersect_values agrees with the direct intersect of the two bitmaps.
    assert_eq!(index.intersect_values(&[100, 200]), both);
    assert_eq!(RoaringRangeBitmap::exactness(), Exactness::Exact);

    // The block really verifies against its BLAKE3, so the planner may use it.
    let block = active.encode();
    let checksum = bitmap_block_checksum(&block);
    let info = verify_bitmap_block(&block, &checksum, true, true);
    assert!(can_use_bitmap(&info));
}
