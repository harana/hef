//! Checks the bookmarks that track how far writes have progressed: how much is durable, how much is visible to readers,
//! and the point a read snapshot is pinned to. The snapshot bookmark can never run ahead of what is visible, gaps
//! filled by empty placeholder ranges still count as continuous, and the older aliases for these bookmarks keep
//! returning the same values.

use hef::artifacts::watermark::WatermarkTracker;
use hef::events::{SequencePoint, SequenceRange};

/// conformance: hef-physical-artifacts/explicit-watermark-model/snapshot-bound-respected
#[test]
fn snapshot_bound_respected() {
    // A captured snapshot_watermark is at most the current visibility_watermark; void ranges count for contiguity; the
    // compatibility aliases hold.
    let mut tracker = WatermarkTracker::new();
    tracker.record_durable(SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 8,
    });
    tracker.record_visible(SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 3,
    });
    // A void range published with zero rows keeps visibility contiguous.
    tracker.record_visible(SequenceRange {
        epoch: 1,
        first_sequence: 4,
        last_sequence: 5,
    });
    let visibility = tracker.visibility_watermark().unwrap();
    assert_eq!(visibility, SequencePoint { epoch: 1, sequence: 5 });
    let snapshot = tracker.capture_snapshot_watermark().unwrap();
    assert!(snapshot <= visibility);
    assert_eq!(tracker.durable_journal_cursor(), tracker.commit_watermark());
    assert_eq!(
        tracker.commit_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 8 })
    );
    assert_eq!(tracker.live_queryable_cursor(), Some(visibility));
}
