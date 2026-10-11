//! Stores a subject's current state, such as a chat room's membership, as a tree of small immutable chunks, so two
//! successive states share almost every chunk, and a reader finds one key, or what changed between two states, by
//! fetching only a few of them.
//!
//! A state map takes each state key (a state type and a state key, both interned to `u32` ids) to the sequence number
//! of the event that set it. The map is built into a prolly tree: leaves hold the entries, each interior chunk holds
//! its children's first keys and addresses, and the root chunk's address is the state root. A chunk ends after an
//! entry once it holds at least 4 KiB of entries and a hash of the entry's key and level hits a fixed pattern (a
//! rolling hash whose window is one entry), or before an entry that would take it past 16 KiB. A changed value never
//! moves a boundary and an inserted or removed key only moves the boundaries next to it, so the same map always builds
//! the same tree and a one-key change rewrites one leaf and its ancestors.
//!
//! A chunk's plaintext, every integer little-endian:
//!
//! - header: `"HSC1"`, level `u8` (0 for a leaf), three zero bytes, entry count `u32`;
//! - leaf entries, strictly ascending by key: type id `u32`, state key id `u32`, event sequence number `u64` (16 B);
//! - interior entries, strictly ascending by key: the child's first type id `u32` and state key id `u32`, then the
//!   child's 32-byte address (40 B);
//! - a CRC-64/NVME of everything before it, `u64`.
//!
//! Each chunk is sealed with the tenant's [`StateChunkKey`], which also gives its address, and stored once per tenant
//! under that address: identical chunks seal to identical bytes, so deduplication survives encryption.
//!
//! See: hef-physical-artifacts/spec.md

use super::constant::{
    STATE_CHUNK_BOUNDARY_SPAN_BYTES, STATE_CHUNK_CRC_LEN, STATE_CHUNK_HEADER_LEN, STATE_CHUNK_MAGIC,
    STATE_CHUNK_MAX_BYTES, STATE_CHUNK_MIN_BYTES, STATE_INTERIOR_ENTRY_LEN, STATE_LEAF_ENTRY_LEN,
};
use crate::error::{FormatError, StateChunkError, StorageError};
use crate::events::TenantId;
use crate::file::bytes::{Reader, Writer};
use crate::file::integrity::crc64_nvme;
use crate::indexes::stable_hash;
use crate::object_store::{ObjectStore, state_chunk_object_key};
use crate::security::StateChunkKey;
use hashbrown::HashSet;
use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};

/// One key of a subject's state: the kind of state and which instance of it, as interned ids. Keys sort by type id,
/// then by state key id.
///
/// See: hef-physical-artifacts/spec.md
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct StateKey {
    pub state_key_id: u32,
    pub type_id: u32,
}

impl Ord for StateKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.type_id, self.state_key_id).cmp(&(other.type_id, other.state_key_id))
    }
}

impl PartialOrd for StateKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Where a subject's state stood after one event: the address of its state tree's root chunk and how many keys the
/// state holds.
///
/// See: hef-physical-artifacts/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateRoot {
    pub address: [u8; 32],
    pub entry_count: u32,
}

/// One sealed chunk, ready to store under its address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedStateChunk {
    pub address: [u8; 32],
    pub bytes: Vec<u8>,
}

/// A state map built into chunks: its root, and every chunk of the tree.
///
/// See: hef-physical-artifacts/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTree {
    /// Children before their parents and the root last, so storing them in order never stores a chunk before the
    /// chunks it names.
    pub chunks: Vec<SealedStateChunk>,
    pub root: StateRoot,
}

impl StateTree {
    /// The chunks of this tree that `previous` does not have, in storing order: all an upload has to store when
    /// `previous` is already stored.
    pub fn new_chunks(&self, previous: &StateTree) -> Vec<SealedStateChunk> {
        let stored: HashSet<&[u8; 32]> = previous.chunks.iter().map(|chunk| &chunk.address).collect();
        self.chunks
            .iter()
            .filter(|chunk| !stored.contains(&chunk.address))
            .cloned()
            .collect()
    }
}

/// One key whose value differs between two states. The side that lacks the key holds `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateChange {
    pub after: Option<u64>,
    pub before: Option<u64>,
    pub key: StateKey,
}

/// Where a tenant's state chunks are read from. The application backs it with durable storage; in production that is
/// [`ObjectStoreStateChunks`], the object store uploads put the chunks in.
///
/// See: hef-physical-artifacts/spec.md
pub trait StateChunkStore {
    /// The bytes stored under `address`, or `None` when nothing is stored there.
    fn get(&self, address: &[u8; 32]) -> Result<Option<Vec<u8>>, StorageError>;
}

