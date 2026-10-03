//! Checks that the mixed-load gate fails when live query p99 exceeds its declared bound while background maintenance is
//! active.

use hef::benchmarks::maintenance_gate_passes;

/// conformance:
/// hef-benchmarks-and-acceptance-gates/maintenance-coexistence-gate/maintenance-pressure-breaks-live-latency
#[test]
fn maintenance_pressure_breaks_live_latency() {
    let bound_micros = 1_000_u64;

    // Live p99 within bound: gate passes, build may ship.
    assert!(maintenance_gate_passes(500, bound_micros));
    assert!(maintenance_gate_passes(bound_micros, bound_micros));

    // Live p99 one microsecond over bound: gate fails, build does not ship.
    assert!(!maintenance_gate_passes(bound_micros + 1, bound_micros));
    assert!(!maintenance_gate_passes(5_000, bound_micros));
}
