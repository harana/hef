use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotedColumn, PromotionPlan, column_ids};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::layout::footer::ColumnKind;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};

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

fn filter(clauses: Vec<FilterClause>) -> Filter {
    Filter { clauses }
}

fn stats(statistics: Vec<Statistic>) -> GranuleStatistics {
    GranuleStatistics { statistics }
}

#[test]
fn minmax_outside_range_proves_absent() {
    let stat = Statistic::MinMaxBound {
        column_id: 1,
        max: 10,
        min: 0,
    };
    // Looking for 100, which is above [0, 10].
    assert_eq!(stat.judge(&eq_clause(1, 100)), StatVerdict::ProvenAbsent);
    // Looking for 5, which is inside [0, 10].
    assert_eq!(stat.judge(&eq_clause(1, 5)), StatVerdict::CouldMatch);
}

#[test]
fn point_membership_never_answers_range() {
    let stat = Statistic::PointMembership {
        column_id: 1,
        members: vec![1, 2, 3],
    };
    assert!(stat.relevant_to(&eq_clause(1, 1)));
    // The spec forbids using a point-membership filter to answer a range.
    assert!(!stat.relevant_to(&range_clause(1, 0, 100)));
}

#[test]
fn range_filter_answers_range_not_presence() {
    let stat = Statistic::RangeEmptiness {
        column_id: 1,
        non_empty_spans: vec![(0, 10), (50, 60)],
    };
    assert!(stat.relevant_to(&range_clause(1, 0, 5)));
    assert!(!stat.relevant_to(&presence_clause(1)));
    // [20, 30] misses both non-empty spans => proven absent.
    assert_eq!(stat.judge(&range_clause(1, 20, 30)), StatVerdict::ProvenAbsent);
    // [5, 55] overlaps both spans => could match.
    assert_eq!(stat.judge(&range_clause(1, 5, 55)), StatVerdict::CouldMatch);
}

#[test]
fn fully_null_column_proves_value_absent() {
    let stat = Statistic::NullCount {
        column_id: 1,
        null_count: 8,
        row_count: 8,
    };
    assert_eq!(stat.judge(&eq_clause(1, 5)), StatVerdict::ProvenAbsent);

    let partial = Statistic::NullCount {
        column_id: 1,
        null_count: 4,
        row_count: 8,
    };
    assert_eq!(partial.judge(&eq_clause(1, 5)), StatVerdict::CouldMatch);
}

#[test]
fn one_expression_holds_one_term_per_relevant_statistic() {
    // A granule declares min/max on col 1, point membership on col 2, and a range filter on col 3. A null-count on col
    // 9 is irrelevant (no clause).
    let declared = stats(vec![
        Statistic::MinMaxBound {
            column_id: 1,
            max: 100,
            min: 0,
        },
        Statistic::PointMembership {
            column_id: 2,
            members: vec![7, 8, 9],
        },
        Statistic::RangeEmptiness {
            column_id: 3,
            non_empty_spans: vec![(0, 50)],
        },
        Statistic::NullCount {
            column_id: 9,
            null_count: 0,
            row_count: 4,
        },
    ]);
    let f = filter(vec![eq_clause(1, 5), eq_clause(2, 8), range_clause(3, 10, 20)]);
    let expr = build_pruning_expression(&f, &declared);
    // One term per declared-and-relevant statistic: 3 (col 9 excluded).
    assert_eq!(expr.term_count(), 3);

    // A single keep/drop decision from one loop.
    let outcome = evaluate(&expr, &declared);
    assert!(!outcome.drop);
}

#[test]
fn missing_statistic_keeps_granule() {
    // The filter touches col 1, but the granule declares no statistic at all.
    let declared = stats(vec![]);
    let f = filter(vec![eq_clause(1, 5)]);
    let expr = build_pruning_expression(&f, &declared);
    assert_eq!(expr.term_count(), 0);
    let outcome = evaluate(&expr, &declared);
    // Absence of a statistic never drops data — and never confirms the clause either, so the row-level filter has to
    // run: without it every row of the granule comes back for a predicate nothing evaluated (issue #7486).
    assert!(!outcome.drop);
    assert!(outcome.residual_required);
}

