use super::*;

fn range(epoch: u64, first: u64, last: u64) -> SequenceRange {
    SequenceRange {
        epoch,
        first_sequence: first,
        last_sequence: last,
    }
}

#[test]
fn commit_watermark_falls_back_to_the_last_fully_covered_epoch() {
    // Regression: only the highest epoch was consulted, so a new epoch whose first durable range did not start at
    // FIRST_SEQUENCE (a failed first lease not yet voided) regressed the commit watermark to None.
    let mut tracker = WatermarkTracker::new();
    tracker.record_durable(range(1, 1, 10));
    tracker.record_durable(range(2, 3, 5));
    assert_eq!(
        tracker.commit_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 10 }),
        "an incomplete newer epoch must not erase the prior epoch's fully durable prefix"
    );
    // Once voids close the hole at the epoch start, the watermark moves into epoch 2.
    tracker.record_durable(range(2, 1, 2));
    assert_eq!(
        tracker.commit_watermark(),
        Some(SequencePoint { epoch: 2, sequence: 5 })
    );
}

#[test]
fn visibility_watermark_falls_back_across_epochs_too() {
    let mut tracker = WatermarkTracker::new();
    tracker.record_visible(range(1, 1, 7));
    tracker.record_visible(range(2, 4, 4));
    assert_eq!(
        tracker.visibility_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 7 })
    );
}

#[test]
fn visibly_covers_answers_coverage_within_any_epoch() {
    // Regression: acknowledgement needed a cross-epoch coverage answer; the single watermark point could only express
    // the newest epoch, stalling ReadAfterAck acknowledgements for earlier epochs forever.
    let mut tracker = WatermarkTracker::new();
    tracker.record_visible(range(1, 1, 10));
    tracker.record_visible(range(2, 1, 1));
    assert!(tracker.visibly_covers(SequencePoint { epoch: 1, sequence: 10 }));
    assert!(tracker.visibly_covers(SequencePoint { epoch: 2, sequence: 1 }));
    assert!(!tracker.visibly_covers(SequencePoint { epoch: 1, sequence: 11 }));
    assert!(!tracker.visibly_covers(SequencePoint { epoch: 2, sequence: 2 }));
    // A gap at an epoch's start means nothing in that epoch is covered yet, and unknown epochs are never covered.
    tracker.record_visible(range(3, 3, 5));
    assert!(!tracker.visibly_covers(SequencePoint { epoch: 3, sequence: 3 }));
    assert!(!tracker.visibly_covers(SequencePoint { epoch: 4, sequence: 1 }));
}

#[test]
fn range_set_insert_merges_adjacent_and_out_of_order_ranges() {
    // Adjacent ranges (touching at the boundary with no overlap) merge into one.
    let mut adjacent = RangeSet::default();
    adjacent.insert(1, 3);
    adjacent.insert(4, 6);
    assert!(adjacent.covers(1, 6), "adjacent ranges must merge into a single range");
    assert_eq!(adjacent.max_covered(), Some(6));
    assert_eq!(adjacent.contiguous_prefix_end(FIRST_SEQUENCE), Some(6));

    // Out-of-order and overlapping inserts still collapse to one contiguous range.
    let mut out_of_order = RangeSet::default();
    out_of_order.insert(5, 6);
    out_of_order.insert(1, 2);
    out_of_order.insert(2, 4);
    assert!(out_of_order.covers(1, 6), "out-of-order overlapping inserts must merge");
    assert_eq!(out_of_order.contiguous_prefix_end(FIRST_SEQUENCE), Some(6));

    // A detached range stays separate: the contiguous prefix stops at the first hole.
    let mut split = RangeSet::default();
    split.insert(1, 2);
    split.insert(5, 6);
    assert_eq!(split.contiguous_prefix_end(FIRST_SEQUENCE), Some(2));
    assert!(!split.covers(1, 6), "a hole leaves the ranges un-merged");
}

#[test]
fn seal_epoch_fills_abandoned_gaps_as_void_and_advances_visibility() {
    // Sequences 4..=6 were reserved but never written (abandoned): a hole in both durable and visible coverage.
    let mut tracker = WatermarkTracker::new();
    tracker.record_durable(range(1, 1, 3));
    tracker.record_durable(range(1, 7, 9));
    tracker.record_visible(range(1, 1, 3));
    tracker.record_visible(range(1, 7, 9));
    assert_eq!(
        tracker.visibility_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 3 }),
        "before sealing, visibility stalls at the abandoned hole"
    );

    tracker.seal_epoch(1);
    // The abandoned hole is now a void range (zero rows), so both watermarks reach the epoch's end.
    assert_eq!(
        tracker.commit_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 9 })
    );
    assert_eq!(
        tracker.visibility_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 9 })
    );
}

