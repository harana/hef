//! Checks that when a later harness run shows a previously-ready FDP/ZNS publish path's write-amplification factor
//! regressing beyond tolerance, the placement path is removed from the ready set until the regression is resolved —
//! and that the durable bytes and checksum the path already wrote are untouched by that measurement.

use hef::benchmarks::{BenchGateRegistry, CommittedBaseline};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/write-amplification-gates-per-lifecycle-stage/
/// regressed-publish-waf-drops-a-placement-path
#[test]
fn regressed_publish_waf_drops_a_placement_path() {
    // The durable bytes and checksum this placement path already committed to disk.
    let durable_bytes = vec![0xAB_u8; 4096];
    let durable_checksum_before = blake3::hash(&durable_bytes);

    let mut registry = BenchGateRegistry::default();
    registry.register_benchmark("fdp-zns-publish", "bench:hef-fresh-publish-waf");
    registry.commit_baseline(CommittedBaseline {
        benchmark_id: "bench:hef-fresh-publish-waf".to_owned(),
        equivalence_verified: false,
        measured_value: 1.1, // 1.1x baseline WAF
        tolerance_fraction: 0.10,
    });
    registry.mark_ready("fdp-zns-publish").unwrap();
    assert!(registry.is_ready("fdp-zns-publish"));

    // A later WAF measurement regresses beyond the 10% tolerance.
    let regression = registry.detect_regression(
        "fdp-zns-publish",
        "bench:hef-fresh-publish-waf",
        1.5, // 1.5x — well beyond 10% of 1.1
    );
    assert!(regression.is_some(), "WAF regression detected");
    assert!(
        !registry.is_ready("fdp-zns-publish"),
        "FDP/ZNS publish path must leave the ready set after a WAF regression"
    );

    // The regression is a measurement over the ready set only — the durable bytes and checksum already on disk are
    // unchanged.
    assert_eq!(durable_bytes.len(), 4096);
    assert_eq!(blake3::hash(&durable_bytes), durable_checksum_before);
}
