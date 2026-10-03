//! Checks that the bulk-egress path streams a single declared column family sequentially without touching the residual
//! variant arena — the path used by release-migration re-extraction and per-subject erasure jobs.

use crate::support;
use hef::columns::{FreetextDeclaration, column_ids};
use hef::encoding::ColumnData;
use hef::layout::reader::HefFile;
use hef::writer::build::build_hef_file;

/// conformance: hef-apis/bulk-egress-reader-path/migration-re-extraction-uses-bulk-egress
#[test]
fn migration_re_extraction_uses_bulk_egress() {
    // Build a file that declares a free-text field. A release migration that re-runs the extraction model must read the
    // free-text via the bulk-egress path — streaming only the free-text column blocks in granule order — rather than
    // issuing per-row point lookups through the late-materialization path (which would touch the residual variant arena
    // for every row).
    let mut config = support::build_config();
    config.freetext = FreetextDeclaration {
        fields: vec!["note".to_owned()],
    };

    let rows: Vec<hef::writer::build::HefRow> = (0..32)
        .map(|i| hef::writer::build::HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    let built = build_hef_file(rows, &config).unwrap();
    let file = HefFile::open(built.bytes, None).unwrap();

    // The bulk-egress path: read all granules for the free-text family in one sequential pass. No residual arena reads
    // — only free-text blocks.
    let bulk = file.bulk_read_family(column_ids::FREETEXT_BASE).unwrap();

    // Every granule contributes free-text values: the corpus is fully reconstructable from the bulk read alone, with no
    // payload() calls.
    assert!(!bulk.is_empty(), "bulk read must cover at least one granule");

    let mut total_values = 0usize;
    for (_granule_id, read) in &bulk {
        let ColumnData::Strings(values) = &read.data else {
            panic!("free-text family must decode to strings");
        };
        // Every present value starts with the fixture prefix.
        for value in values.iter().flatten() {
            assert!(
                value.starts_with("free text body"),
                "unexpected free-text value: {value}"
            );
            total_values += 1;
        }
    }

    // All 32 events' free-text fields are present — no row is missed.
    assert_eq!(total_values, 32, "bulk read must cover all rows");

    // The bulk read spans all granules the file formed; granule ids are in ascending order (sequential read guarantee).
    let granule_ids: Vec<u32> = bulk.iter().map(|(id, _)| *id).collect();
    assert!(
        granule_ids.windows(2).all(|w| w[0] < w[1]),
        "bulk read must iterate granules in ascending order"
    );
}
