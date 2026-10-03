use super::*;

#[test]
fn whole_token_lookup_has_no_false_negatives() {
    let values = ["the quick brown fox", "jumped over the lazy dog"];
    let index = TextTokenIndex::build_from_values(&values, false);
    // Every token in the source text must be found.
    for token in ["the", "quick", "brown", "fox", "jumped", "over", "lazy", "dog"] {
        assert!(
            index.might_contain_token(token),
            "token '{token}' must be found in the index"
        );
    }
}

#[test]
fn absent_token_may_be_false_positive_but_never_false_negative() {
    // Build over a small, controlled set so we can reason about what is absent.
    let values = ["alpha beta"];
    let index = TextTokenIndex::build_from_values(&values, false);
    // The inserted tokens must be found.
    assert!(index.might_contain_token("alpha"));
    assert!(index.might_contain_token("beta"));
    // A completely different token is almost certainly absent.  The filter can return true (false positive) but must
    // never return false for an inserted token. We can only assert the negative direction: if we inserted it, we find
    // it. (We cannot assert absence without knowing the exact hash collision behaviour.)
}

#[test]
fn ngram_layer_enables_substring_pruning() {
    let values = ["harana analytics platform"];
    let index = TextTokenIndex::build_from_values(&values, true);
    assert!(index.has_ngrams(), "n-gram flag must be recorded");
    // A substring whose trigrams all appear in "analytics" must survive.
    assert!(index.might_contain_substring("ana")); // trigrams of "analytics" include "ana"
    // A substring with a trigram that is definitely absent should be pruned. "zqx" is not a trigram of any word in our
    // values.
    assert!(!index.might_contain_substring("zqx"));
}

#[test]
fn substring_spanning_token_boundary_is_found() {
    let values = ["hello world"];
    let index = TextTokenIndex::build_from_values(&values, true);
    // "lo wo" straddles the space between "hello" and "world"; its trigrams must still be present.
    assert!(index.might_contain_substring("lo wo"));
    assert!(index.might_contain_substring("hello world"));
}

#[test]
fn without_ngrams_substring_check_conservatively_keeps_the_block() {
    let values = ["hello world"];
    let index = TextTokenIndex::build_from_values(&values, false);
    assert!(!index.has_ngrams());
    // Without n-gram data the index has no information about substrings, so it conservatively keeps the block (returns
    // true) for any substring query.
    assert!(index.might_contain_substring("zzz"));
    assert!(index.might_contain_substring("world"));
}

#[test]
fn exactness_is_always_inexact_no_false_negative() {
    let index = TextTokenIndex::build_from_values(&["test"], false);
    assert_eq!(index.exactness(), Exactness::InexactNoFalseNegative);
}

#[test]
fn round_trip_without_ngrams() {
    let values = ["one two three", "four five"];
    let index = TextTokenIndex::build_from_values(&values, false);
    let bytes = index.encode();
    let decoded = TextTokenIndex::decode(&bytes).expect("decode must succeed");
    assert_eq!(decoded, index);
    assert!(!decoded.has_ngrams());
    assert!(decoded.might_contain_token("one"));
    assert!(decoded.might_contain_token("five"));
}

#[test]
fn round_trip_with_ngrams() {
    let values = ["searching for substrings"];
    let index = TextTokenIndex::build_from_values(&values, true);
    let bytes = index.encode();
    let decoded = TextTokenIndex::decode(&bytes).expect("decode must succeed");
    assert_eq!(decoded, index);
    assert!(decoded.has_ngrams());
    assert!(decoded.might_contain_token("searching"));
}

#[test]
fn decode_rejects_wrong_magic() {
    let bad = [0u8; 16];
    assert!(TextTokenIndex::decode(&bad).is_err());
}

#[test]
fn tokenize_splits_on_whitespace_and_punctuation() {
    let tokens: Vec<Cow<'_, str>> = tokenize("hello, world! foo-bar").collect();
    assert!(tokens.iter().any(|t| t == "hello"));
    assert!(tokens.iter().any(|t| t == "world"));
    assert!(tokens.iter().any(|t| t == "foo"));
    assert!(tokens.iter().any(|t| t == "bar"));
    // Apostrophe is not a separator so "don't" stays as one token.
    let tokens2: Vec<Cow<'_, str>> = tokenize("don't stop").collect();
    assert!(tokens2.iter().any(|t| t == "don't"));
    assert!(tokens2.iter().any(|t| t == "stop"));
}

