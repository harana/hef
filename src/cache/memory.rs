//! The memory tier: keeps the hottest HEF blocks in RAM and, when space runs low, compresses the least-recently-used
//! ones into a cold segment — or drops them, when they don't compress well enough to keep — so a re-read pays a
//! microsecond decompression instead of a trip to slower storage.
//!
//! See: hef-hardware-deployment/spec.md

use super::api::CacheTier;
use super::constant::{COLD_COMPRESSION_BAR_DEN, COLD_COMPRESSION_BAR_NUM, ENTRY_OVERHEAD_BYTES, MIN_ENTRIES};
use super::model::BlockKey;
use super::observability::ColdSegmentMetrics;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

/// How one block's bytes are held: uncompressed and ready to serve, or lz4-compressed in the cold segment after losing
/// an eviction round, waiting to be decompressed and promoted on its next read.
enum SlotBytes {
    /// The lz4 frame carries its decompressed size, so promotion needs nothing but these bytes.
    Cold(Box<[u8]>),
    Hot(Arc<[u8]>),
}

impl SlotBytes {
    /// Bytes this block charges against the tier's capacity: the payload size when hot, the compressed size when cold.
    fn charged_len(&self) -> u64 {
        match self {
            SlotBytes::Cold(compressed) => compressed.len() as u64,
            SlotBytes::Hot(bytes) => bytes.len() as u64,
        }
    }
}

/// One held block: its bytes and the logical tick it was last touched at, so eviction can find the least-recently-used
/// entry.
struct Slot {
    bytes: SlotBytes,
    last_access: u64,
}

#[derive(Default)]
struct State {
    /// Every held key indexed by its last-access tick, so the least-recently-used block is the first entry — an
    /// O(log n) eviction pick instead of an O(n) scan.
    by_access: BTreeMap<u64, BlockKey>,
    clock: u64,
    entries: BTreeMap<BlockKey, Slot>,
    used_bytes: u64,
}

/// A size-bounded in-memory cache of HEF block bytes that evicts the least-recently-used block when it runs out of
/// room.
///
/// Eviction under pressure is a demotion first, not a drop: the block is lz4-compressed and kept in a cold segment,
/// charged at its compressed size against the same `capacity_bytes`, so the tier never exceeds its configured
/// footprint. A read that finds its block cold decompresses it, promotes it back to hot, and returns the exact bytes
/// that were stored. Blocks that miss the compression bar, and cold blocks picked by a later eviction round, are
/// dropped for good. An explicit [`evict`](CacheTier::evict) drops a block from both segments.
///
/// "Room" is both a byte budget and an entry budget, so a flood of tiny blocks cannot grow per-entry bookkeeping past
/// roughly the configured capacity.
///
/// See: hef-hardware-deployment/spec.md
pub struct MemoryTier {
    capacity_bytes: u64,
    max_entries: usize,
    metrics: ColdSegmentMetrics,
    state: Mutex<State>,
}

impl std::fmt::Debug for MemoryTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("MemoryTier")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("entry_count", &state.entries.len())
            .field("used_bytes", &state.used_bytes)
            .finish()
    }
}

impl MemoryTier {
    /// A tier that holds at most `capacity_bytes` of block data before it starts evicting the least-recently-used
    /// blocks. The number of entries it will hold at once is derived from the same figure.
    pub fn new(capacity_bytes: u64) -> Self {
        let derived = usize::try_from(capacity_bytes / ENTRY_OVERHEAD_BYTES).unwrap_or(usize::MAX);
        Self {
            capacity_bytes,
            max_entries: derived.max(MIN_ENTRIES),
            metrics: ColdSegmentMetrics::default(),
            state: Mutex::new(State::default()),
        }
    }

    /// How many blocks are currently held, hot or cold.
    pub fn entry_count(&self) -> usize {
        self.state().entries.len()
    }

    /// The counters that say whether the compressed cold segment is earning its RAM: hit rates by segment, achieved
    /// compression ratio, and time spent compressing and decompressing.
    pub fn metrics(&self) -> &ColdSegmentMetrics {
        &self.metrics
    }

