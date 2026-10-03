//! Checks that a list inside each file declares which capabilities it relies on. If the file requires a capability the
//! reader does not have, the reader refuses it; if it merely names an optional speed-up the reader lacks, the file is
//! still valid and simply read without that speed-up.

use super::spec_text;
use hef::compat::check_features;
use hef::error::FormatError;
use hef::layout::required_features;

/// conformance: hef-file-layout/feature-directory-governs-capabilities/unknown-required-feature
#[test]
fn unknown_required_feature() {
    // A declared required feature beyond this reader refuses: the file is not served.
    assert!(matches!(
        check_features(required_features::ALL | (1 << 62), 0),
        Err(FormatError::UnknownRequiredFeature { .. })
    ));
}

/// conformance: hef-file-layout/feature-directory-governs-capabilities/missing-optional-acceleration
#[test]
fn missing_optional_acceleration() {
    // A file without an optional feature remains valid: the gate returns an empty usable set rather than an error.
    let usable = check_features(required_features::ALL, 0).unwrap();
    assert_eq!(usable, 0, "no optional features declared, file still valid");

    let spec = spec_text();
    assert!(
        spec.contains(
            "The planner SHALL use an optional feature only when the footer declares it and its block checksum \
             verifies; absence of an optional feature SHALL leave the file valid with queries falling back to \
             another valid plan."
        ),
        "spec must commit the planner to falling back to another valid plan when an optional acceleration is absent"
    );
    assert!(
        spec.contains("the planner falls back to another valid plan or an authorized column scan"),
        "spec scenario must state the missing-optional-acceleration outcome explicitly"
    );
}
