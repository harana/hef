//! Checks that a cold open on `cold-cache/S3-durable` fails the metadata-economics gate when it issues more than the
//! gated requests-per-cold-open count without a recorded justification, and that a regression drops the path from the
//! ready set the same way the harness's `paths.*` gate does.

use hef::benchmarks::{BenchGateRegistry, ColdOpenMeasurement, CommittedBaseline, cold_open_gate_passes};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/metadata-economics-gates-for-cold-opens-and-pruning/
/// cold-open-exceeds-the-one-request-budget
#[test]
fn cold_open_exceeds_the_one_request_budget() {
    // At the target: passes.
    assert!(cold_open_gate_passes(&ColdOpenMeasurement {
        justification_recorded: false,
        request_count: 1,
    }));

    // Over target with no recorded justification: fails.
    assert!(!cold_open_gate_passes(&ColdOpenMeasurement {
        justification_recorded: false,
        request_count: 3,
    }));

    // Over target with a recorded justification: passes.
    assert!(cold_open_gate_passes(&ColdOpenMeasurement {
        justification_recorded: true,
        request_count: 3,
    }));

    // A previously-ready cold-open path is dropped from the ready set when a later measurement regresses the
    // requests-per-cold-open count beyond the gated budget, naming the benchmark and the delta.
    let mut registry = BenchGateRegistry::default();
    registry.register_benchmark(
        "entity-id-cold-open-s3-durable",
        "bench:hef-cold-open-requests-s3-durable",
    );
    registry.commit_baseline(CommittedBaseline {
        benchmark_id: "bench:hef-cold-open-requests-s3-durable".to_owned(),
        equivalence_verified: false,
        measured_value: 1.0, // 1 request — the gated budget
        tolerance_fraction: 0.0,
    });
    registry.mark_ready("entity-id-cold-open-s3-durable").unwrap();
    assert!(registry.is_ready("entity-id-cold-open-s3-durable"));

    let regression = registry
        .detect_regression(
            "entity-id-cold-open-s3-durable",
            "bench:hef-cold-open-requests-s3-durable",
            3.0, // 3 requests — over the 1-request budget
        )
        .expect("cold-open regression detected");
    assert_eq!(regression.benchmark_id, "bench:hef-cold-open-requests-s3-durable");
    assert_eq!(regression.baseline_value, 1.0);
    assert_eq!(regression.measured_value, 3.0);
    assert!(
        !registry.is_ready("entity-id-cold-open-s3-durable"),
        "cold-open path must leave the ready set once it regresses beyond the one-request budget"
    );
}
