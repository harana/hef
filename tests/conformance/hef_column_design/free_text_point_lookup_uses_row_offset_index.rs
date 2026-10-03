//! Checks that a single-row read of a declared free-text field resolves the row's byte range from the per-row
//! byte-offset index instead of decoding the whole granule's free-text block, and that the value it returns matches
//! what the whole-block decode path yields.

use crate::support;
use hef::columns::FreetextDeclaration;
use hef::events::variant::VariantValue;
use hef::layout::optional_features;
use hef::layout::reader::{HefFile, PayloadRead};

/// conformance: hef-column-design/per-row-byte-offset-index-makes-wide-typed-columns-point-accessible/free-text-point-lookup-skips-the-whole-granule-decode
#[test]
fn free_text_point_lookup_skips_the_whole_granule_decode() {
    let mut config = support::build_config();
    config.freetext = FreetextDeclaration {
        fields: vec!["note".to_owned()],
    };
    // The index is opt-in; this scenario is about the file that carries one.
    config.freetext_row_offset_index = true;
    let rows: Vec<hef::writer::build::HefRow> = (0..24)
        .map(|i| hef::writer::build::HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let built = hef::writer::build::build_hef_file(rows, &config).unwrap();

    // The writer declares the per-row byte-offset index for the declared free-text columns of a build that asked
    // for one.
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a build that opted into the per-row offset index must carry it"
    );
    assert!(!built.footer.freetext_row_offsets.is_empty());

    let file = HefFile::open(built.bytes, None).unwrap();

    // A single-row point lookup (via payload reconstruction) must return exactly the value the row was built with —
    // the same value a whole-granule free-text block decode would yield — for every row, whether reached through the
    // per-row index or not.
    for i in 0..24u64 {
        let PayloadRead::Value(VariantValue::Object(fields)) = file.payload(i).unwrap() else {
            panic!("payload present for row {i}");
        };
        assert_eq!(
            fields.get("note"),
            Some(&VariantValue::String(format!("free text body number {i}"))),
            "row {i} free-text point lookup"
        );
    }
}
