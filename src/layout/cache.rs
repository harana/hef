//! Keeps recently read pieces of remote files close at hand, in memory and optionally on a local disk, so reading the
//! same file again does not go back to object storage.
//!
//! The cache sits between a remote file's range source and the decoder. It holds the stored bytes exactly as the
//! object holds them, keyed by tenant, file, kind of piece, and byte range. It is rebuildable acceleration state:
//! object storage plus the manifest stay authoritative, and losing any entry, or the whole cache, only costs a
//! refetch. Nothing is trusted because it came from here: the reader proves every cached piece against the file's
//! authenticated checksums before it uses it, drops a piece that fails, and fetches it again. Because only stored bytes
//! are ever admitted, a subject-encrypted block stays encrypted in the cache and destroying its key still makes it
//! unreadable.
//!
//! The memory tier lives here. The local disk tier is an interface ([`CacheStore`]) the deploying application backs,
//! for example with files on a node-local NVMe volume.
//!
//! See: hef-hardware-deployment/spec.md

use crate::events::TenantId;
use crate::file::FileError;
use hashbrown::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

/// Which part of a stored file a cached piece holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// The file's trailing bytes as one tail fetch returns them: the footer region plus any proof appendix.
    Footer,
    /// A range inside one stripe that a graph read fetched for its edge blocks, widened to the proof leaves that
    /// verify it.
    GraphIndex,
    /// A range inside one stripe, widened to the proof leaves that verify it.
    Stripe,
}

/// Names one cached piece of one stored file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockKey {
    pub file_id: u128,
    pub kind: BlockKind,
    pub len: u64,
    /// Measured from the first byte of the stored object.
    pub offset: u64,
    /// Part of every key, so a piece one tenant's reader admitted is never served to another tenant's reader.
    pub tenant_id: TenantId,
}

/// The local disk tier of a [`BlockCache`], backed by the deploying application (for example files on a node-local
/// NVMe volume, sized and evicted by the application).
///
/// Whatever it returns is untrusted: the reader verifies a piece before using it and calls
/// [`remove`](Self::remove) on one that fails. A failure here is treated as a miss, never as a failed read.
pub trait CacheStore: Send + Sync {
    /// The bytes stored under `key`, or `None` when nothing is.
    fn get(&self, key: &BlockKey) -> Result<Option<Vec<u8>>, FileError>;

    /// Stores `bytes` under `key`, replacing anything already there.
    fn put(&self, key: &BlockKey, bytes: &[u8]) -> Result<(), FileError>;

    /// Drops whatever is stored under `key`; removing a missing key is not an error.
    fn remove(&self, key: &BlockKey) -> Result<(), FileError>;
}

/// A shared cache of stored file pieces that remote readers check before asking object storage, with a byte-budgeted
/// memory tier and an optional application-backed disk tier underneath it.
///
/// Share one between every reader on a node (pass the same `Arc` to [`super::reader::HefFile::open_remote`]) so a
/// second open of a file is served from here. Eviction only ever costs a refetch, never a different result.
///
/// See: hef-hardware-deployment/spec.md
pub struct BlockCache {
    disk: Option<Arc<dyn CacheStore>>,
    memory: Mutex<MemoryTier>,
}

/// The in-memory tier: least-recently-used pieces are dropped once the held bytes pass the budget.
///
/// Recency is a doubly linked list threaded through `slots` by index, so a hit moves its piece to the front and an
/// eviction takes the back without searching every entry. Slots freed by eviction or removal are reused.
#[derive(Debug)]
struct MemoryTier {
    budget_bytes: u64,
    entries: HashMap<BlockKey, usize>,
    free_slots: Vec<usize>,
    held_bytes: u64,
    least_recent: Option<usize>,
    most_recent: Option<usize>,
    slots: Vec<MemorySlot>,
}

/// One position in the memory tier's recency list; `bytes` is `None` while the slot is free.
#[derive(Debug)]
struct MemorySlot {
    bytes: Option<Arc<[u8]>>,
    key: BlockKey,
    newer: Option<usize>,
    older: Option<usize>,
}

impl std::fmt::Debug for BlockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockCache")
            .field("disk", &self.disk.is_some())
            .field("memory_bytes", &self.memory_bytes())
            .finish()
    }
}

impl BlockCache {
    /// A cache holding at most `memory_budget_bytes` in memory, over `disk` when the application provides a disk tier.
    /// A piece larger than the whole memory budget skips the memory tier and lives only on disk.
    pub fn new(memory_budget_bytes: u64, disk: Option<Arc<dyn CacheStore>>) -> Self {
        Self {
            disk,
            memory: Mutex::new(MemoryTier {
                budget_bytes: memory_budget_bytes,
                entries: HashMap::new(),
                free_slots: Vec::new(),
                held_bytes: 0,
                least_recent: None,
                most_recent: None,
                slots: Vec::new(),
            }),
        }
    }

