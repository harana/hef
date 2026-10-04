//! The disk tier: keeps HEF blocks as files on a local disk (typically NVMe) so repeat reads skip object storage, and
//! evicts the least-recently-used block when the configured space fills.
//!
//! See: hef-hardware-deployment/spec.md

use super::api::CacheTier;
use super::constant::DISK_TIER_DIR;
use super::model::BlockKey;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// One cached block's bookkeeping: enough to read it back, verify it, and evict the coldest one when the disk fills.
/// The bytes themselves live in a file; this is rebuildable metadata.
struct Entry {
    /// The BLAKE3 the file's bytes are re-verified against before they are served, so a corrupt or truncated local
    /// copy is caught and treated as a miss rather than served.
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

/// A disk-backed cache tier: it keeps at most a configured number of bytes of HEF blocks as files under one directory
/// and evicts the least-recently-used block when that space fills.
///
/// Each block is written to a temporary file, synced, and renamed into place, so a crash never leaves a half-written
/// file under a block's name. Every read is re-verified against the BLAKE3 recorded when the block was admitted; a
/// missing, unreadable, or corrupt file is a miss that drops the entry, never a served bad byte. File names are a hash
/// of the block's key, so no tenant id or file id appears on the disk.
///
/// See: hef-hardware-deployment/spec.md
pub struct DiskTier {
    capacity_bytes: u64,
    dir: PathBuf,
    state: Mutex<State>,
}

impl std::fmt::Debug for DiskTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state();
        f.debug_struct("DiskTier")
            .field("capacity_bytes", &self.capacity_bytes)
            .field("dir", &self.dir)
            .field("entry_count", &state.entries.len())
            .field("used_bytes", &state.used_bytes)
            .finish()
    }
}

impl DiskTier {
    /// Opens a disk tier that holds at most `capacity_bytes` of block data in a cache directory under `root`.
    ///
    /// Block files a previous process left in that directory are deleted here: a fresh tier has no bookkeeping for
    /// them, so they could never be served or evicted — only silently fill the disk past `capacity_bytes` across
    /// restarts. Everything in the directory is rebuildable, so clearing it costs at most a re-fetch.
    pub fn open(root: impl AsRef<Path>, capacity_bytes: u64) -> Self {
        let dir = root.as_ref().join(DISK_TIER_DIR);
        let _ = std::fs::remove_dir_all(&dir);
        Self {
            capacity_bytes,
            dir,
            state: Mutex::new(State::default()),
        }
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

    pub(super) fn path_of(&self, key: &BlockKey) -> PathBuf {
        self.dir.join(key.digest().to_hex().as_str())
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

    /// Writes `bytes` to a temporary file, syncs it, and renames it over the block's file, so the block's name only
    /// ever points at a complete copy.
    fn write_file(&self, key: &BlockKey, bytes: &[u8]) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let target = self.path_of(key);
        let staging = target.with_extension("tmp");
        let result = std::fs::File::create(&staging)
            .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
            .and_then(|()| std::fs::rename(&staging, &target));
        if result.is_err() {
            let _ = std::fs::remove_file(&staging);
        }
        result
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
            let _ = std::fs::remove_file(self.path_of(key));
        }
    }

    fn get(&self, key: &BlockKey) -> Option<Vec<u8>> {
        let (blake3, size) = {
            let state = self.state();
            let entry = state.entries.get(key)?;
            (entry.blake3, entry.size)
        };
        match std::fs::read(self.path_of(key)) {
            Ok(bytes) if bytes.len() as u64 == size && blake3::hash(&bytes) == blake3 => {
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
        if self.write_file(key, bytes).is_err() {
            // A failed write leaves nothing recorded, so the block is simply not cached.
            return;
        }
        let mut state = self.state();
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        // Another thread may have admitted the same block while this one was writing; the file holds the same bytes
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
                let _ = std::fs::remove_file(self.path_of(&victim));
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
