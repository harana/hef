use super::*;

#[test]
fn sequence_point_orders_epoch_before_sequence() {
    // A later epoch outranks any sequence in an earlier epoch, so a huge sequence in epoch 1 still sorts below the
    // smallest sequence in epoch 2 — the epoch-major watermark ordering the codebase relies on.
    let early_epoch_high_sequence = SequencePoint {
        epoch: 1,
        sequence: u64::MAX,
    };
    let late_epoch_low_sequence = SequencePoint { epoch: 2, sequence: 0 };
    assert!(late_epoch_low_sequence > early_epoch_high_sequence);

    // Within one epoch, the sequence breaks the tie.
    let lower = SequencePoint { epoch: 5, sequence: 3 };
    let higher = SequencePoint { epoch: 5, sequence: 4 };
    assert!(higher > lower);
}

#[test]
fn sequence_range_orders_epoch_before_sequences() {
    let early_epoch_high_sequences = SequenceRange {
        epoch: 1,
        first_sequence: 900,
        last_sequence: u64::MAX,
    };
    let late_epoch_low_sequences = SequenceRange {
        epoch: 2,
        first_sequence: 0,
        last_sequence: 1,
    };
    assert!(late_epoch_low_sequences > early_epoch_high_sequences);

    // Same epoch: first_sequence then last_sequence break the tie.
    let lower = SequenceRange {
        epoch: 3,
        first_sequence: 0,
        last_sequence: 5,
    };
    let higher = SequenceRange {
        epoch: 3,
        first_sequence: 1,
        last_sequence: 2,
    };
    assert!(higher > lower);
}
