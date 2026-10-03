//! Checks that probabilistic membership filters are workload-adaptive: a low-cardinality field (few distinct values
//! over many rows) is left to an exact bitmap or value set rather than a Ribbon/Bloom filter, while a high-cardinality
//! lookup field does warrant one. Also confirms the filter choice prefers a binary fuse filter for an immutable,
//! build-time-known key set.

use hef::indexes::probabilistic::{
    MembershipFilterChoice, choose_membership_filter, should_build_probabilistic_filter,
};

/// conformance:
/// hef-query-metadata-and-indexes/workload-adaptive-probabilistic-filters/
/// low-cardinality-field-avoids-probabilistic-filter
#[test]
fn low_cardinality_field_avoids_probabilistic_filter() {
    // A low-cardinality field (few distinct values over many rows) is better served by a bitmap/value-set:
    // should_build_probabilistic_filter returns false (no Ribbon/Bloom).
    let low_cardinality_distinct = 12;
    let many_rows = 5_000_000;
    assert!(
        !should_build_probabilistic_filter(low_cardinality_distinct, many_rows),
        "a low-cardinality field should not get a probabilistic filter"
    );

    // A high-cardinality field returns true (positive control).
    let high_cardinality_distinct = 4_500_000;
    assert!(
        should_build_probabilistic_filter(high_cardinality_distinct, many_rows),
        "a high-cardinality field should get a probabilistic filter"
    );

    // choose_membership_filter still prefers binary fuse for an immutable known key set.
    assert_eq!(
        choose_membership_filter(true),
        MembershipFilterChoice::BinaryFuse,
        "immutable build-time-known key sets prefer the binary fuse filter"
    );
}
