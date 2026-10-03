//! Tracks how far the event log is durable and how far it is safe to query, as a few "watermark" positions per epoch.
//!
//! A watermark is the highest sequence such that every sequence below it is accounted for: the commit watermark covers
//! everything durably written (events and the placeholder void ranges that fill abandoned gaps), and the visibility
//! watermark covers everything safe for a query to read. Snapshots are captured at or below the visibility watermark.

use crate::events::{SequencePoint, SequenceRange};

/// Epoch sequences are allocated from 1; contiguity for watermark purposes is measured from here. Internal constant,
/// never an operator knob.
pub const FIRST_SEQUENCE: u64 = 1;

/// A set of disjoint inclusive sequence ranges within one epoch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RangeSet {
    /// Sorted, non-overlapping, non-adjacent `(first, last)` ranges.
    ranges: Vec<(u64, u64)>,
}

impl RangeSet {
    /// Adds the inclusive range `first..=last`, merging it with any ranges it touches or overlaps so the set stays
    /// disjoint. An inverted range (`last < first`) is ignored.
    pub fn insert(&mut self, first: u64, last: u64) {
        if last < first {
            return;
        }
        // Fast path: a committed frame's range almost always extends, or lands strictly after, the most recently
        // inserted range (sequences are committed in increasing order), so the common case updates or appends in
        // place instead of reallocating and rebuilding the whole set.
        match self.ranges.last_mut() {
            None => {
                self.ranges.push((first, last));
                return;
            }
            Some((last_start, last_end)) if first >= *last_start && first <= last_end.saturating_add(1) => {
                *last_end = (*last_end).max(last);
                return;
            }
            Some((_, last_end)) if first > last_end.saturating_add(1) => {
                self.ranges.push((first, last));
                return;
            }
            Some(_) => {}
        }
        let mut merged = Vec::with_capacity(self.ranges.len() + 1);
        let mut new_first = first;
        let mut new_last = last;
        let mut placed = false;
        for (start, end) in &self.ranges {
            if end.saturating_add(1) < new_first && !placed {
                merged.push((*start, *end));
            } else if new_last.saturating_add(1) < *start {
                if !placed {
                    merged.push((new_first, new_last));
                    placed = true;
                }
                merged.push((*start, *end));
            } else {
                // Overlapping or adjacent: absorb.
                new_first = new_first.min(*start);
                new_last = new_last.max(*end);
            }
        }
        if !placed {
            merged.push((new_first, new_last));
        }
        self.ranges = merged;
    }

    /// Highest `n` such that `FIRST_SEQUENCE..=n` is fully covered.
    pub fn contiguous_prefix_end(&self, from: u64) -> Option<u64> {
        let (start, end) = self.ranges.first()?;
        if *start > from {
            return None;
        }
        Some(*end)
    }

    /// True when a single stored range fully contains `first..=last`.
    pub fn covers(&self, first: u64, last: u64) -> bool {
        self.ranges.iter().any(|(start, end)| *start <= first && last <= *end)
    }

    /// True when no ranges have been added.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// The highest sequence in any stored range, or `None` when empty.
    pub fn max_covered(&self) -> Option<u64> {
        self.ranges.last().map(|(_, end)| *end)
    }

    /// The uncovered `(first, last)` subranges of `from..=to` — the holes between the stored ranges. Used at seal time
    /// to find an epoch's abandoned reservations without disturbing the stored ranges.
    fn gaps(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        let mut holes = Vec::new();
        let mut cursor = from;
        for (start, end) in &self.ranges {
            if *end < from {
                continue;
            }
            if *start > to {
                break;
            }
            if *start > cursor {
                holes.push((cursor, start - 1));
            }
            cursor = end.saturating_add(1);
            if cursor > to {
                break;
            }
        }
        if cursor <= to {
            holes.push((cursor, to));
        }
        holes
    }
}

#[derive(Debug, Clone, Default)]
struct EpochCoverage {
    /// Durably covered by an event frame or a committed void range.
    durable: RangeSet,
    sealed: bool,
    /// Published into reader-local LiveOverlay or manifest-published HEF and safe for the query mode; void ranges
    /// count as covered (they publish zero rows).
    visible: RangeSet,
}

/// Tracks the three explicit watermarks across epochs. Epochs advance monotonically; a sealed epoch's interior gaps are
/// permanently abandoned reservations and count as void coverage (its commit watermark is its highest durable
/// sequence).
#[derive(Debug, Default)]
pub struct WatermarkTracker {
    epochs: std::collections::BTreeMap<u64, EpochCoverage>,
}

