//! Requirement: Uniform SkipIndex model.
use hef::indexes::model::{PredicateShape, RowRange, SkipIndex, choose_skip_index, pushdown_residual_required};
use hef::indexes::{Exactness, Granularity, SkipIndexKind};

/// conformance: hef-query-metadata-and-indexes/uniform-skipindex-model/inexact-skip-index-keeps-a-filter-above
#[test]
fn inexact_skip_index_keeps_a_filter_above() {
    // A SkipIndex declared with `inexact_no_false_negative` exactness — here a Bloom membership filter used for
    // equality pushdown — never drops a matching block, but a surviving block may hold no match. So when it is used to
    // prune, an exact predicate filter must still be retained above or inside the scan.
    let bloom = SkipIndex {
        block_ref: 0,
        column_id: 3,
        exactness: SkipIndexKind::SplitBlockBloomFilter.default_exactness(),
        fpr: Some(0.01),
        granularity: Granularity::Granule,
        index_id: 1,
        kind: SkipIndexKind::SplitBlockBloomFilter,
        projection_id: 0,
        row_range: RowRange {
            first_row_ordinal: 0,
            row_count: 4096,
        },
    };

    // The descriptor's exactness is inexact, and the inexact contract demands a residual filter.
    assert_eq!(bloom.exactness, Exactness::InexactNoFalseNegative);
    assert!(
        bloom.exactness.requires_residual_filter(),
        "an inexact skip index forces a residual filter"
    );
    assert!(
        pushdown_residual_required(bloom.kind),
        "pushdown with an inexact kind keeps a filter above the scan"
    );

    // An exact kind, by contrast, needs no residual filter: its keep/drop is final.
    let minmax_exact = SkipIndexKind::MinMax;
    assert_eq!(minmax_exact.default_exactness(), Exactness::Exact);
    assert!(!pushdown_residual_required(minmax_exact));
}

/// conformance: hef-query-metadata-and-indexes/uniform-skipindex-model/point-filter-does-not-answer-a-range-predicate
#[test]
fn point_filter_does_not_answer_a_range_predicate() {
    // A column carrying only point-membership filters cannot answer a range predicate: the planner returns nothing
    // rather than misusing a point filter.
    let point_only = [
        SkipIndexKind::EntityHashFilter,
        SkipIndexKind::AccountHashFilter,
        SkipIndexKind::OpportunityHashFilter,
        SkipIndexKind::BinaryFuseFilter,
        SkipIndexKind::RibbonFilter,
        SkipIndexKind::SplitBlockBloomFilter,
    ];
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &point_only),
        None,
        "a point-membership filter must not answer a range predicate"
    );
    // None of those kinds even claims to answer a range.
    for kind in point_only {
        assert!(
            !kind.answers(PredicateShape::RangeBound),
            "{kind:?} must not answer a range predicate"
        );
    }

    // Once a range-capable kind is present — a range_filter or min/max — the planner uses one of those for the range
    // predicate, never the point filter.
    let with_range_filter = [
        SkipIndexKind::EntityHashFilter,
        SkipIndexKind::RangeFilter,
        SkipIndexKind::SplitBlockBloomFilter,
    ];
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &with_range_filter),
        Some(SkipIndexKind::RangeFilter)
    );

    let with_minmax = [SkipIndexKind::BinaryFuseFilter, SkipIndexKind::MinMax];
    assert_eq!(
        choose_skip_index(PredicateShape::RangeBound, &with_minmax),
        Some(SkipIndexKind::MinMax)
    );
}
