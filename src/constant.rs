//! Tuning constants for the crate-root modules that live directly under `src/`.

/// A derived-column recompute queue at or below this many slots is never compacted, so a small queue does not pay
/// for a compaction pass on every republish. Above it, the queue compacts once completed slots make up at least
/// half of it.
pub const RECOMPUTE_QUEUE_COMPACTION_FLOOR: usize = 64;
