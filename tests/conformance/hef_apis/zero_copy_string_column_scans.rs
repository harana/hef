//! Checks the reader's zero-copy whole-column string read: it yields exactly the rows, nulls, and presence the
//! materializing read decodes, backed by shared buffers instead of per-value allocations, and declines blocks it does
//! not cover so callers fall back identically.

use crate::support;
use arrow_array::Array;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::layout::reader::HefFile;

/// conformance: hef-apis/zero-copy-string-column-scans/view-read-matches-the-materializing-read
#[test]
fn view_read_matches_the_materializing_read() {
    let built = support::built_file(48);
    let shredded_string = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "kind")
        .expect("the kind path is shredded")
        .column_id;
    let file = HefFile::open(built.bytes, None).unwrap();

    for granule in &file.footer().granules {
        // A sparse shredded string column and the dense entity-id column both agree with the materializing read.
        for column_id in [shredded_string, column_ids::ENTITY_ID] {
            let materialized = file.read_column(column_id, granule.granule_id).unwrap();
            let (presence, views) = file
                .read_column_string_views(column_id, granule.granule_id)
                .unwrap()
                .expect("a string column has a view read");
            assert_eq!(presence, materialized.presence);
            let ColumnData::Strings(values) = &materialized.data else {
                panic!("string columns decode as strings");
            };
            assert_eq!(views.len(), values.len());
            for (index, value) in values.iter().enumerate() {
                match value {
                    Some(text) => assert_eq!(views.value(index), text, "row {index}"),
                    None => assert!(views.is_null(index), "row {index} must be null"),
                }
            }
        }
    }
}

/// conformance: hef-apis/zero-copy-string-column-scans/uncovered-blocks-fall-back-identically
#[test]
fn uncovered_blocks_fall_back_identically() {
    let built = support::built_file(32);
    let file = HefFile::open(built.bytes, None).unwrap();
    for granule in &file.footer().granules {
        // A numeric column has no view read; the caller falls back to the materializing read, which still decodes.
        assert!(
            file.read_column_string_views(column_ids::SEQUENCE, granule.granule_id)
                .unwrap()
                .is_none()
        );
        let read = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        assert!(read.data.row_count() > 0);
    }
}
