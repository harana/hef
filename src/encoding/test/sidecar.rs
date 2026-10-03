use super::*;
use std::cmp::Ordering;

/// The comparison a prefix key answers must be the comparison the values themselves answer — for every pair of values
/// it is willing to answer for.
fn agrees(left: &str, right: &str) -> bool {
    match PrefixKey::of(left).compare(PrefixKey::of(right)) {
        Some(ordering) => ordering == left.cmp(right),
        // Declining is always allowed; the caller falls back to the real text.
        None => true,
    }
}

#[test]
fn a_prefix_key_is_exactly_eight_bytes() {
    assert_eq!(size_of::<PrefixKey>(), PREFIX_KEY_BYTES);
    assert_eq!(PrefixKey::of("abc").to_stored().len(), 8);
}

#[test]
fn prefix_keys_order_values_the_way_the_values_order() {
    let values = [
        "",
        "a",
        "ab",
        "abc",
        "abcdef",
        "abcdefg",
        "abcdefgh",
        "abcdefgi",
        "abcdefh",
        "b",
        "banana",
        "zzzzzzzzzz",
        "\u{0}",
        "a\u{0}",
        "a\u{0}b",
        "id-00010",
        "id-00020",
    ];
    for left in values {
        for right in values {
            assert!(agrees(left, right), "{left:?} vs {right:?}");
        }
    }
}

#[test]
fn a_prefix_key_decides_values_that_differ_inside_the_prefix() {
    assert_eq!(
        PrefixKey::of("apple").compare(PrefixKey::of("banana")),
        Some(Ordering::Less)
    );
    // Shorter first when one value is a prefix of the other and the key holds all of it.
    assert_eq!(PrefixKey::of("ab").compare(PrefixKey::of("abc")), Some(Ordering::Less));
    // Values the key holds whole and that match are equal, with no fallback needed.
    assert_eq!(
        PrefixKey::of("abc").compare(PrefixKey::of("abc")),
        Some(Ordering::Equal)
    );
}

#[test]
fn a_prefix_key_declines_only_when_both_values_fill_it_and_tie() {
    assert_eq!(
        PrefixKey::of("https://example.com/a").compare(PrefixKey::of("https://example.com/b")),
        None
    );
    // Seven bytes is the whole key, so even two equal-length values that fill it are undecidable.
    assert_eq!(PrefixKey::of("abcdefg").compare(PrefixKey::of("abcdefg")), None);
    // One byte less and the key holds the whole value, so it decides.
    assert_eq!(
        PrefixKey::of("abcdef").compare(PrefixKey::of("abcdef")),
        Some(Ordering::Equal)
    );
}

#[test]
fn a_prefix_key_round_trips_through_its_stored_bytes() {
    for value in ["", "a", "abcdefg", "abcdefghijk"] {
        let key = PrefixKey::of(value);
        assert_eq!(PrefixKey::from_stored(key.to_stored()), Some(key), "{value:?}");
    }
}

/// A length byte past the prefix width is a key no writer produces; reading it back refuses rather than inventing a
/// comparison the block cannot support.
#[test]
fn a_forged_prefix_key_length_refuses() {
    let mut stored = PrefixKey::of("abcdefg").to_stored();
    if let Some(last) = stored.last_mut() {
        *last = 200;
    }
    assert_eq!(PrefixKey::from_stored(stored), None);
}

#[test]
fn a_fingerprint_never_rules_out_a_substring_that_is_really_there() {
    let corpus = [
        "",
        "a",
        "hello world",
        "https://example.com/items/42",
        "MiXeD CaSe",
        "ünïcödé",
    ];
    let needles = ["", "a", "o w", "example", "42", "ZZZ", "case", "ö"];
    for value in corpus {
        let value_bits = StringFingerprint::of(value.as_bytes());
        for needle in needles {
            let needle_bits = StringFingerprint::of(needle.as_bytes());
            if value.contains(needle) {
                assert!(
                    value_bits.might_contain(needle_bits),
                    "{value:?} contains {needle:?} but its fingerprint denied it"
                );
            }
        }
    }
}

/// The point of the fingerprint: a needle whose bytes are absent is rejected without looking at the text.
#[test]
fn a_fingerprint_rejects_a_needle_whose_bytes_are_absent() {
    let value = StringFingerprint::of(b"aaaa");
    assert!(!value.might_contain(StringFingerprint::of(b"z")));
    assert!(value.might_contain(StringFingerprint::of(b"a")));
    assert!(value.might_contain(StringFingerprint::default()));
}

/// Bucketing on the low five bits folds ASCII case, so a case-insensitive search needs no second fingerprint.
#[test]
fn a_fingerprint_folds_ascii_case() {
    assert_eq!(StringFingerprint::of(b"abc"), StringFingerprint::of(b"ABC"));
    assert!(StringFingerprint::of(b"Hello").might_contain(StringFingerprint::of(b"hello")));
}

#[test]
fn a_union_carries_every_needle_bucket() {
    let both = StringFingerprint::of(b"ab").union(StringFingerprint::of(b"cd"));
    assert!(StringFingerprint::of(b"abcd").might_contain(both));
    assert!(!StringFingerprint::of(b"abc").might_contain(both));
}

#[test]
fn a_fingerprint_round_trips_through_its_stored_bits() {
    let bits = StringFingerprint::of(b"round trip");
    assert_eq!(StringFingerprint::from_bits(bits.to_bits()), bits);
}
