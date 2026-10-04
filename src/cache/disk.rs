//! The disk tier: keeps HEF blocks as files on one or more local disks (typically NVMe) so repeat reads skip object
//! storage, and evicts the least-recently-used block when the configured space fills.
//!
//! See: hef-hardware-deployment/spec.md

use super::api::CacheTier;
use super::constant::DISK_TIER_DIR;
use super::error::PlacementError;
use super::model::BlockKey;
use super::placement::{LocalLayout, LocalVolume, PlacementPolicy, PlannedWrite, delete_writes, stage_local_copies};
use super::volume::{CacheVolume, open_cache_volume};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// One cached block's bookkeeping: enough to read it back, verify it, and evict the coldest one when the disks fill.
/// The bytes themselves live on the volumes; this is rebuildable metadata.
struct Entry {
    /// The BLAKE3 the reassembled bytes are re-verified against before they are served, so a corrupt or truncated
    /// local copy is caught and treated as a miss rather than served.
    blake3: blake3::Hash,
    last_access: u64,
    size: u64,
}

#[derive(Default)]
struct State {
    clock: u64,
    entries: BTreeMap<BlockKey, Entry>,
    used_bytes: u64,
}

/// A disk-backed cache tier: it keeps at most a configured number of bytes of HEF blocks spread across the node's
/// local disks and evicts the least-recently-used block when they fill.
///
/// Every block is placed through the configured layout — one disk, mirrored across disks, or striped across them —
/// and each copy is written to a temporary file, synced, and renamed into place, so a crash never leaves a half-written
/// copy under a block's name. Every read is re-verified against the BLAKE3 recorded when the block was admitted; a
/// missing, unreadable, or corrupt copy is a miss that drops the entry, never a served bad byte. A mirrored block
/// survives one failed disk: the next copy is tried. File names are a hash of the block's key, so no tenant id or file
/// id appears on any disk.
///
/// See: hef-hardware-deployment/spec.md
pub struct DiskTier {
    capacity_bytes: u64,
    policy: PlacementPolicy,
    state: Mutex<State>,
    volumes: BTreeMap<String, Box<dyn CacheVolume>>,
}

impl std::fmt::Debug for DiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("DiskTier")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("entry_count", &state.entries.len())
            .field("layout", &self.policy.layout())
            .field("used_bytes", &state.used_bytes)
            .field("volumes", &self.volumes.len())
            .finish()
    }
}

impl DiskTier {
    /// Builds a disk tier holding at most `capacity_bytes` of block data across `volumes`, placing each block by
    /// `policy`'s layout. The volumes' ids must match the ids `policy` was built from.
    ///
    /// Blocks a previous process left on the volumes are deleted here: a fresh tier has no bookkeeping for them, so
    /// they could never be served or evicted — only silently fill the disks past `capacity_bytes` across restarts.
    /// Only the tier's own cache directory is cleared, so a volume root shared with other files never loses them.
    pub fn new(capacity_bytes: u64, policy: PlacementPolicy, volumes: Vec<Box<dyn CacheVolume>>) -> Self {
        let volumes: BTreeMap<String, Box<dyn CacheVolume>> = volumes
            .into_iter()
            .map(|volume| (volume.id().to_owned(), volume))
            .collect();
        for volume in volumes.values() {
            volume.clear_directory(DISK_TIER_DIR);
        }
        Self {
            capacity_bytes,
            policy,
            state: Mutex::new(State::default()),
            volumes,
        }
    }

    /// Builds a disk tier from configuration: validates the volumes for `layout`, opens each one on the accelerated
    /// write path where the host supports it and the portable path otherwise, and clears what a previous process left.
    /// Fails only when the volume set is invalid for the layout (for example a `mirror` with a single disk).
    pub fn open(capacity_bytes: u64, layout: LocalLayout, volumes: Vec<LocalVolume>) -> Result<Self, PlacementError> {
        let policy = PlacementPolicy::new(layout, volumes)?;
        let opened = policy.volumes().iter().map(open_cache_volume).collect();
        Ok(Self::new(capacity_bytes, policy, opened))
    }

    /// How many blocks are currently cached.
    pub fn entry_count(&self) -> usize {
        self.state().entries.len()
    }

