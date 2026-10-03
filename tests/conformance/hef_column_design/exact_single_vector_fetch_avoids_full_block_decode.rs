//! Checks that an exact single-vector fetch by row ordinal reads that row's vector via the per-row byte-offset index
//! rather than decoding the whole vector block, that the fetch returns the same bytes the whole-block decode would,
//! and that the approximate ANN retrieval path — which reads the granule's vector block whole, the same block the
//! index sits beside — is unaffected by the index's presence.
use super::file_with_stored_vectors;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::layout::footer::{EmbeddingRowOffsets, encode_footer};
use hef::layout::optional_features;
use hef::layout::reader::HefFile;
use hef::writer::build::BuiltHef;

/// Splices a per-row byte-offset index for the embedding column into a built file's footer. The index's raw bytes are
/// appended after the stripe region — never inside it — so the existing stripe checksums stay valid; row `i`'s index
/// entry points at `row_bytes[i]`, independent of whatever the granule's own vector block holds for that row.
fn with_embedding_row_index(built: &BuiltHef, row_bytes: &[Vec<u8>]) -> Vec<u8> {
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

    let mut footer = built.footer.clone();
    footer.optional_feature_flags |= optional_features::TYPED_COLUMN_ROW_OFFSETS;
    footer.embedding_row_offsets = vec![EmbeddingRowOffsets {
        bytes_len: bytes_block.len() as u64,
        bytes_offset: extra_offset + offsets_block.len() as u64 - stripe.file_offset,
        column_id: column_ids::EMBEDDING_BASE,
        granule_id: granule.granule_id,
        offsets_len: offsets_block.len() as u64,
        offsets_offset: extra_offset - stripe.file_offset,
    }];
    let new_footer_blob = encode_footer(&footer);

    let mut spliced = built.bytes[..stripe_region_end].to_vec();
    spliced.extend_from_slice(&offsets_block);
    spliced.extend_from_slice(&bytes_block);
    spliced.extend_from_slice(&new_footer_blob);
    spliced.extend_from_slice(&(new_footer_blob.len() as u64).to_le_bytes());
    spliced.extend_from_slice(b"HEF1");
    spliced
}

/// conformance:
/// hef-column-design/per-row-byte-offset-index-makes-wide-typed-columns-point-accessible/single-vector-fetched-by-row-ordinal
#[test]
fn single_vector_fetched_by_row_ordinal_via_index() {
    let stored_vectors: Vec<String> = (0..6).map(|i| format!("stored-vector-{i}")).collect();
    let built = file_with_stored_vectors(&stored_vectors);
    let mirrored = with_embedding_row_index(
        &built,
        &stored_vectors
            .iter()
            .map(|v| v.clone().into_bytes())
            .collect::<Vec<_>>(),
    );
    let file = HefFile::open(mirrored, None).unwrap();
    assert_ne!(
        file.usable_optional_features() & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "the spliced file must carry the per-row offset index"
    );

    // Fetching a candidate's stored vector by row ordinal (late-materialized re-ranking) resolves the row's offset
    // entry and then its vector bytes directly, for every row.
    for (i, expected) in stored_vectors.iter().enumerate() {
        assert_eq!(
            file.embedding_value_for_row(column_ids::EMBEDDING_BASE, i as u64)
                .unwrap(),
            Some(expected.clone().into_bytes()),
            "row {i} exact fetch by ordinal"
        );
    }
}

/// conformance:
/// hef-column-design/internal-embedding-vector-columns-isolated-from-public-output/exact-single-vector-fetch-avoids-full-block-decode
#[test]
fn exact_single_vector_fetch_avoids_full_block_decode() {
    let stored_vectors: Vec<String> = (0..6).map(|i| format!("stored-vector-{i}")).collect();
    let built = file_with_stored_vectors(&stored_vectors);
    let granule_id = built.footer.granules[0].granule_id;

    // The whole-block decode of the granule's own vector block: the same read an approximate ANN index build would
    // perform to scan candidate vectors. Read before any index is spliced in, so it exercises the plain file.
    let plain = HefFile::open(built.bytes.clone(), None).unwrap();
    let whole_block = plain.read_column(column_ids::EMBEDDING_BASE, granule_id).unwrap();
    let ColumnData::Strings(whole_block_strings) = &whole_block.data else {
        panic!("embedding column decodes as strings");
    };
    let whole_block_values: Vec<Vec<u8>> = whole_block_strings
        .iter()
        .map(|v| v.expect("every row carries a vector").as_bytes().to_vec())
        .collect();
    assert_eq!(
        whole_block_values,
        stored_vectors
            .iter()
            .map(|vector| vector.as_bytes().to_vec())
            .collect::<Vec<_>>(),
        "whole-block decode returns the vectors the writer stored"
    );

    // An index that mirrors the granule's own vector block: fetching row i via the index must return the same bytes
    // the whole-block decode yielded for row i.
    let mirrored = with_embedding_row_index(&built, &whole_block_values);
    let file = HefFile::open(mirrored, None).unwrap();
    for (i, expected) in whole_block_values.iter().enumerate() {
        let via_index = file
            .embedding_value_for_row(column_ids::EMBEDDING_BASE, i as u64)
            .unwrap();
        assert_eq!(
            via_index.as_ref(),
            Some(expected),
            "row {i} exact fetch must match the whole-block decode"
        );
    }

    // The approximate ANN path is unaffected: the granule's own vector block still decodes exactly as it did before
    // the index existed — the index adds the exact per-row path beside the block ANN retrieval would scan, it does not
    // disturb the block itself.
    let after = file.read_column(column_ids::EMBEDDING_BASE, granule_id).unwrap();
    assert_eq!(
        after.data, whole_block.data,
        "the vector block an approximate ANN scan would read is unchanged by the index"
    );

    // An index entry that disagrees with the block proves the exact fetch reads via the index rather than falling
    // through to a whole-block decode: had it decoded the whole block, it would return the block's own value for row 2
    // instead of the index's.
    let mut divergent = whole_block_values.clone();
    divergent[2] = b"exact-fetch-only-row-2".to_vec();
    let spliced = with_embedding_row_index(&built, &divergent);
    let file = HefFile::open(spliced, None).unwrap();
    assert_eq!(
        file.embedding_value_for_row(column_ids::EMBEDDING_BASE, 2).unwrap(),
        Some(b"exact-fetch-only-row-2".to_vec()),
        "row 2 exact fetch must read the index's bytes, not decode the granule's whole vector block"
    );
}
