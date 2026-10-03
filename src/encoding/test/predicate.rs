use super::*;
use crate::encoding::{CascadeStrategy, Compression, encode_block, encode_block_forced_for_conformance};
use crate::file::bytes::Writer;
use std::collections::BTreeSet;

fn decoded(pipeline: super::PipelineId, bytes: &[u8]) -> StringColumn {
    match decode_block(pipeline, bytes).unwrap() {
        ColumnData::Strings(values) => values,
        other => panic!("expected strings, got {other:?}"),
    }
}

/// Encodes `values`, then asserts that the compressed-data kernel (when it applies) agrees row-for-row with a plain
/// decode-and-filter, that the auto-fallback wrapper always agrees, and that decoding round-trips. Returns `true` when
/// the fast kernel handled the predicate, `false` when it declined (so the caller fell back to a full decode).
fn check(values: &[Option<String>], predicate: &StringPredicate, random_access: bool) -> bool {
    let column: StringColumn = values.to_vec().into();
    let encoded = encode_block(&ColumnData::Strings(column.clone()), random_access);
    let reference = predicate.filter_decoded(&column);
    assert_eq!(reference.len(), values.len(), "reference mask must cover every row");

    let combined = filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, predicate).unwrap();
    assert_eq!(combined, reference, "fallback wrapper disagreed");

    let round = predicate.filter_decoded(&decoded(encoded.pipeline, &encoded.bytes));
    assert_eq!(round, reference, "decode round-trip disagreed");

    if let Some(count) = count_string_block_shared(encoded.pipeline, &encoded.bytes, predicate, None).unwrap() {
        assert_eq!(
            count,
            reference.iter().filter(|matched| **matched).count(),
            "count-only kernel disagreed with the reference mask"
        );
    }

    match filter_string_block(encoded.pipeline, &encoded.bytes, predicate).unwrap() {
        Some(mask) => {
            assert_eq!(mask, reference, "fast kernel disagreed with reference");
            true
        }
        None => false,
    }
}

fn transform_of(values: &[Option<String>], random_access: bool) -> Transform {
    encode_block(&ColumnData::Strings(values.to_vec().into()), random_access)
        .pipeline
        .transform()
        .unwrap()
}

fn low_cardinality(rows: usize) -> Vec<Option<String>> {
    let palette = ["apple", "banana", "cherry", "date"];
    (0..rows)
        .map(|i| {
            if i % 7 == 0 {
                None
            } else {
                Some(palette[i % palette.len()].to_owned())
            }
        })
        .collect()
}

fn high_cardinality(rows: usize) -> Vec<Option<String>> {
    (0..rows)
        .map(|i| if i % 9 == 0 { None } else { Some(format!("id-{i:05}")) })
        .collect()
}

