use super::*;
use crate::indexes::Exactness;

/// The six point-membership kinds plus the context/path/text kinds that never order their keys — none of these may
/// answer a range predicate.
const POINT_ONLY_KINDS: &[SkipIndexKind] = &[
    SkipIndexKind::AccountHashFilter,
    SkipIndexKind::BinaryFuseFilter,
    SkipIndexKind::ContextLocator,
    SkipIndexKind::EntityHashFilter,
    SkipIndexKind::OpportunityHashFilter,
    SkipIndexKind::PathPresence,
    SkipIndexKind::RibbonFilter,
    SkipIndexKind::SplitBlockBloomFilter,
    SkipIndexKind::TextToken,
];

/// The range-capable kinds (those that can answer RangeBound predicates).
const RANGE_KINDS: &[SkipIndexKind] = &[
    SkipIndexKind::LearnedPosition,
    SkipIndexKind::MinMax,
    SkipIndexKind::RangeFilter,
    SkipIndexKind::SequenceRange,
    SkipIndexKind::TimeRange,
];

#[test]
fn minmax_answers_equality_and_range_but_not_membership() {
    assert!(SkipIndexKind::MinMax.answers(PredicateShape::Equality));
    assert!(SkipIndexKind::MinMax.answers(PredicateShape::RangeBound));
    assert!(!SkipIndexKind::MinMax.answers(PredicateShape::Membership));
}

#[test]
fn range_filter_answers_only_range() {
    assert!(SkipIndexKind::RangeFilter.answers(PredicateShape::RangeBound));
    assert!(!SkipIndexKind::RangeFilter.answers(PredicateShape::Equality));
    assert!(!SkipIndexKind::RangeFilter.answers(PredicateShape::Membership));
}

#[test]
fn membership_kinds_answer_equality_and_membership_never_range() {
    for &kind in &[
        SkipIndexKind::AccountHashFilter,
        SkipIndexKind::BinaryFuseFilter,
        SkipIndexKind::EntityHashFilter,
        SkipIndexKind::OpportunityHashFilter,
        SkipIndexKind::RibbonFilter,
        SkipIndexKind::SplitBlockBloomFilter,
    ] {
        assert!(kind.answers(PredicateShape::Equality), "{kind:?} equality");
        assert!(kind.answers(PredicateShape::Membership), "{kind:?} membership");
        assert!(
            !kind.answers(PredicateShape::RangeBound),
            "{kind:?} must not answer range"
        );
    }
}

#[test]
fn sequence_and_time_range_answer_range() {
    assert!(SkipIndexKind::SequenceRange.answers(PredicateShape::RangeBound));
    assert!(SkipIndexKind::TimeRange.answers(PredicateShape::RangeBound));
}

#[test]
fn default_exactness_matches_spec() {
    for &kind in &[
        SkipIndexKind::LearnedPosition,
        SkipIndexKind::MinMax,
        SkipIndexKind::SequenceRange,
        SkipIndexKind::TimeRange,
    ] {
        assert_eq!(kind.default_exactness(), Exactness::Exact, "{kind:?}");
    }
    for &kind in &[
        SkipIndexKind::AccountHashFilter,
        SkipIndexKind::BinaryFuseFilter,
        SkipIndexKind::EntityHashFilter,
        SkipIndexKind::OpportunityHashFilter,
        SkipIndexKind::RangeFilter,
        SkipIndexKind::RibbonFilter,
        SkipIndexKind::SplitBlockBloomFilter,
    ] {
        assert_eq!(kind.default_exactness(), Exactness::InexactNoFalseNegative, "{kind:?}");
    }
}

#[test]
fn learned_position_answers_range_only() {
    assert!(SkipIndexKind::LearnedPosition.answers(PredicateShape::RangeBound));
    assert!(!SkipIndexKind::LearnedPosition.answers(PredicateShape::Equality));
    assert!(!SkipIndexKind::LearnedPosition.answers(PredicateShape::Membership));
}

#[test]
fn learned_position_is_exact_and_preferred_over_range_filter() {
    // LearnedPosition (Exact) wins over RangeFilter (InexactNoFalseNegative).
    let available = [SkipIndexKind::RangeFilter, SkipIndexKind::LearnedPosition];
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &available),
        Some(SkipIndexKind::LearnedPosition)
    );
}

#[test]
fn all_range_kinds_answer_range_bound() {
    for &kind in RANGE_KINDS {
        assert!(
            kind.answers(PredicateShape::RangeBound),
            "{kind:?} must answer RangeBound"
        );
    }
}

