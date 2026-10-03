//! Checks that a query class is not admitted to the ready set when it passes on warm cache but fails on cold cache,
//! unless it is explicitly excluded.

use hef::benchmarks::{QueryClassMeasurement, query_class_is_ready};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/target-numbers-and-query-ready-gate/
/// query-class-not-marked-ready-on-cold-cache-miss
#[test]
fn query_class_not_marked_ready_on_cold_cache_miss() {
    // Passes warm cache but fails cold cache: not ready.
    assert!(!query_class_is_ready(&QueryClassMeasurement {
        cold_cache_passes: false,
        explicitly_excluded: false,
        warm_cache_passes: true,
    }));

    // Fails both profiles: not ready.
    assert!(!query_class_is_ready(&QueryClassMeasurement {
        cold_cache_passes: false,
        explicitly_excluded: false,
        warm_cache_passes: false,
    }));

    // Passes both profiles: ready.
    assert!(query_class_is_ready(&QueryClassMeasurement {
        cold_cache_passes: true,
        explicitly_excluded: false,
        warm_cache_passes: true,
    }));

    // Explicitly excluded: ready regardless of cache profiles.
    assert!(query_class_is_ready(&QueryClassMeasurement {
        cold_cache_passes: false,
        explicitly_excluded: true,
        warm_cache_passes: false,
    }));
}