/// High-cardinality values long enough for the FSST block to carry per-value keys, and sharing no prefix — so a range
/// bound separates them on their first bytes.
fn long_high_cardinality(rows: usize) -> Vec<Option<String>> {
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

/// The same, but every value opens with the same seven bytes — the worst case for a prefix key, where every row ties
/// with the bound and the kernel has to fall back to the values themselves.
fn long_shared_prefix(rows: usize) -> Vec<Option<String>> {
    (0..rows)
        .map(|i| {
            if i % 9 == 0 {
                None
            } else {
                Some(format!("https://example.com/orders/{i:05}"))
            }
        })
        .collect()
}

fn long_opaque(rows: usize) -> Vec<Option<String>> {
    let filler = "x".repeat(80);
    (0..rows).map(|i| Some(format!("{filler}-{i}"))).collect()
}

fn equals(value: &str) -> StringPredicate {
    StringPredicate::Equals(value.to_owned())
}

fn contains(value: &str) -> StringPredicate {
    StringPredicate::Contains(ContainsMatcher::new(vec![value.to_owned()], false))
}

fn contains_all(values: &[&str], case_insensitive: bool) -> StringPredicate {
    StringPredicate::Contains(ContainsMatcher::new(
        values.iter().map(|value| (*value).to_owned()).collect(),
        case_insensitive,
    ))
}

fn bound(value: &str, inclusive: bool) -> StringBound {
    StringBound {
        inclusive,
        value: value.to_owned(),
    }
}

#[test]
fn low_cardinality_columns_encode_as_a_dictionary() {
    let values = low_cardinality(64);
    assert_eq!(transform_of(&values, true), Transform::DictionaryString);
}

#[test]
fn dictionary_equality_is_answered_from_codes() {
    let values = low_cardinality(64);
    for random_access in [true, false] {
        // A value that exists, a value that does not, both handled fast.
        assert!(check(&values, &equals("banana"), random_access));
        assert!(check(&values, &equals("fig"), random_access));
        assert!(check(
            &values,
            &StringPredicate::NotEquals("cherry".to_owned()),
            random_access
        ));
        assert!(check(
            &values,
            &StringPredicate::InSet(vec!["apple".to_owned(), "date".to_owned(), "kiwi".to_owned()]),
            random_access
        ));
    }
}

#[test]
fn dictionary_ranges_are_answered_from_sorted_codes() {
    let values = low_cardinality(96);
    let cases = [
        StringPredicate::Range {
            lower: Some(bound("banana", true)),
            upper: Some(bound("cherry", true)),
        },
        StringPredicate::Range {
            lower: Some(bound("banana", false)),
            upper: Some(bound("date", false)),
        },
        StringPredicate::Range {
            lower: Some(bound("aaa", true)),
            upper: None,
        },
        StringPredicate::Range {
            lower: None,
            upper: Some(bound("cherry", false)),
        },
        StringPredicate::Range {
            lower: Some(bound("date", false)),
            upper: None,
        },
    ];
    for predicate in &cases {
        for random_access in [true, false] {
            assert!(
                check(&values, predicate, random_access),
                "dictionary range should be handled fast: {predicate:?}"
            );
        }
    }
}

#[test]
fn high_cardinality_columns_encode_as_fsst() {
    let values = high_cardinality(128);
    assert_eq!(transform_of(&values, true), Transform::FsstString);
}

#[test]
fn fsst_equality_class_is_answered_from_compressed_bytes() {
    let values = high_cardinality(128);
    for random_access in [true, false] {
        assert!(check(&values, &equals("id-00042"), random_access));
        // A search term absent from the column still answers correctly (it matches nothing), proving
        // compress-then-compare needs no corpus hit.
        assert!(check(&values, &equals("id-99999"), random_access));
        assert!(check(
            &values,
            &StringPredicate::NotEquals("id-00007".to_owned()),
            random_access
        ));
        assert!(check(
            &values,
            &StringPredicate::InSet(vec!["id-00003".to_owned(), "id-00050".to_owned(), "missing".to_owned(),]),
            random_access
        ));
    }
}

/// A block of long values carries the per-value keys; one of short values does not, because there a decode is cheap
/// enough that the keys would be pure overhead.
#[test]
fn fsst_blocks_carry_per_value_keys_only_when_their_values_are_long_enough() {
    let long = encode_block(&ColumnData::Strings(long_high_cardinality(128).into()), true);
    assert_eq!(long.pipeline.transform().unwrap(), Transform::FsstString);
    assert_eq!(long.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);

    let short = encode_block(&ColumnData::Strings(high_cardinality(128).into()), true);
    assert_eq!(short.pipeline.transform().unwrap(), Transform::FsstString);
    assert_eq!(short.pipeline.side_stream().unwrap(), SideStream::None);
}

/// The keys sit behind the arena, so every decode path reads the block exactly as it did before them.
#[test]
fn a_block_with_per_value_keys_still_decodes_unchanged() {
    let values: Vec<Option<String>> = long_high_cardinality(128);
    let column: StringColumn = values.clone().into();
    for random_access in [true, false] {
        let encoded = encode_block(&ColumnData::Strings(column.clone()), random_access);
        assert_eq!(encoded.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);
        assert_eq!(decoded(encoded.pipeline, &encoded.bytes), column);
        // And one granule of it, through the range decoder.
        let ColumnData::Strings(window) =
            crate::encoding::decode_block_range(encoded.pipeline, &encoded.bytes, 10, 20).unwrap()
        else {
            panic!("expected strings");
        };
        assert_eq!(window, values[10..20].to_vec().into());
        // And straight into Arrow, the other decode path over the same bytes.
        let views = crate::encoding::decode_string_block_views(encoded.pipeline, &encoded.bytes)
            .unwrap()
            .expect("FSST blocks decode into string views");
        let seen: Vec<Option<String>> = views.iter().map(|value| value.map(str::to_owned)).collect();
        assert_eq!(seen, values);
    }
}

/// The headline: a range filter on an FSST block is answered from its prefix keys, not from a full decode — and
/// agrees with the decode row for row, on values that separate on their prefixes and on values that all tie.
#[test]
fn fsst_range_predicates_are_answered_from_prefix_keys() {
    let cases = [
        ("separating prefixes", long_high_cardinality(128)),
        ("shared prefixes", long_shared_prefix(128)),
    ];
    let predicates = [
        StringPredicate::Range {
            lower: Some(bound("00010", true)),
            upper: Some(bound("00050", false)),
        },
        StringPredicate::Range {
            lower: Some(bound("https://example.com/orders/00010", true)),
            upper: Some(bound("https://example.com/orders/00050", false)),
        },
        // A one-sided bound, and a prefix filter expressed as the half-open range it is.
        StringPredicate::Range {
            lower: Some(bound("00100", false)),
            upper: None,
        },
        StringPredicate::Range {
            lower: Some(bound("https://example.com/orders/001", true)),
            upper: Some(bound("https://example.com/orders/002", false)),
        },
        // Bounds that select everything and nothing.
        StringPredicate::Range {
            lower: None,
            upper: Some(bound("zzzz", true)),
        },
        StringPredicate::Range {
            lower: Some(bound("zzzz", true)),
            upper: None,
        },
    ];
    for (name, values) in cases {
        for predicate in &predicates {
            for random_access in [true, false] {
                assert!(
                    check(&values, predicate, random_access),
                    "{name}: FSST range should be answered from the prefix keys: {predicate:?}"
                );
            }
        }
    }
}

/// An inclusive bound and an exclusive one on the same value must disagree exactly on the rows equal to it — the case
/// a prefix key settles only when it holds the whole value.
#[test]
fn fsst_range_bounds_honour_inclusivity() {
    let values = long_high_cardinality(128);
    for inclusive in [true, false] {
        assert!(check(
            &values,
            &StringPredicate::Range {
                lower: Some(bound("00007-order-line-reference-00007", inclusive)),
                upper: Some(bound("00007-order-line-reference-00007", inclusive)),
            },
            true
        ));
    }
}

/// A substring filter on a block with fingerprints agrees with the arena scan, and a needle whose bytes the block
/// never holds is ruled out from the fingerprints alone.
#[test]
fn fsst_substring_candidates_are_pruned_by_fingerprints() {
    let values = long_high_cardinality(128);
    let encoded = encode_block(&ColumnData::Strings(values.clone().into()), true);
    assert_eq!(encoded.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);
    for needle in ["order", "line-reference", "00042", "ZZZ", "~", "\u{0}"] {
        assert!(
            check(&values, &contains(needle), true),
            "fingerprint-pruned contains disagreed for {needle:?}"
        );
    }
    // Case-insensitive and multi-needle searches take the same path.
    assert!(check(&values, &contains_all(&["ORDER", "REFERENCE"], true), true));
    assert!(check(&values, &contains_all(&["order", "nope"], false), true));
}

#[test]
fn fsst_range_predicates_fall_back_to_decode() {
    // Short values, so the block stores no prefix keys and nothing in it orders the rows.
    let values = high_cardinality(128);
    let predicate = StringPredicate::Range {
        lower: Some(bound("id-00010", true)),
        upper: Some(bound("id-00020", false)),
    };
    // The fast kernel declines (FSST codes are not order-preserving)...
    assert!(
        filter_string_block(
            encode_block(&ColumnData::Strings(values.clone().into()), true).pipeline,
            &encode_block(&ColumnData::Strings(values.clone().into()), true).bytes,
            &predicate,
        )
        .unwrap()
        .is_none()
    );
    // ...but the wrapper still returns the correct answer via decode.
    assert!(!check(&values, &predicate, true));
}

/// A forged FSST symbol table (here 300 symbols, past the 255 `fsst::Compressor::rebuild_from` asserts on) must
/// surface as a `FormatError` from the predicate kernel, never abort the process.
#[test]
fn a_forged_fsst_symbol_table_refuses_in_the_predicate_kernel() {
    let mut out = Writer::new();
    out.put_u32(1);
    out.put_u8(1);
    out.put_u16(300);
    for index in 0..300u64 {
        out.put_u64(index);
        out.put_u8(2);
    }
    out.put_u32(1);
    out.put_u32(0);
    out.put_u32(1);
    out.put_u8(0);
    let forged = out.into_bytes();

    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String);
    assert!(filter_string_block(pipeline, &forged, &equals("x")).is_err());
}

