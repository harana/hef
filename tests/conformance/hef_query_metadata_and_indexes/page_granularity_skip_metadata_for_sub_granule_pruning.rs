//! Proves that page-granularity min/max skip metadata narrows the read set to pages that can actually hold matching
//! rows, within a granule that granule-level pruning cannot reject.

use hef::indexes::pruning::*;

/// conformance:
/// hef-query-metadata-and-indexes/page-granularity-skip-metadata-for-sub-granule-pruning/
/// reject-pages-inside-a-surviving-granule
#[test]
fn reject_pages_inside_a_surviving_granule() {
    // The granule spans amount values [0, 499]: granule-level min/max `disjoint(0, 499, 300, MAX)` = false, so the
    // granule survives pruning. At page granularity the five pages cover disjoint sub-ranges; pages 0, 1, and 2 are
    // entirely below the filter threshold and must be rejected.
    let filter = Filter {
        clauses: vec![FilterClause {
            column_id: 1,
            predicate: PredicateKind::Range,
            value_hi: i128::MAX,
            value_lo: 300,
        }],
    };

    // Five pages with disjoint value ranges: page 0: [0,   99]  — max=99  < lo=300 → pruned page 1: [100, 199] —
    // max=199 < lo=300 → pruned page 2: [200, 299] — max=299 < lo=300 → pruned page 3: [300, 399] — overlaps [300, MAX]
    // → survives page 4: [400, 499] — overlaps [300, MAX] → survives
    let page_statistics: Vec<(u32, GranuleStatistics)> = vec![
        (
            0,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 99,
                    min: 0,
                }],
            },
        ),
        (
            1,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 199,
                    min: 100,
                }],
            },
        ),
        (
            2,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 299,
                    min: 200,
                }],
            },
        ),
        (
            3,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 399,
                    min: 300,
                }],
            },
        ),
        (
            4,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 499,
                    min: 400,
                }],
            },
        ),
    ];

    let surviving = prune_pages_in_granule(&filter, &page_statistics, 5);

    assert_eq!(
        surviving,
        vec![3, 4],
        "only the two pages overlapping [300, MAX] survive"
    );
    assert!(
        !surviving.contains(&0) && !surviving.contains(&1) && !surviving.contains(&2),
        "pages 0-2 are entirely below lo=300 and must be rejected"
    );

    // A page with no statistics is always kept: absence of a statistic never drops data (conservative
    // no-false-negatives guarantee).
    let sparse: Vec<(u32, GranuleStatistics)> = vec![
        (
            0,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 99,
                    min: 0,
                }],
            },
        ),
        // pages 1 and 2 have no statistics → always kept
        (
            3,
            GranuleStatistics {
                statistics: vec![Statistic::MinMaxBound {
                    column_id: 1,
                    max: 499,
                    min: 300,
                }],
            },
        ),
    ];
    let sparse_surviving = prune_pages_in_granule(&filter, &sparse, 4);
    assert!(
        sparse_surviving.contains(&1) && sparse_surviving.contains(&2),
        "pages without statistics are always kept"
    );
    assert!(
        !sparse_surviving.contains(&0),
        "page 0 with max=99 < 300 is still pruned"
    );
}
