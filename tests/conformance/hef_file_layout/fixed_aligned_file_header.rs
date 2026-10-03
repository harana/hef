//! Checks that a small fixed-size header at the front of each file is enough to rule the file out fast. From the header
//! alone — before reading the rest of the file — a reader can reject a file whose time or sequence coverage cannot
//! overlap the query.

use crate::support;
use hef::layout::{HEADER_BLOCK_LEN, decode_header};

/// conformance: hef-file-layout/fixed-aligned-file-header/quick-rejection-from-header
#[test]
fn quick_rejection_from_header() {
    // The fixed aligned header alone rejects files whose time/sequence coverage cannot intersect the query, before the
    // footer is read.
    let built = support::built_file(8);
    let header_block = &built.bytes[..HEADER_BLOCK_LEN];
    let header = decode_header(header_block).unwrap();
    assert!(header.may_contain_sequence(1, 1, 4));
    assert!(!header.may_contain_sequence(2, 1, 4), "wrong epoch rejected");
    assert!(!header.may_contain_sequence(1, 100, 200), "outside range rejected");
    assert!(!header.may_contain_occurred(i64::MIN, 0), "before coverage rejected");
}