#[test]
fn raw_string_columns_fall_back_to_decode() {
    let values = long_opaque(32);
    assert_eq!(transform_of(&values, true), Transform::RawString);
    // No equality kernel for raw blocks, but the answer is still correct.
    assert!(!check(&values, &equals("anything"), true));
    assert!(!check(
        &values,
        &StringPredicate::Range {
            lower: Some(bound("a", true)),
            upper: None
        },
        true
    ));
}

/// Every string encoding answers a substring test over its own value arena, and agrees row for row with
/// `str::contains` over the decoded column.
#[test]
fn substring_predicates_are_answered_over_each_encodings_arena() {
    let cases: [(&str, Vec<Option<String>>); 3] = [
        ("dictionary", low_cardinality(96)),
        ("fsst", high_cardinality(128)),
        ("raw", long_opaque(32)),
    ];
    for (name, values) in cases {
        for random_access in [true, false] {
            for needle in ["an", "err", "-1", "zzz", "x"] {
                assert!(
                    check(&values, &contains(needle), random_access),
                    "{name} block should answer `contains {needle}` from its arena"
                );
            }
        }
    }
}

/// A hit that starts inside one value but runs past its end only exists because neighbouring values share one buffer,
/// and is not a match for either of them.
#[test]
fn a_substring_spanning_two_neighbouring_values_is_not_a_match() {
    let values: Vec<Option<String>> = ["hello", "world", "helloworld"]
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect();
    for random_access in [true, false] {
        assert!(check(&values, &contains("owor"), random_access));
        let mask = filter_string_block_or_decode(
            encode_block(&ColumnData::Strings(values.clone().into()), random_access).pipeline,
            &encode_block(&ColumnData::Strings(values.clone().into()), random_access).bytes,
            &contains("owor"),
        )
        .unwrap();
        assert_eq!(mask, vec![false, false, true], "only the joined value contains `owor`");
    }
}

/// A rejected match crossing a boundary may overlap a real match beginning at that boundary. The single-needle finder
/// must restart there, rather than resume after the rejected match and skip the row's valid occurrence.
#[test]
fn a_cross_boundary_hit_does_not_hide_an_overlapping_row_match() {
    let values = vec![Some("a".to_owned()), Some("aaa".to_owned())];
    for random_access in [true, false] {
        let encoded = encode_block(&ColumnData::Strings(values.clone().into()), random_access);
        let predicate = contains("aaa");
        let mask = filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, &predicate).unwrap();
        assert_eq!(mask, vec![false, true]);
        assert_eq!(
            count_string_block_shared(encoded.pipeline, &encoded.bytes, &predicate, None).unwrap(),
            Some(1)
        );
    }
}

/// Empty, one-byte, multibyte UTF-8, overlapping, long, absent, rare, and dense single needles all take the same exact
/// count/mask contract, even though the case-sensitive one-needle implementation uses the specialized SIMD finder.
#[test]
fn specialized_single_needle_shapes_agree_with_decoded_search() {
    let values = vec![
        None,
        Some(String::new()),
        Some("x".to_owned()),
        Some("café 🙂 alpha alpha".to_owned()),
        Some("aaaaaaaa".to_owned()),
        Some("a long needle lives in this one row".to_owned()),
        Some("dense dense dense".to_owned()),
        Some("dense again".to_owned()),
    ];
    for needle in ["", "x", "🙂", "aaa", "a long needle lives", "absent", "alpha", "dense"] {
        for random_access in [true, false] {
            assert!(check(&values, &contains(needle), random_access));
        }
    }
}

/// A conjunction of needles is answered in one pass over the arena, and a value carrying only some of them is no
/// match — including when one needle overlaps another's match.
#[test]
fn a_multi_needle_substring_test_asks_for_every_needle() {
    let values: Vec<Option<String>> = ["alpha beta", "alpha", "beta", "betalpha"]
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect();
    for random_access in [true, false] {
        let encoded = encode_block(&ColumnData::Strings(values.clone().into()), random_access);
        let mask = filter_string_block_or_decode(
            encoded.pipeline,
            &encoded.bytes,
            &contains_all(&["alpha", "beta"], false),
        )
        .unwrap();
        assert_eq!(
            mask,
            vec![true, false, false, true],
            "only the values carrying both needles match"
        );
        // "eta" sits inside "beta": a leftmost-first search would report only "beta" and lose the second needle.
        let overlapping =
            filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, &contains_all(&["beta", "eta"], false))
                .unwrap();
        assert_eq!(
            overlapping,
            vec![true, false, true, true],
            "overlapping needles both count"
        );
    }
}

