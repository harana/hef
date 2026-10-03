//! Checks that the encoder can encode an encoding's own side streams — turning one pipeline level into a bounded,
//! recorded cascade — and that a deeper level is kept only when measuring it on the data proves it shrinks the stream.
//! The recorded cascade decodes deterministically from its description alone, and the plain software decode stays the
//! byte-for-byte reference at every level.

use hef::encoding::decompressor::{Decompressor, SoftwareDecompressor, active_decompressor};
use hef::encoding::{
    CascadeStrategy, ColumnData, MAX_CASCADE_DEPTH, SideStream, Transform, cascade_depth, cascade_level_fits,
    decode_block, decode_block_range, encode_block, encode_block_with_strategy,
};

/// An f64 metric column that is mostly ALP-encodable (exact halves) with an exact-bits exception every `stride` rows
/// (multiples of pi never survive decimal scaling, so each becomes an entry in ALP's exception side stream).
fn alp_column_with_exceptions(rows: usize, stride: usize) -> ColumnData {
    ColumnData::F64(
        (0..rows)
            .map(|i| {
                if i % stride == 0 {
                    std::f64::consts::PI * (i + 1) as f64
                } else {
                    i as f64 * 0.5
                }
            })
            .collect(),
    )
}

/// conformance: hef-encodings-and-compression/recursive-cascade-selection/cascade-an-encoding-s-exception-stream
#[test]
fn cascade_an_encoding_s_exception_stream() {
    // 2000 exceptions spread across 20000 rows: sampling shows the exception index stream shrinks by more than half
    // under a frame-of-reference inner encoding, so the encoder applies it as a second cascade level.
    let data = alp_column_with_exceptions(20_000, 10);
    let block = encode_block(&data, false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::Alp, "ALP selected");
    assert_eq!(
        block.pipeline.side_stream().unwrap(),
        SideStream::ForBitpack,
        "the exception index stream carries a recorded inner encoding"
    );
    // Both levels are recorded in the marks/page-metadata pipeline id, and the block still decodes exactly.
    assert!(cascade_depth(block.pipeline).unwrap() >= 2);
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
}

/// conformance: hef-encodings-and-compression/recursive-cascade-selection/bounded-recursion-depth
#[test]
fn bounded_recursion_depth() {
    // The format declares a small maximum depth, and the encoder's gate refuses a level that would exceed it.
    assert!(MAX_CASCADE_DEPTH <= 4, "the declared maximum stays small");
    assert!(cascade_level_fits(MAX_CASCADE_DEPTH - 1));
    assert!(
        !cascade_level_fits(MAX_CASCADE_DEPTH),
        "a level beyond the declared maximum is never added"
    );

    // However adversarial the data and however size-hungry the strategy, no recorded cascade exceeds the bound.
    let datasets = [
        alp_column_with_exceptions(20_000, 10),
        ColumnData::U64((0..4096u64).map(|i| i * 7).collect()),
        ColumnData::Strings((0..512).map(|i| Some(format!("value-{}", i % 3))).collect()),
    ];
    for data in &datasets {
        for strategy in [CascadeStrategy::DecodeOptimized, CascadeStrategy::SizeOptimized] {
            let block = encode_block_with_strategy(data, false, strategy);
            assert!(
                cascade_depth(block.pipeline).unwrap() <= MAX_CASCADE_DEPTH,
                "{strategy:?}: recorded cascade must stay within the declared maximum"
            );
        }
    }
}

/// conformance: hef-encodings-and-compression/recursive-cascade-selection/deeper-level-kept-only-when-sampling-wins
#[test]
fn deeper_level_kept_only_when_sampling_wins() {
    // Twelve scattered exceptions: bit-packing their index stream costs whole FastLanes vectors and would *grow* the
    // stream, so sampling loses and the encoder leaves the side stream at the shallower plain encoding.
    let sparse = alp_column_with_exceptions(3_000, 250);
    let block = encode_block(&sparse, false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::Alp);
    assert_eq!(
        block.pipeline.side_stream().unwrap(),
        SideStream::None,
        "a losing inner candidate is not applied"
    );
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), sparse);

    // The same rule is what admits the deeper level when it truly wins (the mirror-image dataset).
    let dense = alp_column_with_exceptions(20_000, 10);
    let cascaded = encode_block(&dense, false);
    assert_eq!(cascaded.pipeline.side_stream().unwrap(), SideStream::ForBitpack);
}

/// conformance: hef-encodings-and-compression/recursive-cascade-selection/reader-decodes-the-recorded-cascade-deterministically
#[test]
fn reader_decodes_the_recorded_cascade_deterministically() {
    let data = alp_column_with_exceptions(20_000, 10);
    let block = encode_block(&data, false);
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::ForBitpack);

    // The recorded description alone — the pipeline id's raw u32 plus the bytes — is all a reader needs: no
    // re-derivation from the data, and repeated decodes agree exactly.
    let recorded = hef::encoding::PipelineId(block.pipeline.0);
    let first = decode_block(recorded, &block.bytes).unwrap();
    let second = decode_block(recorded, &block.bytes).unwrap();
    assert_eq!(first, data);
    assert_eq!(first, second);

    // A single granule decodes from the recorded cascade without touching unrelated granules: the range decode loads
    // only the FastLanes vectors that cover the requested rows plus the small exception side streams.
    let ColumnData::F64(all) = &data else { unreachable!() };
    let granule = decode_block_range(recorded, &block.bytes, 5_000, 5_064).unwrap();
    assert_eq!(granule, ColumnData::F64(all[5_000..5_064].to_vec()));
}

/// conformance: hef-encodings-and-compression/recursive-cascade-selection/software-parity-holds-at-every-cascade-level
#[test]
fn software_parity_holds_at_every_cascade_level() {
    // A block whose recorded cascade uses every level the format allows: ALP, an inner encoding of its exception
    // stream, and a trailing compression stage (SizeOptimized admits it on this compressible data).
    let data = alp_column_with_exceptions(20_000, 10);
    let block = encode_block_with_strategy(&data, false, CascadeStrategy::SizeOptimized);
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::ForBitpack);

    // Level: trailing compression. The pure-software decompressor and whatever engine is active (an accelerator, when
    // one was probed and installed) must hand back byte-identical bodies — the software path is the oracle.
    let compression = block.pipeline.compression().unwrap();
    let software = SoftwareDecompressor
        .decompress(compression, &block.bytes)
        .expect("software path always decodes");
    let active = active_decompressor()
        .decompress(compression, &block.bytes)
        .expect("active engine decodes the same bytes");
    assert_eq!(
        software, active,
        "trailing stage: software and active engines agree byte for byte"
    );

    // Levels: inner side-stream encoding and the top-level transform. The full software decode reproduces the values
    // exactly, and re-encoding them lands on byte-identical output — so no level's correctness can depend on an
    // accelerator.
    let decoded = decode_block(block.pipeline, &block.bytes).unwrap();
    assert_eq!(decoded, data);
    let reencoded = encode_block_with_strategy(&decoded, false, CascadeStrategy::SizeOptimized);
    assert_eq!(reencoded.pipeline, block.pipeline);
    assert_eq!(reencoded.bytes, block.bytes);
}