#[test]
fn tokenize_lowercases_tokens() {
    let tokens: Vec<Cow<'_, str>> = tokenize("Foo BAR baZ").collect();
    assert_eq!(tokens, ["foo", "bar", "baz"]);
}

#[test]
fn token_lookup_is_case_insensitive() {
    let index = TextTokenIndex::build_from_values(&["Foo Bar"], false);
    for query in ["foo", "FOO", "Foo", "bar", "BAR"] {
        assert!(
            index.might_contain_token(query),
            "query '{query}' must match regardless of case"
        );
    }
}

#[test]
fn substring_lookup_is_case_insensitive() {
    let index = TextTokenIndex::build_from_values(&["Analytics"], true);
    // Trigrams of the lowercased token must be reachable regardless of the query's case.
    assert!(index.might_contain_substring("ana"));
    assert!(index.might_contain_substring("ANA"));
    assert!(index.might_contain_substring("Nal"));
}

#[test]
fn substring_probe_survives_context_sensitive_case_folding() {
    // `str::to_lowercase` folds a Greek capital sigma to `ς` at word end but `σ` elsewhere. The needle "ΒΟΣ" folded
    // in isolation would probe the trigram "βος", which the value "ΒΟΣΚΟΣ" (folding to "βοσκοσ...") never inserted —
    // pruning a block that contains the exact substring. Both sides must fold context-free (issue #4037).
    let index = TextTokenIndex::build_from_values(&["ΒΟΣΚΟΣ"], true);
    assert!(
        index.might_contain_substring("ΒΟΣ"),
        "a needle ending in capital sigma must not be pruned from a value that contains it"
    );
    // A needle typed with the final-sigma form must fold to the same trigrams as the medial form.
    assert!(index.might_contain_substring("βοσκος"));
    assert!(index.might_contain_substring("ΒΟΣΚΟΣ"));
}

#[test]
fn ngrams_produces_correct_windows() {
    let windows: Vec<&str> = ngrams("abcde", 3).collect();
    assert_eq!(windows, ["abc", "bcd", "cde"]);
}

#[test]
fn ngrams_on_short_string_is_empty() {
    let windows: Vec<&str> = ngrams("ab", 3).collect();
    assert!(windows.is_empty());
}

#[test]
fn hash_token_is_stable() {
    assert_eq!(hash_token("hello"), hash_token("hello"));
    assert_ne!(hash_token("hello"), hash_token("world"));
}

#[test]
fn build_from_empty_is_sound() {
    let index = TextTokenIndex::build_from_values(&[], false);
    // An absent token in an empty index must not report false negatives. (The Bloom filter over zero keys is still a
    // valid filter; an inserted token would always be found, and here there are none.)
    let _ = index.encode();
}

#[test]
fn folding_a_token_into_the_hash_matches_lowercasing_it_first() {
    // The build and probe paths fold case as they hash instead of materializing a lowercased token. A token that
    // hashed differently either way would make a filter built by one path report false negatives to the other.
    for token in [
        "",
        "foo",
        "Foo",
        "BAR",
        "don't",
        "MiXeD123",
        "ÄpFeL",
        "Straße",
        "ΒΟΣΚΟΣ",
        "İstanbul",
    ] {
        assert_eq!(
            hash_folded_token(token),
            hash_token(&token.to_lowercase()),
            "folded hash of '{token}' must match the hash of its lowercased form"
        );
    }
}

#[test]
fn build_from_values_produces_the_same_filter_as_lowercasing_every_token() {
    // Byte-identical output to hashing `tokenize`'s lowercased tokens, so filters written before and after the
    // allocation-free build path are interchangeable on disk.
    let values = ["The Quick BROWN fox", "Ünïcödé ΣΤΙΓΜΑΣ text", "don't STOP -- ever!"];
    let mut expected: Vec<u64> = values
        .iter()
        .flat_map(|v| tokenize(v))
        .map(|t| hash_token(&t))
        .collect();
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(
        TextTokenIndex::build_from_values(&values, false).encode(),
        TextTokenIndex::build(&expected, false).encode()
    );
}