/// A matcher built to ignore ASCII case matches either case, on the arena path and on the decode fallback alike.
#[test]
fn a_case_insensitive_substring_test_matches_either_case() {
    let values: Vec<Option<String>> = ["Alpha Beta", "ALPHA", "gamma"]
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect();
    for random_access in [true, false] {
        let encoded = encode_block(&ColumnData::Strings(values.clone().into()), random_access);
        let folded =
            filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, &contains_all(&["alpha"], true)).unwrap();
        assert_eq!(folded, vec![true, true, false]);
        let exact =
            filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, &contains_all(&["alpha"], false)).unwrap();
        assert_eq!(exact, vec![false, false, false], "a case-sensitive needle stays exact");
    }
}

/// The empty needle is contained in every present value and in none of the null ones, matching `str::contains`.
#[test]
fn the_empty_substring_matches_every_present_row() {
    for values in [low_cardinality(40), high_cardinality(40), long_opaque(8)] {
        assert!(check(&values, &contains(""), true));
    }
}

#[test]
fn null_rows_never_match_any_predicate() {
    // Rows 0, 7, 14, ... are null in the dictionary fixture; 0, 9, 18, ... in the FSST fixture. Every predicate must
    // leave those rows unselected.
    let dict_values = low_cardinality(40);
    let fsst_values = high_cardinality(40);
    let predicates = [
        contains("an"),
        equals("banana"),
        StringPredicate::NotEquals("banana".to_owned()),
        StringPredicate::Range {
            lower: None,
            upper: None,
        },
    ];
    for predicate in &predicates {
        let dict_mask = filter_string_block_or_decode(
            encode_block(&ColumnData::Strings(dict_values.clone().into()), true).pipeline,
            &encode_block(&ColumnData::Strings(dict_values.clone().into()), true).bytes,
            predicate,
        )
        .unwrap();
        for (index, value) in dict_values.iter().enumerate() {
            if value.is_none() {
                assert!(!dict_mask[index], "null dict row {index} matched {predicate:?}");
            }
        }
        let fsst_mask = check_mask(&fsst_values, predicate);
        for (index, value) in fsst_values.iter().enumerate() {
            if value.is_none() {
                assert!(!fsst_mask[index], "null fsst row {index} matched {predicate:?}");
            }
        }
    }
}

fn check_mask(values: &[Option<String>], predicate: &StringPredicate) -> Vec<bool> {
    let encoded = encode_block(&ColumnData::Strings(values.to_vec().into()), true);
    filter_string_block_or_decode(encoded.pipeline, &encoded.bytes, predicate).unwrap()
}

#[test]
fn empty_and_all_null_columns_are_handled() {
    let empty: Vec<Option<String>> = Vec::new();
    assert!(check_mask(&empty, &equals("x")).is_empty());

    let all_null: Vec<Option<String>> = vec![None; 5];
    let mask = check_mask(&all_null, &equals("x"));
    assert_eq!(mask, vec![false; 5]);
}

#[test]
fn delta_encoded_u64_is_answered_over_packed_lanes() {
    // A constant-step sequence encodes as DELTA, which now has its own compressed-data kernel.
    let values: Vec<u64> = (0..300).map(|i| 5_000 + i * 3).collect();
    let block = encode_block(&ColumnData::U64(values.clone()), false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::DeltaBitpack);

    let predicate = NumericPredicate::Range {
        lower: Some(NumericBound {
            inclusive: true,
            value: 5_300,
        }),
        upper: Some(NumericBound {
            inclusive: false,
            value: 5_600,
        }),
    };
    let mask = filter_numeric_block(block.pipeline, &block.bytes, &predicate)
        .unwrap()
        .expect("DELTA range answered from packed lanes");
    let expected: Vec<bool> = values.iter().map(|v| (5_300..5_600).contains(v)).collect();
    assert_eq!(mask, expected);
    // The fast answer equals the full decode-then-filter reference.
    assert_eq!(mask, predicate.filter_decoded(&ColumnData::U64(values)));
}

#[test]
fn delta_encoded_i64_unzigzags_before_comparing() {
    // Signed values (including negatives) are zigzag-mapped onto the u64 transforms before packing; the kernel must
    // unzigzag each reconstructed lane back to the signed value, not compare the raw zigzag code.
    let values: Vec<i64> = (0..200).map(|i| -500 + i * 4).collect();
    let block = encode_block(&ColumnData::I64(values.clone()), false);
    assert!(matches!(
        block.pipeline.transform().unwrap(),
        Transform::ForBitpack | Transform::DeltaBitpack
    ));

    let predicate = NumericPredicate::Range {
        lower: Some(NumericBound {
            inclusive: true,
            value: -100,
        }),
        upper: Some(NumericBound {
            inclusive: false,
            value: 200,
        }),
    };
    let mask = filter_numeric_block(block.pipeline, &block.bytes, &predicate)
        .unwrap()
        .expect("I64 FOR/DELTA answered over packed lanes");
    let expected: Vec<bool> = values.iter().map(|v| (-100..200).contains(v)).collect();
    assert_eq!(mask, expected);
    assert_eq!(mask, predicate.filter_decoded(&ColumnData::I64(values)));
}

