//! Checks that a splice-based rewrite path benchmarked at the rewrite lifecycle stage is not marked ready when its
//! measured write-amplification factor exceeds the gated ceiling for that stage.

use hef::benchmarks::{WafMeasurement, waf_gate_passes};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/write-amplification-gates-per-lifecycle-stage/
/// rewrite-path-held-to-its-write-amplification-floor
#[test]
fn rewrite_path_held_to_its_write_amplification_floor() {
    // At the rewrite-stage ceiling: passes.
    assert!(waf_gate_passes(&WafMeasurement {
        ceiling: 1.2,
        measured_waf: 1.2,
    }));

    // Below the ceiling: passes.
    assert!(waf_gate_passes(&WafMeasurement {
        ceiling: 1.2,
        measured_waf: 1.05,
    }));

    // Exceeds the rewrite-stage ceiling: the splice-based rewrite path is not marked ready.
    assert!(!waf_gate_passes(&WafMeasurement {
        ceiling: 1.2,
        measured_waf: 1.6,
    }));
}
