//! Checks that a reader which does not declare `typed_column_row_offsets` ignores a per-row byte-offset index a file
//! carries, decodes the whole granule block instead, and returns byte-identical values — for both a declared free-text
//! column and an internal embedding/vector column.

use super::file_with_stored_vectors;
use crate::support;
use hef::columns::{FreetextDeclaration, column_ids};
use hef::events::variant::VariantValue;
use hef::layout::footer::{EmbeddingRowOffsets, Footer, encode_footer};
use hef::layout::optional_features;
use hef::layout::reader::{HefFile, PayloadRead};
use hef::writer::build::{BuiltHef, HefRow, build_hef_file};

/// Re-encodes `built`'s footer with `TYPED_COLUMN_ROW_OFFSETS` cleared, leaving every stripe byte (including the
/// index's own data, already written inside the stripe by the writer) untouched — the file still physically carries
/// the index, but no longer declares it, exactly as a reader without the feature would see it.
fn without_declared_row_offsets(built: &BuiltHef) -> Vec<u8> {
    let old_footer_len = u64::from_le_bytes(
        built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let stripe_region_end = built.bytes.len() - (old_footer_len + 12);

    let mut footer = built.footer.clone();
    footer.optional_feature_flags &= !optional_features::TYPED_COLUMN_ROW_OFFSETS;
    let new_footer_blob = encode_footer(&footer);

    let mut spliced = built.bytes[..stripe_region_end].to_vec();
    spliced.extend_from_slice(&new_footer_blob);
    spliced.extend_from_slice(&(new_footer_blob.len() as u64).to_le_bytes());
    spliced.extend_from_slice(b"HEF1");
    spliced
}

/// conformance:
/// hef-column-design/per-row-byte-offset-index-makes-wide-typed-columns-point-accessible/reader-without-the-feature-falls-back-identically-freetext
#[test]
fn freetext_reader_without_index_falls_back_to_whole_granule_decode() {
    let mut config = support::build_config();
    config.freetext = FreetextDeclaration {
        fields: vec!["note".to_owned()],
    };
    // The index is opt-in; this scenario is about a reader meeting a file that carries one.
    config.freetext_row_offset_index = true;
    let rows: Vec<HefRow> = (0..24)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let built = build_hef_file(rows, &config).unwrap();
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a file with declared free-text fields must carry the per-row offset index"
    );
    assert!(!built.footer.freetext_row_offsets.is_empty());

    let with_index = HefFile::open(built.bytes.clone(), None).unwrap();
    assert_ne!(
        with_index.usable_optional_features() & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0
    );

    let fallback_bytes = without_declared_row_offsets(&built);
    let without_index = HefFile::open(fallback_bytes, None).unwrap();
    assert_eq!(
        without_index.usable_optional_features() & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a reader without the feature must not treat the index as usable"
    );

    for i in 0..24u64 {
        let PayloadRead::Value(VariantValue::Object(with_fields)) = with_index.payload(i).unwrap() else {
            panic!("payload present for row {i}");
        };
        let PayloadRead::Value(VariantValue::Object(without_fields)) = without_index.payload(i).unwrap() else {
            panic!("payload present for row {i}");
        };
        assert_eq!(
            with_fields.get("note"),
            without_fields.get("note"),
            "row {i} free-text value must be byte-identical whether or not the reader uses the index"
        );
    }
}

/// Splices a per-row byte-offset index for the embedding column into a built file's footer, once declaring
/// `TYPED_COLUMN_ROW_OFFSETS` and once without — same index bytes, same stripe region, so the only difference between
/// the two files is whether the feature is declared usable.
fn with_and_without_embedding_index(built: &BuiltHef, row_bytes: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>) {
    let granule = built.footer.granules[0];
    let stripe = built
        .footer
        .stripes
        .iter()
        .find(|stripe| stripe.stripe_id == granule.stripe_id)
        .expect("granule's stripe is in the stripe directory");

    let mut offsets_block = Vec::with_capacity(row_bytes.len() * 8);
    let mut bytes_block = Vec::new();
    for row in row_bytes {
        offsets_block.extend_from_slice(&(bytes_block.len() as u32).to_le_bytes());
        offsets_block.extend_from_slice(&(row.len() as u32).to_le_bytes());
        bytes_block.extend_from_slice(row);
    }

    let old_footer_len = u64::from_le_bytes(
        built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let stripe_region_end = built.bytes.len() - (old_footer_len + 12);
    let extra_offset = stripe_region_end as u64;

    let mut with_flag = built.footer.clone();
    with_flag.optional_feature_flags |= optional_features::TYPED_COLUMN_ROW_OFFSETS;
    with_flag.embedding_row_offsets = vec![EmbeddingRowOffsets {
        bytes_len: bytes_block.len() as u64,
        bytes_offset: extra_offset + offsets_block.len() as u64 - stripe.file_offset,
        column_id: column_ids::EMBEDDING_BASE,
        granule_id: granule.granule_id,
        offsets_len: offsets_block.len() as u64,
        offsets_offset: extra_offset - stripe.file_offset,
    }];
    let mut without_flag = with_flag.clone();
    without_flag.optional_feature_flags &= !optional_features::TYPED_COLUMN_ROW_OFFSETS;

    let splice = |footer: &Footer| {
        let footer_blob = encode_footer(footer);
        let mut spliced = built.bytes[..stripe_region_end].to_vec();
        spliced.extend_from_slice(&offsets_block);
        spliced.extend_from_slice(&bytes_block);
        spliced.extend_from_slice(&footer_blob);
        spliced.extend_from_slice(&(footer_blob.len() as u64).to_le_bytes());
        spliced.extend_from_slice(b"HEF1");
        spliced
    };
    (splice(&with_flag), splice(&without_flag))
}

/// conformance:
/// hef-column-design/per-row-byte-offset-index-makes-wide-typed-columns-point-accessible/reader-without-the-feature-falls-back-identically-vector
#[test]
fn vector_reader_without_index_falls_back_to_whole_block_decode() {
    let stored_vectors: Vec<String> = (0..6).map(|i| format!("stored-vector-{i}")).collect();
    let built = file_with_stored_vectors(&stored_vectors);
    let row_bytes: Vec<Vec<u8>> = stored_vectors.iter().map(|v| v.clone().into_bytes()).collect();
    let (with_index_bytes, without_index_bytes) = with_and_without_embedding_index(&built, &row_bytes);

    let with_index = HefFile::open(with_index_bytes, None).unwrap();
    assert_ne!(
        with_index.usable_optional_features() & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "the spliced file must carry the per-row offset index"
    );
    let without_index = HefFile::open(without_index_bytes, None).unwrap();
    assert_eq!(
        without_index.usable_optional_features() & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a reader without the feature must not treat the index as usable"
    );

    for (i, expected) in stored_vectors.iter().enumerate() {
        let via_index = with_index
            .embedding_value_for_row(column_ids::EMBEDDING_BASE, i as u64)
            .unwrap();
        let via_fallback = without_index
            .embedding_value_for_row(column_ids::EMBEDDING_BASE, i as u64)
            .unwrap();
        assert_eq!(
            via_index,
            Some(expected.clone().into_bytes()),
            "row {i} exact fetch by index"
        );
        assert_eq!(
            via_fallback, via_index,
            "row {i} vector value must be byte-identical whether or not the reader uses the index"
        );
    }
}
