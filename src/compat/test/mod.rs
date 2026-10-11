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

/// What a reader built before the graph bits existed implements: bits 0 to 22.
const PRE_GRAPH_KNOWN: u64 = (1 << 23) - 1;

#[test]
fn a_reader_from_before_the_graph_bits_refuses_a_graph_file_and_this_reader_accepts_it() {
    for bit in [
        required_features::GRAPH_DENSE_IDS,
        required_features::GRAPH_CSR_FORWARD,
        required_features::GRAPH_CSR_REVERSE,
        required_features::GRAPH_EVENT_ID_INDEX,
        required_features::GRAPH_CHAIN_COVER,
        required_features::GRAPH_STATE_ROOTS,
    ] {
        let declared = required_features::ALL | bit;
        // The old reader's rule is this module's rule run against its narrower known set.
        assert_ne!(
            declared & !PRE_GRAPH_KNOWN,
            0,
            "an old reader sees an unknown required bit and refuses"
        );
        assert!(check_features(declared, 0).is_ok());
    }
    assert_eq!(
        required_features::ALL & !PRE_GRAPH_KNOWN,
        0,
        "event files stay readable by old readers"
    );
}

/// What a reader built before time-series files existed implements: required bits 0 to 29, optional bits 0 to 12, and
/// pipeline transforms 0 to 13.
const PRE_TIME_SERIES_REQUIRED_KNOWN: u64 = (1 << 30) - 1;
const PRE_TIME_SERIES_OPTIONAL_KNOWN: u64 = (1 << 13) - 1;
const PRE_TIME_SERIES_TRANSFORMS: usize = 14;

#[test]
fn a_reader_from_before_time_series_files_refuses_xor_float_blocks_and_scans_past_rollups() {
    let declared = required_features::ALL | required_features::XOR_FLOAT_BLOCKS;
    assert_ne!(
        declared & !PRE_TIME_SERIES_REQUIRED_KNOWN,
        0,
        "an old reader sees an unknown required bit and refuses at open"
    );
    let usable = check_features(declared, optional_features::TIME_SERIES_ROLLUPS).unwrap();
    assert_eq!(usable, optional_features::TIME_SERIES_ROLLUPS);
    assert_eq!(
        optional_features::TIME_SERIES_ROLLUPS & PRE_TIME_SERIES_OPTIONAL_KNOWN,
        0,
        "an old reader drops the rollups bit and answers by scanning"
    );
    assert_eq!(
        required_features::ALL & !PRE_TIME_SERIES_REQUIRED_KNOWN,
        0,
        "files without Chimp blocks stay readable by old readers"
    );
    // Even past the bit, an old reader has no availability window for transform 14, so it refuses such a block rather
    // than decode it as something else.
    let old_windows = availability::PINNED_AVAILABILITY_WINDOWS
        .get(..PRE_TIME_SERIES_TRANSFORMS)
        .unwrap();
    assert!(validate_pipeline_window(14, (1, 0), old_windows).is_err());
    assert!(validate_pipeline_window_pinned(14, (1, 0)).is_ok());
}
