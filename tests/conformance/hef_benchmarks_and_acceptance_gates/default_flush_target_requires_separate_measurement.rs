//! Checks that the 4 KiB and 16 KiB autonomous-flush targets are benchmarked separately before either is selected as
//! the default HEJ flush target.

use hef::benchmarks::{FlushTargetMeasurements, flush_default_selectable};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/required-write-and-query-benchmark-coverage/
/// default-flush-target-requires-separate-measurement
#[test]
fn default_flush_target_requires_separate_measurement() {
    // Neither target benchmarked: no default may be selected.
    assert!(!flush_default_selectable(&FlushTargetMeasurements {
        four_kib_benchmarked: false,
        sixteen_kib_benchmarked: false,
    }));

    // Only 4 KiB benchmarked: still cannot select a default.
    assert!(!flush_default_selectable(&FlushTargetMeasurements {
        four_kib_benchmarked: true,
        sixteen_kib_benchmarked: false,
    }));

    // Only 16 KiB benchmarked: still cannot select a default.
    assert!(!flush_default_selectable(&FlushTargetMeasurements {
        four_kib_benchmarked: false,
        sixteen_kib_benchmarked: true,
    }));

    // Both targets benchmarked separately: a default may now be selected.
    assert!(flush_default_selectable(&FlushTargetMeasurements {
        four_kib_benchmarked: true,
        sixteen_kib_benchmarked: true,
    }));
}
