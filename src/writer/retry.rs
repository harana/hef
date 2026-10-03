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
use hashbrown::HashMap;
use std::collections::BTreeMap;

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

/// The mutable safe-retry index plus the replay-guard window.
///
/// Every row is keyed by `(tenant, delivery identity)`: a connector delivery identity is only unique within one
/// tenant, so two tenants that happen to share one must never see or overwrite each other's receipts. Rows are also
/// indexed by `(tenant, dedupe)` so a replay-guard check is a lookup rather than a scan, and each row is dropped once
/// its guard window has passed so the store does not grow for the life of the process.
#[derive(Debug, Default)]
pub struct SafeRetryStore {
    /// `(tenant, dedupe)` → the delivery identities whose receipt carries that dedupe identity.
    dedupe_index: HashMap<(TenantId, (u64, u64)), Vec<(u64, u64)>>,
    /// Expiry → row keys recorded with that expiry. An overwritten row leaves a stale entry behind; eviction checks
    /// the row's current expiry before dropping it, so a stale entry is discarded without touching the row.
    expiry_queue: BTreeMap<i64, Vec<(TenantId, (u64, u64))>>,
    rows: HashMap<(TenantId, (u64, u64)), RetryReceipt>,
}

impl SafeRetryStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records (or overwrites) the receipt for one tenant's delivery identity.
    pub fn record(&mut self, receipt: RetryReceipt) {
        self.insert(receipt);
    }

    fn insert(&mut self, receipt: RetryReceipt) {
        let key = (receipt.tenant_id, receipt.delivery_identity);
        let dedupe = receipt.dedupe;
        let expiry = receipt.expiry_physical_nanos;
        if let Some(previous) = self.rows.insert(key, receipt) {
            self.unindex_dedupe(previous.tenant_id, previous.dedupe, previous.delivery_identity);
        }
        self.dedupe_index.entry((key.0, dedupe)).or_default().push(key.1);
        self.expiry_queue.entry(expiry).or_default().push(key);
    }

    fn unindex_dedupe(&mut self, tenant_id: TenantId, dedupe: (u64, u64), delivery_identity: (u64, u64)) {
        if let Some(identities) = self.dedupe_index.get_mut(&(tenant_id, dedupe)) {
            identities.retain(|identity| *identity != delivery_identity);
            if identities.is_empty() {
                self.dedupe_index.remove(&(tenant_id, dedupe));
            }
        }
    }

    /// Drops every receipt whose guard window has passed as of `now_physical_nanos`. The replay guard calls this on
    /// its own; owners that only record receipts should call it periodically so the store stays bounded.
    pub fn evict_expired(&mut self, now_physical_nanos: i64) {
        while let Some((&expiry, _)) = self.expiry_queue.first_key_value() {
            if expiry > now_physical_nanos {
                break;
            }
            let Some((_, keys)) = self.expiry_queue.pop_first() else {
                break;
            };
            for key in keys {
                // An overwritten row may have a later expiry than this stale queue entry: keep it.
                let expired = self
                    .rows
                    .get(&key)
                    .is_some_and(|receipt| receipt.expiry_physical_nanos <= now_physical_nanos);
                if expired && let Some(removed) = self.rows.remove(&key) {
                    self.unindex_dedupe(removed.tenant_id, removed.dedupe, removed.delivery_identity);
                }
            }
        }
    }

    /// The receipt for a delivery identity within `tenant_id`, if one is on record. Another tenant's row for the same
    /// delivery identity is never returned.
    pub fn lookup(&self, tenant_id: TenantId, delivery_identity: (u64, u64)) -> Option<&RetryReceipt> {
        self.rows.get(&(tenant_id, delivery_identity))
    }

    /// The replay-guard rule: replaying a retained HEJ frame must not enqueue duplicate events while the original
    /// acknowledgement is within the replay-guard window. True when this tenant's dedupe identity is already
    /// acknowledged and unexpired. A matching dedupe hash under a different tenant never suppresses the event.
    /// Receipts whose window has passed are evicted on the way through, keeping the store bounded on the ingest path.
    pub fn duplicate_within_guard(&mut self, tenant_id: TenantId, dedupe: (u64, u64), now_physical_nanos: i64) -> bool {
        self.evict_expired(now_physical_nanos);
        self.dedupe_index.get(&(tenant_id, dedupe)).is_some_and(|identities| {
            identities.iter().any(|identity| {
                self.rows.get(&(tenant_id, *identity)).is_some_and(|receipt| {
                    receipt.status_class == StatusClass::Acknowledged
                        && receipt.expiry_physical_nanos > now_physical_nanos
                })
            })
        })
    }

    /// How many delivery rows are on record.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no delivery rows are on record.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Rebuilds missing rows from HEJ after recovery: dedupe hashes and sequence positions come from the replayed
    /// frames themselves. Rows that disagree with HEJ event bytes are repaired (HEJ wins for event replay); rows HEJ
    /// does not cover are untouched.
    pub fn reconstruct_from_replay(
        &mut self,
        frames: &[ReplayedFrame],
        shard: ShardId,
        guard_expiry_physical_nanos: i64,
    ) {
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
                let key = (header.tenant_id, delivery_identity);
                match self.rows.get(&key) {
                    Some(existing) if existing.commit == truth.commit && existing.dedupe == truth.dedupe => {
                        // Row agrees with HEJ: keep it (status/expiry are the mutable index's own business).
                    }
                    _ => {
                        // Missing or disagreeing: HEJ is authoritative; the row is rebuilt from the journal.
                        self.insert(truth);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "test/retry.rs"]
mod tests;