    /// Bytes the memory tier holds right now.
    pub fn memory_bytes(&self) -> u64 {
        self.memory.lock().unwrap_or_else(PoisonError::into_inner).held_bytes
    }

    /// The piece stored under `key`, from memory first and then from disk. A disk hit is copied into memory for the
    /// next read. The caller must verify what it gets.
    pub(crate) fn get(&self, key: &BlockKey) -> Option<Arc<[u8]>> {
        {
            let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(bytes) = memory.get(key) {
                return Some(bytes);
            }
        }
        let bytes: Arc<[u8]> = self.disk.as_ref()?.get(key).ok().flatten()?.into();
        self.memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(*key, Arc::clone(&bytes));
        Some(bytes)
    }

    /// Admits a piece the caller has already verified, into memory and onto disk.
    pub(crate) fn insert(&self, key: BlockKey, bytes: Arc<[u8]>) {
        if let Some(disk) = &self.disk {
            // A disk-tier failure leaves the piece uncached there; the next miss simply refetches it.
            disk.put(&key, &bytes).ok();
        }
        self.memory
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, bytes);
    }

    /// Drops the piece under `key` from both tiers, after it failed verification.
    pub(crate) fn remove(&self, key: &BlockKey) {
        {
            let mut memory = self.memory.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(slot) = memory.entries.remove(key) {
                memory.release(slot);
            }
        }
        if let Some(disk) = &self.disk {
            disk.remove(key).ok();
        }
    }
}

impl MemoryTier {
    fn get(&mut self, key: &BlockKey) -> Option<Arc<[u8]>> {
        let slot = *self.entries.get(key)?;
        self.unlink(slot);
        self.push_most_recent(slot);
        self.slots.get(slot)?.bytes.clone()
    }

    fn insert(&mut self, key: BlockKey, bytes: Arc<[u8]>) {
        let len = bytes.len() as u64;
        if len > self.budget_bytes {
            return;
        }
        let slot = match self.entries.get(&key) {
            Some(&slot) => {
                self.unlink(slot);
                if let Some(replaced) = self.slots.get_mut(slot).and_then(|entry| entry.bytes.replace(bytes)) {
                    self.held_bytes -= replaced.len() as u64;
                }
                slot
            }
            None => {
                let fresh = MemorySlot {
                    bytes: Some(bytes),
                    key,
                    newer: None,
                    older: None,
                };
                let reused = self
                    .free_slots
                    .pop()
                    .and_then(|slot| Some((slot, self.slots.get_mut(slot)?)));
                let slot = match reused {
                    Some((slot, entry)) => {
                        *entry = fresh;
                        slot
                    }
                    None => {
                        self.slots.push(fresh);
                        self.slots.len() - 1
                    }
                };
                self.entries.insert(key, slot);
                slot
            }
        };
        self.push_most_recent(slot);
        self.held_bytes += len;
        while self.held_bytes > self.budget_bytes {
            // The piece just admitted is the most recent, so it is the oldest only once it is all that is left.
            let Some(oldest) = self.least_recent.filter(|&oldest| oldest != slot) else {
                break;
            };
            if let Some(entry) = self.slots.get(oldest) {
                let oldest_key = entry.key;
                self.entries.remove(&oldest_key);
            }
            self.release(oldest);
        }
    }

    /// Unlinks a slot already removed from `entries`, gives back its bytes and keeps it for reuse.
    fn release(&mut self, slot: usize) {
        self.unlink(slot);
        if let Some(bytes) = self.slots.get_mut(slot).and_then(|entry| entry.bytes.take()) {
            self.held_bytes -= bytes.len() as u64;
        }
        self.free_slots.push(slot);
    }

    fn unlink(&mut self, slot: usize) {
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        let (newer, older) = (entry.newer.take(), entry.older.take());
        match newer.and_then(|newer| self.slots.get_mut(newer)) {
            Some(entry) => entry.older = older,
            None => self.most_recent = older,
        }
        match older.and_then(|older| self.slots.get_mut(older)) {
            Some(entry) => entry.newer = newer,
            None => self.least_recent = newer,
        }
    }

    fn push_most_recent(&mut self, slot: usize) {
        let most_recent = self.most_recent;
        if let Some(entry) = self.slots.get_mut(slot) {
            entry.older = most_recent;
        }
        match most_recent.and_then(|previous| self.slots.get_mut(previous)) {
            Some(entry) => entry.newer = Some(slot),
            None => self.least_recent = Some(slot),
        }
        self.most_recent = Some(slot);
    }
}

#[cfg(test)]
#[path = "test/cache.rs"]
mod tests;
