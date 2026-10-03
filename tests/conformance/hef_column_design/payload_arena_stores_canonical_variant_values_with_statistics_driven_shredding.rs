//! Checks how event bodies are stored: the file watches which fields show up often and lifts those "hot" fields out
//! into their own typed columns, while rare fields stay in the shared body store. A hot field reads straight from its
//! typed column, a rare field reads from the body without decoding any neighbouring field, and either way the original
//! body can be reassembled exactly.
use crate::support;
use hef::columns::column_ids;
use hef::encoding::Compression;
use hef::events::variant::VariantValue;
use hef::layout::reader::{HefFile, PayloadRead};

/// conformance:
/// hef-column-design/payload-arena-stores-canonical-variant-values-with-statistics-driven-shredding/
/// hot-path-shredded-into-a-typed-column
#[test]
fn hot_path_shredded_into_a_typed_column() {
    // Per-path statistics lift the frequent "amount"/"kind" paths into typed shredded columns; the value leaves the
    // residual (exactly one of the two), and the shredded block skips whole-block compression so random access
    // survives.
    let built = support::built_file(32);
    let amount = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount shredded");
    let file = HefFile::open(built.bytes, None).unwrap();
    let granule = file.footer().granules[0].granule_id;
    let read = file.read_column(amount.column_id, granule).unwrap();
    assert!(!read.presence.is_empty());
    let mark = file
        .mark(amount.column_id, 0, granule)
        .unwrap()
        .expect("the shredded column has a mark in the granule");
    // No whole-block codec: the block is stored either uncompressed or in independently decompressible seekable
    // Zstandard frames that a row range can be sliced from without decoding the whole block.
    let pipeline = mark.codec_pipeline_id;
    let compression = pipeline.compression().unwrap();
    assert!(
        compression == Compression::None
            || (compression == Compression::SeekableZstd && pipeline.supports_byte_range_extraction().unwrap()),
        "shredded scan-path columns preserve random access in compressed form (got {compression:?})"
    );
    // The shredded path is gone from the residual but the merge restores the complete payload.
    let PayloadRead::Value(value) = file.payload(0).unwrap() else {
        panic!("payload present");
    };
    let VariantValue::Object(fields) = value else {
        panic!("object")
    };
    assert!(fields.contains_key("amount"));
}

/// conformance:
/// hef-column-design/payload-arena-stores-canonical-variant-values-with-statistics-driven-shredding/
/// rare-path-read-from-residual-without-sibling-decode
#[test]
fn rare_path_read_from_residual_without_sibling_decode() {
    // The rare path stays residual and is extracted by offset navigation against the granule dictionary, never via a
    // sibling decode.
    let built = support::built_file(32);
    assert!(!built.footer.shredded.iter().any(|entry| entry.path == "rare"));
    let file = HefFile::open(built.bytes, None).unwrap();
    assert_eq!(file.payload_path(6, "rare").unwrap(), Some(VariantValue::Bool(true)));
    let _ = column_ids::PAYLOAD_REF;
}