/// One tenant's state chunks as they sit in the application's object store, under
/// [`state_chunk_object_key`].
///
/// See: hef-physical-artifacts/spec.md
pub struct ObjectStoreStateChunks<'a> {
    pub objects: &'a dyn ObjectStore,
    pub tenant_id: TenantId,
}

impl StateChunkStore for ObjectStoreStateChunks<'_> {
    fn get(&self, address: &[u8; 32]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self
            .objects
            .get(&state_chunk_object_key(self.tenant_id, address))?
            .map(|stored| stored.bytes))
    }
}

/// One chunk's decoded entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StateChunk {
    /// Each child's first key and address, in key order. `level` is at least 1; the children sit one level below.
    Interior {
        children: Vec<(StateKey, [u8; 32])>,
        level: u8,
    },
    Leaf(Vec<(StateKey, u64)>),
}

impl StateChunk {
    fn level(&self) -> u8 {
        match self {
            StateChunk::Interior { level, .. } => *level,
            StateChunk::Leaf(_) => 0,
        }
    }
}

/// Builds `entries` into a state tree sealed with `key`. The same map under the same key always gives the same tree
/// and root. Fails only for a map of more than `u32::MAX` keys.
pub fn build_state_tree(key: &StateChunkKey, entries: &BTreeMap<StateKey, u64>) -> Result<StateTree, FormatError> {
    let entry_count = u32::try_from(entries.len()).map_err(|_| FormatError::Structural {
        rule: "a state map holds at most u32::MAX keys",
    })?;
    let leaves: Vec<(StateKey, [u8; 8])> = entries.iter().map(|(key, sn)| (*key, sn.to_le_bytes())).collect();
    let mut chunks = Vec::new();
    let mut level = 0u8;
    let mut parents = seal_level(key, level, &leaves, &mut chunks)?;
    while parents.len() > 1 {
        level += 1;
        parents = seal_level(key, level, &parents, &mut chunks)?;
    }
    let (_, address) = parents.first().ok_or(FormatError::Structural {
        rule: "a state tree has a root",
    })?;
    Ok(StateTree {
        chunks,
        root: StateRoot {
            address: *address,
            entry_count,
        },
    })
}

/// Cuts one level's entries into chunks, seals each, appends them to `chunks`, and returns each chunk's first key and
/// address: the entries of the level above. An empty level (the empty map) is one empty leaf.
fn seal_level<const N: usize>(
    key: &StateChunkKey,
    level: u8,
    entries: &[(StateKey, [u8; N])],
    chunks: &mut Vec<SealedStateChunk>,
) -> Result<Vec<(StateKey, [u8; 32])>, FormatError> {
    let entry_len = 8 + N;
    let mut parents = Vec::new();
    let mut start = 0;
    let mut bytes = 0;
    for (end, (entry_key, _)) in (1..).zip(entries) {
        bytes += entry_len;
        let full = bytes + entry_len > STATE_CHUNK_MAX_BYTES;
        if end == entries.len() || full || (bytes >= STATE_CHUNK_MIN_BYTES && is_boundary(level, *entry_key, entry_len))
        {
            parents.push(seal_chunk(
                key,
                level,
                entries.get(start..end).unwrap_or_default(),
                chunks,
            )?);
            start = end;
            bytes = 0;
        }
    }
    if entries.is_empty() {
        parents.push(seal_chunk(key, level, entries, chunks)?);
    }
    Ok(parents)
}

/// Whether a chunk at `level` may end after the entry keyed `entry_key`: a hash of the level and key hits the pattern
/// that gives, on average, one boundary per [`STATE_CHUNK_BOUNDARY_SPAN_BYTES`] of entries.
fn is_boundary(level: u8, entry_key: StateKey, entry_len: usize) -> bool {
    let [t0, t1, t2, t3] = entry_key.type_id.to_le_bytes();
    let [s0, s1, s2, s3] = entry_key.state_key_id.to_le_bytes();
    stable_hash(&[level, t0, t1, t2, t3, s0, s1, s2, s3])
        .is_multiple_of((STATE_CHUNK_BOUNDARY_SPAN_BYTES / entry_len) as u64)
}

fn seal_chunk<const N: usize>(
    key: &StateChunkKey,
    level: u8,
    entries: &[(StateKey, [u8; N])],
    chunks: &mut Vec<SealedStateChunk>,
) -> Result<(StateKey, [u8; 32]), FormatError> {
    let mut out = Writer::with_capacity(STATE_CHUNK_HEADER_LEN + entries.len() * (8 + N) + STATE_CHUNK_CRC_LEN);
    out.put_slice(&STATE_CHUNK_MAGIC);
    out.put_u8(level);
    out.put_slice(&[0; 3]);
    out.put_u32(entries.len() as u32);
    for (entry_key, value) in entries {
        out.put_u32(entry_key.type_id);
        out.put_u32(entry_key.state_key_id);
        out.put_slice(value);
    }
    out.put_u64(crc64_nvme(out.bytes()));
    let (address, bytes) = key.seal(out.bytes()).ok_or(FormatError::Structural {
        rule: "a state chunk is too large to seal",
    })?;
    chunks.push(SealedStateChunk { address, bytes });
    let first = entries.first().map(|(entry_key, _)| *entry_key).unwrap_or_default();
    Ok((first, address))
}