    /// How many bytes of block data are currently cached.
    pub fn used_bytes(&self) -> u64 {
        self.state().used_bytes
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every mutation leaves `State` consistent before it can panic, so a poisoned lock is still safe to use.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where every copy or chunk of one block was planned to land, recomputed from the immutable policy and the key, so
    /// nothing about placement is stored per entry.
    pub(super) fn writes_for(&self, key: &BlockKey) -> Vec<PlannedWrite> {
        self.policy.plan(key.digest().to_hex().as_str()).writes
    }

    /// Records a use of `key` and reports whether it is held. A block key always names the same immutable bytes, so a
    /// held copy never needs rewriting.
    fn touch(&self, key: &BlockKey) -> bool {
        let mut state = self.state();
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        match state.entries.get_mut(key) {
            Some(entry) => {
                entry.last_access = tick;
                true
            }
            None => false,
        }
    }

    /// Reads one block's bytes back from its copies: the first readable copy for a single or mirrored block, or every
    /// stripe chunk concatenated in order for a striped one. `None` when a needed copy is missing or unreadable.
    fn reassemble(&self, writes: &[PlannedWrite]) -> Option<Vec<u8>> {
        if writes.iter().any(|write| write.stripe_index.is_some()) {
            let mut ordered: Vec<&PlannedWrite> = writes.iter().collect();
            ordered.sort_by_key(|write| write.stripe_index);
            let mut out = Vec::new();
            for write in ordered {
                let chunk = self
                    .volumes
                    .get(&write.volume_id)?
                    .read_blob(&write.relative_path)
                    .ok()??;
                out.extend_from_slice(&chunk);
            }
            return Some(out);
        }
        // A missing or unreadable mirror is not a reason to abandon the others: one failed disk must not discard the
        // healthy copies. Only an all-mirror miss is a real miss.
        writes.iter().find_map(|write| {
            self.volumes
                .get(&write.volume_id)
                .and_then(|volume| volume.read_blob(&write.relative_path).ok().flatten())
        })
    }
}

impl CacheTier for DiskTier {
    fn contains(&self, key: &BlockKey) -> bool {
        self.state().entries.contains_key(key)
    }

    fn evict(&self, key: &BlockKey) {
        let removed = {
            let mut state = self.state();
            match state.entries.remove(key) {
                Some(entry) => {
                    state.used_bytes = state.used_bytes.saturating_sub(entry.size);
                    true
                }
                None => false,
            }
        };
        if removed {
            delete_writes(&self.volumes, &self.writes_for(key));
        }
    }

    fn get(&self, key: &BlockKey) -> Option<Vec<u8>> {
        let (blake3, size) = {
            let state = self.state();
            let entry = state.entries.get(key)?;
            (entry.blake3, entry.size)
        };
        match self.reassemble(&self.writes_for(key)) {
            Some(bytes) if bytes.len() as u64 == size && blake3::hash(&bytes) == blake3 => {
                self.touch(key);
                Some(bytes)
            }
            // A missing, unreadable, or corrupt local copy is a miss: drop the entry so the next read re-fetches from
            // durable storage, and never serve an unverified byte.
            _ => {
                self.evict(key);
                None
            }
        }
    }

    fn put(&self, key: &BlockKey, bytes: &[u8]) {
        let incoming = bytes.len() as u64;
        // A block larger than the whole tier is never kept, matching the memory tier.
        if incoming == 0 || incoming > self.capacity_bytes || self.touch(key) {
            return;
        }
        let plan = self.policy.plan(key.digest().to_hex().as_str());
        if stage_local_copies(&self.policy, &plan, bytes, &self.volumes).is_err() {
            // A failed placement has already removed its partial copies and records nothing, so the block is simply
            // not cached.
            return;
        }
        let mut state = self.state();
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        // Another thread may have admitted the same block while this one was writing; the copies hold the same bytes
        // either way, so only the use is recorded.
        if let Some(entry) = state.entries.get_mut(key) {
            entry.last_access = tick;
            return;
        }
        while state.used_bytes.saturating_add(incoming) > self.capacity_bytes {
            let Some(victim) = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(victim, _)| *victim)
            else {
                break;
            };
            if let Some(entry) = state.entries.remove(&victim) {
                state.used_bytes = state.used_bytes.saturating_sub(entry.size);
                delete_writes(&self.volumes, &self.writes_for(&victim));
            }
        }
        state.used_bytes = state.used_bytes.saturating_add(incoming);
        state.entries.insert(
            *key,
            Entry {
                blake3: blake3::hash(bytes),
                last_access: tick,
                size: incoming,
            },
        );
    }
}

#[cfg(test)]
#[path = "test/disk.rs"]
mod tests;
