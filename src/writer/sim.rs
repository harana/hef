//! The test versions of the write side's durable stores, kept in memory so tests can run without a database.
//!
//! See: hef-write-path/spec.md

use super::retry::{RetryReceipt, SafeRetryStore, StatusClass};
use super::state_gc::{StateChunkLedger, StateChunkRecord};
use crate::error::StorageError;
use crate::events::TenantId;
use hashbrown::HashMap;
use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// An in-memory safe-retry store for tests: it forgets everything when dropped, so production code backs
/// `SafeRetryStore` with durable storage instead.
///
/// Rows are also indexed by `(tenant, dedupe)` so a replay-guard check is a lookup rather than a scan, and each row is
/// dropped once its guard window has passed so the store does not grow for the life of the test.
///
/// See: hef-write-path/spec.md
#[derive(Debug, Default)]
pub struct SimSafeRetryStore {
    /// `(tenant, dedupe)` -> the delivery identities whose receipt carries that dedupe identity.
    dedupe_index: HashMap<(TenantId, (u64, u64)), Vec<(u64, u64)>>,
    /// Expiry -> row keys recorded with that expiry. An overwritten row leaves a stale entry behind; eviction checks
    /// the row's current expiry before dropping it, so a stale entry is discarded without touching the row.
    expiry_queue: BTreeMap<i64, Vec<(TenantId, (u64, u64))>>,
    rows: HashMap<(TenantId, (u64, u64)), RetryReceipt>,
}

impl SimSafeRetryStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many delivery rows are on record.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// True when no delivery rows are on record.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn unindex_dedupe(&mut self, tenant_id: TenantId, dedupe: (u64, u64), delivery_identity: (u64, u64)) {
        if let Some(identities) = self.dedupe_index.get_mut(&(tenant_id, dedupe)) {
            identities.retain(|identity| *identity != delivery_identity);
            if identities.is_empty() {
                self.dedupe_index.remove(&(tenant_id, dedupe));
            }
        }
    }
}

impl SafeRetryStore for SimSafeRetryStore {
    fn record(&mut self, receipt: RetryReceipt) {
        let key = (receipt.tenant_id, receipt.delivery_identity);
        let dedupe = receipt.dedupe;
        let expiry = receipt.expiry_physical_nanos;
        if let Some(previous) = self.rows.insert(key, receipt) {
            self.unindex_dedupe(previous.tenant_id, previous.dedupe, previous.delivery_identity);
        }
        self.dedupe_index.entry((key.0, dedupe)).or_default().push(key.1);
        self.expiry_queue.entry(expiry).or_default().push(key);
    }

    fn evict_expired(&mut self, now_physical_nanos: i64) {
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

    fn lookup(&self, tenant_id: TenantId, delivery_identity: (u64, u64)) -> Option<RetryReceipt> {
        self.rows.get(&(tenant_id, delivery_identity)).cloned()
    }

    fn acknowledged_within_guard(
        &mut self,
        tenant_id: TenantId,
        dedupe: (u64, u64),
        now_physical_nanos: i64,
    ) -> Option<RetryReceipt> {
        self.evict_expired(now_physical_nanos);
        let identities = self.dedupe_index.get(&(tenant_id, dedupe))?;
        identities
            .iter()
            .filter_map(|identity| self.rows.get(&(tenant_id, *identity)))
            .find(|receipt| {
                receipt.status_class == StatusClass::Acknowledged && receipt.expiry_physical_nanos > now_physical_nanos
            })
            .cloned()
    }
}

/// An in-memory state chunk ledger for tests: it forgets everything when dropped, so production code backs
/// `StateChunkLedger` with durable storage instead. One lock serialises every call, which makes `touch` and `claim`
/// atomic against each other as the interface requires.
///
/// See: hef-file-lifecycle/spec.md
#[derive(Debug, Default)]
pub struct SimStateChunkLedger {
    records: Mutex<BTreeMap<[u8; 32], StateChunkRecord>>,
}

impl SimStateChunkLedger {
    fn records_mut(&self) -> MutexGuard<'_, BTreeMap<[u8; 32], StateChunkRecord>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn record(address: [u8; 32]) -> StateChunkRecord {
        StateChunkRecord {
            address,
            claimed: false,
            last_used_nanos: i64::MIN,
            marked: false,
        }
    }
}

impl StateChunkLedger for SimStateChunkLedger {
    fn touch(&self, address: &[u8; 32], now_nanos: i64) -> Result<bool, StorageError> {
        let mut records = self.records_mut();
        let record = records.entry(*address).or_insert_with(|| Self::record(*address));
        if record.claimed {
            return Ok(false);
        }
        record.last_used_nanos = record.last_used_nanos.max(now_nanos);
        Ok(true)
    }

    fn clear_marks(&self) -> Result<(), StorageError> {
        for record in self.records_mut().values_mut() {
            record.marked = false;
        }
        Ok(())
    }

    fn mark(&self, address: &[u8; 32]) -> Result<bool, StorageError> {
        let mut records = self.records_mut();
        let record = records.entry(*address).or_insert_with(|| Self::record(*address));
        Ok(!std::mem::replace(&mut record.marked, true))
    }

    fn records_after(
        &self,
        after: Option<&[u8; 32]>,
        limit: usize,
        page: &mut Vec<StateChunkRecord>,
    ) -> Result<(), StorageError> {
        let records = self.records_mut();
        let start = after.map_or(Bound::Unbounded, Bound::Excluded);
        page.clear();
        page.extend(
            records
                .range::<[u8; 32], _>((start, Bound::Unbounded))
                .take(limit)
                .map(|(_, record)| *record),
        );
        Ok(())
    }

    fn claim(&self, address: &[u8; 32], unused_since_nanos: i64) -> Result<bool, StorageError> {
        let mut records = self.records_mut();
        let Some(record) = records.get_mut(address) else {
            return Ok(false);
        };
        let due = !record.claimed && !record.marked && record.last_used_nanos <= unused_since_nanos;
        record.claimed |= due;
        Ok(due)
    }

    fn forget(&self, address: &[u8; 32]) -> Result<(), StorageError> {
        self.records_mut().remove(address);
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/sim.rs"]
mod tests;