/// Decodes one chunk's plaintext, refusing a bad magic, a nonzero reserved byte, a CRC mismatch, entries that do not
/// fill the chunk exactly or are not strictly ascending, and an interior chunk with no children.
pub(crate) fn decode_state_chunk(bytes: &[u8]) -> Result<StateChunk, FormatError> {
    let (body, crc) = bytes
        .split_at_checked(bytes.len().saturating_sub(STATE_CHUNK_CRC_LEN))
        .filter(|(_, crc)| crc.len() == STATE_CHUNK_CRC_LEN)
        .ok_or(FormatError::Truncated { what: "state chunk" })?;
    if crc != crc64_nvme(body).to_le_bytes() {
        return Err(FormatError::Structural {
            rule: "state chunk CRC-64/NVME mismatch",
        });
    }
    let mut reader = Reader::new(body);
    if reader.take(4, "state chunk magic")? != STATE_CHUNK_MAGIC {
        return Err(FormatError::BadMagic { expected: "HSC1" });
    }
    let level = reader.u8("state chunk level")?;
    if reader.take(3, "state chunk reserved bytes")? != [0; 3] {
        return Err(FormatError::ReservedNotZero {
            field: "state chunk reserved bytes",
        });
    }
    let count = reader.u32("state chunk entry count")? as usize;
    let entry_len = if level == 0 {
        STATE_LEAF_ENTRY_LEN
    } else {
        STATE_INTERIOR_ENTRY_LEN
    };
    if count.checked_mul(entry_len) != Some(reader.remaining()) || (level > 0 && count == 0) {
        return Err(FormatError::Structural {
            rule: "state chunk entries fill it exactly, and an interior chunk names a child",
        });
    }
    let mut previous: Option<StateKey> = None;
    let mut leaf = Vec::new();
    let mut children = Vec::new();
    for _ in 0..count {
        let entry_key = StateKey {
            type_id: reader.u32("state entry type id")?,
            state_key_id: reader.u32("state entry state key id")?,
        };
        if previous.is_some_and(|previous| previous >= entry_key) {
            return Err(FormatError::Structural {
                rule: "state chunk keys must be strictly ascending",
            });
        }
        previous = Some(entry_key);
        if level == 0 {
            leaf.push((entry_key, reader.u64("state entry event sequence number")?));
        } else {
            let address = reader.take(32, "state entry child address")?;
            children.push((entry_key, address.try_into().unwrap_or([0; 32])));
        }
    }
    Ok(if level == 0 {
        StateChunk::Leaf(leaf)
    } else {
        StateChunk::Interior { children, level }
    })
}

/// Fetches, opens and decodes the chunk at `address`, checking it sits at `level` when the caller knows it.
pub(crate) fn read_state_chunk(
    store: &dyn StateChunkStore,
    key: &StateChunkKey,
    address: &[u8; 32],
    level: Option<u8>,
) -> Result<StateChunk, StateChunkError> {
    let hex = || hex_simd::encode_to_string(address, hex_simd::AsciiCase::Lower);
    let sealed = store
        .get(address)?
        .ok_or_else(|| StateChunkError::Missing { address: hex() })?;
    let plaintext = key
        .open(address, &sealed)
        .ok_or_else(|| StateChunkError::Unauthentic { address: hex() })?;
    let chunk = decode_state_chunk(&plaintext)?;
    if level.is_some_and(|level| level != chunk.level()) {
        return Err(StateChunkError::Format(FormatError::Structural {
            rule: "a state chunk sits one level below its parent",
        }));
    }
    Ok(chunk)
}

/// The event sequence number `state_key` holds in the state at `root`, or `None` when the state lacks it. Reads one
/// chunk per tree level from `store`, opening each with `key`.
pub fn state_get(
    store: &dyn StateChunkStore,
    key: &StateChunkKey,
    root: &StateRoot,
    state_key: StateKey,
) -> Result<Option<u64>, StateChunkError> {
    let mut address = root.address;
    let mut level = None;
    loop {
        match read_state_chunk(store, key, &address, level)? {
            StateChunk::Leaf(entries) => {
                let found = entries
                    .binary_search_by(|(entry_key, _)| entry_key.cmp(&state_key))
                    .ok();
                return Ok(found.and_then(|index| entries.get(index)).map(|(_, sn)| *sn));
            }
            StateChunk::Interior {
                children,
                level: parent,
            } => {
                let after = children.partition_point(|(first, _)| *first <= state_key);
                let Some((_, child)) = after.checked_sub(1).and_then(|index| children.get(index)) else {
                    return Ok(None);
                };
                address = *child;
                level = Some(parent - 1);
            }
        }
    }
}

