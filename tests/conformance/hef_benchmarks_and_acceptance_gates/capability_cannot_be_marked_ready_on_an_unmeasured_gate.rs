//! Checks that a capability or fast path cannot be marked ready when the gate it must meet has no backing benchmark
//! registered in the harness.

use hef::benchmarks::{BenchGateRegistry, ReadinessError};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/acceptance-gates-are-mechanically-enforced-by-the-committed-harness/
/// capability-cannot-be-marked-ready-on-an-unmeasured-gate
#[test]
fn capability_cannot_be_marked_ready_on_an_unmeasured_gate() {
    let mut registry = BenchGateRegistry::default();

    // A capability with no registered benchmark cannot be marked ready.
    assert_eq!(
        registry.mark_ready("hef-numeric-scan"),
        Err(ReadinessError::NoBenchmark {
            capability: "hef-numeric-scan".to_owned(),
        })
    );
    assert!(!registry.is_ready("hef-numeric-scan"));

    // After a benchmark is registered, the capability may be marked ready.
    registry.register_benchmark("hef-numeric-scan", "bench:hef-scan-avx512");
    registry.mark_ready("hef-numeric-scan").unwrap();
    assert!(registry.is_ready("hef-numeric-scan"));
}