#[test]
fn seal_epoch_does_not_publish_a_durable_event_frame_not_yet_republished() {
    // Sequences 4..=6 are durable real events, not a void — durable coverage has no hole — but they have not been
    // republished into the overlay yet, so visible coverage has a hole there.
    let mut tracker = WatermarkTracker::new();
    tracker.record_durable(range(1, 1, 9));
    tracker.record_visible(range(1, 1, 3));
    tracker.record_visible(range(1, 7, 9));

    tracker.seal_epoch(1);
    // Durable coverage marks nothing void, so sealing must not make the unpublished real events visible; visibility
    // stalls before them so a query never skips real rows.
    assert_eq!(
        tracker.commit_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 9 })
    );
    assert_eq!(
        tracker.visibility_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 3 }),
        "visibility must stall before a durable-but-unpublished range"
    );
}

/// The merge `RangeSet::insert` used before it merged in place: rebuilds the whole list into a fresh vector. Kept here
/// as the reference the in-place merge must match range for range.
fn reference_insert(ranges: &mut Vec<(u64, u64)>, first: u64, last: u64) {
    if last < first {
        return;
    }
    let mut merged = Vec::with_capacity(ranges.len() + 1);
    let mut new_first = first;
    let mut new_last = last;
    let mut placed = false;
    for (start, end) in ranges.iter() {
        if end.saturating_add(1) < new_first && !placed {
            merged.push((*start, *end));
        } else if new_last.saturating_add(1) < *start {
            if !placed {
                merged.push((new_first, new_last));
                placed = true;
            }
            merged.push((*start, *end));
        } else {
            new_first = new_first.min(*start);
            new_last = new_last.max(*end);
        }
    }
    if !placed {
        merged.push((new_first, new_last));
    }
    *ranges = merged;
}

fn assert_inserts_match_reference(inserts: &[(u64, u64)]) {
    let mut set = RangeSet::default();
    let mut reference = Vec::new();
    for &(first, last) in inserts {
        set.insert(first, last);
        reference_insert(&mut reference, first, last);
        assert_eq!(
            set.ranges, reference,
            "after inserting {first}..={last} from {inserts:?}"
        );
    }
    let expected_max = reference.last().map(|(_, end)| *end);
    assert_eq!(set.max_covered(), expected_max);
    let expected_prefix = reference
        .first()
        .filter(|(start, _)| *start <= FIRST_SEQUENCE)
        .map(|(_, end)| *end);
    assert_eq!(set.contiguous_prefix_end(FIRST_SEQUENCE), expected_prefix);
    for &(start, end) in &reference {
        assert!(set.covers(start, end));
    }
    let to = expected_max.unwrap_or(0);
    assert_eq!(set.gaps(0, to), RangeSet { ranges: reference }.gaps(0, to));
}

#[test]
fn range_set_in_place_merge_matches_the_rebuilding_merge() {
    let edge_cases: &[&[(u64, u64)]] = &[
        &[],
        &[(5, 5)],
        &[(5, 3)],
        &[(10, 20), (5, 3)],
        &[(10, 20), (10, 20)],
        &[(1, 5), (10, 20), (2, 3)],
        &[(1, 5), (10, 20), (12, 15)],
        &[(1, 2), (10, 20), (5, 6)],
        &[(1, 2), (10, 20), (30, 40), (3, 9)],
        &[(1, 2), (10, 20), (30, 40), (3, 29)],
        &[(1, 2), (10, 20), (30, 40), (0, 100)],
        &[(1, 2), (10, 20), (30, 40), (15, 35)],
        &[(1, 2), (10, 20), (30, 40), (21, 29)],
        &[(1, 2), (10, 20), (30, 40), (22, 28)],
        &[(0, 0), (u64::MAX, u64::MAX), (1, 1)],
        &[(u64::MAX - 5, u64::MAX), (0, 3), (4, u64::MAX - 6)],
        &[(u64::MAX, u64::MAX), (0, u64::MAX)],
        &[(10, u64::MAX), (0, 8), (9, 9)],
    ];
    for inserts in edge_cases {
        assert_inserts_match_reference(inserts);
    }

    let ascending: Vec<(u64, u64)> = (0..64).map(|i| (i * 3, i * 3 + 1)).collect();
    assert_inserts_match_reference(&ascending);
    let descending: Vec<(u64, u64)> = ascending.iter().rev().copied().collect();
    assert_inserts_match_reference(&descending);

    // Deterministic xorshift so the shuffled and random sequences are reproducible.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut shuffled = ascending.clone();
    for i in (1..shuffled.len()).rev() {
        shuffled.swap(i, (next() % (i as u64 + 1)) as usize);
    }
    assert_inserts_match_reference(&shuffled);
    for _ in 0..200 {
        let inserts: Vec<(u64, u64)> = (0..40)
            .map(|_| {
                let first = next() % 200;
                let last = first.wrapping_add(next() % 12).wrapping_sub(2);
                (first, last)
            })
            .collect();
        assert_inserts_match_reference(&inserts);
    }
}
