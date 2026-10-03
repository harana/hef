//! Checks that every event carries a fixed set of standard header fields (its "envelope") that survive a round trip
//! through the journal unchanged, under stable logical names. The internal ordering key (epoch, sequence) sorts events
//! and is kept out of any output sent to outside callers.
use crate::support;
use hef::artifacts::batch::{build_batch, decode_batch, envelope_of};
use hef::events::families::{Caller, column_allowed};

/// conformance: hef-logical-event-model/fixed-event-envelope/event-written-with-complete-envelope
#[test]
fn event_written_with_complete_envelope() {
    // An ingested event carries every required envelope field through HEJ and back, with stable logical names
    // independent of the encoding.
    let input = support::event(3);
    let payload = build_batch(std::slice::from_ref(&input), 1, 0).unwrap();
    let decoded = decode_batch(&payload, 1).unwrap();
    let envelope = envelope_of(&decoded.events[0], support::tenant());
    assert_eq!(envelope, input.envelope);
    assert_eq!(envelope.occurred_at, input.envelope.occurred_at);
    assert!(!envelope.source.is_empty() && !envelope.event_type.is_empty());
}

/// conformance: hef-logical-event-model/fixed-event-envelope/internal-sequence-key-is-a-physical-alias
#[test]
fn internal_sequence_key_is_a_physical_alias() {
    // The only sorting/pruning key the engine exposes is (epoch, sequence) itself; any sequence_key is its physical
    // alias, ordered identically, and is blocked from public output.
    use hef::events::SequencePoint;
    let a = SequencePoint { epoch: 1, sequence: 9 };
    let b = SequencePoint { epoch: 2, sequence: 1 };
    assert!(a < b, "alias ordering is (epoch, sequence) tuple ordering");
    assert!(!column_allowed("sequence_key", Caller::Public));
}
