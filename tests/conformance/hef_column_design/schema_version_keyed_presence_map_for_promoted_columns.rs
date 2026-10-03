//! Checks that the file can tell two kinds of "missing" apart for a promoted column. For rows written before the field
//! was promoted, the value is read from the original event body instead; for rows written after promotion that simply
//! never carried the field, the value is a genuine null.
use crate::support;
use hef::columns::{PromotedColumn, PromotionPlan, column_ids};
use hef::layout::footer::ColumnKind;
use hef::layout::reader::{HefFile, PayloadRead};
use hef::writer::build::{HefRow, build_hef_file};

fn promoted_kind_plan(since: u32) -> PromotionPlan {
    PromotionPlan {
        columns: vec![PromotedColumn {
            name: "kind".to_owned(),
            path: "kind".to_owned(),
            kind: ColumnKind::String,
            since_schema_version: since,
            substring_searchable: false,
        }],
    }
}

/// conformance: hef-column-design/schema-version-keyed-presence-map-for-promoted-columns/granule-predates-promotion
#[test]
fn granule_predates_promotion() {
    // Rows whose schema version predates V_promote are absent from the typed column; the reader falls back to the
    // payload blocks for them rather than returning NULL.
    let mut rows: Vec<HefRow> = (0..8)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    for row in rows.iter_mut().take(4) {
        row.event.envelope.schema_version = 1; // predates V_promote = 2
    }
    let mut config = support::build_config();
    config.promotion = promoted_kind_plan(2);
    let built = build_hef_file(rows, &config).unwrap();
    assert!(
        built
            .footer
            .presence
            .iter()
            .any(|entry| { entry.column_id == column_ids::PROMOTED_BASE && entry.since_schema_version == 2 })
    );
    let file = HefFile::open(built.bytes, None).unwrap();
    let granule = file.footer().granules[0].granule_id;
    let read = file.read_column(column_ids::PROMOTED_BASE, granule).unwrap();
    for index in 0..4usize {
        assert_eq!(read.presence[index / 8] & (1 << (index % 8)), 0);
        // The value still answers from the payload for those rows.
        assert!(matches!(file.payload(index as u64).unwrap(), PayloadRead::Value(_)));
    }
    for index in 4..8usize {
        assert_ne!(read.presence[index / 8] & (1 << (index % 8)), 0);
    }
}

/// conformance:
/// hef-column-design/schema-version-keyed-presence-map-for-promoted-columns/cross-stream-sparsity-reads-as-null
#[test]
fn cross_stream_sparsity_reads_as_null() {
    // Rows at/above V_promote that simply never had the field read as genuine NULL (absent from the typed column) with
    // no payload fallback signal.
    let mut rows: Vec<HefRow> = (0..6)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    // Remove the field entirely from half the rows (other-stream rows).
    use hef::artifacts::batch::PayloadInput;
    use hef::events::variant::VariantValue;
    for row in rows.iter_mut().take(3) {
        if let PayloadInput::Variant(VariantValue::Object(fields)) = &mut row.event.payload {
            fields.remove("kind");
        }
    }
    let mut config = support::build_config();
    config.promotion = promoted_kind_plan(1);
    let built = build_hef_file(rows, &config).unwrap();
    let file = HefFile::open(built.bytes, None).unwrap();
    let granule = file.footer().granules[0].granule_id;
    let read = file.read_column(column_ids::PROMOTED_BASE, granule).unwrap();
    for index in 0..3usize {
        assert_eq!(
            read.presence[index / 8] & (1 << (index % 8)),
            0,
            "genuine NULL for rows that never had the field"
        );
    }
    for index in 3..6usize {
        assert_ne!(read.presence[index / 8] & (1 << (index % 8)), 0);
    }
}
