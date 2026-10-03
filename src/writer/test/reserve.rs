use super::*;
use crate::events::SequenceRange;
use hashbrown::HashSet;

fn range(first: u64, last: u64) -> SequenceRange {
    SequenceRange {
        epoch: 1,
        first_sequence: first,
        last_sequence: last,
    }
}

#[test]
fn arbitrate_coverage_rejects_a_frame_overlapping_already_covered_sequences() {
    // First durable coverage of a sequence wins; a later frame overlapping it is the rejected duplicate. Here an event
    // frame [1,5] wins and a void record [1,5] for the same range (a later commit_voids pass) is rejected.
    assert_eq!(arbitrate_coverage(&[range(1, 5), range(1, 5)]), HashSet::from_iter([1]));
}

#[test]
fn arbitrate_coverage_keeps_disjoint_contiguous_frames() {
    assert!(arbitrate_coverage(&[range(1, 3), range(4, 6), range(7, 7)]).is_empty());
}

#[test]
fn arbitrate_coverage_rejects_a_partial_overlap() {
    // Overlap need not be exact: [4,8] after [1,5] shares sequences 4-5 and is rejected.
    assert_eq!(arbitrate_coverage(&[range(1, 5), range(4, 8)]), HashSet::from_iter([1]));
}

#[test]
fn arbitrate_coverage_across_epochs_does_not_conflate_sequences() {
    // Same sequence numbers in different epochs never overlap, so both are kept.
    let e1 = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 5,
    };
    let e2 = SequenceRange {
        epoch: 2,
        first_sequence: 1,
        last_sequence: 5,
    };
    assert!(arbitrate_coverage(&[e1, e2]).is_empty());
}

#[test]
fn arbitrate_coverage_rejects_a_frame_starting_before_an_already_covered_range() {
    // The overlap sits after the candidate's first sequence: [1,6] starts before the covered [5,9] and still shares
    // sequences 5-6, so it is rejected.
    assert_eq!(arbitrate_coverage(&[range(5, 9), range(1, 6)]), HashSet::from_iter([1]));
}

#[test]
fn arbitrate_coverage_keeps_many_disjoint_frames_in_any_order() {
    // Ranges arrive in segment order, not sequence order; interleaving them must not turn a disjoint frame into a
    // rejection.
    let ranges: Vec<SequenceRange> = (0..1_000)
        .map(|i| {
            let block = if i % 2 == 0 { i / 2 } else { 999 - i / 2 };
            range(block * 10 + 1, block * 10 + 10)
        })
        .collect();
    assert!(arbitrate_coverage(&ranges).is_empty());
}
