use super::*;
use crate::encoding::{
    CascadeStrategy, Compression, NullStream, NullStreamForm, bitpack, decode_delta_bitpack, encode_block,
    encode_block_forced_for_conformance,
};
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

/// A small xorshift stream, so the randomized key-scan cases replay exactly on every run.
struct KeyScanRng(u64);

impl KeyScanRng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    /// Text over a three-letter alphabet that includes the zero byte the prefix keys pad with, so values tie on all
    /// seven prefix bytes, run shorter than the prefix, and differ from a padded prefix only by length.
    fn text(&mut self, len: usize) -> String {
        (0..len).map(|_| ['a', 'b', '\0'][self.below(3)]).collect()
    }

    /// A value the keys must order: null, empty, shorter than the prefix, or long.
    fn value(&mut self) -> Option<String> {
        match self.below(10) {
            0 => None,
            1 => Some(String::new()),
            2 | 3 => {
                let len = 1 + self.below(9);
                Some(self.text(len))
            }
            _ => {
                let len = 30 + self.below(30);
                Some(self.text(len))
            }
        }
    }

    /// A missing bound, or one on a stored value, a value exactly the prefix width, or a short value, either way
    /// inclusive or exclusive.
    fn bound(&mut self, values: &[Option<String>]) -> Option<StringBound> {
        let value = match self.below(4) {
            0 => return None,
            1 => values
                .get(self.below(values.len()))
                .cloned()
                .flatten()
                .unwrap_or_default(),
            2 => self.text(PREFIX_KEY_PREFIX_LEN),
            _ => {
                let len = self.below(12);
                self.text(len)
            }
        };
        Some(bound(&value, self.below(2) == 0))
    }
}

/// An FSST block over `values` that stores the per-value keys, without a trailing stage.
fn keyed_fsst_block(values: &[Option<String>]) -> (StringColumn, crate::encoding::EncodedBlock) {
    let column: StringColumn = values.to_vec().into();
    let block = encode_block_forced_for_conformance(
        &ColumnData::Strings(column.clone()),
        true,
        CascadeStrategy::NoTrailing,
        Transform::FsstString,
    )
    .unwrap();
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);
    (column, block)
}

/// Row counts around the 64-value scan chunk, so full chunks, short tails and a lone partial chunk all run.
fn key_scan_row_counts() -> [usize; 9] {
    [1, 7, 63, 64, 65, 127, 128, 129, 300]
}

/// The chunked prefix-key range scan agrees row for row with decode-and-filter across random values and bounds:
/// lower-only, upper-only and two-sided ranges, inclusive and exclusive ends, bounds equal to a stored value or to
/// its prefix, and nulls, empty strings and values shorter than the prefix.
#[test]
fn chunked_prefix_key_ranges_agree_with_the_decoded_filter() {
    let mut rng = KeyScanRng(0x9E37_79B9_7F4A_7C15);
    let mut blocks = 0;
    for rows in key_scan_row_counts() {
        for _ in 0..8 {
            let values: Vec<Option<String>> = (0..rows).map(|_| rng.value()).collect();
            // Only a block whose values are long enough on average stores the keys at all.
            if values.iter().flatten().map(String::len).sum::<usize>() < values.iter().flatten().count() * 24 {
                continue;
            }
            blocks += 1;
            let (column, block) = keyed_fsst_block(&values);
            for _ in 0..24 {
                let predicate = StringPredicate::Range {
                    lower: rng.bound(&values),
                    upper: rng.bound(&values),
                };
                let mask = filter_string_block(block.pipeline, &block.bytes, &predicate)
                    .unwrap()
                    .expect("a keyed FSST block answers a range");
                assert_eq!(mask, predicate.filter_decoded(&column), "{rows} rows, {predicate:?}");
            }
        }
    }
    assert!(blocks > 40, "too few keyed blocks were exercised: {blocks}");
}