#[test]
fn conservative_keeps_non_matching_but_never_drops_matching() {
    let declared = stats(vec![Statistic::MinMaxBound {
        column_id: 1,
        max: 10,
        min: 0,
    }]);
    // Predicate value 5 is inside [0, 10] — the granule may hold no row equal to exactly 5, but min/max cannot prove
    // that, so it is kept (conservative).
    let kept = build_pruning_expression(&filter(vec![eq_clause(1, 5)]), &declared);
    assert!(!evaluate(&kept, &declared).drop);

    // A granule that genuinely could hold a match (value inside its bounds) is never dropped.
    let matching = build_pruning_expression(&filter(vec![range_clause(1, 0, 3)]), &declared);
    assert!(!evaluate(&matching, &declared).drop);

    // Only a provably-disjoint predicate drops it.
    let absent = build_pruning_expression(&filter(vec![eq_clause(1, 999)]), &declared);
    assert!(evaluate(&absent, &declared).drop);
}

#[test]
fn inexact_surviving_term_requires_residual() {
    // A membership filter (inexact) keeps the granule: residual is required.
    let declared = stats(vec![Statistic::PointMembership {
        column_id: 1,
        members: vec![5, 6, 7],
    }]);
    let expr = build_pruning_expression(&filter(vec![eq_clause(1, 5)]), &declared);
    let outcome = evaluate(&expr, &declared);
    assert!(!outcome.drop);
    assert!(outcome.residual_required);
}

#[test]
fn bounds_only_surviving_term_requires_residual() {
    // A min/max keep never proves any row matches: `x = 5` inside [0, 10] holds for a granule whose rows are {0, 10}.
    // Eliding the row-level predicate on such a keep would return non-matching rows, so the outcome must demand the
    // residual filter even though the statistic itself is exact (issue #4009).
    let declared = stats(vec![Statistic::MinMaxBound {
        column_id: 1,
        max: 10,
        min: 0,
    }]);
    let expr = build_pruning_expression(&filter(vec![eq_clause(1, 5)]), &declared);
    let outcome = evaluate(&expr, &declared);
    assert!(!outcome.drop);
    assert!(outcome.residual_required);

    // The same holds for a partial-range clause the span merely overlaps.
    let range_expr = build_pruning_expression(&filter(vec![range_clause(1, 5, 100)]), &declared);
    let range_outcome = evaluate(&range_expr, &declared);
    assert!(!range_outcome.drop);
    assert!(range_outcome.residual_required);
}

#[test]
fn presence_keep_still_needs_the_row_level_filter() {
    // Path presence is a Bloom filter, so a positive answer may be a false positive; and even a true hit only says
    // *some* row in the granule carries the path. Eliding the row-level predicate on such a keep would return rows
    // where the path is absent (issue #7485).
    let declared = stats(vec![Statistic::PathPresence {
        column_id: 4,
        present: true,
    }]);
    let expr = build_pruning_expression(&filter(vec![presence_clause(4)]), &declared);
    let outcome = evaluate(&expr, &declared);
    assert!(!outcome.drop);
    assert!(outcome.residual_required);
}

#[test]
fn a_clause_no_statistic_covers_requires_the_residual_filter_beside_a_confirming_one() {
    // One clause is answered by a statistic, a second is answered by none. The granule is kept, and because that
    // second clause was never evaluated its exact predicate must still run over the rows.
    let declared = stats(vec![Statistic::PathPresence {
        column_id: 4,
        present: true,
    }]);
    let expr = build_pruning_expression(&filter(vec![presence_clause(4), eq_clause(1, 5)]), &declared);
    let outcome = evaluate(&expr, &declared);
    assert!(!outcome.drop);
    assert!(outcome.residual_required);
}

#[test]
fn dropped_granule_needs_no_residual() {
    // Even though the surviving membership term would be inexact, a proven-absent drop short-circuits and reports no
    // residual.
    let declared = stats(vec![
        Statistic::MinMaxBound {
            column_id: 1,
            max: 10,
            min: 0,
        },
        Statistic::PointMembership {
            column_id: 1,
            members: vec![5],
        },
    ]);
    // Value 999 is outside min/max => proven absent on the exact term.
    let expr = build_pruning_expression(&filter(vec![eq_clause(1, 999)]), &declared);
    let outcome = evaluate(&expr, &declared);
    assert!(outcome.drop);
    assert!(!outcome.residual_required);
}

#[test]
fn new_statistic_kind_adds_a_term_in_the_same_loop() {
    // PathPresence is a statistic kind; using it does not require a new branch in build/evaluate — it flows through the
    // same loop as min/max.
    let declared = stats(vec![Statistic::PathPresence {
        column_id: 4,
        present: false,
    }]);
    let f = filter(vec![presence_clause(4)]);
    let expr = build_pruning_expression(&f, &declared);
    assert_eq!(expr.term_count(), 1);
    // The path is absent => the presence predicate cannot match => drop.
    assert!(evaluate(&expr, &declared).drop);

    let present = stats(vec![Statistic::PathPresence {
        column_id: 4,
        present: true,
    }]);
    let expr2 = build_pruning_expression(&f, &present);
    assert!(!evaluate(&expr2, &present).drop);
}

