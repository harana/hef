use super::*;
use crate::layout::{optional_features, required_features};

#[test]
fn known_required_and_optional_features_pass() {
    let usable = check_features(required_features::ALL, optional_features::VARIANT_SHREDDED_FIELD_BLOCKS).unwrap();
    assert_eq!(usable, optional_features::VARIANT_SHREDDED_FIELD_BLOCKS);
}

#[test]
fn unknown_required_feature_refuses() {
    let unknown = 1 << 40;
    let error = check_features(required_features::ALL | unknown, 0).unwrap_err();
    assert_eq!(error, FormatError::UnknownRequiredFeature { bits: unknown });
}

#[test]
fn unknown_optional_feature_is_dropped_not_failed() {
    let unknown = 1 << 50;
    let usable = check_features(required_features::ALL, unknown).unwrap();
    assert_eq!(usable & unknown, 0, "unknown optional bit is not usable");
}

#[test]
fn matching_major_version_is_accepted() {
    assert!(check_format_version(SUPPORTED_FORMAT_MAJOR, 7).is_ok());
}

#[test]
fn unsupported_major_version_is_rejected() {
    assert!(check_format_version(SUPPORTED_FORMAT_MAJOR + 1, 0).is_err());
}
