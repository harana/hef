//! Conformance: the planner expresses granule pruning as one conservative falsification expression — "could any row in this granule match the filter?" — built from whatever statistics the granule declares and evaluated uniformly over every granule, so a new statistic adds a term, not a branch.

use hef::indexes::pruning::*;

fn eq_clause(column_id: u32, value: i128) -> FilterClause {
    FilterClause {
        column_id,
        predicate: PredicateKind::Equality,
        value_hi: value,
        value_lo: value,
    }
}

fn range_clause(column_id: u32, lo: i128, hi: i128) -> FilterClause {
    FilterClause {
        column_id,
        predicate: PredicateKind::Range,
        value_hi: hi,
        value_lo: lo,
    }
}

fn presence_clause(column_id: u32) -> FilterClause {
    FilterClause {
        column_id,
        predicate: PredicateKind::Presence,
        value_hi: 0,
        value_lo: 0,
    }
}

/// conformance:
/// hef-query-metadata-and-indexes/pruning-as-a-falsification-expression/one-expression-spans-every-declared-statistic
#[test]
fn one_expression_spans_every_declared_statistic() {
    // A granule declares min/max (col 1), a binary-fuse point membership (col 2), and a range filter (col 3). The
    // filter touches a column each one covers.
    let declared = GranuleStatistics {
        statistics: vec![
            Statistic::MinMaxBound {
                column_id: 1,
                max: 100,
                min: 0,
            },
            Statistic::PointMembership {
                column_id: 2,
                members: vec![10, 20, 30],
            },
            Statistic::RangeEmptiness {
                column_id: 3,
                non_empty_spans: vec![(0, 50)],
            },
        ],
    };
    let filter = Filter {
        clauses: vec![eq_clause(1, 5), eq_clause(2, 20), range_clause(3, 10, 40)],
    };

    // ONE expression whose terms come from those statistics.
    let expr = build_pruning_expression(&filter, &declared);
    // One term per declared-and-relevant statistic — no per-statistic branch.
    assert_eq!(expr.term_count(), 3);

    // Evaluate that one expression for a single keep/drop decision.
    let outcome = evaluate(&expr, &declared);
    // Every term could match (value in bounds, member present, span overlaps).
    assert!(!outcome.drop);
}

/// conformance:
/// hef-query-metadata-and-indexes/pruning-as-a-falsification-expression/
/// a-granule-with-no-matching-row-may-be-kept-but-one-with-a-match-is-never-dropped
#[test]
fn a_granule_with_no_matching_row_may_be_kept_but_one_with_a_match_is_never_dropped() {
    // (1) A granule whose statistics cannot prove the filter unsatisfiable is KEPT even if it in fact holds no matching
    // row. min/max [0, 10] cannot prove there is no row equal to exactly 5, so the granule survives.
    let cannot_prove = GranuleStatistics {
        statistics: vec![Statistic::MinMaxBound {
            column_id: 1,
            max: 10,
            min: 0,
        }],
    };
    let filter = Filter {
        clauses: vec![eq_clause(1, 5)],
    };
    let expr = build_pruning_expression(&filter, &cannot_prove);
    assert!(!evaluate(&expr, &cannot_prove).drop);

    // (2) A granule that DOES hold a match is never dropped: its bounds overlap the predicate range.
    let has_match = GranuleStatistics {
        statistics: vec![Statistic::MinMaxBound {
            column_id: 1,
            max: 10,
            min: 0,
        }],
    };
    let range = Filter {
        clauses: vec![range_clause(1, 2, 4)],
    };
    let expr_match = build_pruning_expression(&range, &has_match);
    assert!(!evaluate(&expr_match, &has_match).drop);

    // (3) A missing statistic contributes the always-keep result: the filter touches col 7, which has no declared
    // statistic.
    let no_stat = GranuleStatistics { statistics: vec![] };
    let other = Filter {
        clauses: vec![eq_clause(7, 42)],
    };
    let expr_missing = build_pruning_expression(&other, &no_stat);
    assert_eq!(expr_missing.term_count(), 0);
    assert!(!evaluate(&expr_missing, &no_stat).drop);

    // And a granule that provably cannot match IS dropped (so "never dropped" is a real constraint, not vacuous): 999
    // is outside [0, 10].
    let absent = Filter {
        clauses: vec![eq_clause(1, 999)],
    };
    let expr_absent = build_pruning_expression(&absent, &cannot_prove);
    assert!(evaluate(&expr_absent, &cannot_prove).drop);
}