/// The DELTA kernel translates `Equals`/`NotEquals`/`InSet` the same way [`filter_for_block`] does — just against the
/// running prefix sum with no base to rebase against — so every fast answer must still equal the full decode.
#[test]
fn delta_encoded_equals_not_equals_and_in_set_match_the_reference() {
    let unsigned: Vec<u64> = (0..300).map(|i| 5_000 + i * 3).collect();
    let block = encode_block_forced_for_conformance(
        &ColumnData::U64(unsigned.clone()),
        false,
        CascadeStrategy::SizeOptimized,
        Transform::DeltaBitpack,
    )
    .unwrap();
    let hit = unsigned[40];
    let predicates = [
        NumericPredicate::Equals(hit as i128),
        NumericPredicate::Equals(4), // below every value: nothing can match
        NumericPredicate::NotEquals(hit as i128),
        NumericPredicate::InSet(vec![hit as i128, unsigned[100] as i128, -9]),
        NumericPredicate::InSet(Vec::new()),
    ];
    for predicate in &predicates {
        let mask = filter_numeric_block(block.pipeline, &block.bytes, predicate)
            .unwrap()
            .expect("DELTA answered from the running prefix sum");
        assert_eq!(
            mask,
            predicate.filter_decoded(&ColumnData::U64(unsigned.clone())),
            "diverged for {predicate:?}"
        );
    }

    let signed: Vec<i64> = (0..200).map(|i| -500 + i * 4).collect();
    let block = encode_block_forced_for_conformance(
        &ColumnData::I64(signed.clone()),
        false,
        CascadeStrategy::SizeOptimized,
        Transform::DeltaBitpack,
    )
    .unwrap();
    let hit = signed[40];
    let predicates = [
        NumericPredicate::Equals(hit as i128),
        NumericPredicate::Equals(i128::from(i64::MAX)), // beyond the domain
        NumericPredicate::NotEquals(hit as i128),
        NumericPredicate::InSet(vec![hit as i128, signed[100] as i128, i128::from(i64::MIN)]),
    ];
    for predicate in &predicates {
        let mask = filter_numeric_block(block.pipeline, &block.bytes, predicate)
            .unwrap()
            .expect("DELTA answered from the running prefix sum");
        assert_eq!(
            mask,
            predicate.filter_decoded(&ColumnData::I64(signed.clone())),
            "diverged for {predicate:?}"
        );
    }
}

/// Decimal128's compressed-data kernel translates the predicate once into native `i128` intervals
/// ([`NativeRangeTest`]); every family of predicate, including the bound-arithmetic edge cases at `i128::MIN`/`MAX`,
/// must still equal the full decode-then-filter reference.
#[test]
fn decimal128_predicate_kernel_matches_the_reference() {
    let values: Vec<i128> = vec![i128::MIN, -1_000, -1, 0, 1, 1_000, i128::MAX];
    let data = ColumnData::Decimal {
        scale: 2,
        values: values.clone(),
    };
    let block = encode_block(&data, false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::Decimal128);

    let predicates = [
        NumericPredicate::Equals(0),
        NumericPredicate::Equals(i128::MIN),
        NumericPredicate::Equals(i128::MAX),
        NumericPredicate::NotEquals(-1),
        NumericPredicate::InSet(vec![i128::MIN, 1, 999]),
        NumericPredicate::InSet(Vec::new()),
        range(Some((i128::MIN, false)), None), // exclusive lower at MIN: never satisfiable
        range(Some((i128::MIN, true)), None),
        range(None, Some((i128::MAX, false))), // exclusive upper at MAX: never satisfiable
        range(None, Some((i128::MAX, true))),
        range(Some((-1_000, true)), Some((1_000, false))),
        range(Some((-1_000, false)), Some((-1_000, false))), // empty: lo > hi after the bound shift
    ];
    for predicate in &predicates {
        let mask = filter_numeric_block(block.pipeline, &block.bytes, predicate)
            .unwrap()
            .expect("Decimal128 answered from the stored mantissa");
        assert_eq!(mask, predicate.filter_decoded(&data), "diverged for {predicate:?}");
    }
}

#[test]
fn u128_beyond_i128_max_behaves_like_positive_infinity() {
    let huge = i128::MAX as u128 + 1;
    let data = ColumnData::U128(vec![7, huge, u128::MAX, 3]);

    // A lower-only range: the two overflowing values clear any i128 lower bound; the small ones below it do not.
    let lower_only = NumericPredicate::Range {
        lower: Some(NumericBound {
            inclusive: true,
            value: 5,
        }),
        upper: None,
    };
    assert_eq!(lower_only.filter_decoded(&data), vec![true, true, true, false]);

    // An upper bound at i128::MAX excludes the overflowing values (they exceed it) but keeps the small ones.
    let bounded = NumericPredicate::Range {
        lower: None,
        upper: Some(NumericBound {
            inclusive: true,
            value: i128::MAX,
        }),
    };
    assert_eq!(bounded.filter_decoded(&data), vec![true, false, false, true]);

    // Equality against i128::MAX must not spuriously match an overflowing value; `!=` always does.
    assert_eq!(
        NumericPredicate::Equals(i128::MAX).filter_decoded(&data),
        vec![false, false, false, false]
    );
    assert_eq!(
        NumericPredicate::NotEquals(i128::MAX).filter_decoded(&data),
        vec![true, true, true, true]
    );
}

/// A forged FOR block whose base plus a packed delta overflows u64 must refuse from the compressed-data fast path,
/// exactly as the full decode does — never a mask over silently wrapped values. Contract: the fast answer equals the
/// slow path row for row, so both must surface the same structural error.
#[test]
fn a_forged_for_block_that_overflows_the_base_is_rejected_like_the_full_decode() {
    // base = u64::MAX with a single packed delta of 1 overflows base + delta.
    let mut body = Writer::new();
    body.put_u64(u64::MAX);
    crate::encoding::bitpack(&[1u64], 1, &mut body);
    let bytes = body.into_bytes();

    let pipeline = PipelineId::new(Transform::ForBitpack, Compression::None, ValueKind::U64);
    assert!(filter_numeric_block(pipeline, &bytes, &NumericPredicate::Equals(0)).is_err());
    assert!(decode_block(pipeline, &bytes).is_err());
}

/// Builds a FOR bit-packed u64 block directly (base + FastLanes deltas), bypassing transform selection so the test
/// controls the base and width exactly.
fn for_u64_block(values: &[u64]) -> (PipelineId, Vec<u8>) {
    let mut body = Writer::new();
    crate::encoding::encode_for_bitpack(values, &mut body);
    (
        PipelineId::new(Transform::ForBitpack, Compression::None, ValueKind::U64),
        body.into_bytes(),
    )
}

