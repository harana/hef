//! Checks that the small set of "context" fields an investigation needs can be served from their own dedicated columns,
//! so building an evidence card never has to read back through the full raw event body.
use crate::support;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::events::families::{Caller, column_allowed};
use hef::layout::footer::ColumnKind;
use hef::layout::reader::HefFile;
use hef::writer::build::{AnalyticalColumn, HefRow, build_hef_file};

/// conformance: hef-column-design/context-projection-columns-avoid-raw-payload-scans/evidence-card-from-context-columns
#[test]
fn evidence_card_from_context_columns() {
    // Context projection columns (context_title, context_summary, etc.) carry data-class labels and are public-safe
    // when the caller is authorized. They are NOT in the default public-blocked list — access flows through the
    // data-class authorization layer, not through the blanket prefix block that covers embedding_ and internal_
    // columns.
    for col in [
        "context_title",
        "context_summary",
        "context_entity_label",
        "context_source_label",
        "context_metric_label",
        "context_status_label",
        "context_amount_display",
        "context_period_ref",
        "context_snippet_ref",
        "context_lineage_ref",
    ] {
        assert!(
            column_allowed(col, Caller::Public),
            "{col} must be public-safe (authorized via data-class labels, not blocked by default)"
        );
    }

    // Build a file that includes a context_title column derived from the event envelope. An evidence card can be
    // assembled from this column alone, without touching the payload arena.
    let rows: Vec<HefRow> = (0..8)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let context_titles: Vec<Option<String>> = rows
        .iter()
        .map(|row| Some(format!("Event #{}", row.sequence)))
        .collect();
    let mut config = support::build_config();
    config.analytical_columns = vec![AnalyticalColumn {
        column_id: column_ids::CONTEXT_BASE,
        data: ColumnData::Strings(context_titles.into()),
        internal_only: false,
        kind: ColumnKind::String,
        name: "context_title".to_owned(),
        substring_searchable: false,
    }];
    let built = build_hef_file(rows, &config).unwrap();

    // The context column is present in the file's column directory and is explicitly not internal-only — public callers
    // can read it directly.
    let context_col = built
        .footer
        .columns
        .iter()
        .find(|c| c.name == "context_title")
        .expect("context_title column must be in the footer");
    assert!(!context_col.internal_only, "context columns are public-safe");
    assert_eq!(context_col.column_id, column_ids::CONTEXT_BASE);

    // The column can be read back without accessing the payload arena: the evidence card is assembled from the context
    // column block alone.
    let file = HefFile::open(built.bytes, None).unwrap();
    let granule_id = file.footer().granules[0].granule_id;
    let read = file.read_column(column_ids::CONTEXT_BASE, granule_id).unwrap();
    let ColumnData::Strings(values) = read.data else {
        panic!("context_title must be a String column");
    };
    assert_eq!(values.len(), 8);
    assert_eq!(values.get(0), Some(Some("Event #1")));
    assert_eq!(values.get(7), Some(Some("Event #8")));
}
