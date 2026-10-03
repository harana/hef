//! Conformance tests for the `hef-encodings-and-compression` capability.

use std::path::{Path, PathBuf};

mod adaptive_per_block_encoding_selection;
mod bit_packed_integer_streams_use_the_fastlanes_transposed_layout;
mod compressed_data_numeric_predicates;
mod compressed_data_string_predicates;
mod index_bitmap_and_payload_compression_families;
mod lifecycle_selected_cascade_strategies;
mod mandatory_representations_for_money_and_floats;
mod monotonic_columns_use_fastlanes_style_candidates;
mod portable_decoder_reference_for_optional_encoding_blocks;
mod qpl_deflate_as_an_iaa_accelerated_software_parity_compression_family;
mod recursive_cascade_selection;
mod self_describing_per_page_encoding_descriptor;
mod shredded_blocks_use_the_adaptive_encoder;
mod strings_preserve_random_access_decode;

/// Returns the repository root, which is the crate manifest directory.
pub(super) fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}
