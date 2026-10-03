//! Conformance tests for the `hef-file-layout` capability.

use std::path::{Path, PathBuf};

mod closed_set_of_file_shapes_open_producers_and_queries;
mod compact_and_wide_layout_classes;
mod feature_directory_governs_capabilities;
mod fixed_aligned_file_header;
mod granule_directory_and_authoritative_marks;
mod layout_projections_are_read_alternatives;
mod pages_align_to_a_recorded_io_granularity_and_decode_independently;
mod pages_are_independently_addressable_within_a_granule;
mod stripe_granule_page_and_mini_block_model;

/// Returns the repository root, which is the crate manifest directory.
pub(super) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Returns the text of the hef-file-layout spec.
pub(super) fn spec_text() -> String {
    let path = repo_root()
        .join("openspec")
        .join("specs")
        .join("hef-file-layout")
        .join("spec.md");
    std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("openspec/specs/hef-file-layout/spec.md must exist"))
}
