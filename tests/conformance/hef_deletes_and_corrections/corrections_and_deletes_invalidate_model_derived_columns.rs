//! Requirement: Corrections and deletes invalidate model-derived columns.

use hef::deletes::DerivedColumnLedger;
use hef::events::EventId;
use hef::typed_id::TypedIdTestExt;

/// conformance:
/// hef-deletes-and-corrections/corrections-and-deletes-invalidate-model-derived-columns/
/// stale-score-never-served-after-correction
#[test]
fn stale_score_never_served_after_correction() {
    let corrected_event = EventId::new_test_id(1);
    let hawkes_child = EventId::new_test_id(2);
    let unrelated_event = EventId::new_test_id(3);

    let mut ledger = DerivedColumnLedger::new();
    ledger.republish(corrected_event, 0.92, 10);
    ledger.republish(hawkes_child, 0.41, 10);
    ledger.republish(unrelated_event, 0.05, 10);

    // A correction supersedes `corrected_event`, whose published anomaly score has one model-neighbor
    // (`hawkes_child`, the Hawkes-process child that depends on it).
    ledger.invalidate(corrected_event, &[hawkes_child]);

    // Both the corrected event and its model-neighbor read as pending, not their stale pre-correction scores; no
    // query serves the invalidated numbers.
    assert_eq!(ledger.published_value(corrected_event), None);
    assert_eq!(ledger.published_value(hawkes_child), None);
    // An event outside the dependency closure is untouched.
    assert_eq!(ledger.published_value(unrelated_event), Some(0.05));

    assert_eq!(ledger.pending_recomputes(), &[corrected_event, hawkes_child]);

    // Once the enqueued recompute republishes fresh values, they are served again and drop out of the queue.
    ledger.republish(corrected_event, 0.15, 11);
    ledger.republish(hawkes_child, 0.09, 11);
    assert_eq!(ledger.published_value(corrected_event), Some(0.15));
    assert_eq!(ledger.published_value(hawkes_child), Some(0.09));
    assert!(ledger.pending_recomputes().is_empty());
}