/// Builds a FOR bit-packed i64 block directly: values are zigzag-mapped onto the u64 transform exactly as
/// `encode_ints` does.
fn for_i64_block(values: &[i64]) -> (PipelineId, Vec<u8>) {
    let codes: Vec<u64> = values.iter().map(|v| crate::encoding::zigzag(*v)).collect();
    let mut body = Writer::new();
    crate::encoding::encode_for_bitpack(&codes, &mut body);
    (
        PipelineId::new(Transform::ForBitpack, Compression::None, ValueKind::I64),
        body.into_bytes(),
    )
}

fn range(lower: Option<(i128, bool)>, upper: Option<(i128, bool)>) -> NumericPredicate {
    NumericPredicate::Range {
        lower: lower.map(|(value, inclusive)| NumericBound { inclusive, value }),
        upper: upper.map(|(value, inclusive)| NumericBound { inclusive, value }),
    }
}

/// Every packed-domain FOR answer must equal the decode-then-filter reference row for row — the spec's exactness
/// contract for the compressed-data evaluator.
fn assert_for_block_matches_reference(pipeline: PipelineId, bytes: &[u8], predicates: &[NumericPredicate]) {
    let data = decode_block(pipeline, bytes).unwrap();
    for predicate in predicates {
        let mask = filter_numeric_block(pipeline, bytes, predicate)
            .unwrap()
            .expect("FOR blocks answer from packed lanes");
        assert_eq!(
            mask,
            predicate.filter_decoded(&data),
            "packed-domain answer diverged for {predicate:?}"
        );
    }
}

#[test]
fn for_encoded_u64_answers_every_predicate_shape_over_packed_lanes() {
    let mut state: u64 = 0xA076_1D64_78BD_642F;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    // Values spanning several FastLanes vectors on a large base, so rebased bounds and vector boundaries both bite.
    let base = 5_000_000_000u64;
    let values: Vec<u64> = (0..2500).map(|_| base + (next() % 100_000)).collect();
    let hit = values[1701] as i128;
    let (pipeline, bytes) = for_u64_block(&values);
    let predicates = vec![
        NumericPredicate::Equals(hit),
        NumericPredicate::Equals(3), // below the base: nothing can match
        NumericPredicate::Equals(-7),
        NumericPredicate::Equals(i128::from(u64::MAX) + 5), // beyond the domain
        NumericPredicate::NotEquals(hit),
        NumericPredicate::NotEquals(-1), // unrepresentable: everything differs
        NumericPredicate::InSet(vec![hit, hit, 3, -9, i128::from(u64::MAX) + 1, values[40] as i128]),
        NumericPredicate::InSet(Vec::new()),
        range(Some((hit, true)), Some((hit + 5_000, false))),
        range(Some((hit, false)), Some((hit + 5_000, true))),
        range(None, Some((hit, true))),
        range(Some((hit, true)), None),
        range(Some((-50, true)), Some((3, true))), // wholly below the base
        range(Some((10, true)), Some((5, true))),  // inverted: empty
        range(Some((i128::MIN, false)), Some((i128::MAX, false))), // saturating bound arithmetic
        range(None, None),
    ];
    assert_for_block_matches_reference(pipeline, &bytes, &predicates);
}

#[test]
fn for_encoded_i64_splits_signed_ranges_across_the_zigzag_parity() {
    let mut state: u64 = 0x8500_92C7_44FD_92B5;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    // Values on both sides of zero, plus the extremes, across several vectors.
    let mut values: Vec<i64> = (0..2500).map(|_| (next() % 200_000) as i64 - 100_000).collect();
    values.extend_from_slice(&[i64::MIN, i64::MAX, -1, 0, 1]);
    let hit = i128::from(values[901]);
    let (pipeline, bytes) = for_i64_block(&values);
    let predicates = vec![
        NumericPredicate::Equals(hit),
        NumericPredicate::Equals(i128::from(i64::MIN)),
        NumericPredicate::Equals(i128::from(i64::MAX)),
        NumericPredicate::Equals(i128::from(i64::MAX) + 1), // beyond the domain
        NumericPredicate::NotEquals(hit),
        NumericPredicate::NotEquals(i128::MIN),
        NumericPredicate::InSet(vec![hit, 0, -1, i128::from(i64::MIN), i128::from(u64::MAX) as i128 + 9]),
        range(Some((-500, true)), Some((500, true))), // straddles zero: even and odd halves both live
        range(Some((-500, false)), Some((500, false))),
        range(Some((-99_000, true)), Some((-3, false))), // wholly negative: odd codes only
        range(Some((3, false)), Some((99_000, true))),   // wholly positive: even codes only
        range(None, Some((0, true))),
        range(Some((0, false)), None),
        range(Some((i128::from(i64::MIN), true)), Some((i128::from(i64::MIN), true))), // collapses to a point
        range(Some((i128::MAX - 1, true)), None),                                      // above the domain: empty
        range(None, None),
    ];
    assert_for_block_matches_reference(pipeline, &bytes, &predicates);
}

#[test]
fn a_zero_width_for_block_answers_from_the_base_alone() {
    let values = vec![42u64; 300];
    let (pipeline, bytes) = for_u64_block(&values);
    let predicates = vec![
        NumericPredicate::Equals(42),
        NumericPredicate::Equals(41),
        NumericPredicate::NotEquals(42),
        range(Some((42, false)), None),
        range(None, Some((42, true))),
    ];
    assert_for_block_matches_reference(pipeline, &bytes, &predicates);
}