impl WatermarkTracker {
    /// Starts a tracker with no coverage recorded for any epoch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a durably covered range: an event frame or a committed void record.
    pub fn record_durable(&mut self, range: SequenceRange) {
        self.epochs
            .entry(range.epoch)
            .or_default()
            .durable
            .insert(range.first_sequence, range.last_sequence);
    }

    /// Records a range published for the query mode (LiveOverlay or HEF). Void ranges are recorded here too: they
    /// publish zero rows but count for contiguity.
    pub fn record_visible(&mut self, range: SequenceRange) {
        self.epochs
            .entry(range.epoch)
            .or_default()
            .visible
            .insert(range.first_sequence, range.last_sequence);
    }

    /// Seals an epoch (leader change or crash recovery): every sequence below its highest durable frame that is not
    /// covered by a durable event frame or void record becomes a permanently abandoned void range — replay reconstructs
    /// coverage with interior void ranges rather than stalling at the first gap.
    pub fn seal_epoch(&mut self, epoch: u64) {
        if let Some(coverage) = self.epochs.get_mut(&epoch) {
            coverage.sealed = true;
            let Some(max) = coverage.durable.max_covered() else {
                return;
            };
            // Interior gaps in durable coverage are permanently abandoned reservations — void ranges. Capture them
            // before filling durable, then publish exactly those voids into visible coverage: a void range carries
            // zero rows, so it is always safe to read. A sequence that is durably covered but not yet visible is a
            // real event frame still awaiting republication into the overlay, not a void; sealing must not make it
            // visible or a query would skip real rows.
            let voids = coverage.durable.gaps(FIRST_SEQUENCE, max);
            coverage.durable.insert(FIRST_SEQUENCE, max);
            for (first, last) in voids {
                coverage.visible.insert(first, last);
            }
        }
    }

    fn watermark_over(&self, pick: impl Fn(&EpochCoverage) -> &RangeSet) -> Option<SequencePoint> {
        // The watermark lives in the highest epoch whose coverage reaches back to the epoch start. A newer epoch
        // whose first covered range has not yet reached `FIRST_SEQUENCE` (a failed first lease awaiting voids) must
        // not regress the watermark: the previous epoch's fully covered prefix still answers until the new epoch's
        // prefix forms.
        self.epochs.iter().rev().find_map(|(epoch, coverage)| {
            let end = pick(coverage).contiguous_prefix_end(FIRST_SEQUENCE)?;
            Some(SequencePoint {
                epoch: *epoch,
                sequence: end,
            })
        })
    }

    /// Highest `(epoch, sequence)` such that every lower sequence in the epoch is durably covered by an event frame or
    /// a committed void range.
    pub fn commit_watermark(&self) -> Option<SequencePoint> {
        self.watermark_over(|coverage| &coverage.durable)
    }

    /// Highest `(epoch, sequence)` published and safe for the query mode.
    pub fn visibility_watermark(&self) -> Option<SequencePoint> {
        self.watermark_over(|coverage| &coverage.visible)
    }

    /// Captures a stable snapshot upper bound: `<= visibility_watermark`.
    pub fn capture_snapshot_watermark(&self) -> Option<SequencePoint> {
        self.visibility_watermark()
    }

    /// Compatibility alias: `durable_journal_cursor = commit_watermark`.
    pub fn durable_journal_cursor(&self) -> Option<SequencePoint> {
        self.commit_watermark()
    }

    /// Compatibility alias: `live_queryable_cursor` is the LiveOverlay component of `visibility_watermark`.
    pub fn live_queryable_cursor(&self) -> Option<SequencePoint> {
        self.visibility_watermark()
    }

    /// True when the durable coverage of `range`'s epoch fully contains it.
    pub fn durably_covers(&self, range: &SequenceRange) -> bool {
        self.epochs
            .get(&range.epoch)
            .is_some_and(|coverage| coverage.durable.covers(range.first_sequence, range.last_sequence))
    }

    /// True when `point` and every sequence before it in its epoch are published and safe to read. This is the
    /// cross-epoch acknowledgement query: an event in an earlier, fully published epoch stays acknowledgeable after
    /// later epochs begin, which the single `visibility_watermark` point cannot express.
    pub fn visibly_covers(&self, point: SequencePoint) -> bool {
        self.epochs
            .get(&point.epoch)
            .is_some_and(|coverage| coverage.visible.covers(FIRST_SEQUENCE, point.sequence))
    }
}

#[cfg(test)]
#[path = "test/watermark.rs"]
mod tests;