#[test]
fn optimized_ngram_build_is_byte_identical_to_the_reference_path() {
    let values = [
        "The Quick BROWN fox",
        "mixed CASE across a boundary",
        "Ünïcödé ΣΤΙΓΜΑΣ text",
        "short ab",
    ];
    let mut expected = Vec::new();
    for value in values {
        expected.extend(tokenize(value).map(|token| hash_token(&token)));
        let folded = fold_for_ngrams(value);
        expected.extend(ngrams(&folded, NGRAM_LEN).map(hash_token));
    }
    expected.sort_unstable();
    expected.dedup();
    assert_eq!(
        TextTokenIndex::build_from_values(&values, true).encode(),
        TextTokenIndex::build(&expected, true).encode()
    );
}

#[test]
fn iterator_build_matches_slice_build() {
    let values = ["alpha beta", "Gamma delta", "hello world"];
    assert_eq!(
        TextTokenIndex::build_from_iter(values.iter().copied(), true),
        TextTokenIndex::build_from_values(&values, true)
    );
}

#[test]
fn non_ascii_token_lookup_is_case_insensitive() {
    // Tokens that leave the ASCII fast path still fold through full Unicode lowercasing on both sides.
    let index = TextTokenIndex::build_from_values(&["ÄPFEL Straße ΣΤΙΓΜΑ"], false);
    for query in ["äpfel", "ÄPFEL", "ÄpFeL", "straße", "STRAßE", "στιγμα", "ΣΤΙΓΜΑ"] {
        assert!(
            index.might_contain_token(query),
            "query '{query}' must match regardless of case"
        );
    }
}

#[test]
fn split_tokens_borrows_the_original_casing() {
    let tokens: Vec<&str> = split_tokens("Hello, World! don't").collect();
    assert_eq!(tokens, ["Hello", "World", "don't"]);
}

/// The builder as it stood before the trigram bitmap: every token and window hashed into one set, the filter sized
/// by the set's length. The bitmap build must produce its bytes exactly.
fn reference_build(values: &[&str], include_ngrams: bool) -> TextTokenIndex {
    let mut hashes = HashSet::new();
    for text in values {
        for token in split_tokens(text) {
            hashes.insert(hash_folded_token(token));
        }
        if include_ngrams {
            let folded = fold_for_ngrams(text);
            hashes.extend(ngrams(&folded, NGRAM_LEN).map(hash_token));
        }
    }
    let hashes: Vec<u64> = hashes.into_iter().collect();
    TextTokenIndex::build(&hashes, include_ngrams)
}

#[test]
fn bitmap_build_is_byte_identical_to_the_reference_builder() {
    let cases: &[&[&str]] = &[
        &[],
        &[""],
        &["   ", "\t\n", " \r\n\x0B\x0C "],
        &["--", "!!!", "'", "''"],
        &["a", "ab", "abc", "ABC", "aBc"],
        // Three-letter tokens are the same key as the trigram of their bytes; the reference counts them once.
        &["the fox ate the cat", "THE FOX", "fox"],
        &[
            "The Quick BROWN fox",
            "mixed CASE across a boundary",
            "don't STOP -- ever!",
        ],
        &["Ünïcödé ΣΤΙΓΜΑΣ text", "ΒΟΣΚΟΣ", "Straße", "İstanbul", "中文"],
        // A Kelvin sign lowercases to ASCII `k`, so a non-ASCII token can fold to a three-byte ASCII key.
        &["\u{212A}IT kit", "\u{212A}"],
        &["ascii then ünïcode", "ünïcode then ascii", "x ü y"],
    ];
    for values in cases {
        for include_ngrams in [false, true] {
            assert_eq!(
                TextTokenIndex::build_from_values(values, include_ngrams).encode(),
                reference_build(values, include_ngrams).encode(),
                "values {values:?}, include_ngrams {include_ngrams}"
            );
        }
    }
}

