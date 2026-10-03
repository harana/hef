//! Checks that compressed text columns can still be read a chunk at a time. Decoding one chunk of a compressed string
//! column reads only that chunk's pages, without having to touch any other chunk.
use crate::support;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::layout::reader::HefFile;

/// conformance:
/// hef-encodings-and-compression/strings-preserve-random-access-decode/decode-one-granule-of-an-fsst-column
#[test]
fn decode_one_granule_of_an_fsst_column() {
    // One granule of a high-cardinality string column decodes through its own mark without touching any other granule's
    // pages.
    let built = support::built_file(48);
    let file = HefFile::open(built.bytes, None).unwrap();
    assert!(file.footer().granules.len() > 1);
    let target = file.footer().granules[1];
    let read = file.read_column(column_ids::ENTITY_ID, target.granule_id).unwrap();
    let ColumnData::Strings(values) = read.data else {
        panic!("string column")
    };
    assert_eq!(values.len(), target.row_count as usize);
    let first_row = target.first_row_ordinal;
    assert_eq!(values.get(0).flatten(), Some(format!("opp-{first_row}").as_str()));
}