/// ALP float predicates are answered in the scaled-integer domain; the answer must equal decode-then-filter for every
/// predicate shape, with exceptions judged on their stored bits and escaped vectors on their raw payloads.
#[test]
fn alp_filter_matches_decode_then_filter_including_exceptions_and_escapes() {
    // Vector 0: clean two-decimal values with a sprinkle of unrepresentable exceptions. Vector 1: mostly noise, so the
    // whole vector escapes and stores raw floats. Vector 2 (partial): clean again, crossing zero.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    let mut values: Vec<f64> = Vec::new();
    for i in 0..1024u64 {
        if i % 97 == 3 {
            values.push(std::f64::consts::PI + i as f64);
        } else {
            values.push(((next() % 400_000) as i64 - 200_000) as f64 / 100.0);
        }
    }
    for _ in 0..1024 {
        values.push(f64::from_bits(next()));
    }
    for _ in 0..500 {
        values.push(((next() % 400_000) as i64 - 200_000) as f64 / 100.0);
    }
    values[0] = 1_234.56;
    values[2070] = -0.0;
    values[2071] = 0.0;
    let block = encode_block(&ColumnData::F64(values.clone()), false);
    assert_eq!(
        block.pipeline.transform().unwrap(),
        Transform::Alp,
        "fixture must pick ALP"
    );
    let body = crate::encoding::remove_trailing(block.pipeline.compression().unwrap(), &block.bytes).unwrap();
    assert_eq!(
        body.first().copied(),
        Some(crate::encoding::constant::ALP_VECTOR_ESCAPE_SENTINEL),
        "the noise vector must escape, or this test loses its raw-payload coverage"
    );
    let data = decode_block(block.pipeline, &block.bytes).unwrap();

    let float_range = |lower: Option<(f64, bool)>, upper: Option<(f64, bool)>| FloatPredicate::Range {
        lower: lower.map(|(value, inclusive)| FloatBound { inclusive, value }),
        upper: upper.map(|(value, inclusive)| FloatBound { inclusive, value }),
    };
    let predicates = vec![
        FloatPredicate::Equals(1_234.56),
        FloatPredicate::Equals(1_234.567),
        FloatPredicate::Equals(values[314]), // whatever landed there, exception or not
        FloatPredicate::Equals(values[1500]), // a raw escaped payload
        FloatPredicate::Equals(0.0),
        FloatPredicate::Equals(-0.0),
        FloatPredicate::Equals(f64::NAN),
        FloatPredicate::Equals(f64::INFINITY),
        FloatPredicate::NotEquals(1_234.56),
        FloatPredicate::NotEquals(f64::NAN),
        FloatPredicate::InSet(vec![1_234.56, values[1500], f64::NAN, -0.0]),
        FloatPredicate::InSet(Vec::new()),
        float_range(Some((-350.0, true)), Some((350.0, false))),
        float_range(Some((-350.0, false)), Some((350.0, true))),
        float_range(None, Some((0.0, true))),
        float_range(Some((0.0, false)), None),
        float_range(Some((f64::NAN, true)), None),
        float_range(None, Some((f64::NAN, false))),
        float_range(Some((f64::NEG_INFINITY, true)), Some((f64::INFINITY, true))),
        float_range(Some((1_000.0, true)), Some((1.0, true))), // inverted: empty
        float_range(None, None),
    ];
    for predicate in &predicates {
        let mask = filter_float_block(block.pipeline, &block.bytes, predicate)
            .unwrap()
            .expect("ALP blocks answer in the integer domain");
        let reference = match &data {
            ColumnData::F64(decoded) => predicate.filter_decoded(decoded),
            other => panic!("expected floats, got {other:?}"),
        };
        assert_eq!(mask, reference, "integer-domain answer diverged for {predicate:?}");
    }
}

/// Free-text notes shaped like the benchmark's: a dense needle in about one value per hundred, a rare one in a
/// handful, nulls sprinkled in, and enough of them that the block spans several seekable frames.
fn framed_notes(rows: usize) -> Vec<Option<String>> {
    (0..rows)
        .map(|i| {
            if i % 5 == 0 {
                None
            } else if i % 4_000 == 1 {
                Some(format!(
                    "row {i} escalated to the regulator-callback after a threshold breach"
                ))
            } else if i % 97 == 0 {
                Some(format!(
                    "row {i} raised on the escalation-path after a threshold breach"
                ))
            } else {
                Some(format!("row {i} settled without incident on the standard path"))
            }
        })
        .collect()
}

/// Values too short for the block to carry per-value keys, still numerous enough to span several frames, and of
/// varying length so that frame boundaries cannot all line up with value starts.
fn framed_short(rows: usize) -> Vec<Option<String>> {
    (0..rows)
        .map(|i| (i % 7 != 0).then(|| format!("id-{i:06}{}", "-".repeat(i % 5))))
        .collect()
}

/// How many frame boundaries of a framed FSST block fall inside some value's codes.
fn values_straddling_frames(bytes: &[u8]) -> usize {
    let table = super::super::seekable_zstd::seek_table(bytes).unwrap();
    let body = super::super::remove_trailing(Compression::SeekableZstd, bytes).unwrap();
    let mut reader = Reader::new(&body);
    let header = super::super::read_string_header(Transform::FsstString, &mut reader).unwrap();
    let offsets = super::super::read_offsets(&mut reader, header.present_count).unwrap();
    let data_start = header.offsets_start + (header.present_count + 1) * 4;
    (1..table.num_frames())
        .map(|frame| table.frame_start_decomp(frame).unwrap() as usize)
        .filter(|boundary| {
            offsets
                .windows(2)
                .any(|pair| data_start + pair[0] < *boundary && *boundary < data_start + pair[1])
        })
        .count()
}

