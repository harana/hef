use super::*;
use crate::encoding::{ColumnData, encode_block};

fn descriptor_of(data: &ColumnData) -> (Transform, PageDescriptor) {
    let encoded = encode_block(data, false);
    let transform = encoded.pipeline.transform().unwrap();
    let descriptor = extract_descriptor(encoded.pipeline, &encoded.bytes).unwrap();
    (transform, descriptor)
}

/// A dictionary-encoded string column exposes its sorted, deduplicated entries straight from the page header, so a
/// consumer can translate a literal to a code without decoding any row.
#[test]
fn dictionary_descriptor_lists_the_sorted_deduplicated_entries() {
    let values: Vec<Option<String>> = (0..2000)
        .map(|i| Some(["EUR", "USD", "NOK"][i % 3].to_owned()))
        .collect();
    let (transform, descriptor) = descriptor_of(&ColumnData::Strings(values.into()));
    assert_eq!(transform, Transform::DictionaryString);
    match descriptor {
        PageDescriptor::Dictionary { entries } => {
            assert_eq!(entries, vec!["EUR".to_owned(), "NOK".to_owned(), "USD".to_owned()]);
        }
        other => panic!("expected a dictionary descriptor, got {other:?}"),
    }
}

/// A monotonic integer column bit-packs through FOR or DELTA; either way the descriptor reports a bit width in range so
/// a consumer can estimate decode cost without unpacking a value.
#[test]
fn monotonic_integers_yield_a_bitpacked_descriptor_with_a_valid_width() {
    let values: Vec<u64> = (0..8192u64).map(|i| 1_700_000_000_000 + i * 17).collect();
    let (transform, descriptor) = descriptor_of(&ColumnData::U64(values));
    match (transform, descriptor) {
        (Transform::ForBitpack, PageDescriptor::ForBitpack { width, .. })
        | (Transform::DeltaBitpack, PageDescriptor::DeltaBitpack { width, .. }) => {
            assert!(width <= 64, "bit width {width} out of range");
        }
        (transform, descriptor) => panic!("unexpected transform/descriptor pair: {transform:?} / {descriptor:?}"),
    }
}

/// A long-run column encodes as RLE, and its descriptor reports the run count the predicate engine uses to skip
/// stretches cheaply.
#[test]
fn run_length_column_reports_its_run_count() {
    let values: Vec<u64> = (0..64u64)
        .flat_map(|run| std::iter::repeat_n(run * 1_000_003, 64))
        .collect();
    let (transform, descriptor) = descriptor_of(&ColumnData::U64(values));
    assert_eq!(transform, Transform::Rle);
    match descriptor {
        PageDescriptor::Rle { run_count } => assert_eq!(run_count, 64),
        other => panic!("expected an rle descriptor, got {other:?}"),
    }
}

/// A metric float column encodes with ALP; its descriptor names the exponent and exception count without
/// reconstructing a float.
#[test]
fn alp_float_column_reports_an_alp_descriptor() {
    let values: Vec<f64> = (0..2048).map(|i| (i as f64) * 0.25 + 10.5).collect();
    let (transform, descriptor) = descriptor_of(&ColumnData::F64(values));
    assert_eq!(transform, Transform::Alp);
    assert!(matches!(descriptor, PageDescriptor::Alp { .. }));
}

/// A high-cardinality string column encodes with FSST; its descriptor reports a non-empty symbol table.
#[test]
fn fsst_column_reports_a_symbol_count() {
    let values: Vec<Option<String>> = (0..2000)
        .map(|i| (i % 17 != 0).then(|| format!("https://example.com/opportunity/{i}/stage")))
        .collect();
    let (transform, descriptor) = descriptor_of(&ColumnData::Strings(values.into()));
    assert_eq!(transform, Transform::FsstString);
    match descriptor {
        PageDescriptor::Fsst { symbol_count } => assert!(symbol_count > 0),
        other => panic!("expected an fsst descriptor, got {other:?}"),
    }
}

/// Decode cost is a relative, unit-free estimate that orders encodings cheapest-first: plain and RLE are the floor,
/// bit-packing scales with width, and FSST is the most expensive per row.
#[test]
fn decode_cost_orders_encodings_cheapest_first() {
    assert_eq!(PageDescriptor::Plain.decode_cost_per_row(), 1);
    assert_eq!(PageDescriptor::Rle { run_count: 4 }.decode_cost_per_row(), 1);
    // A narrower bit-packed stream never costs more than a wider one.
    assert!(
        PageDescriptor::ForBitpack { base: 0, width: 8 }.decode_cost_per_row()
            <= PageDescriptor::ForBitpack { base: 0, width: 64 }.decode_cost_per_row()
    );
    // DELTA carries the running prefix sum, so it costs more than FOR at the same width.
    assert!(
        PageDescriptor::ForBitpack { base: 0, width: 32 }.decode_cost_per_row()
            < PageDescriptor::DeltaBitpack {
                first_value: 0,
                width: 32
            }
            .decode_cost_per_row()
    );
    // FSST per-character decode is the most expensive; a small dictionary is far cheaper.
    assert!(
        PageDescriptor::Fsst { symbol_count: 200 }.decode_cost_per_row()
            > PageDescriptor::Dictionary {
                entries: vec!["a".to_owned()]
            }
            .decode_cost_per_row()
    );
}