#[test]
fn bitmap_build_matches_the_reference_builder_on_generated_values() {
    // A fixed-seed linear congruential generator, so a failure reproduces.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |bound: usize| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((state >> 33) as usize) % bound
    };
    let alphabet: Vec<char> =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789   ,.-'!\t\nüÜßΣσς中\u{212A}"
            .chars()
            .collect();
    let probes = ["the", "fox", "ab", "abc", "ΒΟΣ", "kit", "üb", "a b", "zqx", "12", "x"];
    for case in 0..300 {
        let value_count = next(8);
        let values: Vec<String> = (0..value_count)
            .map(|_| {
                let length = next(24);
                (0..length)
                    .map(|_| alphabet.get(next(alphabet.len())).copied().unwrap_or(' '))
                    .collect()
            })
            .collect();
        let values: Vec<&str> = values.iter().map(String::as_str).collect();
        for include_ngrams in [false, true] {
            let built = TextTokenIndex::build_from_values(&values, include_ngrams);
            let reference = reference_build(&values, include_ngrams);
            assert_eq!(
                built.encode(),
                reference.encode(),
                "case {case}: values {values:?}, include_ngrams {include_ngrams}"
            );
            for probe in probes {
                assert_eq!(
                    built.might_contain_token(probe),
                    reference.might_contain_token(probe),
                    "case {case}: token probe {probe:?} over {values:?}"
                );
                assert_eq!(
                    built.might_contain_substring(probe),
                    reference.might_contain_substring(probe),
                    "case {case}: substring probe {probe:?} over {values:?}"
                );
            }
        }
    }
}

#[test]
fn scratch_is_empty_when_the_next_build_on_the_thread_starts() {
    // Two builds back to back on one thread reuse the scratch; nothing from the first may leak into the second.
    let first = TextTokenIndex::build_from_values(&["alpha beta gamma delta", "the fox"], true);
    assert!(first.might_contain_token("alpha"));
    let second = TextTokenIndex::build_from_values(&["omega"], true);
    assert_eq!(second.encode(), reference_build(&["omega"], true).encode());
}

#[test]
fn ascii_separator_matches_the_character_predicate() {
    for byte in 0u8..128 {
        let c = char::from(byte);
        assert_eq!(
            is_ascii_token_separator(byte),
            c.is_whitespace() || (c.is_ascii_punctuation() && c != '\''),
            "byte {byte:#04x}"
        );
    }
}

#[test]
fn ascii_tokens_split_exactly_as_the_character_splitter() {
    for text in [
        "Hello, World! don't",
        "  a\tb\nc\x0Bd\x0Ce\rf ",
        "--x--",
        "",
        "'quoted'",
    ] {
        let by_char: Vec<&[u8]> = split_tokens(text).map(str::as_bytes).collect();
        let by_byte: Vec<&[u8]> = split_ascii_tokens(text.as_bytes()).collect();
        assert_eq!(by_byte, by_char, "text {text:?}");
    }
}

#[test]
fn every_trigram_code_hashes_distinctly() {
    // The filter is sized from the bitmap's population count, which equals the reference builder's distinct-hash
    // count only if no two marked codes share a hash. Only folded codes are ever marked: one for every three bytes
    // that carry no ASCII uppercase.
    let folded: Vec<u8> = (0u8..128).filter(|byte| !byte.is_ascii_uppercase()).collect();
    let folded: &[u8] = &folded;
    let mut hashes: Vec<u64> = folded
        .iter()
        .flat_map(|&a| folded.iter().flat_map(move |&b| folded.iter().map(move |&c| [a, b, c])))
        .map(|window| trigram_hash(trigram_code(window)))
        .collect();
    assert_eq!(hashes.len(), folded.len().pow(3));
    hashes.sort_unstable();
    hashes.dedup();
    assert_eq!(
        hashes.len(),
        folded.len().pow(3),
        "two folded trigram codes share a hash"
    );
}

#[test]
fn trigram_code_round_trips_through_its_hash() {
    for window in [*b"abc", *b"ABC", *b"a b", *b"\x00\x7f9", *b"'-'"] {
        assert_eq!(
            trigram_hash(trigram_code(window)),
            stable_hash_ascii_lowercase(&window),
            "window {window:?}"
        );
    }
}