/// The chunked fingerprint scan finds exactly the rows a decoded search does, in both the mask and the count-only
/// kernels, across chunk tails and needles that are common, rare, absent, or need several fingerprint bits.
#[test]
fn chunked_fingerprint_candidates_agree_with_the_decoded_search() {
    let mut rng = KeyScanRng(0xD1B5_4A32_D192_ED03);
    for rows in key_scan_row_counts() {
        let mut values: Vec<Option<String>> = (0..rows)
            .map(|_| {
                let len = 30 + rng.below(30);
                Some(rng.text(len))
            })
            .collect();
        for value in values.iter_mut().step_by(5) {
            *value = rng.value();
        }
        if let Some(Some(last)) = values.last_mut() {
            last.push_str("-tail-marker");
        }
        let (column, block) = keyed_fsst_block(&values);
        let mut predicates: Vec<StringPredicate> = ["a", "ab\0b", "aaaaaaaa", "z", "-tail-marker", ""]
            .into_iter()
            .map(contains)
            .collect();
        predicates.push(contains_all(&["A", "B"], true));
        for predicate in &predicates {
            let reference = predicate.filter_decoded(&column);
            let mask = filter_string_block(block.pipeline, &block.bytes, predicate)
                .unwrap()
                .expect("a keyed FSST block answers a substring test");
            assert_eq!(mask, reference, "{rows} rows, {predicate:?}");
            let count = count_string_block_shared(block.pipeline, &block.bytes, predicate, None)
                .unwrap()
                .expect("a keyed FSST block counts a substring test");
            assert_eq!(
                count,
                reference.iter().filter(|hit| **hit).count(),
                "{rows} rows, {predicate:?}"
            );
        }
    }
}

/// A stored prefix key whose length byte runs past the prefix width refuses the range scan, even when it sits in a
/// later chunk's tail rather than the first chunk.
#[test]
fn a_forged_prefix_key_length_refuses_the_range_scan() {
    let values: Vec<Option<String>> = (0..100).map(|i| Some(format!("{i:05}-a-long-enough-value"))).collect();
    let (_, block) = keyed_fsst_block(&values);
    let mut forged = block.bytes.clone();
    let prefixes_start = forged.len() - values.len() * (PREFIX_KEY_BYTES + StringFingerprint::STORED_BYTES);
    forged[prefixes_start + 70 * PREFIX_KEY_BYTES + PREFIX_KEY_PREFIX_LEN] = PREFIX_KEY_PREFIX_LEN as u8 + 1;
    let predicate = StringPredicate::Range {
        lower: Some(bound("00010", true)),
        upper: None,
    };
    assert!(filter_string_block(block.pipeline, &block.bytes, &predicate).is_ok());
    assert!(filter_string_block(block.pipeline, &forged, &predicate).is_err());
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

/// A raw DELTA body: the first value, then `codes` (zigzag deltas) bit-packed at `width`, with the stream's count
/// header rewritten to `count` so trailing codes become nonzero padding the filter must ignore.
fn delta_body(first: u64, codes: &[u64], width: u32, count: usize) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.put_u64(first);
    bitpack(codes, width, &mut writer);
    let mut body = writer.into_bytes();
    body[8..12].copy_from_slice(&(count as u32).to_le_bytes());
    body
}

/// The DELTA filter walks the stream one unpacked vector at a time; its verdicts must equal decoding every value and
/// filtering, across vector boundaries, extreme widths, wrapping prefix sums and nonzero padding past the count.
#[test]
fn delta_filter_matches_decode_then_filter_across_vector_boundaries() {
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    for width in [0u32, 1, 63, 64] {
        for count in [0usize, 1, 1023, 1024, 1025, 2048, 2049] {
            for first in [0u64, u64::MAX - 3, i64::MAX as u64 - 1] {
                // Pad the codes to whole vectors with nonzero values; only `count` of them are logical.
                let padded = count.div_ceil(1024).max(1) * 1024;
                let codes: Vec<u64> = (0..padded)
                    .map(|_| if width == 0 { 0 } else { next() & mask(width) | 1 })
                    .collect();
                let body = delta_body(first, &codes, width, count);
                let mut values = vec![first];
                for code in &codes[..count] {
                    let last = *values.last().unwrap();
                    values.push(last.wrapping_add(unzigzag(*code) as u64));
                }
                let hit = values[values.len() / 2];
                for (kind, column) in [
                    (ValueKind::U64, ColumnData::U64(values.clone())),
                    (
                        ValueKind::I64,
                        ColumnData::I64(values.iter().map(|value| unzigzag(*value)).collect()),
                    ),
                ] {
                    let signed_hit = i128::from(unzigzag(hit));
                    let hit = if kind == ValueKind::U64 {
                        i128::from(hit)
                    } else {
                        signed_hit
                    };
                    let predicates = [
                        NumericPredicate::Equals(hit),
                        NumericPredicate::NotEquals(hit),
                        NumericPredicate::InSet(vec![hit, -1, i128::from(u64::MAX)]),
                        NumericPredicate::InSet(Vec::new()),
                        NumericPredicate::Range {
                            lower: Some(NumericBound {
                                inclusive: false,
                                value: hit - 1000,
                            }),
                            upper: Some(NumericBound {
                                inclusive: true,
                                value: hit,
                            }),
                        },
                        NumericPredicate::Range {
                            lower: None,
                            upper: Some(NumericBound {
                                inclusive: false,
                                value: 0,
                            }),
                        },
                    ];
                    for predicate in &predicates {
                        assert_eq!(
                            filter_delta_block(&body, predicate, kind).unwrap(),
                            predicate.filter_decoded(&column),
                            "width {width}, count {count}, first {first}, {kind:?}, {predicate:?}"
                        );
                    }
                }
            }
        }
    }
}

