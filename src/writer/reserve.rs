//! Hands each worker a contiguous block of sequence numbers to commit, and reclaims any block a stalled worker fails to
//! use in time.
//!
//! A worker is given its final `(epoch, sequence)` range only when it claims pending records for a frame. The range is
//! a lease, not a permanent claim: one not made durable before its deadline is abandoned and closed by a placeholder
//! void record so the contiguous commit watermark can advance past it. For any one range a void record and an event
//! frame are mutually exclusive, with durable commit order deciding the winner.

use super::error::LeaseError;
use crate::artifacts::watermark::FIRST_SEQUENCE;
use crate::events::SequenceRange;
use hashbrown::HashSet;
use std::collections::BTreeMap;

/// The low-load force-commit idle target: 50 µs internal maximum unless benchmark gates prove a better value. Not an
/// operator knob.
pub const FORCE_COMMIT_IDLE_NANOS: u64 = 50_000;

/// Lease deadline = this internal multiple of the force-commit interval, selected by benchmark gates later; never an
/// operator knob.
pub const LEASE_INTERVAL_MULTIPLE: u64 = 64;

/// The lease duration in nanoseconds.
pub const LEASE_NANOS: u64 = FORCE_COMMIT_IDLE_NANOS * LEASE_INTERVAL_MULTIPLE;

/// One outstanding reservation lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationLease {
    pub deadline_monotonic_nanos: u64,
    pub lease_id: u64,
    pub range: SequenceRange,
}

/// Per-epoch contiguous sequence allocator with lease tracking.
#[derive(Debug)]
pub struct SequenceAllocator {
    epoch: u64,
    next_lease_id: u64,
    next_sequence: u64,
    outstanding: BTreeMap<u64, ReservationLease>,
}

impl SequenceAllocator {
    /// A fresh allocator for `epoch`, handing out sequences from the start with no leases outstanding.
    pub fn new(epoch: u64) -> Self {
        Self {
            epoch,
            next_sequence: FIRST_SEQUENCE,
            next_lease_id: 1,
            outstanding: BTreeMap::new(),
        }
    }

    /// The epoch this allocator hands out sequences within.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Reserves one contiguous `(epoch, sequence)` range under a lease.
    pub fn reserve(&mut self, count: u64, now_monotonic_nanos: u64) -> ReservationLease {
        let count = count.max(1);
        let first_sequence = self.next_sequence;
        self.next_sequence += count;
        let lease = ReservationLease {
            lease_id: self.next_lease_id,
            range: SequenceRange {
                epoch: self.epoch,
                first_sequence,
                last_sequence: first_sequence + count - 1,
            },
            deadline_monotonic_nanos: now_monotonic_nanos + LEASE_NANOS,
        };
        self.next_lease_id += 1;
        self.outstanding.insert(lease.lease_id, lease.clone());
        lease
    }

    /// Consumes the lease for hardening. A caller whose lease deadline has passed gets `LeaseError::Expired` back
    /// instead of a silent success, so it can decide whether the underlying work is still worth keeping (see
    /// `accept_late`) or should be abandoned to `expire`.
    pub fn harden(&mut self, lease_id: u64, now_monotonic_nanos: u64) -> Result<SequenceRange, LeaseError> {
        let lease = self.outstanding.get(&lease_id).ok_or(LeaseError::Unknown)?;
        if now_monotonic_nanos > lease.deadline_monotonic_nanos {
            return Err(LeaseError::Expired);
        }
        let lease = self.outstanding.remove(&lease_id).ok_or(LeaseError::Unknown)?;
        Ok(lease.range)
    }

    /// Consumes a lease past its deadline, for when the caller already made the reserved range durable before
    /// `harden` reported it expired. Removes the lease from `outstanding` so `expire` can never also close the same
    /// range with a void record. Fails only if the lease is already gone (already reaped by `expire`).
    pub fn accept_late(&mut self, lease_id: u64) -> Result<SequenceRange, LeaseError> {
        self.outstanding
            .remove(&lease_id)
            .map(|lease| lease.range)
            .ok_or(LeaseError::Unknown)
    }

    /// The `(lease id, range)` of every lease whose deadline has passed, without removing any. The caller writes a
    /// durable void record for each range and then retires that lease with `close_voided`; a lease whose void write
    /// fails is simply not retired, so it stays outstanding for a later pass rather than being dropped uncovered.
    pub fn expired_leases(&self, now_monotonic_nanos: u64) -> Vec<(u64, SequenceRange)> {
        self.outstanding
            .iter()
            .filter(|(_, lease)| now_monotonic_nanos > lease.deadline_monotonic_nanos)
            .map(|(id, lease)| (*id, lease.range))
            .collect()
    }

    /// Retires a lease once its abandoned range has been closed by a durable void record, returning that range. Fails
    /// only if the lease is already gone.
    pub fn close_voided(&mut self, lease_id: u64) -> Option<SequenceRange> {
        self.outstanding.remove(&lease_id).map(|lease| lease.range)
    }

    /// Collects expired leases: each abandoned range must be closed by an internal HEJ void record covering that exact
    /// range so the contiguous watermark advances past it. Voided sequences are permanently skipped.
    pub fn expire(&mut self, now_monotonic_nanos: u64) -> Vec<SequenceRange> {
        let expired: Vec<u64> = self
            .outstanding
            .iter()
            .filter(|(_, lease)| now_monotonic_nanos > lease.deadline_monotonic_nanos)
            .map(|(id, _)| *id)
            .collect();
        expired
            .into_iter()
            .filter_map(|id| self.outstanding.remove(&id))
            .map(|lease| lease.range)
            .collect()
    }

    /// How many reservation leases are currently outstanding.
    pub fn outstanding_leases(&self) -> usize {
        self.outstanding.len()
    }
}

/// Durable-commit-order arbitration for void/event mutual exclusion: given ranges in segment order, the first durable
/// coverage of a sequence wins and any later frame overlapping already-covered sequences is rejected on detection.
/// Returns the indexes of rejected frames, for O(1) membership tests against the (potentially 10⁴–10⁵-frame) input.
pub fn arbitrate_coverage(ranges: &[SequenceRange]) -> HashSet<usize> {
    // Accepted ranges never overlap each other, so only two of them can overlap a candidate: the nearest one starting
    // at or before it, and the next one starting after. Keying them by `(epoch, first sequence)` finds both in
    // O(log n), where rescanning every accepted range costs O(n) per frame.
    let mut covered: BTreeMap<(u64, u64), u64> = BTreeMap::new();
    let mut rejected = HashSet::default();
    for (index, range) in ranges.iter().enumerate() {
        let key = (range.epoch, range.first_sequence);
        let overlaps_earlier = covered
            .range(..=key)
            .next_back()
            .is_some_and(|((epoch, _), last)| *epoch == range.epoch && *last >= range.first_sequence);
        let overlaps_later = covered
            .range(key..)
            .next()
            .is_some_and(|((epoch, first), _)| *epoch == range.epoch && *first <= range.last_sequence);
        if overlaps_earlier || overlaps_later {
            rejected.insert(index);
        } else {
            covered.insert(key, range.last_sequence);
        }
    }
    rejected
}

#[cfg(test)]
#[path = "test/reserve.rs"]
mod tests;
