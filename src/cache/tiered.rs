//! Stacks one cache tier in front of another — RAM in front of NVMe — and falls through to durable storage only when
//! both miss.
//!
//! See: hef-hardware-deployment/spec.md

use super::api::CacheTier;
use super::model::BlockKey;
use super::observability::TieredCacheMetrics;

/// Two cache tiers read in order: `upper` first, then `lower`, then durable storage.
///
/// A block found only in `lower` is promoted into `upper`; a block fetched from durable storage is offered to both.
/// The stack is itself a [`CacheTier`], so stacks can nest and the embedding application can swap either tier for its
/// own. [`metrics`](Self::metrics) counts the reads that reached durable storage, which on object storage is the
/// number of paid GETs.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug)]
pub struct TieredCache<U: CacheTier, L: CacheTier> {
    lower: L,
    metrics: TieredCacheMetrics,
    upper: U,
}

impl<U: CacheTier, L: CacheTier> TieredCache<U, L> {
    /// A stack that reads `upper` first and `lower` second.
    pub fn new(upper: U, lower: L) -> Self {
        Self {
            lower,
            metrics: TieredCacheMetrics::default(),
            upper,
        }
    }

    /// The tier read second.
    pub fn lower(&self) -> &L {
        &self.lower
    }

    /// How many reads the lower tier caught and how many went to durable storage.
    pub fn metrics(&self) -> &TieredCacheMetrics {
        &self.metrics
    }

    /// The tier read first.
    pub fn upper(&self) -> &U {
        &self.upper
    }

    /// Returns the block's bytes from the first tier that holds them, or calls `fetch` to read them from durable
    /// storage and offers the result to both tiers. An error from `fetch` is returned unchanged and nothing is cached.
    pub fn get_or_fetch<E>(&self, key: &BlockKey, fetch: impl FnOnce() -> Result<Vec<u8>, E>) -> Result<Vec<u8>, E> {
        if let Some(bytes) = self.get(key) {
            return Ok(bytes);
        }
        self.metrics.record_durable_read();
        let bytes = fetch()?;
        self.put(key, &bytes);
        Ok(bytes)
    }
}

impl<U: CacheTier, L: CacheTier> CacheTier for TieredCache<U, L> {
    fn contains(&self, key: &BlockKey) -> bool {
        self.upper.contains(key) || self.lower.contains(key)
    }

    fn evict(&self, key: &BlockKey) {
        self.upper.evict(key);
        self.lower.evict(key);
    }

    fn get(&self, key: &BlockKey) -> Option<Vec<u8>> {
        if let Some(bytes) = self.upper.get(key) {
            return Some(bytes);
        }
        let bytes = self.lower.get(key)?;
        self.metrics.record_lower_hit();
        self.upper.put(key, &bytes);
        Some(bytes)
    }

    fn put(&self, key: &BlockKey, bytes: &[u8]) {
        self.upper.put(key, bytes);
        self.lower.put(key, bytes);
    }
}

#[cfg(test)]
#[path = "test/tiered.rs"]
mod tests;
