use super::*;
use crate::encoding::predicate::{StringBound, StringPredicate};

fn dictionary(entries: &[&str]) -> GlobalDictionary {
    GlobalDictionary::from_sorted(entries.iter().map(|entry| (*entry).to_owned()).collect())
}

fn map(dictionary: &GlobalDictionary, local_entries: &[&str]) -> Vec<Option<u64>> {
    dictionary.local_to_global_map(
        &local_entries
            .iter()
            .map(|entry| (*entry).to_owned())
            .collect::<Vec<_>>(),
    )
}

fn bound(value: &str, inclusive: bool) -> StringBound {
    StringBound {
        inclusive,
        value: value.to_owned(),
    }
}

/// A local value absent from the global dictionary can still fall inside a range predicate, so the pruning-safe
/// verdict keeps it — absence alone proves nothing about where the string sorts.
#[test]
fn an_absent_local_value_inside_a_range_is_kept() {
    let global = dictionary(&["apple", "zebra"]);
    let filter = global.translate(&StringPredicate::Range {
        lower: Some(bound("b", true)),
        upper: Some(bound("y", true)),
    });
    // "mango" is absent from the global dictionary yet inside the range.
    let local = map(&global, &["apple", "mango"]);
    assert_eq!(local, vec![Some(0), None]);
    assert!(filter.matches_local_code(1, &local));
}

/// An `Equals` needle absent from the global dictionary can still equal an absent local value (the pre-dictionary
/// file may hold exactly that string), so the absent value is kept; a needle present in the dictionary can never
/// equal an absent value, so that one is still dropped.
#[test]
fn equals_keeps_an_absent_local_value_only_when_the_needle_is_absent_too() {
    let global = dictionary(&["apple", "zebra"]);
    let local = map(&global, &["mango"]);
    assert_eq!(local, vec![None]);

    let absent_needle = global.translate(&StringPredicate::Equals("mango".to_owned()));
    assert!(absent_needle.matches_local_code(0, &local));

    let present_needle = global.translate(&StringPredicate::Equals("apple".to_owned()));
    assert!(!present_needle.matches_local_code(0, &local));
}

/// `InSet` drops an absent local value only when every needle is present in the dictionary; one absent needle could
/// equal the absent value, so the value is kept.
#[test]
fn in_set_keeps_an_absent_local_value_only_when_some_needle_is_absent() {
    let global = dictionary(&["apple", "zebra"]);
    let local = map(&global, &["mango"]);

    let all_present = global.translate(&StringPredicate::InSet(vec!["apple".to_owned(), "zebra".to_owned()]));
    assert!(!all_present.matches_local_code(0, &local));

    let one_absent = global.translate(&StringPredicate::InSet(vec!["apple".to_owned(), "mango".to_owned()]));
    assert!(one_absent.matches_local_code(0, &local));
}

/// `NotEquals` always keeps an absent local value: with a present needle the two strings are provably different, and
/// with an absent needle they could differ — the pruning-safe verdict never drops a possibly matching row.
#[test]
fn not_equals_keeps_absent_local_values() {
    let global = dictionary(&["apple", "zebra"]);
    let local = map(&global, &["mango"]);

    let present_needle = global.translate(&StringPredicate::NotEquals("apple".to_owned()));
    assert!(present_needle.matches_local_code(0, &local));

    let absent_needle = global.translate(&StringPredicate::NotEquals("mango".to_owned()));
    assert!(absent_needle.matches_local_code(0, &local));
}

/// Present local codes keep their exact global-code verdicts, and a local code beyond the map never matches.
#[test]
fn present_codes_stay_exact_and_out_of_map_codes_never_match() {
    let global = dictionary(&["apple", "mango", "zebra"]);
    let local = map(&global, &["mango", "zebra"]);
    assert_eq!(local, vec![Some(1), Some(2)]);

    let filter = global.translate(&StringPredicate::Equals("mango".to_owned()));
    assert!(filter.matches_local_code(0, &local));
    assert!(!filter.matches_local_code(1, &local));
    assert!(!filter.matches_local_code(99, &local));

    assert!(filter.matches_global_code(1));
    assert!(!filter.matches_global_code(2));
}

/// The merged dictionary is the sorted union of the files' own alphabets, so one code space covers the whole set and
/// a value only one file carries still gets a code.
#[test]
fn a_file_set_dictionary_merges_the_files_alphabets_in_order() {
    let first: Vec<String> = ["checkout", "login"].iter().map(|v| (*v).to_owned()).collect();
    let second: Vec<String> = ["login", "signup"].iter().map(|v| (*v).to_owned()).collect();
    let global = GlobalDictionary::from_file_alphabets([first.as_slice(), second.as_slice()]);
    assert_eq!(global.code_for("checkout"), Some(0));
    assert_eq!(global.code_for("login"), Some(1));
    assert_eq!(global.code_for("signup"), Some(2));
    assert_eq!(global.code_for("logout"), None);
}

/// A file's local codes stay usable through the merged dictionary: its own alphabet maps onto the merged order.
#[test]
fn a_files_local_codes_map_onto_the_merged_order() {
    let first: Vec<String> = ["checkout", "login"].iter().map(|v| (*v).to_owned()).collect();
    let second: Vec<String> = ["login", "signup"].iter().map(|v| (*v).to_owned()).collect();
    let global = GlobalDictionary::from_file_alphabets([first.as_slice(), second.as_slice()]);
    assert_eq!(global.local_to_global_map(&second), vec![Some(1), Some(2)]);
}

/// An empty file-set has no values, so every lookup is absent rather than a panic.
#[test]
fn an_empty_file_set_has_no_codes() {
    let global = GlobalDictionary::from_file_alphabets([]);
    assert_eq!(global.code_for("login"), None);
}

/// Sorted and unsorted local alphabets map slot for slot exactly as independent lookups do: duplicates right after a
/// hit or a miss, values below, between and above the dictionary, empty and Unicode strings, long shared prefixes,
/// and an empty dictionary or local slice.
#[test]
fn local_to_global_map_matches_independent_lookups() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    let pool: Vec<String> = (0..200u64)
        .map(|i| match i % 4 {
            0 => format!("{i:04}"),
            1 => format!("shared-prefix-{}-{i:04}", "x".repeat(40)),
            2 => format!("\u{e9}t\u{e9}-{i}"),
            _ => String::new(),
        })
        .collect();
    for _ in 0..200 {
        let global: BTreeSet<String> = (0..next(60)).map(|_| pool[next(200) as usize].clone()).collect();
        let global = GlobalDictionary::from_sorted(global.into_iter().collect());
        let mut local: Vec<String> = (0..next(40)).map(|_| pool[next(200) as usize].clone()).collect();
        let independent = |local: &[String]| local.iter().map(|value| global.code_for(value)).collect::<Vec<_>>();
        assert_eq!(global.local_to_global_map(&local), independent(&local));
        local.sort();
        assert_eq!(global.local_to_global_map(&local), independent(&local));
    }
    let global = dictionary(&["b", "d", "f"]);
    assert_eq!(
        map(&global, &["a", "b", "b", "c", "c", "d", "g", "g"]),
        vec![None, Some(0), Some(0), None, None, Some(1), None, None]
    );
    assert_eq!(map(&dictionary(&[]), &["a", "a"]), vec![None, None]);
    assert_eq!(map(&global, &[]), Vec::<Option<u64>>::new());
}
