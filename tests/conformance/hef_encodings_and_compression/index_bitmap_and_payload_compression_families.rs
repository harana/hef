//! Checks the families of compression the format allows for its different parts — lookup indexes, yes/no bitmaps, and
//! event bodies — including compact storage of low-variety bitmap indexes.

use hef::indexes::Exactness;
use hef::indexes::bitmap::{BitmapIndex, RoaringRangeBitmap};

/// conformance: hef-encodings-and-compression/index-bitmap-and-payload-compression-families/bitmap-compression
#[test]
fn bitmap_compression() {
    // A low-cardinality dimension (a status with three values over 10k rows) stored as a bitmap index keeps each
    // value's row set in the Roaring-style native compressed form: sorted, disjoint row runs, not one bit per row.
    let mut index = BitmapIndex::new();
    index.insert(0, 0..4_000u64);
    index.insert(1, 4_000..9_000u64);
    index.insert(2, 9_000..10_000u64);
    assert_eq!(index.len(), 3);

    let active = index.bitmap(0).expect("value 0 indexed");
    assert_eq!(
        active.ranges().len(),
        1,
        "a dense run compresses to one range, not 4000 bits"
    );
    assert_eq!(active.count(), 4_000);
    let encoded = active.encode();
    assert!(
        encoded.len() < 4_000 / 8,
        "native compression stores runs, far below one bit per row"
    );
    assert_eq!(RoaringRangeBitmap::decode(&encoded).unwrap(), *active);

    // The compressed sets intersect directly — the AND of two conditions never expands to raw row ids — and stay
    // exact: no false positives, no false negatives.
    assert_eq!(RoaringRangeBitmap::exactness(), Exactness::Exact);
    let window = RoaringRangeBitmap::from_rows(3_500..4_500);
    let both = active.intersect(&window);
    assert_eq!(both.count(), 500);
    assert!(both.contains(3_999) && !both.contains(4_000));
}
