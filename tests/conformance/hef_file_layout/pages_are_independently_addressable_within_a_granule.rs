//! Checks that individual pages within a granule's column block are independently addressable by their compressed byte
//! offset, so a reader can fetch one page without reading the rest of the block.

use crate::support;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::layout::optional_features;
use hef::layout::reader::HefFile;

/// conformance:
/// hef-file-layout/pages-are-independently-addressable-within-a-granule/read-one-page-without-the-rest-of-the-granule
#[test]
fn read_one_page_without_the_rest_of_the_granule() {
    let built = support::built_file(48);
    let file = HefFile::open(built.bytes, None).unwrap();

    assert!(
        file.footer().optional_feature_flags & optional_features::PER_PAGE_MARKS != 0,
        "writer must declare PER_PAGE_MARKS"
    );

    // Pick the last granule's SEQUENCE column.
    let granule = file.footer().granules.last().copied().unwrap();

    // Reading via the per-page directory must give the same values as reading the whole granule block.
    let via_page = file.read_page(column_ids::SEQUENCE, granule.granule_id, 0).unwrap();
    let via_block = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();

    let ColumnData::U64(page_values) = via_page.data else {
        panic!("expected u64 column")
    };
    let ColumnData::U64(block_values) = via_block.data else {
        panic!("expected u64 column")
    };

    assert_eq!(
        page_values, block_values,
        "page-level read must match the full granule-block read (equivalence oracle)"
    );
    assert_eq!(page_values.len(), granule.row_count as usize);
    assert_eq!(page_values[0], granule.first_sequence);

    // The page directory entry must point to exactly the same byte range as the column mark (single-page granule: page
    // bytes == block bytes).
    let page_entry = file
        .footer()
        .page_directory
        .iter()
        .find(|e| e.column_id == column_ids::SEQUENCE && e.granule_id == granule.granule_id && e.page_index == 0)
        .expect("page directory entry must exist");
    let mark = file
        .footer()
        .marks
        .iter()
        .find(|m| m.column_id == column_ids::SEQUENCE && m.granule_id == granule.granule_id)
        .expect("column mark must exist");

    assert_eq!(page_entry.compressed_offset, mark.compressed_offset);
    assert_eq!(page_entry.compressed_len, mark.compressed_size);
    assert_eq!(page_entry.row_count, granule.row_count);
    assert_eq!(page_entry.first_row_ordinal, granule.first_row_ordinal);
}

/// conformance:
/// hef-file-layout/pages-are-independently-addressable-within-a-granule/older-reader-falls-back-to-granule-granularity
#[test]
fn older_reader_falls_back_to_granule_granularity() {
    // A reader that reads at granule granularity (read_column) must return results identical to the per-page path on a
    // file carrying PER_PAGE_MARKS.
    let built = support::built_file(32);
    let file = HefFile::open(built.bytes, None).unwrap();

    assert!(
        file.footer().optional_feature_flags & optional_features::PER_PAGE_MARKS != 0,
        "file must carry PER_PAGE_MARKS for this test to be meaningful"
    );

    for granule in file.footer().granules.clone() {
        // Granule-level read (what an older reader would do).
        let granule_read = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        // Page-level read using the directory.
        let page_read = file.read_page(column_ids::SEQUENCE, granule.granule_id, 0).unwrap();

        let ColumnData::U64(granule_values) = granule_read.data else {
            panic!("expected u64 column")
        };
        let ColumnData::U64(page_values) = page_read.data else {
            panic!("expected u64 column")
        };
        assert_eq!(
            granule_values, page_values,
            "granule-level read must match page-level read (backward-compatibility equivalence)"
        );
    }
}