    /// How many bytes of block data are currently held. A cold block counts at its compressed size, so this is the
    /// tier's true footprint against `capacity_bytes`.
    pub fn used_bytes(&self) -> u64 {
        self.state().used_bytes
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every mutation leaves `State` consistent before it can panic, so a poisoned lock is still safe to use.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Makes room along both budgets — the payload bytes the caller configured, and the entry count that bounds the
    /// per-entry bookkeeping those bytes do not account for — until `incoming_bytes` and `incoming_entries` more fit.
    ///
    /// Room comes from the least-recently-used block each round. A hot block is demoted first: compressed and kept in
    /// the cold segment at its compressed size, provided lz4 shrinks it past the keep bar. A block that misses the bar,
    /// a block that is already cold, and any block picked while the entry count is what overflows (demotion frees no
    /// entry) is dropped. Every round strictly shrinks a budget, so the loop always terminates.
    fn make_room(&self, state: &mut State, incoming_bytes: u64, incoming_entries: usize) {
        while state.used_bytes.saturating_add(incoming_bytes) > self.capacity_bytes
            || state.entries.len() + incoming_entries > self.max_entries
        {
            let over_entries = state.entries.len() + incoming_entries > self.max_entries;
            let Some((victim_tick, victim_key)) = state.by_access.pop_first() else {
                break;
            };
            let Some(slot) = state.entries.remove(&victim_key) else {
                continue;
            };
            state.used_bytes = state.used_bytes.saturating_sub(slot.bytes.charged_len());
            let SlotBytes::Hot(bytes) = slot.bytes else {
                continue;
            };
            if over_entries {
                continue;
            }
            let started = Instant::now();
            let compressed = lz4_flex::compress_prepend_size(&bytes);
            let compress_nanos = started.elapsed().as_nanos() as u64;
            if compressed.len() as u64 * COLD_COMPRESSION_BAR_DEN > bytes.len() as u64 * COLD_COMPRESSION_BAR_NUM {
                self.metrics.record_incompressible_drop(compress_nanos);
                continue;
            }
            self.metrics
                .record_demotion(bytes.len() as u64, compressed.len() as u64, compress_nanos);
            state.used_bytes = state.used_bytes.saturating_add(compressed.len() as u64);
            state.by_access.insert(victim_tick, victim_key);
            state.entries.insert(
                victim_key,
                Slot {
                    bytes: SlotBytes::Cold(compressed.into_boxed_slice()),
                    last_access: victim_tick,
                },
            );
        }
    }
}

impl CacheTier for MemoryTier {
    fn contains(&self, key: &BlockKey) -> bool {
        self.state().entries.contains_key(key)
    }

    fn evict(&self, key: &BlockKey) {
        let mut state = self.state();
        if let Some(slot) = state.entries.remove(key) {
            state.by_access.remove(&slot.last_access);
            state.used_bytes = state.used_bytes.saturating_sub(slot.bytes.charged_len());
        }
    }

    fn get(&self, key: &BlockKey) -> Option<Vec<u8>> {
        let mut guard = self.state();
        let state = &mut *guard;
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        let Some(slot) = state.entries.get_mut(key) else {
            self.metrics.record_miss();
            return None;
        };
        state.by_access.remove(&slot.last_access);
        slot.last_access = tick;
        state.by_access.insert(tick, *key);
        let compressed = match &slot.bytes {
            SlotBytes::Hot(bytes) => {
                self.metrics.record_hot_hit();
                return Some(bytes.to_vec());
            }
            SlotBytes::Cold(compressed) => compressed,
        };
        let compressed_len = compressed.len() as u64;
        let started = Instant::now();
        let Ok(bytes) = lz4_flex::decompress_size_prepended(compressed) else {
            // Cold bytes are this tier's own lz4 output, so this cannot happen short of memory corruption; treat it as
            // a miss and drop the slot rather than serve anything.
            state.entries.remove(key);
            state.by_access.remove(&tick);
            state.used_bytes = state.used_bytes.saturating_sub(compressed_len);
            self.metrics.record_miss();
            return None;
        };
        self.metrics.record_cold_hit(started.elapsed().as_nanos() as u64);
        let promoted: Arc<[u8]> = bytes.into();
        state.used_bytes = state
            .used_bytes
            .saturating_sub(compressed_len)
            .saturating_add(promoted.len() as u64);
        slot.bytes = SlotBytes::Hot(promoted.clone());
        // Promotion grew the block back to its full size, which can push past the byte budget; the promoted block
        // carries the newest tick, so making room can never pick it while anything colder remains.
        self.make_room(state, 0, 0);
        Some(promoted.to_vec())
    }

    fn put(&self, key: &BlockKey, bytes: &[u8]) {
        let incoming = bytes.len() as u64;
        let mut guard = self.state();
        let state = &mut *guard;
        // A put replaces whatever was held under this key, so drop the old slot first — reclaiming its bytes — before
        // deciding whether the new bytes can be kept. An empty or too-large replacement still evicts the stale copy.
        if let Some(old) = state.entries.remove(key) {
            state.by_access.remove(&old.last_access);
            state.used_bytes = state.used_bytes.saturating_sub(old.bytes.charged_len());
        }
        if incoming == 0 || incoming > self.capacity_bytes {
            return;
        }
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        self.make_room(state, incoming, 1);
        state.used_bytes = state.used_bytes.saturating_add(incoming);
        state.by_access.insert(tick, *key);
        state.entries.insert(
            *key,
            Slot {
                bytes: SlotBytes::Hot(Arc::from(bytes)),
                last_access: tick,
            },
        );
    }
}

#[cfg(test)]
#[path = "test/memory.rs"]
mod tests;