/// The frame-by-frame count of an FSST block under seekable frames agrees with the whole-block kernel and with a
/// decode-and-search, for dense, rare, absent, match-all and multi-needle searches, whether or not the block stores
/// per-value keys — and the blocks are built so that values straddle frame boundaries.
#[test]
fn a_framed_fsst_block_counts_substrings_frame_by_frame_like_the_whole_block_kernel() {
    let cases = [
        (
            framed_notes(24_000),
            SideStream::FsstValueKeys,
            vec![
                "escalation-path",
                "regulator-callback",
                "quicksilver-ratchet",
                "",
                "row",
            ],
        ),
        (
            framed_short(40_000),
            SideStream::None,
            vec!["id-0001", "id-039999", "zz", "", "id-"],
        ),
    ];
    for (values, side, needles) in cases {
        let column: StringColumn = values.into();
        let block = encode_block(&ColumnData::Strings(column.clone()), true);
        assert_eq!(block.pipeline.transform().unwrap(), Transform::FsstString);
        assert_eq!(block.pipeline.compression().unwrap(), Compression::SeekableZstd);
        assert_eq!(block.pipeline.side_stream().unwrap(), side);
        let frames = super::super::seekable_zstd::seek_table(&block.bytes)
            .unwrap()
            .num_frames();
        assert!(
            frames >= 3,
            "{side:?}: the block must span several frames, not {frames}"
        );
        assert!(
            values_straddling_frames(&block.bytes) > 0,
            "{side:?}: some value's codes must cross a frame boundary"
        );
        let body = super::super::remove_trailing(Compression::SeekableZstd, &block.bytes).unwrap();
        let mut predicates: Vec<StringPredicate> = needles.iter().map(|needle| contains(needle)).collect();
        predicates.push(contains_all(&[needles[0], "threshold"], false));
        predicates.push(contains_all(&[needles[0].to_ascii_uppercase().as_str()], true));
        for predicate in &predicates {
            let StringPredicate::Contains(matcher) = predicate else {
                unreachable!("every predicate here is a substring test");
            };
            let reference = predicate.filter_decoded(&column).iter().filter(|hit| **hit).count();
            let framed = count_string_block_shared(block.pipeline, &block.bytes, predicate, None)
                .unwrap()
                .expect("the framed kernel answers a substring test");
            assert_eq!(framed, reference, "{side:?}: framed count for {predicate:?}");
            assert_eq!(
                count_fsst_contains(&body, side, matcher).unwrap(),
                reference,
                "{side:?}: whole-block count for {predicate:?}"
            );
        }
    }
}

/// A substring scan of a framed block that stores per-value keys inflates the frames holding its header and offsets,
/// its fingerprints, and its candidate values — never the ones holding the prefix keys, which the writer set apart in
/// frames of their own, as it did the fingerprints, for exactly this.
#[test]
fn a_framed_contains_scan_leaves_the_prefix_key_frames_compressed() {
    let column: StringColumn = framed_notes(24_000).into();
    let block = encode_block(&ColumnData::Strings(column.clone()), true);
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);
    let table = super::super::seekable_zstd::seek_table(&block.bytes).unwrap();
    let body = super::super::remove_trailing(Compression::SeekableZstd, &block.bytes).unwrap();
    let mut reader = Reader::new(&body);
    let header = super::super::read_string_header(Transform::FsstString, &mut reader).unwrap();
    let offsets = super::super::read_offsets(&mut reader, header.present_count).unwrap();
    let data_start = header.offsets_start + (header.present_count + 1) * 4;
    let prefixes_start = data_start + offsets.last().copied().unwrap();
    let fingerprints_start = prefixes_start + header.present_count * PREFIX_KEY_BYTES;
    let frames_over = |start: usize, end: usize| -> BTreeSet<u32> {
        (table.frame_index_decomp(start as u64)..=table.frame_index_decomp(end as u64 - 1)).collect()
    };
    for run_start in [prefixes_start, fingerprints_start] {
        let frame = table.frame_index_decomp(run_start as u64);
        assert_eq!(
            table.frame_start_decomp(frame).unwrap(),
            run_start as u64,
            "a frame starts where the run at {run_start} does"
        );
    }
    let prefix_frames = frames_over(prefixes_start, fingerprints_start);
    let fingerprint_frames = frames_over(fingerprints_start, body.len());
    assert!(prefix_frames.is_disjoint(&fingerprint_frames));
    assert!(prefix_frames.len() >= 2, "the prefix keys span frames of their own");
    // A needle found in every arena frame inflates the header, the offsets, the arena, and the fingerprints; one no
    // fingerprint admits inflates only what it takes to learn that.
    let dense = frames_over(0, prefixes_start).union(&fingerprint_frames).count();
    let pruned = frames_over(0, data_start).union(&fingerprint_frames).count();
    assert!(dense < table.num_frames() as usize && pruned < dense);
    for (needle, expected) in [("escalation-path", dense), ("zz", pruned)] {
        let predicate = contains(needle);
        let reference = predicate.filter_decoded(&column).iter().filter(|hit| **hit).count();
        super::super::seekable_zstd::take_frames_inflated();
        let count = count_string_block_shared(block.pipeline, &block.bytes, &predicate, None)
            .unwrap()
            .unwrap();
        assert_eq!(count, reference, "{needle}: count");
        assert_eq!(
            super::super::seekable_zstd::take_frames_inflated(),
            expected,
            "{needle}: frames inflated"
        );
    }
}

/// A framed block whose frames were overwritten or cut short is refused by the frame-by-frame count, not answered.
#[test]
fn a_damaged_framed_fsst_block_is_refused_by_the_frame_by_frame_count() {
    let block = encode_block(&ColumnData::Strings(framed_notes(24_000).into()), true);
    let frames_len = super::super::seekable_zstd::seek_table(&block.bytes)
        .unwrap()
        .size_comp() as usize;
    let predicate = contains("escalation-path");

    let mut zeroed = block.bytes.clone();
    zeroed[..frames_len].fill(0);
    assert!(count_string_block_shared(block.pipeline, &zeroed, &predicate, None).is_err());

    let mut cut = block.bytes[..frames_len - 10].to_vec();
    cut.extend_from_slice(&block.bytes[frames_len..]);
    assert!(count_string_block_shared(block.pipeline, &cut, &predicate, None).is_err());

    assert!(count_string_block_shared(block.pipeline, &block.bytes[..frames_len], &predicate, None).is_err());
}

/// A scan fans out only over several granules that each carry enough decode work to pay for a worker hand-off.
#[test]
fn a_scan_fans_out_only_over_several_granules_worth_a_worker_each() {
    assert!(!scan_in_parallel(1, u64::MAX));
    assert!(!scan_in_parallel(8, PARALLEL_SCAN_MIN_GRANULE_BYTES - 1));
    assert!(scan_in_parallel(2, PARALLEL_SCAN_MIN_GRANULE_BYTES));
}
