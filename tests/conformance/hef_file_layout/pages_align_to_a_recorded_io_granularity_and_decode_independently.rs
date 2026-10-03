//! Checks that column block boundaries are aligned to the IO granularity recorded in the footer, and that each block decodes independently using only its own bytes — no neighbouring blocks required.

use crate::support;
use hef::columns::column_ids;
use hef::encoding::decode_block;
use hef::file::bytes::Reader;
use hef::layout::reader::HefFile;
use hef::layout::{decode_presence_frame, optional_features, required_features};
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

    // Pruning leaves three non-adjacent surviving pages: pick the first, middle, and last SEQUENCE blocks. Every row
    // here shares one epoch, so the epoch column's blocks are elided (zero stored bytes); sequence numbers differ per
    // row, so every SEQUENCE block stores real bytes to fetch.
    let sequence_marks: Vec<_> = built
        .footer
        .marks
        .iter()
        .filter(|m| m.column_id == column_ids::SEQUENCE)
        .collect();
    assert!(
        sequence_marks.len() >= 3,
        "need at least 3 granules for non-adjacent pages"
    );

    let stride = (sequence_marks.len() - 1) / 2;
    let survivors = [
        sequence_marks[0],
        sequence_marks[stride],
        sequence_marks[sequence_marks.len() - 1],
    ];

    // The presence frame's layout depends on whether the file declares compressed presence streams.
    let compressed_presence = built.footer.required_feature_flags & required_features::COMPRESSED_PRESENCE != 0;

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

        // Strip the presence frame, then decode the block body independently — no neighbouring page bytes required.
        let mut frame = Reader::new(page_bytes);
        decode_presence_frame(&mut frame, mark.row_count, compressed_presence).unwrap();
        let body = frame.take(frame.remaining(), "block body").unwrap();
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
