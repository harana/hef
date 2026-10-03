//! Checks that a file whose writer stored no per-row byte-offset index still answers a single-row read of a declared
//! free-text field without decoding the whole granule's free-text block: the block's own per-value offsets carry the
//! point access, and the values match what the same rows return from a file that does carry the index.

use crate::support;
use hef::columns::FreetextDeclaration;
use hef::events::variant::VariantValue;
use hef::layout::optional_features;
use hef::layout::reader::HefFile;
use hef::writer::build::{BuiltHef, HefRow, build_hef_file};

/// A free-text body per row: distinct enough that the column does not collapse to a dictionary, and short enough that
/// the encoder picks the FSST text transform — the per-value-addressable encoding the point-access guarantee rests on.
fn body(i: u64) -> String {
    format!("note {i}: the account escalated after a threshold breach")
}

fn rows() -> Vec<HefRow> {
    (0..48)
        .map(|i| {
            let mut event = support::event(i);
            let hef::artifacts::batch::PayloadInput::Variant(VariantValue::Object(ref mut fields)) = event.payload
            else {
                unreachable!("the sample event carries a variant object payload");
            };
            fields.insert("note".to_owned(), VariantValue::String(body(i)));
            HefRow {
                epoch: 1,
                sequence: i + 1,
                event,
            }
        })
        .collect()
}

fn build(freetext_row_offset_index: bool) -> BuiltHef {
    let mut config = support::build_config();
    config.freetext = FreetextDeclaration {
        fields: vec!["note".to_owned()],
    };
    config.freetext_row_offset_index = freetext_row_offset_index;
    build_hef_file(rows(), &config).unwrap()
}

/// conformance: hef-column-design/per-row-byte-offset-index-makes-wide-typed-columns-point-accessible/free-text-written-without-the-index-still-points-accesses
#[test]
fn free_text_written_without_the_index_still_points_accesses() {
    let plain = build(false);

    // Nothing of the index is stored or declared: no duplicate copy of the free text rides the file.
    assert_eq!(
        plain.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a default build declares no per-row byte-offset index"
    );
    assert!(
        plain.footer.freetext_row_offsets.is_empty(),
        "a default build stores no per-row byte-offset index"
    );

    let file = HefFile::open(plain.bytes.clone(), None).unwrap();
    let column = plain
        .footer
        .freetext
        .iter()
        .find(|entry| entry.declared_field == "note")
        .expect("the writer gave the declared free-text field a column")
        .column_id;
    let granule = plain.footer.granules.first().expect("the file holds granules");
    let mark = file
        .mark(column, 0, granule.granule_id)
        .unwrap()
        .expect("the free-text column has a mark in every granule");
    assert!(
        mark.codec_pipeline_id.supports_byte_range_extraction().unwrap(),
        "the free-text block's encoding must be per-value addressable for the block to carry point access"
    );

    // A point read of one row must not decode the granule's whole free-text block. The reader caches every block it
    // decodes whole, so an empty decoded cache after the read is the proof that it did not.
    assert_eq!(
        file.decoded_cache_bytes(),
        0,
        "nothing is decoded before the first read"
    );
    let row = granule.first_row_ordinal + 1;
    assert_eq!(
        file.payload_path(row, "note").unwrap(),
        Some(VariantValue::String(body(row))),
        "row {row} read from the block's own per-value offsets"
    );
    assert_eq!(
        file.decoded_cache_bytes(),
        0,
        "a point read through the block's own offsets decodes no whole granule block"
    );

    // Every row reads back exactly as it does from a file that carries the index: dropping the index changes no value.
    let indexed = build(true);
    assert_ne!(
        indexed.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "the opted-in build carries the index this compares against"
    );
    let indexed_file = HefFile::open(indexed.bytes, None).unwrap();
    for ordinal in 0..48u64 {
        assert_eq!(
            file.payload_path(ordinal, "note").unwrap(),
            indexed_file.payload_path(ordinal, "note").unwrap(),
            "row {ordinal} must read identically with and without the index"
        );
    }
}
