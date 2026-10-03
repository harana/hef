//! Checks that an accelerator backend is rejected and the software path is used whenever any of the seven enablement
//! conditions fails.

use hef::benchmarks::{AcceleratorConditions, AcceleratorPath, accelerator_path};

fn all_passing() -> AcceleratorConditions {
    AcceleratorConditions {
        batch_exceeds_threshold: true,
        benefit_is_positive: true,
        encoding_compatible: true,
        fallback_available: true,
        parity_passed: true,
        provider_detected: true,
        self_test_passed: true,
    }
}

/// conformance:
/// hef-benchmarks-and-acceptance-gates/accelerator-enablement-gates/accelerator-falls-back-on-failed-parity
#[test]
fn accelerator_falls_back_on_failed_parity() {
    // All conditions met: accelerated path selected.
    assert_eq!(accelerator_path(&all_passing()), AcceleratorPath::Accelerated);

    // Failed parity: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            parity_passed: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );

    // Failed provider detection: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            provider_detected: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );

    // Failed self-test: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            self_test_passed: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );

    // Incompatible encoding: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            encoding_compatible: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );

    // Batch below threshold: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            batch_exceeds_threshold: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );

    // No measured benefit: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            benefit_is_positive: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );

    // No fallback available: software path.
    assert_eq!(
        accelerator_path(&AcceleratorConditions {
            fallback_available: false,
            ..all_passing()
        }),
        AcceleratorPath::Software
    );
}
