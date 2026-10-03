//! Checks that common numeric filters are answered straight from a column's compressed bytes — FOR packed lanes for
//! integers, the stored mantissa for decimal money, and the ALP integer representation for floats — and that anything
//! the fast path cannot handle still decodes and filters correctly.

use hef::encoding::predicate::*;
use hef::encoding::{ColumnData, Transform, decode_block, encode_block};

/// Values that scatter within a fixed range so FOR wins over DELTA.
fn scattered_u64(n: usize) -> Vec<u64> {
    (0..n).map(|i| (i % 50) as u64 * 3 + 100).collect()
}

fn decode_u64(data: &ColumnData) -> Vec<u64> {
    match data {
        ColumnData::U64(v) => v.clone(),
        other => panic!("expected U64, got {other:?}"),
    }
}

fn decode_f64(data: &ColumnData) -> Vec<f64> {
    match data {
        ColumnData::F64(v) => v.clone(),
        other => panic!("expected F64, got {other:?}"),
    }
}

fn bound_i(value: i128, inclusive: bool) -> NumericBound {
    NumericBound { inclusive, value }
}

fn bound_f(value: f64, inclusive: bool) -> FloatBound {
    FloatBound { inclusive, value }
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-numeric-predicates/range-answered-over-fastlanes-packed-lanes
#[test]
fn range_answered_over_fastlanes_packed_lanes() {
    let values = scattered_u64(200);
    let data = ColumnData::U64(values.clone());
    let block = encode_block(&data, false);
    assert_eq!(
        block.pipeline.transform().unwrap(),
        Transform::ForBitpack,
        "test requires a FOR-encoded block"
    );

    // Range: 109 ≤ value < 130 (selects some but not all rows).
    let predicate = NumericPredicate::Range {
        lower: Some(bound_i(109, true)),
        upper: Some(bound_i(130, false)),
    };
    let mask = filter_numeric_block(block.pipeline, &block.bytes, &predicate)
        .unwrap()
        .expect("FOR range answered from packed lanes");
    let expected: Vec<bool> = values.iter().map(|v| *v >= 109 && *v < 130).collect();
    assert_eq!(mask, expected, "packed-lane range filter disagrees with expected");

    // The fast answer equals the full decode-then-filter.
    assert_eq!(mask, predicate.filter_decoded(&data));
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-numeric-predicates/
/// decimal128-equality-answered-from-the-integer-mantissa
#[test]
fn decimal128_equality_answered_from_the_integer_mantissa() {
    // Mantissas with scale=2: 1050 → $10.50, 2000 → $20.00, etc.
    let data = ColumnData::Decimal {
        values: vec![1050, 2000, 750, 1050, 3000],
        scale: 2,
    };
    let block = encode_block(&data, false);

    // Equality: mantissa == 1050.
    let eq_pred = NumericPredicate::Equals(1050);
    let mask = filter_numeric_block(block.pipeline, &block.bytes, &eq_pred)
        .unwrap()
        .expect("decimal128 equality answered from mantissa");
    assert_eq!(mask, vec![true, false, false, true, false]);
    assert_eq!(mask, eq_pred.filter_decoded(&data));

    // Range: 900 ≤ mantissa ≤ 1500.
    let range_pred = NumericPredicate::Range {
        lower: Some(bound_i(900, true)),
        upper: Some(bound_i(1500, true)),
    };
    let mask2 = filter_numeric_block(block.pipeline, &block.bytes, &range_pred)
        .unwrap()
        .expect("decimal128 range answered from mantissa");
    assert_eq!(mask2, vec![true, false, false, true, false]);
    assert_eq!(mask2, range_pred.filter_decoded(&data));
}

/// conformance: hef-encodings-and-compression/compressed-data-numeric-predicates/alp-predicate-decodes-only-survivors
#[test]
fn alp_predicate_decodes_only_survivors() {
    // Integer-valued floats round-trip through ALP exactly.
    let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
    let data = ColumnData::F64(values.clone());
    let block = encode_block(&data, false);
    assert_eq!(
        block.pipeline.transform().unwrap(),
        Transform::Alp,
        "test requires an ALP-encoded block"
    );

    // Predicate: 25.0 ≤ value < 75.0.
    let predicate = FloatPredicate::Range {
        lower: Some(bound_f(25.0, true)),
        upper: Some(bound_f(75.0, false)),
    };
    let mask = filter_float_block(block.pipeline, &block.bytes, &predicate)
        .unwrap()
        .expect("ALP predicate answered from integer representation");
    let decoded_values = decode_f64(&decode_block(block.pipeline, &block.bytes).unwrap());
    let expected: Vec<bool> = decoded_values.iter().map(|v| *v >= 25.0 && *v < 75.0).collect();
    assert_eq!(mask, expected);

    // Exact: equals full decode-then-filter.
    assert_eq!(mask, predicate.filter_decoded(&decoded_values));
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-numeric-predicates/
/// unsupported-encoding-or-predicate-falls-back-to-decode
#[test]
fn unsupported_encoding_or_predicate_falls_back_to_decode() {
    // Plain U64 blocks do not have a compressed-data fast path. Encode a few values that force PlainU64 (only one
    // distinct value, but we want plain — use a small vec so the cost analysis picks plain).
    let data = ColumnData::U64(vec![42, 43, 44]);
    let block = encode_block(&data, false);
    let predicate = NumericPredicate::Equals(43);

    // filter_numeric_block_or_decode always returns an answer — it falls back to a full decode when the fast path
    // declines.
    let mask = filter_numeric_block_or_decode(block.pipeline, &block.bytes, &predicate).unwrap();
    assert_eq!(mask, predicate.filter_decoded(&data));

    // Plain F64 blocks do not have a float fast path either.
    let float_data = ColumnData::F64(vec![1e300, 2e300, 3e300]); // outside ALP range
    let float_block = encode_block(&float_data, false);
    let float_pred = FloatPredicate::Equals(2e300);
    let float_mask = filter_float_block_or_decode(float_block.pipeline, &float_block.bytes, &float_pred).unwrap();
    let raw_floats = decode_f64(&decode_block(float_block.pipeline, &float_block.bytes).unwrap());
    assert_eq!(float_mask, float_pred.filter_decoded(&raw_floats));
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-numeric-predicates/compressed-data-numeric-filter-equals-full-decode
#[test]
fn compressed_data_numeric_filter_equals_full_decode() {
    // Integer column: FOR-encoded.
    let u64_values = scattered_u64(200);
    let u64_data = ColumnData::U64(u64_values.clone());
    let u64_block = encode_block(&u64_data, false);
    let int_predicates = [
        NumericPredicate::Equals(109),
        NumericPredicate::NotEquals(100),
        NumericPredicate::InSet(vec![100, 106, 112]),
        NumericPredicate::Range {
            lower: Some(bound_i(103, true)),
            upper: Some(bound_i(140, false)),
        },
    ];
    let decoded_u64 = ColumnData::U64(decode_u64(&decode_block(u64_block.pipeline, &u64_block.bytes).unwrap()));
    for pred in &int_predicates {
        let fast = filter_numeric_block_or_decode(u64_block.pipeline, &u64_block.bytes, pred).unwrap();
        let slow = pred.filter_decoded(&decoded_u64);
        assert_eq!(fast, slow, "{pred:?}");
        // Null rows are never selected (none here, so all selected rows have a value).
    }

    // Decimal column.
    let dec_data = ColumnData::Decimal {
        values: (1..=20_i128).map(|i| i * 50).collect(),
        scale: 2,
    };
    let dec_block = encode_block(&dec_data, false);
    let dec_pred = NumericPredicate::Range {
        lower: Some(bound_i(200, true)),
        upper: Some(bound_i(700, false)),
    };
    let dec_fast = filter_numeric_block_or_decode(dec_block.pipeline, &dec_block.bytes, &dec_pred).unwrap();
    assert_eq!(dec_fast, dec_pred.filter_decoded(&dec_data));

    // Float (ALP) column.
    let f64_values: Vec<f64> = (0..80).map(|i| i as f64 * 0.5).collect();
    let f64_data = ColumnData::F64(f64_values.clone());
    let f64_block = encode_block(&f64_data, false);
    let f64_pred = FloatPredicate::Range {
        lower: Some(bound_f(10.0, true)),
        upper: Some(bound_f(30.0, false)),
    };
    let f64_fast = filter_float_block_or_decode(f64_block.pipeline, &f64_block.bytes, &f64_pred).unwrap();
    let decoded_f64 = decode_f64(&decode_block(f64_block.pipeline, &f64_block.bytes).unwrap());
    assert_eq!(f64_fast, f64_pred.filter_decoded(&decoded_f64));
}