/// conformance:
/// hef-query-metadata-and-indexes/pruning-as-a-falsification-expression/a-new-statistic-adds-a-term-not-a-branch
#[test]
fn a_new_statistic_adds_a_term_not_a_branch() {
    // A statistic kind (PathPresence) participates by contributing a term to the existing expression and is evaluated
    // by the SAME loop as every other kind. The call sites below (`build_pruning_expression`, `evaluate`) are identical
    // to those used for min/max — no new per-statistic branch is needed.
    let filter = Filter {
        clauses: vec![presence_clause(4)],
    };

    let absent = GranuleStatistics {
        statistics: vec![Statistic::PathPresence {
            column_id: 4,
            present: false,
        }],
    };
    let expr = build_pruning_expression(&filter, &absent);
    assert_eq!(expr.term_count(), 1);
    // The path is absent => the same evaluation loop drops the granule.
    assert!(evaluate(&expr, &absent).drop);

    let present = GranuleStatistics {
        statistics: vec![Statistic::PathPresence {
            column_id: 4,
            present: true,
        }],
    };
    let expr_present = build_pruning_expression(&filter, &present);
    assert!(!evaluate(&expr_present, &present).drop);

    // Confirm the uniform path: the statistic renders its own verdict, which the loop consumes without knowing the
    // kind.
    assert_eq!(
        Statistic::PathPresence {
            column_id: 4,
            present: false,
        }
        .judge(&presence_clause(4)),
        StatVerdict::ProvenAbsent
    );
}

/// conformance:
/// hef-query-metadata-and-indexes/pruning-as-a-falsification-expression/an-inexact-term-keeps-the-residual-filter
#[test]
fn an_inexact_term_keeps_the_residual_filter() {
    // A surviving term comes from an inexact_no_false_negative SkipIndex — a bloom/ribbon-style point membership
    // filter. The granule survives only as a candidate, and residual_required is true.
    let declared = GranuleStatistics {
        statistics: vec![Statistic::PointMembership {
            column_id: 1,
            members: vec![5, 6, 7],
        }],
    };
    let filter = Filter {
        clauses: vec![eq_clause(1, 5)],
    };
    let expr = build_pruning_expression(&filter, &declared);
    let outcome = evaluate(&expr, &declared);

    assert!(!outcome.drop, "membership reports the value present");
    assert!(
        outcome.residual_required,
        "an inexact surviving term forces an exact filter above/inside the scan"
    );

    // A range-emptiness filter is likewise inexact: a surviving range candidate also forces a residual.
    let range_declared = GranuleStatistics {
        statistics: vec![Statistic::RangeEmptiness {
            column_id: 2,
            non_empty_spans: vec![(0, 100)],
        }],
    };
    let range_filter = Filter {
        clauses: vec![range_clause(2, 10, 20)],
    };
    let range_expr = build_pruning_expression(&range_filter, &range_declared);
    let range_outcome = evaluate(&range_expr, &range_declared);
    assert!(!range_outcome.drop);
    assert!(range_outcome.residual_required);
}

/// conformance:
/// hef-query-metadata-and-indexes/pruning-as-a-falsification-expression/
/// aggregate-and-sketch-shortcuts-stay-above-pruning
#[test]
fn aggregate_and_sketch_shortcuts_stay_above_pruning() {
    let declared = GranuleStatistics {
        statistics: vec![Statistic::MinMaxBound {
            column_id: 1,
            max: 10,
            min: 0,
        }],
    };

    // A query answerable from an exact aggregate block / cube cell / sketch takes that shortcut path; it is NOT
    // downgraded to min/max-only granule pruning.
    let answerable = Query {
        answerable_from_aggregate: true,
        filter: Filter {
            clauses: vec![eq_clause(1, 5)],
        },
    };
    assert_eq!(plan_query(&answerable, &declared), QueryPlan::AggregateShortcut);

    // A query the shortcut cannot answer falls through to a granule-pruning scan — the two tiers compose; pruning never
    // replaces the shortcut.
    let not_answerable = Query {
        answerable_from_aggregate: false,
        filter: Filter {
            clauses: vec![eq_clause(1, 5)],
        },
    };
    match plan_query(&not_answerable, &declared) {
        QueryPlan::Scan { pruning } => assert_eq!(pruning.term_count(), 1),
        QueryPlan::AggregateShortcut => panic!("expected a scan plan when no aggregate answers it"),
    }
}
