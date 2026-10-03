//! Checks that HEF's bit-packed integer streams use the FastLanes transposed layout: they round-trip exactly even when
//! the value count ends mid-vector (the final 1024-value vector is zero-padded), and the ALP and dictionary kernels —
//! which bit-pack their scaled integers and their codes — ride the same layout.

use hef::encoding::{ColumnData, StringColumn, Transform, decode_block, encode_block};

fn round_trip(data: ColumnData) {
    let encoded = encode_block(&data, false);
    let decoded = decode_block(encoded.pipeline, &encoded.bytes).unwrap();
    assert_eq!(decoded, data);
}

/// conformance:
/// hef-encodings-and-compression/bit-packed-integer-streams-use-the-fastlanes-transposed-layout/
/// integer-columns-round-trip-across-vector-boundaries
#[test]
fn integer_columns_round_trip_across_vector_boundaries() {
    // 3000 is not a multiple of the 1024-value FastLanes vector, so the final vector is zero-padded; decode must drop
    // that padding and return the 3000 originals exactly.
    let monotonic: Vec<u64> = (0..3000u64).map(|i| 1_700_000_000_000 + i * 17).collect();
    let encoded = encode_block(&ColumnData::U64(monotonic.clone()), false);
    assert!(
        matches!(
            encoded.pipeline.transform().unwrap(),
            Transform::ForBitpack | Transform::DeltaBitpack
        ),
        "monotonic data must select a FastLanes FOR/DELTA candidate"
    );
    let decoded = decode_block(encoded.pipeline, &encoded.bytes).unwrap();
    assert_eq!(decoded, ColumnData::U64(monotonic));

    // Signed monotonic data exercises the zigzag DELTA path across a boundary.
    let signed: Vec<i64> = (0..1025i64).map(|i| -5_000_000 + i * 13).collect();
    round_trip(ColumnData::I64(signed));
}

/// conformance:
/// hef-encodings-and-compression/bit-packed-integer-streams-use-the-fastlanes-transposed-layout/
/// alp-and-dictionary-streams-ride-the-same-transposed-layout
#[test]
fn alp_and_dictionary_streams_ride_the_same_transposed_layout() {
    // ALP packs its scaled integers through the FastLanes layout; this column spans more than two 1024-value vectors.
    let metric: Vec<f64> = (0..2049).map(|i| (i as f64) * 0.25 + 10.5).collect();
    let alp = encode_block(&ColumnData::F64(metric.clone()), false);
    assert_eq!(alp.pipeline.transform().unwrap(), Transform::Alp);
    assert_eq!(decode_block(alp.pipeline, &alp.bytes).unwrap(), ColumnData::F64(metric));

    // A dictionary block packs its per-row codes through the same layout.
    let labels: StringColumn = (0..2500).map(|i| Some(["EUR", "USD", "NOK", "GBP"][i % 4])).collect();
    let dict = encode_block(&ColumnData::Strings(labels.clone()), false);
    assert_eq!(dict.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(
        decode_block(dict.pipeline, &dict.bytes).unwrap(),
        ColumnData::Strings(labels)
    );
}
