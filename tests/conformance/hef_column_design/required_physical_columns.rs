//! Checks that every file always contains the small set of columns the engine needs to function, and that the
//! bookkeeping ones (such as the internal sequence and body pointer) exist for internal use but are never handed back
//! to outside callers.
use crate::support;
use hef::columns::REQUIRED_COLUMNS;
use hef::events::families::{Caller, authorize_columns, column_allowed};
use hef::layout::reader::HefFile;

/// conformance: hef-column-design/required-physical-columns/internal-scan-columns-withheld
#[test]
fn internal_scan_columns_withheld() {
    let built = support::built_file(8);
    let file = HefFile::open(built.bytes, None).unwrap();

    // Every required column chunk is present in the file and readable.
    for spec in REQUIRED_COLUMNS {
        assert!(
            file.footer().columns.iter().any(|c| c.column_id == spec.column_id),
            "required column {} missing",
            spec.name
        );
        let granule = file.footer().granules[0].granule_id;
        file.read_column(spec.column_id, granule).unwrap();
    }

    // The file's own column directory marks the internal scan columns correctly, the scan boundary blocks them for
    // public callers, and internal callers retain full access. These three checks together prove that the columns are
    // withheld at the public boundary — the opaque-cursor form that the HTTP API layer presents is built on top of this
    // mechanism.
    for internal in ["epoch", "sequence", "payload_ref"] {
        let descriptor = file
            .footer()
            .columns
            .iter()
            .find(|c| c.name == internal)
            .unwrap_or_else(|| panic!("column {internal} not in footer"));
        assert!(
            descriptor.internal_only,
            "{internal} must be marked internal_only in the column directory"
        );
        assert!(
            !column_allowed(internal, Caller::Public),
            "{internal} must be blocked at the public scan boundary"
        );
        assert!(
            column_allowed(internal, Caller::Internal),
            "{internal} must remain readable by internal callers"
        );
    }

    // authorize_columns — the scan-phase entry point — drops the internal columns from a mixed public projection and
    // keeps only the public-safe ones.
    let (allowed, dropped) = authorize_columns(
        &["occurred_at", "event_type_id", "epoch", "sequence", "payload_ref"],
        Caller::Public,
    );
    assert_eq!(allowed, vec!["occurred_at", "event_type_id"]);
    assert_eq!(dropped, vec!["epoch", "sequence", "payload_ref"]);
}