#[test]
fn evaluate_matches_per_statistic_decision() {
    // Build the single expression and, separately, fold the verdicts by hand ("per-statistic pruning"); the keep/drop
    // must agree.
    let declared = stats(vec![
        Statistic::MinMaxBound {
            column_id: 1,
            max: 10,
            min: 0,
        },
        Statistic::RangeEmptiness {
            column_id: 2,
            non_empty_spans: vec![(100, 200)],
        },
    ]);
    let f = filter(vec![eq_clause(1, 5), range_clause(2, 0, 50)]);

    // Per-statistic: col 2 range [0,50] misses [100,200] => proven absent => drop.
    let manual_drop = declared.statistics.iter().any(|s| {
        f.clauses
            .iter()
            .any(|c| s.relevant_to(c) && s.judge(c) == StatVerdict::ProvenAbsent)
    });

    let expr = build_pruning_expression(&f, &declared);
    let outcome = evaluate(&expr, &declared);
    assert_eq!(outcome.drop, manual_drop);
    assert!(outcome.drop);
}

#[test]
fn aggregate_query_takes_shortcut_not_pruning() {
    let query = Query {
        answerable_from_aggregate: true,
        filter: filter(vec![eq_clause(1, 5)]),
    };
    let declared = stats(vec![Statistic::MinMaxBound {
        column_id: 1,
        max: 10,
        min: 0,
    }]);
    assert_eq!(plan_query(&query, &declared), QueryPlan::AggregateShortcut);
}

#[test]
fn non_aggregate_query_falls_through_to_scan() {
    let query = Query {
        answerable_from_aggregate: false,
        filter: filter(vec![eq_clause(1, 5)]),
    };
    let declared = stats(vec![Statistic::MinMaxBound {
        column_id: 1,
        max: 10,
        min: 0,
    }]);
    match plan_query(&query, &declared) {
        QueryPlan::Scan { pruning } => assert_eq!(pruning.term_count(), 1),
        QueryPlan::AggregateShortcut => panic!("expected a scan plan"),
    }
}

#[test]
fn evaluate_uses_the_granule_under_test_not_the_template() {
    // The expression is built from one granule's declared shape, but evaluated against another granule whose own
    // min/max proves the value absent.
    let template = stats(vec![Statistic::MinMaxBound {
        column_id: 1,
        max: 1000,
        min: 0,
    }]);
    let f = filter(vec![eq_clause(1, 500)]);
    let expr = build_pruning_expression(&f, &template);

    let other_granule = stats(vec![Statistic::MinMaxBound {
        column_id: 1,
        max: 10,
        min: 0,
    }]);
    // 500 is outside the other granule's [0, 10] => it is dropped.
    assert!(evaluate(&expr, &other_granule).drop);
}

fn indexed_row(i: u64) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    payload.insert("amount".to_owned(), VariantValue::Int(1_000 + i as i64));
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xBEEF_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(7),
                stream_id: StreamId(2),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64 * 1000),
                ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64 * 1000),
                source: "crm".to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i,
                entity_id_hash_high: 1,
                entity_id: Some(format!("opp-{i}")),
                actor_id_hash_low: 3,
                actor_id: None,
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: 100 + i,
                dedupe_hash_high: 6,
                schema_version: 2,
                flags: EventFlags(0),
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: i,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

fn indexed_config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration { fields: Vec::new() },
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 5,
        promotion: PromotionPlan {
            columns: vec![PromotedColumn {
                name: "amount_promoted".to_owned(),
                path: "amount".to_owned(),
                kind: ColumnKind::I64,
                since_schema_version: 1,
                substring_searchable: false,
            }],
        },
        targets: LayoutTargets {
            index_granularity: 64,
            index_granularity_bytes: 1 << 30,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(7),
    }
}

