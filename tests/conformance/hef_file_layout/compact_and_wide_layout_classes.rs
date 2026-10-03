//! Checks that the writer picks a layout sized to the file: small files use the compact layout automatically. The two
//! layouts are just variants of the one format, so either reads back identically.

use crate::support;
use hef::layout::LayoutClass;

/// conformance: hef-file-layout/compact-and-wide-layout-classes/small-file-uses-compact
#[test]
fn small_file_uses_compact() {
    // Below min_bytes_for_wide_part the writer selects layout_class = compact automatically; the class is a declared
    // variant of the one format, readable identically.
    let built = support::built_file(16);
    assert!(built.bytes.len() < 10 * 1024 * 1024);
    assert_eq!(built.header.layout_class, LayoutClass::Compact);
    hef::layout::reader::HefFile::open(built.bytes, None).unwrap();
}
