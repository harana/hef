//! Checks that there is only a small, fixed set of file shapes, even though many different producers write files and
//! many different queries read them. A new service can attach its own derived columns, and an unusual query can be
//! answered, without anyone inventing a new file format.

use super::spec_text;

/// conformance:
/// hef-file-layout/closed-set-of-file-shapes-open-producers-and-queries/new-service-attaches-derived-columns
#[test]
fn new_service_attaches_derived_columns() {
    let spec = spec_text();

    assert!(
        spec.contains(
            "any service MAY attach a derived-columns sibling against a base HEF, declared in the manifest with \
             lineage, with zero format change"
        ),
        "spec must commit a new analytical service to attaching a derived-columns sibling file against the base HEF, \
         declared in the manifest with producer lineage, rather than inventing a new file shape"
    );
    assert!(
        spec.contains(
            "it attaches a derived-columns sibling file against the base HEF declared in the manifest with producer \
             lineage, with no change to the file format"
        ),
        "spec scenario must state that attaching per-event model outputs costs zero format change"
    );
}

/// conformance: hef-file-layout/closed-set-of-file-shapes-open-producers-and-queries/exotic-query-needs-no-exotic-file
#[test]
fn exotic_query_needs_no_exotic_file() {
    let spec = spec_text();

    assert!(
        spec.contains(
            "tenant-defined dimensions via automatic workload-driven promotion, with a re-sorted copy when a \
             dimension becomes a hot grouping key"
        ),
        "spec must route a previously unindexed payload dimension through workload-driven promotion, not a new file \
         shape"
    );
    assert!(
        spec.contains(
            "the dimension is served by workload-driven promotion (and optionally a re-sorted copy), not \
             by a new file shape"
        ),
        "spec scenario must state the exotic-query outcome explicitly"
    );
    assert!(
        spec.contains(
            "New analytical capability SHALL NOT add new physical shapes or a catch-all auxiliary \
             container"
        ),
        "spec must close the physical shape set against ad hoc auxiliary containers"
    );
}
