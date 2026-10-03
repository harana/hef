//! Conformance tests for the `hef-column-design` capability.

use crate::support;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::file::integrity::hash_tree;
use hef::layout::footer::{ColumnKind, Footer, IntegrityGapEntry};
use hef::writer::build::{AnalyticalColumn, BuiltHef, HefRow, build_hef_file};

mod context_projection_columns_avoid_raw_payload_scans;
mod exact_single_vector_fetch_avoids_full_block_decode;
mod free_text_point_lookup_uses_row_offset_index;
mod free_text_shredded_by_schema_declaration;
mod free_text_without_the_index_points_accesses_through_the_block;
mod internal_embedding_vector_columns_isolated_from_public_output;
mod payload_arena_stores_canonical_variant_values_with_statistics_driven_shredding;
mod promotion_backfill_via_manifest_native_vertical_projection;
mod reader_without_index_falls_back_identically;
mod required_physical_columns;
mod schema_version_keyed_presence_map_for_promoted_columns;
mod strongly_typed_promoted_columns;

pub fn file_with_stored_vectors(vectors: &[String]) -> BuiltHef {
    let rows: Vec<HefRow> = (0..vectors.len() as u64)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let mut config = support::build_config();
    config.analytical_columns = vec![AnalyticalColumn {
        column_id: column_ids::EMBEDDING_BASE,
        data: ColumnData::Strings(vectors.iter().cloned().map(Some).collect()),
        internal_only: true,
        kind: ColumnKind::String,
        name: "embedding_vec".to_owned(),
        substring_searchable: false,
    }];
    let built = build_hef_file(rows, &config).unwrap();
    assert_eq!(built.footer.granules.len(), 1, "test needs every row in one granule");
    built
}

/// Declares the bytes a test appended after the last stripe as the data area's trailing integrity gap, so the footer's
/// stripes and gaps still cover every byte up to the footer exactly once. `bytes_before_footer` is the spliced file up
/// to where its new footer will start.
pub fn commit_trailing_gap(footer: &mut Footer, bytes_before_footer: &[u8]) {
    let tail_start = footer
        .stripes
        .iter()
        .map(|stripe| stripe.file_offset + stripe.byte_len)
        .max()
        .expect("a built file has stripes");
    footer.integrity_gaps.retain(|gap| gap.file_offset < tail_start);
    footer.integrity_gaps.push(IntegrityGapEntry {
        blake3: *hash_tree(&bytes_before_footer[tail_start as usize..]).as_bytes(),
        byte_len: bytes_before_footer.len() as u64 - tail_start,
        file_offset: tail_start,
    });
}
