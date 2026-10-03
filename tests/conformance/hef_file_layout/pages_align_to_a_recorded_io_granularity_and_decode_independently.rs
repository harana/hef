//! Checks that column block boundaries are aligned to the IO granularity recorded in the footer, and that each block decodes independently using only its own bytes — no neighbouring blocks required.

use crate::support;
use hef::encoding::decode_block;
use hef::layout::optional_features;
use hef::layout::reader::HefFile;
use hef::writer::build::{HefBuildConfig, HefRow, build_hef_file};

/// conformance: hef-file-layout/pages-align-to-a-recorded-io-granularity-and-decode-independently/aligned-fetch-of-surviving-pages
#[test]
fn aligned_fetch_of_surviving_pages() {
    const ALIGNMENT: u32 = 4096;

    // Build a file with 4 KiB IO alignment and enough rows for at least six granules (index_granularity = 16), giving non-adjacent survivors.
    let config = HefBuildConfig {
        io_alignment_bytes: ALIGNMENT,
        ..support::build_config()
    };
    let rows: Vec<HefRow> = (0..96)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let built = build_hef_file(rows, &config).unwrap();

    // The optional feature flag must be declared so a reader knows the alignment is in effect.
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::PAGE_IO_ALIGNMENT,
        0,
        "PAGE_IO_ALIGNMENT must be declared in optional_feature_flags",
    );

    // The alignment granularity must be recorded in the footer.
    assert_eq!(built.footer.io_alignment_bytes, ALIGNMENT);

    // Every column block must start at an offset that is a multiple of the alignment so a reader can fetch it with an aligned direct read.
    for mark in &built.footer.marks {
        assert_eq!(
            mark.compressed_offset % u64::from(ALIGNMENT),
            0,
            "column {} granule {} offset {} is not aligned to {} bytes",
            mark.column_id,
            mark.granule_id,
            mark.compressed_offset,
            ALIGNMENT,
        );
    }

    // Pruning leaves three non-adjacent surviving pages: pick the first, middle, and last column blocks for column 0.
    let col0_marks: Vec<_> = built.footer.marks.iter().filter(|m| m.column_id == 0).collect();
    assert!(col0_marks.len() >= 3, "need at least 3 granules for non-adjacent pages");

    let stride = (col0_marks.len() - 1) / 2;
    let survivors = [col0_marks[0], col0_marks[stride], col0_marks[col0_marks.len() - 1]];

    // Reference reader over the full file — used only to verify the result, not to supply bytes for decoding.
    let reader = HefFile::open(built.bytes.clone(), None).unwrap();

    for mark in survivors {
        // Fetch only this page's byte range — nothing before or after it. Mark offsets are stripe-relative, so resolve
        // the file position as the block's stripe base (`StripeEntry.file_offset`) plus the relative offset.
        let stripe_id = built
            .footer
            .granules
            .iter()
            .find(|g| g.granule_id == mark.granule_id)
            .expect("granule for mark")
            .stripe_id;
        let stripe_base = built
            .footer
            .stripes
            .iter()
            .find(|s| s.stripe_id == stripe_id)
            .expect("stripe for granule")
            .file_offset as usize;
        let start = stripe_base + mark.compressed_offset as usize;
        let end = start + mark.compressed_size as usize;
        let page_bytes = &built.bytes[start..end];

        // Parse the 4-byte presence prefix, then decode the block body independently — no neighbouring page bytes required.
        let presence_len = u32::from_le_bytes(page_bytes[..4].try_into().unwrap()) as usize;
        let body = &page_bytes[4 + presence_len..];
        let isolated = decode_block(mark.codec_pipeline_id, body).unwrap();

        // The isolated decode must produce data identical to a full column read through the reader, proving independent decodability.
        let full = reader.read_column(mark.column_id, mark.granule_id).unwrap();
        assert_eq!(
            isolated, full.data,
            "independent page decode must match full read for column {} granule {}",
            mark.column_id, mark.granule_id,
        );
    }
}