/// One side of a diff still to compare, in key order: a whole subtree not yet read, or one leaf entry.
#[derive(Debug, Clone, Copy)]
enum Pending {
    Entry(StateKey, u64),
    Subtree {
        address: [u8; 32],
        first: StateKey,
        level: u8,
    },
}

/// Every key whose value differs between the states at `before` and `after`, in key order. Subtrees the two trees
/// share are recognised by address and never read, so a small change costs a few chunk reads whatever the state's
/// size.
pub fn state_diff(
    store: &dyn StateChunkStore,
    key: &StateChunkKey,
    before: &StateRoot,
    after: &StateRoot,
) -> Result<Vec<StateChange>, StateChunkError> {
    let mut changes = Vec::new();
    if before.address == after.address {
        return Ok(changes);
    }
    let mut old = VecDeque::new();
    let mut new = VecDeque::new();
    expand(store, key, &before.address, None, &mut old)?;
    expand(store, key, &after.address, None, &mut new)?;
    loop {
        let (expand_old, popped_old, popped_new) = match (old.front().copied(), new.front().copied()) {
            (None, None) => return Ok(changes),
            (Some(Pending::Subtree { address: a, .. }), Some(Pending::Subtree { address: b, .. })) if a == b => {
                (None, true, true)
            }
            (Some(Pending::Entry(old_key, old_sn)), Some(Pending::Entry(new_key, new_sn))) => {
                match old_key.cmp(&new_key) {
                    Ordering::Less => {
                        changes.push(change(old_key, Some(old_sn), None));
                        (None, true, false)
                    }
                    Ordering::Greater => {
                        changes.push(change(new_key, None, Some(new_sn)));
                        (None, false, true)
                    }
                    Ordering::Equal => {
                        if old_sn != new_sn {
                            changes.push(change(old_key, Some(old_sn), Some(new_sn)));
                        }
                        (None, true, true)
                    }
                }
            }
            (Some(Pending::Entry(old_key, old_sn)), Some(Pending::Subtree { first, .. })) if old_key < first => {
                changes.push(change(old_key, Some(old_sn), None));
                (None, true, false)
            }
            (Some(Pending::Subtree { first, .. }), Some(Pending::Entry(new_key, new_sn))) if new_key < first => {
                changes.push(change(new_key, None, Some(new_sn)));
                (None, false, true)
            }
            (
                Some(Pending::Subtree {
                    first: a, level: la, ..
                }),
                Some(Pending::Subtree {
                    first: b, level: lb, ..
                }),
            ) => (Some(a < b || (a == b && la >= lb)), false, false),
            (Some(Pending::Subtree { .. }), _) => (Some(true), false, false),
            (_, Some(Pending::Subtree { .. })) => (Some(false), false, false),
            (Some(Pending::Entry(old_key, old_sn)), None) => {
                changes.push(change(old_key, Some(old_sn), None));
                (None, true, false)
            }
            (None, Some(Pending::Entry(new_key, new_sn))) => {
                changes.push(change(new_key, None, Some(new_sn)));
                (None, false, true)
            }
        };
        if popped_old {
            old.pop_front();
        }
        if popped_new {
            new.pop_front();
        }
        if let Some(expand_old) = expand_old {
            let side = if expand_old { &mut old } else { &mut new };
            if let Some(Pending::Subtree { address, level, .. }) = side.pop_front() {
                expand(store, key, &address, Some(level), side)?;
            }
        }
    }
}

fn change(key: StateKey, before: Option<u64>, after: Option<u64>) -> StateChange {
    StateChange { after, before, key }
}

/// Reads the chunk at `address` and puts its contents at the front of `side`, in key order.
fn expand(
    store: &dyn StateChunkStore,
    key: &StateChunkKey,
    address: &[u8; 32],
    level: Option<u8>,
    side: &mut VecDeque<Pending>,
) -> Result<(), StateChunkError> {
    match read_state_chunk(store, key, address, level)? {
        StateChunk::Leaf(entries) => {
            for (entry_key, sn) in entries.into_iter().rev() {
                side.push_front(Pending::Entry(entry_key, sn));
            }
        }
        StateChunk::Interior { children, level } => {
            for (first, address) in children.into_iter().rev() {
                side.push_front(Pending::Subtree {
                    address,
                    first,
                    level: level - 1,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/state_chunk.rs"]
mod tests;
