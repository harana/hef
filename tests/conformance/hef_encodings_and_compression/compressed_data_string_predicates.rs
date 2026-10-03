//! Checks that common string filters are answered straight from a column's compressed bytes — dictionary codes (which
//! keep value order) for equality and ranges, FSST's symbol table for equality, and an FSST block's per-value prefix
//! keys and byte fingerprints for ranges and substrings — and that anything the fast path can't do still decodes and
//! filters to the same answer.

use hef::encoding::predicate::{
    ContainsMatcher, StringBound, StringPredicate, filter_string_block, filter_string_block_or_decode,
};
use hef::encoding::{ColumnData, PipelineId, SideStream, StringColumn, Transform, decode_block, encode_block};

fn low_cardinality(rows: usize) -> StringColumn {
    let palette = ["apple", "banana", "cherry", "date"];
    (0..rows)
        .map(|i| {
            if i % 7 == 0 {
                None
            } else {
                Some(palette[i % palette.len()])
            }
        })
        .collect()
}

fn high_cardinality(rows: usize) -> StringColumn {
    (0..rows)
        .map(|i| if i % 9 == 0 { None } else { Some(format!("id-{i:05}")) })
        .collect()
}

/// High-cardinality values long enough for their FSST block to carry the per-value keys.
fn long_high_cardinality(rows: usize) -> StringColumn {
    (0..rows)
        .map(|i| {
            if i % 9 == 0 {
                None
            } else {
                Some(format!("{i:05}-order-line-reference-{i:05}"))
            }
        })
        .collect()
}

fn decode_strings(pipeline: PipelineId, bytes: &[u8]) -> StringColumn {
    match decode_block(pipeline, bytes).unwrap() {
        ColumnData::Strings(values) => values,
        other => panic!("expected strings, got {other:?}"),
    }
}

fn bound(value: &str, inclusive: bool) -> StringBound {
    StringBound {
        inclusive,
        value: value.to_owned(),
    }
}

/// conformance: hef-encodings-and-compression/compressed-data-string-predicates/dictionary-codes-preserve-value-order
#[test]
fn dictionary_codes_preserve_value_order() {
    // A half-open range [banana, date) must select exactly the rows whose value is banana or cherry. That is only
    // correct if the integer codes share the strings' order, which is what the requirement guarantees.
    let values = low_cardinality(64);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::DictionaryString);

    let predicate = StringPredicate::Range {
        lower: Some(bound("banana", true)),
        upper: Some(bound("date", false)),
    };
    let mask = filter_string_block(encoded.pipeline, &encoded.bytes, &predicate)
        .unwrap()
        .expect("dictionary range is answered from codes");
    assert_eq!(mask, predicate.filter_decoded(&values));
    for (index, value) in values.iter().enumerate() {
        let expected = matches!(value, Some("banana") | Some("cherry"));
        assert_eq!(mask[index], expected, "row {index} = {value:?}");
    }
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-string-predicates/dictionary-equality-and-range-answered-from-codes
#[test]
fn dictionary_equality_and_range_answered_from_codes() {
    let values = low_cardinality(80);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    let predicates = [
        StringPredicate::Equals("cherry".to_owned()),
        StringPredicate::InSet(vec!["apple".to_owned(), "date".to_owned()]),
        StringPredicate::Range {
            lower: Some(bound("banana", true)),
            upper: None,
        },
    ];
    for predicate in &predicates {
        // `Some` proves the answer came from the codes, not a decode fallback.
        let mask = filter_string_block(encoded.pipeline, &encoded.bytes, predicate)
            .unwrap()
            .expect("dictionary predicate answered from codes");
        assert_eq!(mask, predicate.filter_decoded(&values), "{predicate:?}");
    }
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-string-predicates/fsst-equality-answered-from-compressed-bytes
#[test]
fn fsst_equality_answered_from_compressed_bytes() {
    let values = high_cardinality(96);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::FsstString);

    let predicates = [
        StringPredicate::Equals("id-00042".to_owned()),
        StringPredicate::Equals("id-99999".to_owned()), // absent: matches nothing
        StringPredicate::NotEquals("id-00007".to_owned()),
        StringPredicate::InSet(vec!["id-00003".to_owned(), "id-00050".to_owned()]),
    ];
    for predicate in &predicates {
        let mask = filter_string_block(encoded.pipeline, &encoded.bytes, predicate)
            .unwrap()
            .expect("FSST equality-class answered from compressed bytes");
        assert_eq!(mask, predicate.filter_decoded(&values), "{predicate:?}");
    }
}

