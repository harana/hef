//! Checks that decimal numbers (such as money) and floating-point numbers are always stored in a form that round-trips
//! bit-for-bit. A space-saving float encoding is used only when it can reproduce the exact value; otherwise the encoder
//! falls back so nothing is ever silently rounded.
use hef::encoding::{ColumnData, Transform, decode_block, encode_block};

/// conformance:
/// hef-encodings-and-compression/mandatory-representations-for-money-and-floats/float-metric-outside-tolerance
#[test]
fn float_metric_outside_tolerance() {
    // f64 values that ALP cannot round-trip exactly are not encoded with ALP: the encoder falls back, and the round
    // trip stays bit-exact either way (the storage tolerance contract is exactness). Magnitudes beyond ALP's
    // scaled-integer range fail every exponent's round-trip check.
    let pathological: Vec<f64> = (1..512).map(|i| (i as f64) * 1.0e300).collect();
    let encoded = encode_block(&ColumnData::F64(pathological.clone()), false);
    assert_ne!(encoded.pipeline.transform().unwrap(), Transform::Alp);
    let decoded = decode_block(encoded.pipeline, &encoded.bytes).unwrap();
    assert_eq!(decoded, ColumnData::F64(pathological));
    // Friendly decimals do select ALP — the candidate is real.
    let friendly: Vec<f64> = (0..512).map(|i| (i as f64) * 0.5).collect();
    let encoded = encode_block(&ColumnData::F64(friendly), false);
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::Alp);
}
