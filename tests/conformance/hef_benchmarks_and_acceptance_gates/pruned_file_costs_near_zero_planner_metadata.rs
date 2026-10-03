//! Checks that a file pruned out during planning on a build claiming lazy per-stripe marks costs near-zero planner
//! metadata bytes, and that the gate fails if it still costs its full share of granule-level marks bytes, dropping the
//! path from the ready set the same way the harness's `paths.*` gate does.

use hef::benchmarks::{
    BenchGateRegistry, CommittedBaseline, PrunedFileMetadataMeasurement, pruned_file_metadata_gate_passes,
};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/metadata-economics-gates-for-cold-opens-and-pruning/
/// pruned-file-costs-near-zero-planner-metadata
#[test]
fn pruned_file_costs_near_zero_planner_metadata() {
    // Near-zero bytes read for the pruned file: passes.
    assert!(pruned_file_metadata_gate_passes(&PrunedFileMetadataMeasurement {
        full_granule_marks_share_bytes: 4096,
        planner_metadata_bytes_read: 0,
    }));

    // Still costs its full granule-level marks share: fails.
    assert!(!pruned_file_metadata_gate_passes(&PrunedFileMetadataMeasurement {
        full_granule_marks_share_bytes: 4096,
        planner_metadata_bytes_read: 4096,
    }));

    // Costs more than its full share: fails.
    assert!(!pruned_file_metadata_gate_passes(&PrunedFileMetadataMeasurement {
        full_granule_marks_share_bytes: 4096,
        planner_metadata_bytes_read: 5000,
    }));

    // A previously-ready pruned-file path is dropped from the ready set when a later measurement regresses planner
    // metadata bytes above the near-zero budget, naming the benchmark and the delta.
    let mut registry = BenchGateRegistry::default();
    registry.register_benchmark("pruned-file-planner-bytes", "bench:hef-pruned-file-planner-bytes");
    registry.commit_baseline(CommittedBaseline {
        benchmark_id: "bench:hef-pruned-file-planner-bytes".to_owned(),
        equivalence_verified: false,
        measured_value: 0.0, // near-zero — the gated budget
        tolerance_fraction: 0.0,
    });
    registry.mark_ready("pruned-file-planner-bytes").unwrap();
    assert!(registry.is_ready("pruned-file-planner-bytes"));

    let regression = registry
        .detect_regression(
            "pruned-file-planner-bytes",
            "bench:hef-pruned-file-planner-bytes",
            4096.0, // the file's full granule-level marks share, not near-zero
        )
        .expect("planner-metadata-bytes regression detected");
    assert_eq!(regression.benchmark_id, "bench:hef-pruned-file-planner-bytes");
    assert_eq!(regression.baseline_value, 0.0);
    assert_eq!(regression.measured_value, 4096.0);
    assert!(
        !registry.is_ready("pruned-file-planner-bytes"),
        "pruned-file path must leave the ready set once planner bytes regress above the near-zero budget"
    );
}
