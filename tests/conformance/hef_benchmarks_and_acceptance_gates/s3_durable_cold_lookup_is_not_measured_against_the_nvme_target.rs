//! Checks that an `entity_id` lookup on `cold-cache/S3-durable` is held to the object-store round-trip budget and is
//! never passed or failed against the sub-5 ms `warm`/`cold-cache/NVMe-durable` figure.

use hef::benchmarks::{S3DurableLookupMeasurement, s3_durable_lookup_gate_passes};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/target-numbers-and-query-ready-gate/
/// s3-durable-cold-lookup-is-not-measured-against-the-nvme-target
#[test]
fn s3_durable_cold_lookup_is_not_measured_against_the_nvme_target() {
    // At the round-trip target and within its own budget: passes, even though 12 ms is well above the 5 ms
    // NVMe-cold figure — this profile is never judged against that figure.
    assert!(s3_durable_lookup_gate_passes(&S3DurableLookupMeasurement {
        dependent_round_trips: 2,
        elapsed_millis: 12.0,
        per_request_millis_budget: 8.0,
    }));

    // Over the round-trip target: fails, regardless of elapsed time.
    assert!(!s3_durable_lookup_gate_passes(&S3DurableLookupMeasurement {
        dependent_round_trips: 3,
        elapsed_millis: 4.0,
        per_request_millis_budget: 8.0,
    }));

    // Within the round-trip target but over its own per-request budget: fails.
    assert!(!s3_durable_lookup_gate_passes(&S3DurableLookupMeasurement {
        dependent_round_trips: 2,
        elapsed_millis: 20.0,
        per_request_millis_budget: 8.0,
    }));
}
