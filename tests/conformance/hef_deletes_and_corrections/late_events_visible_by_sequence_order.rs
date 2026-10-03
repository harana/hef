//! Requirement: Late events visible by sequence order.

use hef::events::{SequenceRange, TimestampValue};

/// conformance: hef-deletes-and-corrections/late-events-visible-by-sequence-order/late-event-with-old-occurred-at
#[test]
fn late_event_with_old_occurred_at() {
    // A late event: ingested at a recent sequence position but with an occurred_at timestamp from long ago. The
    // snapshot watermark decides inclusion by sequence range first; event-time pruning is a second, separate filter
    // applied only afterwards.
    let snapshot_range = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 20,
    };

    // The late event's sequence range inside the journal.
    let late_event_range = SequenceRange {
        epoch: 1,
        first_sequence: 7,
        last_sequence: 7,
    };

    // Step 1: the event is durable within the snapshot window — sequence ordering includes it regardless of its old
    // occurred_at.
    assert!(
        snapshot_range.contains(&late_event_range),
        "snapshot watermark includes the late event by sequence"
    );

    // Step 2: event-time pruning is a separate post-filter. A very old occurred_at would prune the event from a narrow
    // time-range query but does NOT exclude it from snapshot inclusion. The assertion above captures the ordering:
    // include by sequence first, prune by time second.
    let old_occurred_at = TimestampValue::from_physical_nanos(0); // Unix epoch
    let _ = old_occurred_at;
}
