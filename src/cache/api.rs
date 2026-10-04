//! The one interface every cache tier implements, so tiers can be stacked, swapped, or supplied by the embedding
//! application.
//!
//! See: hef-hardware-deployment/spec.md

use super::model::BlockKey;

/// A place that can hold a copy of HEF file bytes so a repeat read skips object storage.
///
/// Implementations only accelerate reads: a `get` that returns `None` sends the caller back to durable storage, so a
/// tier may drop anything at any time without changing what a reader sees. Methods take `&self` because a read also
/// records a use for eviction; each tier keeps its own interior mutability and can be shared across threads.
///
/// See: hef-hardware-deployment/spec.md
pub trait CacheTier: Send + Sync {
    /// Whether a copy of the block is held right now.
    fn contains(&self, key: &BlockKey) -> bool;

    /// Drops the held copy of the block, if any.
    fn evict(&self, key: &BlockKey);

    /// The held bytes of the block, or `None` when the tier has no usable copy. Counts as a use for eviction.
    fn get(&self, key: &BlockKey) -> Option<Vec<u8>>;

    /// Offers the block's bytes to the tier. The tier may decline to keep them (for example when they are larger than
    /// the whole tier) and may evict colder blocks to make room.
    fn put(&self, key: &BlockKey, bytes: &[u8]);
}
