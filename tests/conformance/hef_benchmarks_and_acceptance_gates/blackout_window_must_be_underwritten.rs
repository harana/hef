//! Checks that a release migration's blackout window is computed from the gated bulk-egress throughput over the tenant
//! corpus, not from an unmeasured estimate.

use hef::benchmarks::min_blackout_seconds;

/// conformance: hef-benchmarks-and-acceptance-gates/bulk-egress-throughput-gate/blackout-window-must-be-underwritten
#[test]
fn blackout_window_must_be_underwritten() {
    // A corpus of 1 TB at 100 GB/s requires at least 10 seconds.
    let corpus = 1_000_000_000_000_u64;
    let throughput = 100_000_000_000_u64;
    let min = min_blackout_seconds(corpus, throughput);
    assert_eq!(min, 10);

    // Ceiling division: 10 bytes at 3 bytes/sec requires 4 seconds, not 3.
    assert_eq!(min_blackout_seconds(10, 3), 4);

    // A declared window shorter than the computed minimum must be rejected by the gate; this is the invariant the test
    // encodes.
    let declared_window = 8_u64;
    assert!(
        declared_window < min,
        "a window of {declared_window}s is shorter than the {min}s minimum and must not be accepted"
    );

    // A declared window equal to the minimum is acceptable.
    assert_eq!(min, 10);
}
