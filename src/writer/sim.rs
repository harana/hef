//! The test versions of the write side's durable stores, kept in memory so tests can run without a database.
//!
//! See: hef-write-path/spec.md

use super::retry::{RetryReceipt, SafeRetryStore, StatusClass};
use crate::events::TenantId;
use hashbrown::HashMap;
use std::collections::BTreeMap;

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

#[cfg(test)]
#[path = "test/sim.rs"]
mod tests;
