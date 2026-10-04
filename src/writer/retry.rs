//! Lets a client safely retry a delivery without creating a duplicate event, by remembering where each delivery landed
//! in the durable log.
//!
//! The journal is the one protected copy of an event's payload; the rows here are mutable *pointers* into the journal,
//! never a second copy. They can be rebuilt after a restart or takeover from retained replay-guard rows plus journal
//! coverage, and whenever a row disagrees with the journal bytes, the journal wins and the row is repaired or dropped.

use crate::artifacts::batch::decode_batch;
use crate::artifacts::frame::decode_frame;
use crate::artifacts::segment::ReplayedFrame;
use crate::events::{SequencePoint, TenantId};
use crate::invariants::ShardId;

/// Response status class recorded with the receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    Acknowledged,
    /// The events were appended but their durability is unknown: the barrier failed after the append, so the frame may
    /// or may not be persisted. The receipt exists so a client retry of the same delivery is recognised instead of
    /// appending a second copy; it never counts as an acknowledgement.
    Indeterminate,
    Rejected,
}

/// One safe-retry row: client/connector delivery identity, scope, commit receipt, HEJ frame cursor, status class,
/// expiry, and minimal dedupe metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryReceipt {
    pub commit: SequencePoint,
    pub dedupe: (u64, u64),
    pub delivery_identity: (u64, u64),
    pub expiry_physical_nanos: i64,
    pub frame_offset: u64,
    pub shard: ShardId,
    pub status_class: StatusClass,
    pub tenant_id: TenantId,
}

/// Remembers where each client delivery landed in the journal, so a retried delivery is answered with its original
/// receipt instead of being stored a second time.
///
/// The application backs this with durable storage, so receipts survive a restart and the replay guard keeps working
/// across it. Rows are keyed by `(tenant, delivery identity)`: a connector delivery identity is only unique within one
/// tenant, so two tenants that happen to share one must never see or overwrite each other's receipts.
/// `writer::sim::SimSafeRetryStore` is the in-memory version for tests.
///
/// See: hef-write-path/spec.md
pub trait SafeRetryStore {
    /// Records (or overwrites) the receipt for one tenant's delivery identity.
    fn record(&mut self, receipt: RetryReceipt);

    /// Drops every receipt whose guard window has passed as of `now_physical_nanos`. Callers that only record receipts
    /// should call it periodically so the store stays bounded.
    fn evict_expired(&mut self, now_physical_nanos: i64);

    /// The receipt for a delivery identity within `tenant_id`, if one is on record. Another tenant's row for the same
    /// delivery identity is never returned.
    fn lookup(&self, tenant_id: TenantId, delivery_identity: (u64, u64)) -> Option<RetryReceipt>;

    /// The acknowledged, unexpired receipt carrying this tenant's dedupe identity, if one is on record. A matching
    /// dedupe hash under a different tenant never matches. Receipts whose window has passed are evicted on the way
    /// through, keeping the store bounded on the ingest path.
    fn acknowledged_within_guard(
        &mut self,
        tenant_id: TenantId,
        dedupe: (u64, u64),
        now_physical_nanos: i64,
    ) -> Option<RetryReceipt>;

    /// The replay-guard rule: replaying a retained HEJ frame must not enqueue duplicate events while the original
    /// acknowledgement is within the replay-guard window. True when this tenant's dedupe identity is already
    /// acknowledged and unexpired.
    fn duplicate_within_guard(&mut self, tenant_id: TenantId, dedupe: (u64, u64), now_physical_nanos: i64) -> bool {
        self.acknowledged_within_guard(tenant_id, dedupe, now_physical_nanos)
            .is_some()
    }

    /// Rebuilds missing rows from HEJ after recovery: dedupe hashes and sequence positions come from the replayed
    /// frames themselves. Rows that disagree with HEJ event bytes are repaired (HEJ wins for event replay); rows HEJ
    /// does not cover are untouched.
    fn reconstruct_from_replay(&mut self, frames: &[ReplayedFrame], shard: ShardId, guard_expiry_physical_nanos: i64) {
        for frame in frames {
            if frame.header.is_void_record() {
                continue;
            }
            let Ok((header, payload)) = decode_frame(&frame.frame_bytes) else {
                continue;
            };
            let Ok(batch) = decode_batch(payload, header.event_count) else {
                continue;
            };
            for (row, event) in batch.events.iter().enumerate() {
                let delivery_identity = (
                    event.variable.connector_delivery_hash_low,
                    event.variable.connector_delivery_hash_high,
                );
                let truth = RetryReceipt {
                    delivery_identity,
                    tenant_id: header.tenant_id,
                    commit: SequencePoint {
                        epoch: header.epoch,
                        sequence: header.first_sequence + row as u64,
                    },
                    shard,
                    frame_offset: frame.offset,
                    status_class: StatusClass::Acknowledged,
                    expiry_physical_nanos: guard_expiry_physical_nanos,
                    dedupe: (event.fixed.dedupe_hash_low, event.fixed.dedupe_hash_high),
                };
                match self.lookup(header.tenant_id, delivery_identity) {
                    Some(existing) if existing.commit == truth.commit && existing.dedupe == truth.dedupe => {
                        // Row agrees with HEJ: keep it (status/expiry are the mutable index's own business).
                    }
                    _ => {
                        // Missing or disagreeing: HEJ is authoritative; the row is rebuilt from the journal.
                        self.record(truth);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "test/retry.rs"]
mod tests;
