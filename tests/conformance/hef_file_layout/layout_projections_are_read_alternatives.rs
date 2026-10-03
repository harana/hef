//! Checks that a file may keep the same rows arranged more than one way, as alternative reading orders for the same
//! data. A query picks at most one such arrangement per range, and an arrangement that splits columns into a side file
//! is re-joined to the rows by position.

use super::spec_text;

/// conformance: hef-file-layout/layout-projections-are-read-alternatives/one-projection-per-range
#[test]
fn one_projection_per_range() {
    let spec = spec_text();

    assert!(
        spec.contains(
            "a query snapshot SHALL select exactly one projection plan per logical range so events cannot be \
             double-counted"
        ),
        "spec must commit a query snapshot to exactly one projection plan per logical range"
    );
    assert!(
        spec.contains("it selects exactly one projection plan for that range"),
        "spec scenario must state the one-projection-per-range outcome explicitly"
    );
}

/// conformance: hef-file-layout/layout-projections-are-read-alternatives/vertical-projection-joined-by-ordinal
#[test]
fn vertical_projection_joined_by_ordinal() {
    let spec = spec_text();

    assert!(
        spec.contains(
            "vertical projections SHALL carry the same deletion-vector and correction generations as \
             their base"
        ),
        "spec must commit vertical projections to the same deletion-vector and correction generations as their base"
    );
    assert!(
        spec.contains(
            "the `QueryEngine` reads the column from the projection row-aligned by ordinal against the base scan, \
             at the same deletion-vector and correction generations"
        ),
        "spec scenario must state the vertical-projection-joined-by-ordinal outcome explicitly"
    );
}