/// Drives the writer's own promotion-gated `page_minmax` emission (`hef/writer/build.rs`) through the pruning
/// evaluator: a hot (promoted) column earns page-level min/max that reject every page but the ones a value could fall
/// in, while a cold (required, unpromoted) column carries no page-level entries at all and still prunes correctly at
/// granule granularity — fetching every page of a surviving granule, exactly as a granule-only baseline would.
#[test]
fn hot_column_prunes_pages_cold_column_falls_back_to_granule_granularity() {
    let rows: Vec<HefRow> = (0..17).map(indexed_row).collect();
    let built = build_hef_file(rows.clone(), &indexed_config()).unwrap();
    let granule_id = built.footer.granules[0].granule_id;
    let hot_column_id = column_ids::PROMOTED_BASE;
    let cold_column_id = column_ids::SEQUENCE;

    // The hot column's page bounds, straight from the footer's PAGE_MINMAX section: pages of 5 rows each over amounts
    // 1000..=1016, so [1000,1004], [1005,1009], [1010,1014], [1015,1016].
    let mut hot_pages: Vec<_> = built
        .footer
        .page_minmax
        .iter()
        .filter(|entry| entry.column_id == hot_column_id && entry.granule_id == granule_id)
        .collect();
    hot_pages.sort_by_key(|entry| entry.page_index);
    assert_eq!(hot_pages.len(), 4, "17 rows at 5 rows/page must split into 4 pages");

    let hot_page_statistics: Vec<(u32, GranuleStatistics)> = hot_pages
        .iter()
        .map(|entry| {
            (
                entry.page_index,
                GranuleStatistics {
                    statistics: vec![Statistic::MinMaxBound {
                        column_id: hot_column_id,
                        max: entry.max_i128.expect("hot page carries a bound"),
                        min: entry.min_i128.expect("hot page carries a bound"),
                    }],
                },
            )
        })
        .collect();

    // [1005, 1014] falls inside pages 1 and 2's spans and outside pages 0 and 3's — the page-level stats must reject
    // exactly the two pages that cannot hold a matching value.
    let hot_filter = filter(vec![range_clause(hot_column_id, 1005, 1014)]);
    let surviving_hot_pages = prune_pages_in_granule(&hot_filter, &hot_page_statistics, hot_pages.len() as u32);
    assert_eq!(
        surviving_hot_pages,
        vec![1, 2],
        "page-level min/max must reject every page whose span cannot hold the queried range"
    );

    // The cold column earns no page-level entries at all — the writer defaults it to granule-level stats.
    assert!(
        built
            .footer
            .page_minmax
            .iter()
            .all(|entry| entry.column_id != cold_column_id),
        "a column the promotion plan does not mention must carry no page-level minmax entries"
    );

    let cold_total_pages = built
        .footer
        .page_directory
        .iter()
        .filter(|entry| entry.column_id == cold_column_id && entry.granule_id == granule_id)
        .map(|entry| entry.page_index + 1)
        .max()
        .unwrap_or(0);
    assert_eq!(
        cold_total_pages,
        hot_pages.len() as u32,
        "both columns are split into the same page geometry"
    );

    // No declared statistics for the cold column: every page is kept, identical to a granule-only baseline that reads
    // every page of a surviving granule without ever consulting page-level metadata.
    let cold_filter = filter(vec![range_clause(cold_column_id, 5, 8)]);
    let surviving_cold_pages = prune_pages_in_granule(&cold_filter, &[], cold_total_pages);
    assert_eq!(
        surviving_cold_pages,
        (0..cold_total_pages).collect::<Vec<_>>(),
        "a cold column's pages must all be kept — absence of page-level stats never prunes a page"
    );

    // The cold column still prunes correctly at granule granularity: the granule's own sequence bounds decide keep or
    // drop for the whole granule, with the same conservative, never-false-negative guarantee as the hot path.
    let granule = built
        .footer
        .granules
        .iter()
        .find(|granule| granule.granule_id == granule_id)
        .expect("built granule");
    let granule_stats = GranuleStatistics {
        statistics: vec![Statistic::SequenceBounds {
            column_id: cold_column_id,
            max_sequence: granule.last_sequence as i128,
            min_sequence: granule.first_sequence as i128,
        }],
    };
    let kept = evaluate(&build_pruning_expression(&cold_filter, &granule_stats), &granule_stats);
    assert!(
        !kept.drop,
        "sequence range [5, 8] falls inside the granule's own span, so granule-level pruning must keep it"
    );
    let dropped = evaluate(
        &build_pruning_expression(
            &filter(vec![range_clause(cold_column_id, 10_000, 10_001)]),
            &granule_stats,
        ),
        &granule_stats,
    );
    assert!(
        dropped.drop,
        "a range wholly outside the granule's sequence span must still be dropped without any page-level stats"
    );
}