/// conformance: hef-encodings-and-compression/compressed-data-string-predicates/fsst-range-falls-back-to-decode
#[test]
fn fsst_range_falls_back_to_decode() {
    // Short values, so the block carries no per-value keys and nothing in it orders the rows.
    let values = high_cardinality(96);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::FsstString);

    let predicate = StringPredicate::Range {
        lower: Some(bound("id-00010", true)),
        upper: Some(bound("id-00050", false)),
    };
    // The fast kernel declines (FSST codes are not order-preserving)...
    assert!(
        filter_string_block(encoded.pipeline, &encoded.bytes, &predicate)
            .unwrap()
            .is_none()
    );
    // ...and the wrapper still returns the correct answer via a full decode.
    let answered = filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, &predicate).unwrap();
    assert_eq!(answered, predicate.filter_decoded(&values));
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-string-predicates/fsst-range-answered-from-per-value-prefix-keys
#[test]
fn fsst_range_answered_from_per_value_prefix_keys() {
    let values = long_high_cardinality(128);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    assert_eq!(encoded.pipeline.transform().unwrap(), Transform::FsstString);
    assert_eq!(encoded.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);

    let predicates = [
        StringPredicate::Range {
            lower: Some(bound("00010", true)),
            upper: Some(bound("00050", false)),
        },
        // A prefix filter is the half-open range its prefix names.
        StringPredicate::Range {
            lower: Some(bound("0001", true)),
            upper: Some(bound("0002", false)),
        },
        StringPredicate::Range {
            lower: Some(bound("00007-order-line-reference-00007", true)),
            upper: Some(bound("00007-order-line-reference-00007", true)),
        },
    ];
    for predicate in &predicates {
        let mask = filter_string_block(encoded.pipeline, &encoded.bytes, predicate)
            .unwrap()
            .expect("FSST range answered from the block's prefix keys");
        assert_eq!(mask, predicate.filter_decoded(&values), "{predicate:?}");
    }
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-string-predicates/fsst-substring-candidates-pruned-by-byte-fingerprints
#[test]
fn fsst_substring_candidates_pruned_by_byte_fingerprints() {
    let values = long_high_cardinality(128);
    let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
    assert_eq!(encoded.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);

    for needle in ["order", "00042", "ZZZ", "~"] {
        let predicate = StringPredicate::Contains(ContainsMatcher::new(vec![needle.to_owned()], false));
        let mask = filter_string_block(encoded.pipeline, &encoded.bytes, &predicate)
            .unwrap()
            .expect("FSST substring answered over the block's own values");
        assert_eq!(mask, predicate.filter_decoded(&values), "{needle:?}");
    }
    // A needle whose bytes no value holds is ruled out by the fingerprints alone.
    let absent = StringPredicate::Contains(ContainsMatcher::new(vec!["~".to_owned()], false));
    assert!(
        filter_string_block(encoded.pipeline, &encoded.bytes, &absent)
            .unwrap()
            .expect("answered from the fingerprints")
            .iter()
            .all(|selected| !selected)
    );
}

/// conformance:
/// hef-encodings-and-compression/compressed-data-string-predicates/compressed-data-filter-equals-full-decode
#[test]
fn compressed_data_filter_equals_full_decode() {
    let predicates = [
        StringPredicate::Equals("banana".to_owned()),
        StringPredicate::NotEquals("banana".to_owned()),
        StringPredicate::InSet(vec!["apple".to_owned(), "id-00010".to_owned()]),
        StringPredicate::Range {
            lower: Some(bound("b", true)),
            upper: Some(bound("e", false)),
        },
    ];
    for values in [low_cardinality(70), high_cardinality(70)] {
        let encoded = encode_block(&ColumnData::Strings(values.clone()), true);
        let decoded = decode_strings(encoded.pipeline, &encoded.bytes);
        for predicate in &predicates {
            let answered = filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, predicate).unwrap();
            // Exact: equals a full decode-then-filter, row for row.
            assert_eq!(answered, predicate.filter_decoded(&decoded), "{predicate:?}");
            // Null rows are never selected.
            for (index, value) in decoded.iter().enumerate() {
                if value.is_none() {
                    assert!(!answered[index], "null row {index} matched {predicate:?}");
                }
            }
        }
    }
}
