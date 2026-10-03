//! Checks that shredded payload columns use the same adaptive encoder and stored block format as every other column:
//! the writer's shredded marks record an adaptive transform, a single granule reads back without decoding the rest of
//! the block, round-trips are exact and deterministic, and the retired Vortex transform id is rejected as corruption.

use crate::support;
use hef::encoding::{
    ColumnData, Compression, PipelineId, StringColumn, decode_block, decode_block_range, encode_block,
};
use hef::layout::reader::HefFile;

/// conformance: hef-encodings-and-compression/shredded-blocks-use-the-adaptive-encoder/shredded-cascade-uses-the-adaptive-encoder
#[test]
fn shredded_cascade_uses_the_adaptive_encoder() {
    // A low-cardinality shredded string column: its cascade (dictionary codes over the sorted distinct values) comes
    // from the same adaptive encoder as every other column, not a second serialization private to shredded blocks.
    let statuses = ["lost", "open", "pending", "won"];
    let data = ColumnData::Strings(
        (0..600)
            .map(|i| Some(statuses[i % statuses.len()].to_owned()))
            .collect(),
    );
    let block = encode_block(&data, true);
    assert!(block.pipeline.transform().is_ok(), "an adaptive transform is recorded");
    assert_ne!(
        block.pipeline.0 & 0xFF,
        11,
        "the retired Vortex transform id is never written"
    );
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);

    // The write path itself routes shredded scan-path columns through the adaptive encoder: a built file's shredded
    // column marks record an adaptive pipeline that decodes through the one generic block entry point.
    let built = support::built_file(32);
    let shredded = built.footer.shredded.first().expect("a shredded column exists");
    let mark = built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == shredded.column_id)
        .expect("shredded column has a mark");
    assert!(mark.codec_pipeline_id.transform().is_ok());
    assert_ne!(mark.codec_pipeline_id.0 & 0xFF, 11);

    let file = HefFile::open(built.bytes, None).unwrap();
    let granule = file.footer().granules[0].granule_id;
    let read = file.read_column(shredded.column_id, granule).unwrap();
    assert!(
        read.data.row_count() > 0,
        "shredded rows read back through the normal path"
    );
}

/// conformance: hef-encodings-and-compression/shredded-blocks-use-the-adaptive-encoder/random-access-preserved-in-compressed-form
#[test]
fn random_access_preserved_in_compressed_form() {
    // No whole-block heavyweight compression stage sits over a shredded block, and one granule of rows decodes from
    // the stored form via the range decoder — the rest of the block is never materialized.
    let values: StringColumn = (0..2_000).map(|i| Some(["eu", "us", "apac"][i % 3])).collect();
    let data = ColumnData::Strings(values.clone());
    let block = encode_block(&data, true);
    assert_ne!(block.pipeline.compression().unwrap(), Compression::Lz4);
    assert_ne!(block.pipeline.compression().unwrap(), Compression::Zstd1);
    assert_ne!(block.pipeline.compression().unwrap(), Compression::Zstd3);
    let granule = decode_block_range(block.pipeline, &block.bytes, 700, 764).unwrap();
    assert_eq!(granule, ColumnData::Strings(values.slice(700, 764)));
}

/// conformance: hef-encodings-and-compression/shredded-blocks-use-the-adaptive-encoder/serialization-round-trips-under-conformance
#[test]
fn serialization_round_trips_under_conformance() {
    // Values, logical types (the ColumnData variant), and row order all survive the round trip exactly, for every
    // column kind a shredded block can carry.
    let datasets = [
        ColumnData::U64((0..3_000u64).map(|i| i * 37 + 5).collect()),
        ColumnData::I64((-750..750i64).map(|i| i * 13).collect()),
        ColumnData::F64((0..1_500).map(|i| i as f64 * 0.75 - 200.0).collect()),
        ColumnData::Decimal {
            values: (0..400i128).map(|i| i * 250 - 50_000).collect(),
            scale: 2,
        },
        ColumnData::U128((0..64u128).map(|i| i << 64 | i).collect()),
        ColumnData::Strings(
            (0..900)
                .map(|i| {
                    if i % 11 == 0 {
                        None
                    } else {
                        Some(format!("kind-{}", i % 6))
                    }
                })
                .collect(),
        ),
    ];
    for data in &datasets {
        let block = encode_block(data, true);
        assert_eq!(&decode_block(block.pipeline, &block.bytes).unwrap(), data);
    }
}

/// conformance: hef-encodings-and-compression/shredded-blocks-use-the-adaptive-encoder/shredded-bytes-are-deterministic
#[test]
fn shredded_bytes_are_deterministic() {
    // Cross-node byte identity is a stated invariant: the same content encodes to the same bytes and pipeline id.
    let data = ColumnData::Strings((0..1_024).map(|i| Some(format!("event-{:05}", i % 40))).collect());
    let a = encode_block(&data, true);
    let b = encode_block(&data, true);
    assert_eq!(a.bytes, b.bytes);
    assert_eq!(a.pipeline, b.pipeline);
}

/// conformance: hef-encodings-and-compression/shredded-blocks-use-the-adaptive-encoder/retired-transform-id-is-rejected
#[test]
fn retired_transform_id_is_rejected() {
    // Transform id 11 named the removed Vortex shredded serialization. A block still claiming it is structural
    // corruption: the id resolves to no transform and the block never decodes as something else.
    let pipeline = PipelineId(11);
    assert!(pipeline.transform().is_err());
    assert!(decode_block(pipeline, &[0u8; 32]).is_err());
    assert!(decode_block_range(pipeline, &[0u8; 32], 0, 8).is_err());
}
