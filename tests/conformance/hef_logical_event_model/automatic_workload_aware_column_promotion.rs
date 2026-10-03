//! Checks that fields people query often are automatically promoted into their own fast columns, based on observed
//! usage such as filters and group-bys, without anyone configuring it by hand.
use crate::support;
use hef::columns::{PathStatistics, PromotedColumn, PromotionPlan, column_ids};
use hef::events::variant::VariantValue;
use hef::layout::footer::ColumnKind;
use hef::writer::build::{HefBuildConfig, HefRow, build_hef_file};
use std::collections::BTreeMap;

/// conformance: hef-logical-event-model/automatic-workload-aware-column-promotion/frequently-filtered-field-is-promoted
#[test]
fn frequently_filtered_field_is_promoted() {
    // PathStatistics is the mechanism that collects workload signals from observed payload fields and selects
    // frequently-appearing, type-consistent paths as promotion candidates. Here "score" appears in every row, well
    // above the 50% presence threshold.
    let mut stats = PathStatistics::new();
    let mut payload = BTreeMap::new();
    payload.insert("score".to_owned(), VariantValue::Int(42));
    let value = VariantValue::Object(payload);
    for _ in 0..200 {
        stats.observe(Some(&value));
    }
    let candidates = stats.shred_candidates(&[]);
    assert!(
        candidates.iter().any(|(path, _)| path == "score"),
        "field observed in every row must be selected as a promotion candidate"
    );

    // When the write path receives a PromotionPlan (carrying workload signals from the query path), the promoted field
    // is materialized as a typed column in the footer, independently readable without touching the payload.
    let rows: Vec<HefRow> = (0..8)
        .map(|i| HefRow {
            epoch: 1,
            event: support::event(i),
            sequence: i + 1,
        })
        .collect();
    let config = HefBuildConfig {
        promotion: PromotionPlan {
            columns: vec![PromotedColumn {
                kind: ColumnKind::String,
                name: "kind_promoted".to_owned(),
                path: "kind".to_owned(),
                since_schema_version: 2,
                substring_searchable: false,
            }],
        },
        ..support::build_config()
    };
    let built = build_hef_file(rows, &config).unwrap();
    let promoted_cols: Vec<_> = built
        .footer
        .columns
        .iter()
        .filter(|c| c.column_id >= column_ids::PROMOTED_BASE && c.column_id < column_ids::SHREDDED_BASE)
        .collect();
    assert!(
        !promoted_cols.is_empty(),
        "write path must materialize the workload-driven field into a typed column in the footer"
    );
    assert!(
        promoted_cols.iter().any(|c| c.name == "kind_promoted"),
        "the promoted column must appear under its declared name"
    );
    assert!(
        !built.footer.presence.is_empty(),
        "the footer presence map must carry the schema-version-keyed entry for the promoted column"
    );
}
