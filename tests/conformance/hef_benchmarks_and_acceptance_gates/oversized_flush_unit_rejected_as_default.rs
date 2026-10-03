//! Checks that a 128 KiB or larger normal flush unit is treated as a negative control only and is never selected as the
//! HEJ frame default.

use hef::benchmarks::{NEGATIVE_CONTROL_FLUSH_BYTES, flush_unit_valid_as_default};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/latency-paper-alignment-gates/oversized-flush-unit-rejected-as-default
#[test]
fn oversized_flush_unit_rejected_as_default() {
    // The threshold itself is excluded from the default set.
    assert!(!flush_unit_valid_as_default(NEGATIVE_CONTROL_FLUSH_BYTES));
    // Anything at or above the threshold is a negative control only.
    assert!(!flush_unit_valid_as_default(256 * 1024));
    assert!(!flush_unit_valid_as_default(1024 * 1024));

    // Flush units smaller than the threshold are eligible as defaults.
    assert!(flush_unit_valid_as_default(64 * 1024));
    assert!(flush_unit_valid_as_default(16 * 1024));
    assert!(flush_unit_valid_as_default(4 * 1024));
}