/// The fused DELTA filter, which rebuilds each vector's running totals into a buffer and compares them there, must
/// give the same verdicts as decoding the block and filtering the decoded column, for random deltas of every sign,
/// counts that end mid-vector, and sums that wrap.
#[test]
fn delta_filter_matches_decode_then_filter_decoded() {
    let mut state: u64 = 0xD1B5_4A32_D192_ED03;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for width in [0u32, 3, 20, 64] {
        for count in [0usize, 1, 17, 1000, 1024, 3333] {
            let first = next();
            let codes: Vec<u64> = (0..count).map(|_| next() & mask(width)).collect();
            let mut writer = Writer::new();
            writer.put_u64(first);
            bitpack(&codes, width, &mut writer);
            let body = writer.into_bytes();
            let unsigned = decode_delta_bitpack::<u64>(&mut Reader::new(&body)).unwrap();
            let signed = decode_delta_bitpack::<i64>(&mut Reader::new(&body)).unwrap();
            let pivot = unsigned.get(unsigned.len() / 2).copied().unwrap_or(0);
            for (kind, column, hit) in [
                (ValueKind::U64, ColumnData::U64(unsigned.clone()), i128::from(pivot)),
                (
                    ValueKind::I64,
                    ColumnData::I64(signed.clone()),
                    i128::from(unzigzag(pivot)),
                ),
            ] {
                let predicates = [
                    NumericPredicate::Equals(hit),
                    NumericPredicate::NotEquals(hit),
                    NumericPredicate::InSet(vec![hit, 0, hit + 1]),
                    NumericPredicate::Range {
                        lower: Some(NumericBound {
                            inclusive: true,
                            value: hit,
                        }),
                        upper: None,
                    },
                ];
                for predicate in &predicates {
                    assert_eq!(
                        filter_delta_block(&body, predicate, kind).unwrap(),
                        predicate.filter_decoded(&column),
                        "width {width}, count {count}, {kind:?}, {predicate:?}"
                    );
                }
            }
        }
    }
}