#[test]
fn residual_not_required_for_learned_position() {
    assert!(
        !pushdown_residual_required(SkipIndexKind::LearnedPosition),
        "LearnedPosition is exact after local confirmation: no residual filter needed"
    );
}

#[test]
fn choose_never_returns_a_point_filter_for_a_range() {
    // Only point-membership-style kinds available: a range predicate must get no answer, never one of these.
    let chosen = choose_skip_index(PredicateShape::RangeBound, POINT_ONLY_KINDS);
    assert_eq!(chosen, None);
}

#[test]
fn choose_prefers_minmax_or_range_filter_for_a_range() {
    // With a point filter mixed in, a range predicate still picks a range-capable kind and never the point filter.
    let available = [
        SkipIndexKind::EntityHashFilter,
        SkipIndexKind::RangeFilter,
        SkipIndexKind::SplitBlockBloomFilter,
    ];
    let chosen = choose_skip_index(PredicateShape::RangeBound, &available);
    assert_eq!(chosen, Some(SkipIndexKind::RangeFilter));

    let available = [SkipIndexKind::BinaryFuseFilter, SkipIndexKind::MinMax];
    let chosen = choose_skip_index(PredicateShape::RangeBound, &available);
    assert_eq!(chosen, Some(SkipIndexKind::MinMax));
}

#[test]
fn choose_prefers_exact_over_inexact_for_a_range() {
    // MinMax (exact) wins over RangeFilter (inexact) when both can answer a range.
    let available = [SkipIndexKind::RangeFilter, SkipIndexKind::MinMax];
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &available),
        Some(SkipIndexKind::MinMax)
    );
    // Order in `available` must not change the exact-wins outcome.
    let available = [SkipIndexKind::MinMax, SkipIndexKind::RangeFilter];
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &available),
        Some(SkipIndexKind::MinMax)
    );
}

#[test]
fn choose_returns_a_membership_kind_for_equality() {
    let available = [SkipIndexKind::BinaryFuseFilter];
    assert_eq!(
        choose_skip_index(PredicateShape::Equality, &available),
        Some(SkipIndexKind::BinaryFuseFilter)
    );
    // MinMax also answers equality and is exact, so it wins when offered.
    let available = [SkipIndexKind::BinaryFuseFilter, SkipIndexKind::MinMax];
    assert_eq!(
        choose_skip_index(PredicateShape::Equality, &available),
        Some(SkipIndexKind::MinMax)
    );
}

#[test]
fn choose_returns_none_when_nothing_answers() {
    // A membership-only column cannot answer a range predicate.
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &[SkipIndexKind::RibbonFilter]),
        None
    );
    // No indexes at all.
    assert_eq!(choose_skip_index(PredicateShape::Equality, &[]), None);
}

#[test]
fn residual_required_only_for_inexact_kinds() {
    assert!(!pushdown_residual_required(SkipIndexKind::MinMax));
    assert!(!pushdown_residual_required(SkipIndexKind::SequenceRange));
    assert!(!pushdown_residual_required(SkipIndexKind::TimeRange));
    assert!(pushdown_residual_required(SkipIndexKind::RangeFilter));
    assert!(pushdown_residual_required(SkipIndexKind::BinaryFuseFilter));
    assert!(pushdown_residual_required(SkipIndexKind::RibbonFilter));
    assert!(pushdown_residual_required(SkipIndexKind::SplitBlockBloomFilter));
}

#[test]
fn residual_required_agrees_with_exactness_contract() {
    for &kind in POINT_ONLY_KINDS {
        assert_eq!(
            pushdown_residual_required(kind),
            kind.default_exactness().requires_residual_filter(),
            "{kind:?}"
        );
    }
}

#[test]
fn skip_index_descriptor_is_a_plain_value() {
    // The descriptor carries only labels and pointers; it is Copy and comparable.
    let descriptor = SkipIndex {
        block_ref: 42,
        column_id: 7,
        exactness: Exactness::Exact,
        fpr: None,
        granularity: Granularity::Granule,
        index_id: 1,
        kind: SkipIndexKind::MinMax,
        projection_id: 0,
        row_range: RowRange {
            first_row_ordinal: 100,
            row_count: 50,
        },
    };
    let copy = descriptor;
    assert_eq!(descriptor, copy);
    assert_eq!(copy.row_range.first_row_ordinal, 100);
}
