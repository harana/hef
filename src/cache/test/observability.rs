use super::*;

#[test]
fn counters_accumulate_across_recordings() {
    let metrics = ColdSegmentMetrics::default();
    metrics.record_hot_hit();
    metrics.record_cold_hit(40);
    metrics.record_cold_hit(60);
    metrics.record_miss();
    metrics.record_demotion(1000, 250, 7);
    metrics.record_demotion(1000, 250, 3);
    metrics.record_incompressible_drop(5);
    assert_eq!(metrics.hot_hits(), 1);
    assert_eq!(metrics.cold_hits(), 2);
    assert_eq!(metrics.misses(), 1);
    assert_eq!(metrics.decompress_nanos(), 100);
    assert_eq!(metrics.compress_nanos(), 15, "kept and dropped compressions both count");
    assert_eq!(metrics.demoted_original_bytes(), 2000);
    assert_eq!(metrics.demoted_compressed_bytes(), 500);
    assert_eq!(metrics.incompressible_drops(), 1);
}

#[test]
fn compression_ratio_is_original_over_compressed_and_zero_before_any_demotion() {
    let metrics = ColdSegmentMetrics::default();
    assert_eq!(metrics.compression_ratio(), 0.0);
    metrics.record_demotion(1000, 250, 1);
    assert_eq!(metrics.compression_ratio(), 4.0);
}

#[test]
fn tiered_counters_accumulate() {
    let metrics = TieredCacheMetrics::default();
    metrics.record_durable_read();
    metrics.record_lower_hit();
    metrics.record_lower_hit();
    assert_eq!(metrics.durable_reads(), 1);
    assert_eq!(metrics.lower_hits(), 2);
}