/// The streaming DELTA filter keeps the stream's validation: a width past 64, a truncated header or packed vector and a
/// zero-width count past the block bound are refused, while bytes after the stream are still ignored.
#[test]
fn delta_filter_keeps_its_stream_validation() {
    let predicate = NumericPredicate::NotEquals(0);
    let body = delta_body(7, &[2; 2048], 2, 1500);
    assert_eq!(
        filter_delta_block(&body, &predicate, ValueKind::U64).unwrap().len(),
        1501
    );

    let mut too_wide = body.clone();
    too_wide[12] = 65;
    assert!(filter_delta_block(&too_wide, &predicate, ValueKind::U64).is_err());
    assert!(filter_delta_block(&body[..10], &predicate, ValueKind::U64).is_err());
    let mut three_vectors = body.clone();
    three_vectors[8..12].copy_from_slice(&2100u32.to_le_bytes());
    assert!(filter_delta_block(&three_vectors, &predicate, ValueKind::U64).is_err());
    assert!(filter_delta_block(&body[..body.len() - 1], &predicate, ValueKind::U64).is_err());

    let too_many_zeros = delta_body(7, &[], 0, (1 << 24) + 1);
    assert!(filter_delta_block(&too_many_zeros, &predicate, ValueKind::U64).is_err());
    let zeros = delta_body(7, &[], 0, 3);
    assert_eq!(
        filter_delta_block(&zeros, &predicate, ValueKind::U64).unwrap(),
        vec![true; 4]
    );

    let mut trailing = body.clone();
    trailing.extend_from_slice(&[0xFF; 5]);
    assert_eq!(
        filter_delta_block(&trailing, &predicate, ValueKind::U64).unwrap(),
        filter_delta_block(&body, &predicate, ValueKind::U64).unwrap()
    );
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
        // Unsorted, repeated, adjacent and NaN members, and sets large enough to need the merged-interval search.
        FloatPredicate::InSet(vec![
            values[2400],
            values[10],
            values[2400],
            0.0,
            -0.0,
            values[10],
            f64::INFINITY,
        ]),
        FloatPredicate::InSet(vec![0.01, 0.02, 0.03, 0.0, f64::NAN, 0.02]),
        FloatPredicate::InSet(values.iter().step_by(7).rev().copied().collect()),
        FloatPredicate::InSet(
            values
                .iter()
                .step_by(3)
                .chain(values.iter().step_by(5))
                .copied()
                .collect(),
        ),
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

/// An ALP `IN` set translates to ascending, disjoint intervals: duplicates and adjacent members collapse once at
/// translation, so a lane is answered by one search however the set was written.
#[test]
fn alp_in_sets_translate_to_merged_intervals() {
    let power = ALP_POWERS[2];
    let test = AlpIntTest::translate(
        &FloatPredicate::InSet(vec![0.05, 0.01, 0.02, 0.01, f64::NAN, -0.03]),
        power,
    );
    assert!(!test.negate);
    assert_eq!(test.ranges, vec![(-3, -3), (1, 2), (5, 5)]);
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

/// Multi-needle and case-insensitive matching gives the same verdict as checking each needle on its own, on both sides
/// of the one-word stack mark (63/64/65 needles) and well past it, with repeated, overlapping and empty needles, empty
/// values and multibyte text.
#[test]
fn multi_needle_matching_agrees_with_per_needle_checks_across_mark_sizes() {
    let contains_each = |needles: &[String], value: &str, fold: bool| {
        let value = if fold {
            value.to_ascii_lowercase()
        } else {
            value.to_owned()
        };
        needles.iter().all(|needle| {
            let needle = if fold {
                needle.to_ascii_lowercase()
            } else {
                needle.clone()
            };
            value.contains(&needle)
        })
    };
    let all: String = (0..200).map(|i| format!("<{i}>")).collect();
    let values = [
        String::new(),
        all.clone(),
        all.to_ascii_uppercase(),
        all.replace("<64>", ""),
        format!("\u{e9}\u{e9}{all}\u{1F600}"),
        "abcabc".to_owned(),
    ];
    for count in [1usize, 2, 63, 64, 65, 130] {
        let mut needles: Vec<String> = (0..count).map(|i| format!("<{}>", i % 66)).collect();
        if count > 1 {
            needles[count - 1] = String::new();
        }
        for case_insensitive in [false, true] {
            let matcher = ContainsMatcher::new(needles.clone(), case_insensitive);
            for value in &values {
                assert_eq!(
                    matcher.matches(value),
                    contains_each(&needles, value, case_insensitive),
                    "{count} needles, case-insensitive {case_insensitive}"
                );
            }
        }
    }
    let overlapping = ContainsMatcher::new(vec!["abca".to_owned(), "cab".to_owned(), "\u{e9}".to_owned()], true);
    assert!(!overlapping.matches("abcabc"));
    assert!(overlapping.matches("ABCABC\u{e9}"));
}

/// The merge as it was before it compacted in place: sort, then copy each merged run into a fresh vector.
fn merge_intervals_by_copying<T: Copy + Ord>(mut intervals: Vec<(T, T)>, after: fn(T) -> T) -> Vec<(T, T)> {
    intervals.sort_unstable_by_key(|&(lo, _)| lo);
    let mut merged: Vec<(T, T)> = Vec::with_capacity(intervals.len());
    for (lo, hi) in intervals {
        match merged.last_mut() {
            Some((_, last_hi)) if lo <= after(*last_hi) => {
                if hi > *last_hi {
                    *last_hi = hi;
                }
            }
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// Interval lists covering empty, singleton, duplicate, equal-start, nested, overlapping, adjacent, disjoint and
/// shuffled inputs, built from `points` (which should include the type's extremes) plus deterministic pseudo-random
/// picks among them.
fn interval_cases<T: Copy + Ord>(points: &[T]) -> Vec<Vec<(T, T)>> {
    let ordered = |a: T, b: T| if a <= b { (a, b) } else { (b, a) };
    let mut cases: Vec<Vec<(T, T)>> = vec![Vec::new()];
    for &a in points {
        cases.push(vec![(a, a)]);
        cases.push(vec![(a, a), (a, a)]);
        for &b in points {
            cases.push(vec![ordered(a, b)]);
            cases.push(vec![ordered(a, b), ordered(a, b)]);
            cases.push(vec![(a, a), ordered(a, b)]);
            cases.push(vec![ordered(b, a), (b, b), (a, a)]);
        }
    }
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for len in 0..40 {
        for _ in 0..25 {
            let case = (0..len)
                .map(|_| {
                    let a = points[(next() % points.len() as u64) as usize];
                    let b = points[(next() % points.len() as u64) as usize];
                    ordered(a, b)
                })
                .collect();
            cases.push(case);
        }
    }
    cases
}

#[test]
fn merging_intervals_in_place_matches_merging_by_copying() {
    let u64_points = [
        u64::MIN,
        1,
        2,
        3,
        4,
        5,
        7,
        8,
        10,
        11,
        100,
        u64::MAX / 2,
        u64::MAX - 2,
        u64::MAX - 1,
        u64::MAX,
    ];
    for case in interval_cases(&u64_points) {
        let expected = merge_intervals_by_copying(case.clone(), |hi: u64| hi.saturating_add(1));
        assert_eq!(merge_intervals(case.clone()), expected, "u64 input {case:?}");
    }

    let i128_points = [
        i128::MIN,
        i128::MIN + 1,
        i128::MIN + 2,
        -5,
        -2,
        -1,
        0,
        1,
        2,
        3,
        5,
        6,
        i128::MAX - 2,
        i128::MAX - 1,
        i128::MAX,
    ];
    for case in interval_cases(&i128_points) {
        let expected = merge_intervals_by_copying(case.clone(), |hi: i128| hi.saturating_add(1));
        assert_eq!(merge_intervals_i128(case.clone()), expected, "i128 input {case:?}");
    }
}

/// The row-mask weave as it stood before it took the verdicts by value: a second mask built row by row for every null
/// form. Kept here as the reference the current weave must match, mask for mask and error for error.
fn reference_weave_present(nulls: &NullStream, present: &[bool]) -> Result<Vec<bool>, FormatError> {
    let mut mask = Vec::with_capacity(nulls.row_count());
    let mut verdicts = present.iter();
    nulls.each_row(|set| {
        if set {
            let verdict = verdicts.next().ok_or(FormatError::Structural {
                rule: "null bitmap disagrees with present count",
            })?;
            mask.push(*verdict);
        } else {
            mask.push(false);
        }
        Ok(())
    })?;
    if verdicts.next().is_some() {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    Ok(mask)
}

/// Whether row `i` carries a value, for each named null shape the weave tests sweep.
/// A named rule saying whether row `n` holds a value.
type NullShape = (&'static str, fn(usize) -> bool);

fn null_shapes() -> [NullShape; 4] {
    [
        ("all absent", |_| false),
        ("all present", |_| true),
        ("clustered", |i| (i / 100) % 3 != 1),
        ("scattered", |i| i % 2 == 0),
    ]
}

/// Null streams in each of the four stored forms, over row counts on both sides of a packed vector's 1024 rows.
fn null_streams_of_every_form() -> Vec<(String, NullStream<'static>)> {
    let mut streams = vec![(
        "all present x0".to_owned(),
        NullStream {
            form: NullStreamForm::AllPresent,
            row_count: 0,
        },
    )];
    for rows in [1usize, 2, 1023, 1024, 1025] {
        for (shape, present) in null_shapes() {
            let values: StringColumn = (0..rows).map(|i| present(i).then(|| format!("v{i}"))).collect();
            streams.push((format!("{shape} x{rows}"), NullStream::from_values(&values)));
        }
    }
    streams
}

/// Weaving takes its verdicts by value and must still give the same mask, or the same error, as the copying weave for
/// every null form: too few, exactly enough, and too many verdicts, all-true, all-false, and mixed.
#[test]
fn weaving_owned_verdicts_matches_the_copying_weave_for_every_null_form() {
    let mut forms = BTreeSet::new();
    for (name, nulls) in null_streams_of_every_form() {
        forms.insert(match nulls.form {
            NullStreamForm::AllAbsent => "all absent",
            NullStreamForm::AllPresent => "all present",
            NullStreamForm::Raw(_) => "raw",
            NullStreamForm::Runs(_) => "runs",
        });
        let present_count = nulls.present_count();
        for len in [present_count.saturating_sub(1), present_count, present_count + 1] {
            let verdict_shapes: [fn(usize) -> bool; 3] = [|_| true, |_| false, |i| i % 3 == 0];
            for verdict in verdict_shapes {
                let verdicts: Vec<bool> = (0..len).map(verdict).collect();
                assert_eq!(
                    weave_present(&nulls, verdicts.clone()),
                    reference_weave_present(&nulls, &verdicts),
                    "{name}: {len} verdicts for {present_count} present rows"
                );
            }
        }
    }
    assert_eq!(forms.len(), 4, "every null form is exercised: {forms:?}");
}

/// When every row is present the verdicts already are the row mask, so weaving hands back the very vector it was
/// given (same allocation, same capacity, same length), and refuses one verdict too few or too many with the same
/// error as before.
#[test]
fn weaving_an_all_present_block_returns_the_verdict_vector_itself() {
    for rows in [1usize, 2, 1023, 1024, 1025] {
        let values: StringColumn = (0..rows).map(|i| Some(format!("v{i}"))).collect();
        let nulls = NullStream::from_values(&values);
        assert!(matches!(nulls.form, NullStreamForm::AllPresent), "x{rows}: all present");
        let mut verdicts = Vec::with_capacity(rows + 64);
        verdicts.extend((0..rows).map(|i| i % 3 == 0));
        let expected = verdicts.clone();
        let (pointer, capacity) = (verdicts.as_ptr(), verdicts.capacity());
        let mask = weave_present(&nulls, verdicts).unwrap();
        assert_eq!(mask.as_ptr(), pointer, "x{rows}: same allocation");
        assert_eq!(mask.capacity(), capacity, "x{rows}: same capacity");
        assert_eq!(mask, expected, "x{rows}: same verdicts");
        for len in [rows - 1, rows + 1] {
            assert_eq!(
                weave_present(&nulls, vec![true; len]),
                Err(FormatError::Structural {
                    rule: "null bitmap disagrees with present count",
                }),
                "x{rows}: {len} verdicts refused"
            );
        }
    }
}

/// Every string predicate kernel that weaves its verdicts into a row mask must agree with decode-then-filter under
/// each null form, at row counts on both sides of a packed vector, across dictionary, FSST (with and without
/// per-value keys) and raw blocks, including present empty strings and non-ASCII text.
#[test]
fn string_predicate_masks_agree_with_decoding_under_every_null_form() {
    let datasets: [fn(usize) -> String; 4] = [
        |i| ["apple", "banana", "", "d\u{e9}j\u{e0}"][i % 4].to_owned(),
        |i| format!("id-{i:05}"),
        |i| format!("{i:05}-order-line-reference-{i:05}"),
        |i| format!("{}-{i}", "x".repeat(80)),
    ];
    let predicates = [
        equals("banana"),
        equals("id-00007"),
        StringPredicate::NotEquals("apple".to_owned()),
        StringPredicate::InSet(vec!["apple".to_owned(), "id-00002".to_owned(), "apple".to_owned()]),
        contains("an"),
        contains("0"),
        contains(""),
        contains("\u{e9}"),
        StringPredicate::Range {
            lower: Some(bound("00100", true)),
            upper: Some(bound("id-00500", false)),
        },
    ];
    for rows in [1usize, 2, 1023, 1024, 1025] {
        for (_, present) in null_shapes() {
            for value in datasets {
                let values: Vec<Option<String>> = (0..rows).map(|i| present(i).then(|| value(i))).collect();
                for predicate in &predicates {
                    for random_access in [false, true] {
                        check(&values, predicate, random_access);
                    }
                }
            }
        }
    }
}

/// A deterministic stream of pseudo-random u64s, so the randomized kernel tests are reproducible.
fn lcg(seed: u64) -> impl FnMut() -> u64 {
    let mut state = seed;
    move || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state >> 11
    }
}

/// The FOR answer for every predicate must equal `matches_i128` on each decoded value, row for row.
fn assert_for_block_matches_i128(pipeline: PipelineId, bytes: &[u8], values: &[i128], predicates: &[NumericPredicate]) {
    for predicate in predicates {
        let mask = filter_numeric_block(pipeline, bytes, predicate)
            .unwrap()
            .expect("FOR blocks answer from packed lanes");
        let reference: Vec<bool> = values.iter().map(|v| predicate.matches_i128(*v)).collect();
        assert_eq!(mask, reference, "packed lane kernel diverged for {predicate:?}");
    }
}

/// Each specialised packed lane kernel (point, negated point, one interval, small sets of both padded sizes, the
/// parity-split zigzag range) and the binary-search fallback agree with the reference over a block whose length is
/// not a multiple of 1024, so the padded tail of the last vector is unpacked and then dropped.
#[test]
fn specialised_packed_lane_kernels_agree_with_the_reference() {
    let mut next = lcg(0x5851_F42D_4C95_7F2D);
    let base = 1_000_000u64;
    let values: Vec<u64> = (0..3077).map(|_| base + next() % 5_000).collect();
    let wide: Vec<i128> = values.iter().map(|v| i128::from(*v)).collect();
    let hit = wide[2900];
    let set = |len: i128| NumericPredicate::InSet((0..len).map(|i| wide[0] + i * 37 - 200).collect());
    let predicates = vec![
        NumericPredicate::Equals(hit),
        NumericPredicate::Equals(i128::from(base) - 1),
        NumericPredicate::Equals(i128::from(u64::MAX)),
        NumericPredicate::NotEquals(hit),
        NumericPredicate::NotEquals(i128::from(u64::MAX) + 1),
        range(Some((hit - 300, true)), Some((hit + 300, false))),
        range(Some((hit, true)), Some((hit, true))),
        range(Some((0, true)), Some((i128::from(u64::MAX), true))),
        range(Some((i128::from(u64::MAX), false)), None),
        range(Some((i128::MIN, false)), Some((i128::from(base) + 2, false))),
        range(Some((hit, true)), Some((hit - 1, true))),
        set(2),
        set(4),
        set(5),
        set(16),
        set(17),
        set(40),
        // Adjacent members merge into wider intervals before the set is padded.
        NumericPredicate::InSet(vec![
            hit,
            hit + 1,
            hit + 2,
            wide[7],
            wide[7] + 1,
            -5,
            i128::from(u64::MAX) + 3,
        ]),
        NumericPredicate::InSet(Vec::new()),
    ];
    let (pipeline, bytes) = for_u64_block(&values);
    assert_for_block_matches_i128(pipeline, &bytes, &wide, &predicates);

    let signed: Vec<i64> = (0..3077).map(|_| (next() % 6_000) as i64 - 3_000).collect();
    let wide: Vec<i128> = signed.iter().map(|v| i128::from(*v)).collect();
    let hit = wide[3070];
    let set = |len: i128| NumericPredicate::InSet((0..len).map(|i| i * 41 - 400).collect());
    let predicates = vec![
        NumericPredicate::Equals(hit),
        NumericPredicate::Equals(-hit - 1),
        NumericPredicate::NotEquals(hit),
        NumericPredicate::NotEquals(i128::from(i64::MIN) - 1),
        range(Some((-700, true)), Some((900, false))),
        range(Some((-700, false)), Some((-2, true))),
        range(Some((0, true)), Some((1_500, true))),
        range(Some((i128::from(i64::MIN), true)), None),
        range(None, Some((i128::from(i64::MAX), false))),
        range(Some((i128::MIN, true)), Some((i128::MIN, false))),
        range(Some((i128::MAX, false)), None),
        range(Some((5, true)), Some((5, true))),
        set(3),
        set(4),
        set(9),
        set(16),
        set(25),
    ];
    let (pipeline, bytes) = for_i64_block(&signed);
    assert_for_block_matches_i128(pipeline, &bytes, &wide, &predicates);
}

/// A set of ranges that mixes plain delta points with a parity-split zigzag pair has no specialised kernel; it falls
/// back to the binary search, which still matches the scalar per-lane test in either polarity.
#[test]
fn mixed_delta_points_and_zigzag_ranges_fall_back_to_the_search() {
    let mut next = lcg(0x2127_599B_F432_5C37);
    let deltas: Vec<u64> = (0..1500).map(|_| next() % 512).collect();
    let mut body = Writer::new();
    bitpack(&deltas, 9, &mut body);
    let bytes = body.into_bytes();
    let stream = read_packed_stream(&mut Reader::new(&bytes)).unwrap();
    let base = 40;
    for negate in [false, true] {
        let test = PackedLaneTest {
            negate,
            ranges: MergedLaneRanges::from_ranges(vec![
                PackedLaneRange::Delta { hi: 7, lo: 7 },
                PackedLaneRange::Zigzag {
                    even: (100, 300),
                    odd: (151, 401),
                },
            ]),
        };
        let mut out = vec![false; stream.padded_count()];
        test.apply(&stream, base, &mut out);
        out.truncate(deltas.len());
        let expected: Vec<bool> = deltas.iter().map(|delta| test.matches(base, *delta)).collect();
        assert_eq!(out, expected);
    }
}

/// The decoded integer filters (constant, point, negated point, one range, both padded small-set sizes and the
/// binary-search fallback) agree with `matches_i128` row for row, including bounds outside each type's domain,
/// exclusive bounds at the `i128` extremes, empty ranges, and `IN` members no value of the type can equal.
#[test]
fn decoded_integer_filters_agree_with_the_reference() {
    let mut next = lcg(0x9FB2_1C65_1E98_DF25);
    let mut unsigned: Vec<u64> = (0..3001).map(|_| next() % 4_000).collect();
    unsigned.extend_from_slice(&[0, 1, u64::MAX, u64::MAX - 1, 1 << 63]);
    let mut signed: Vec<i64> = (0..3001).map(|_| (next() % 4_000) as i64 - 2_000).collect();
    signed.extend_from_slice(&[i64::MIN, i64::MIN + 1, i64::MAX, i64::MAX - 1, -1, 0, 1]);
    let set = |len: i128, start: i128| NumericPredicate::InSet((0..len).map(|i| start + i * 53).collect());
    let extremes = [
        i128::MIN,
        i128::from(i64::MIN) - 1,
        i128::from(i64::MIN),
        -1,
        0,
        i128::from(i64::MAX),
        i128::from(u64::MAX),
        i128::from(u64::MAX) + 1,
        i128::MAX,
    ];
    let mut predicates = vec![
        NumericPredicate::InSet(Vec::new()),
        NumericPredicate::InSet(vec![-1, i128::from(u64::MAX) + 1, i128::MIN]),
        NumericPredicate::InSet(vec![
            5,
            6,
            7,
            8,
            -3,
            -2,
            100,
            i128::from(i64::MIN),
            i128::from(u64::MAX),
        ]),
        range(None, None),
        range(Some((10, true)), Some((9, true))),
        range(Some((-300, true)), Some((900, false))),
        range(Some((-300, false)), Some((-300, true))),
    ];
    for len in [2, 4, 5, 16, 17, 40] {
        predicates.push(set(len, -900));
        predicates.push(set(len, 100));
    }
    for &n in &extremes {
        predicates.push(NumericPredicate::Equals(n));
        predicates.push(NumericPredicate::NotEquals(n));
        for inclusive in [false, true] {
            predicates.push(range(Some((n, inclusive)), None));
            predicates.push(range(None, Some((n, inclusive))));
            predicates.push(range(Some((n, inclusive)), Some((1_000, true))));
            predicates.push(range(Some((-1_000, true)), Some((n, inclusive))));
        }
    }
    for value in [unsigned[17], unsigned[2999]] {
        predicates.push(NumericPredicate::Equals(i128::from(value)));
        predicates.push(NumericPredicate::NotEquals(i128::from(value)));
    }
    for value in [signed[17], signed[2999]] {
        predicates.push(NumericPredicate::Equals(i128::from(value)));
        predicates.push(NumericPredicate::NotEquals(i128::from(value)));
    }
    let unsigned_data = ColumnData::U64(unsigned.clone());
    let signed_data = ColumnData::I64(signed.clone());
    for predicate in &predicates {
        let expected: Vec<bool> = unsigned
            .iter()
            .map(|v| predicate.matches_i128(i128::from(*v)))
            .collect();
        assert_eq!(
            predicate.filter_decoded(&unsigned_data),
            expected,
            "u64 diverged for {predicate:?}"
        );
        let expected: Vec<bool> = signed.iter().map(|v| predicate.matches_i128(i128::from(*v))).collect();
        assert_eq!(
            predicate.filter_decoded(&signed_data),
            expected,
            "i64 diverged for {predicate:?}"
        );
    }
}

/// The decoded float filters agree with `matches_f64` row for row, with NaN, signed zeros, infinities, subnormals
/// and the largest finite values both in the data and as bounds or set members.
#[test]
fn decoded_float_filters_agree_with_the_reference() {
    let mut next = lcg(0x1405_7B7E_F767_814F);
    let specials = [
        f64::NAN,
        -f64::NAN,
        0.0,
        -0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::MAX,
        f64::MIN,
        1.5,
        -2.25,
    ];
    let mut values: Vec<f64> = (0..3001).map(|_| (next() % 2_000) as f64 / 4.0 - 250.0).collect();
    values.extend((0..500).map(|_| f64::from_bits(next() << 11 ^ next())));
    values.extend_from_slice(&specials);
    let float_range = |lower: Option<(f64, bool)>, upper: Option<(f64, bool)>| FloatPredicate::Range {
        lower: lower.map(|(value, inclusive)| FloatBound { inclusive, value }),
        upper: upper.map(|(value, inclusive)| FloatBound { inclusive, value }),
    };
    let mut predicates = vec![
        FloatPredicate::InSet(Vec::new()),
        FloatPredicate::InSet(vec![f64::NAN]),
        FloatPredicate::InSet(vec![-0.0, 1.5, f64::NAN, values[9]]),
        FloatPredicate::InSet(vec![0.0, -2.25, f64::NAN, values[9], values[10]]),
        FloatPredicate::InSet(values.iter().step_by(150).copied().collect()),
        FloatPredicate::InSet(values.iter().step_by(40).copied().chain(specials).collect()),
        float_range(None, None),
        float_range(Some((-10.0, true)), Some((10.0, false))),
        float_range(Some((10.0, true)), Some((-10.0, true))),
    ];
    for &n in specials.iter().chain(&[values[3], values[3002]]) {
        predicates.push(FloatPredicate::Equals(n));
        predicates.push(FloatPredicate::NotEquals(n));
        for inclusive in [false, true] {
            predicates.push(float_range(Some((n, inclusive)), None));
            predicates.push(float_range(None, Some((n, inclusive))));
            predicates.push(float_range(Some((n, inclusive)), Some((100.0, true))));
            predicates.push(float_range(Some((-100.0, false)), Some((n, inclusive))));
        }
    }
    for predicate in &predicates {
        let expected: Vec<bool> = values.iter().map(|v| predicate.matches_f64(*v)).collect();
        assert_eq!(
            predicate.filter_decoded(&values),
            expected,
            "f64 diverged for {predicate:?}"
        );
    }
}
