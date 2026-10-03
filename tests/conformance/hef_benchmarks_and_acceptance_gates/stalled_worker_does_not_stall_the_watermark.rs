//! Checks that a crashed or stalled ingest worker does not permanently block the commit watermark. The void-record path
//! must close the abandoned reservation range within the lease deadline so watermark advancement can continue past it.

use hef::artifacts::watermark::WatermarkTracker;
use hef::writer::reserve::{LEASE_NANOS, SequenceAllocator};

/// conformance:
/// hef-benchmarks-and-acceptance-gates/latency-paper-alignment-gates/stalled-worker-does-not-stall-the-watermark
#[test]
fn stalled_worker_does_not_stall_the_watermark() {
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();

    // Worker A reserves a range but then stalls — it never calls harden().
    let _stalled_lease = allocator.reserve(10, 0);

    // Worker B reserves the next range and commits it successfully.
    let b_lease = allocator.reserve(5, 0);
    let b_range = allocator.harden(b_lease.lease_id, 0).unwrap();
    watermarks.record_durable(b_range);

    // The watermark cannot advance past the gap left by the stalled worker: B's range starts at sequence 11 so the
    // contiguous prefix is stuck.
    assert!(
        watermarks.commit_watermark().is_none(),
        "watermark cannot advance past the gap left by the stalled worker"
    );

    // Once the lease deadline expires, expire() returns the abandoned range.
    let expired = allocator.expire(LEASE_NANOS + 1);
    assert_eq!(expired.len(), 1, "one abandoned range recovered");
    let void_range = expired[0];

    // Recording the void range as durable closes the gap and frees the watermark to advance past both ranges.
    watermarks.record_durable(void_range);
    let wm = watermarks
        .commit_watermark()
        .expect("watermark advances after void record");
    assert!(
        wm.sequence >= b_range.last_sequence,
        "watermark ({}) must reach at least the end of B's range ({})",
        wm.sequence,
        b_range.last_sequence,
    );
}
