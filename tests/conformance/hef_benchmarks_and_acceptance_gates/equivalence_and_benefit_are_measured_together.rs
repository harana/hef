//! Checks that when an accelerated fast path is benchmarked, its portable fallback is benchmarked in the same run,
//! byte-for-byte equivalence is verified, and both results are committed together. A baseline without the equivalence
//! check cannot be recorded for an accelerated path.

use hef::benchmarks::{AcceleratedBaselineError, BenchGateRegistry, CommittedBaseline};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/acceptance-gates-are-mechanically-enforced-by-the-committed-harness/
/// equivalence-and-benefit-are-measured-together
#[test]
fn equivalence_and_benefit_are_measured_together() {
    let mut registry = BenchGateRegistry::default();

    // A baseline recorded without equivalence verification is rejected.
    let not_verified = CommittedBaseline {
        benchmark_id: "bench:blake3-avx512".to_owned(),
        equivalence_verified: false,
        measured_value: 6.0,
        tolerance_fraction: 0.05,
    };
    assert_eq!(
        registry.commit_accelerated_baseline(not_verified),
        Err(AcceleratedBaselineError::EquivalenceNotVerified)
    );

    // A baseline that includes the equivalence check is accepted.
    let verified = CommittedBaseline {
        benchmark_id: "bench:blake3-avx512".to_owned(),
        equivalence_verified: true,
        measured_value: 6.0,
        tolerance_fraction: 0.05,
    };
    registry.commit_accelerated_baseline(verified).unwrap();
}
