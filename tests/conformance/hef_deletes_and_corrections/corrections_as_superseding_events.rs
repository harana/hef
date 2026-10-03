//! Requirement: Corrections as superseding events.

use hef::deletes::{CorrectionMetadata, CorrectionType, DeletionVector, latest_corrected_deletion_vector};
use hef::events::EventId;
use hef::typed_id::TypedIdTestExt;

/// conformance: hef-deletes-and-corrections/corrections-as-superseding-events/latest-corrected-view
#[test]
fn latest_corrected_view() {
    let original_event = EventId::new_test_id(0xDEAD);
    let correcting_event = EventId::new_test_id(0xBEEF);
    let untouched_event = EventId::new_test_id(0xCAFE);

    // The file's rows, in ordinal order: the original event, its correction, and an unrelated event.
    let events = [
        (0u64, original_event),
        (1u64, correcting_event),
        (2u64, untouched_event),
    ];

    let correction = CorrectionMetadata {
        correction_epoch: 2,
        correction_generation: 1,
        correction_sequence: 99,
        correction_type: CorrectionType::Replacement,
        corrects_event_id: original_event,
    };

    // A raw-history view applies no supersession vector: every ordinal, including the superseded original, stays
    // visible.
    let raw_history = DeletionVector::new();
    assert!(!raw_history.is_deleted(0));

    // A query selecting the latest-corrected view applies the supersession vector: the superseded event is
    // suppressed and the correction (and everything else) is shown.
    let latest_corrected = latest_corrected_deletion_vector(&events, &[correction]);
    assert!(latest_corrected.is_deleted(0)); // original_event: suppressed
    assert!(!latest_corrected.is_deleted(1)); // correcting_event: shown
    assert!(!latest_corrected.is_deleted(2)); // untouched_event: shown
    assert_eq!(latest_corrected.deleted_count(), 1);
}
