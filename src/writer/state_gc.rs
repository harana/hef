//! Deletes the state chunks no published file needs any more, once they have gone unused for a grace period.
//!
//! State chunks are shared between roots, so a chunk cannot leave with the file that first stored it. A sweep instead
//! marks every chunk reachable from the state roots of the tenant's files in the head catalogue generation, then
//! deletes each unmarked chunk no upload has stored or reused for [`STATE_CHUNK_GRACE_NANOS`]. The grace period covers
//! readers still holding an older root and uploads whose file is not yet published.
//!
//! Which chunks exist, when an upload last used each, and the marks of the current sweep live in the application's
//! durable [`StateChunkLedger`], since a tenant's chunks can outgrow memory. An upload touches each chunk in the ledger
//! before storing it, and a sweep claims a chunk in the ledger before deleting it; those two steps are atomic against
//! each other, so a sweep never deletes a chunk an upload has just reused. Like the file sweeper, the application runs
//! this periodically, one sweep per tenant at a time.
//!
//! See: hef-file-lifecycle/spec.md

use super::constant::{STATE_CHUNK_GRACE_NANOS, STATE_CHUNK_SWEEP_PAGE_RECORDS};
use crate::artifacts::state_chunk::{ObjectStoreStateChunks, StateChunk, StateRoot, read_state_chunk};
use crate::clock::Clock;
use crate::error::{StateChunkError, StorageError};
use crate::events::TenantId;
use crate::layout::required_features;
use crate::lifecycle::{HefFileEntry, ManifestGeneration};
use crate::object_store::{ObjectStore, state_chunk_object_key};
use crate::security::StateChunkKey;

/// The application's durable record of one tenant's stored state chunks: when an upload last stored or reused each,
/// whether the current sweep found it referenced, and whether a sweep has claimed it for deletion.
///
/// `touch` and `claim` must be atomic against each other (one conditional row update each, say): that is what stops a
/// sweep from deleting a chunk an upload has just reused.
///
/// See: hef-file-lifecycle/spec.md
pub trait StateChunkLedger {
    /// Records that an upload is storing or reusing the chunk at `address` at `now_nanos`, creating its record if it
    /// has none. Returns `false`, changing nothing, when a sweep has claimed the chunk for deletion.
    fn touch(&self, address: &[u8; 32], now_nanos: i64) -> Result<bool, StorageError>;

    /// Unmarks every chunk, before a sweep starts marking.
    fn clear_marks(&self) -> Result<(), StorageError>;

    /// Marks the chunk at `address` as referenced, recording it as never used by an upload if it has no record.
    /// Returns `true` when it was not marked yet.
    fn mark(&self, address: &[u8; 32]) -> Result<bool, StorageError>;

    /// Replaces the contents of `page` with up to `limit` records, in increasing address order, whose addresses come
    /// strictly after `after` (from the lowest address when `None`). A page shorter than `limit` means no records come
    /// after it. Read from an ordered index on the address, so a page costs no more than its own records, and hold no
    /// lock or transaction once the call returns: the sweep claims and forgets records between pages.
    fn records_after(
        &self,
        after: Option<&[u8; 32]>,
        limit: usize,
        page: &mut Vec<StateChunkRecord>,
    ) -> Result<(), StorageError>;

    /// Claims the chunk at `address` for deletion when it is unmarked, unclaimed, and no upload used it after
    /// `unused_since_nanos`. Returns whether it did.
    fn claim(&self, address: &[u8; 32], unused_since_nanos: i64) -> Result<bool, StorageError>;

    /// Drops the record of a chunk whose object was deleted.
    fn forget(&self, address: &[u8; 32]) -> Result<(), StorageError>;
}

/// What the ledger holds for one stored state chunk.
///
/// See: hef-file-lifecycle/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateChunkRecord {
    pub address: [u8; 32],
    /// A sweep claimed the chunk and may have stopped before deleting it; the next sweep finishes the deletion.
    pub claimed: bool,
    /// Nanoseconds since the Unix epoch when an upload last stored or reused the chunk; `i64::MIN` when none has.
    pub last_used_nanos: i64,
    pub marked: bool,
}

/// The state roots a published graph file records. The application implements it by opening the file's footer the way
/// its readers do, with whatever keys an encrypted footer needs, and returning its
/// [`state_roots`](crate::layout::footer::Footer::state_roots).
///
/// See: hef-file-lifecycle/spec.md
pub trait FileStateRoots {
    /// The state roots recorded in the file `entry` names.
    fn state_roots(&self, entry: &HefFileEntry) -> Result<Vec<StateRoot>, StateChunkError>;
}

/// What one state chunk sweep did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateChunkSweep {
    /// The chunks whose objects were deleted.
    pub deleted: Vec<[u8; 32]>,
    /// How many chunks the sweep found referenced.
    pub marked: u64,
}

/// Deletes the state chunks of `tenant_id` that no file in `head` references and no upload has used for
/// [`STATE_CHUNK_GRACE_NANOS`], and returns what it did.
///
/// Every file of the tenant in `head` that declares state roots counts, whatever its state: an `Outdated` or
/// `DeleteOnDestroy` file may still be read by a running query. Marking reads each root's interior chunks, opened with
/// `key`, through `objects`; a chunk a root needs that is missing or does not open fails the sweep before anything is
/// deleted. A chunk a previous sweep claimed but did not finish deleting is deleted now. Each object is deleted before
/// its ledger record is dropped, so a crash in between leaves the record for the next sweep, whose repeat delete is
/// harmless.
///
/// The ledger is read a page of [`STATE_CHUNK_SWEEP_PAGE_RECORDS`] records at a time in address order, so the sweep's
/// own memory does not grow with the ledger (the returned list of deleted chunks still does). A record an upload adds
/// during the sweep is visited only if its address is past the current page; either way it was just used, so it is
/// kept until a later sweep.
pub fn sweep_state_chunks(
    head: &ManifestGeneration,
    tenant_id: TenantId,
    roots: &dyn FileStateRoots,
    key: &StateChunkKey,
    ledger: &dyn StateChunkLedger,
    objects: &dyn ObjectStore,
    clock: &dyn Clock,
) -> Result<StateChunkSweep, StateChunkError> {
    ledger.clear_marks()?;
    let chunks = ObjectStoreStateChunks { objects, tenant_id };
    let mut sweep = StateChunkSweep::default();
    for entry in head.files.iter().filter(|entry| {
        entry.tenant_id == tenant_id && entry.required_feature_flags & required_features::GRAPH_STATE_ROOTS != 0
    }) {
        for root in roots.state_roots(entry)? {
            let mut pending = vec![(root.address, None)];
            while let Some((address, level)) = pending.pop() {
                if !ledger.mark(&address)? {
                    continue;
                }
                sweep.marked += 1;
                // A leaf names no chunks, so it is marked without being read.
                if level == Some(0) {
                    continue;
                }
                if let StateChunk::Interior { children, level } = read_state_chunk(&chunks, key, &address, level)? {
                    pending.extend(children.into_iter().map(|(_, child)| (child, Some(level - 1))));
                }
            }
        }
    }

    let unused_since = clock.now_nanos().saturating_sub(STATE_CHUNK_GRACE_NANOS);
    let mut page = Vec::with_capacity(STATE_CHUNK_SWEEP_PAGE_RECORDS);
    let mut after = None;
    loop {
        ledger.records_after(after.as_ref(), STATE_CHUNK_SWEEP_PAGE_RECORDS, &mut page)?;
        for record in &page {
            if record.claimed
                || (!record.marked
                    && record.last_used_nanos <= unused_since
                    && ledger.claim(&record.address, unused_since)?)
            {
                objects.delete(&state_chunk_object_key(tenant_id, &record.address))?;
                ledger.forget(&record.address)?;
                sweep.deleted.push(record.address);
            }
        }
        match page.last() {
            Some(last) if page.len() == STATE_CHUNK_SWEEP_PAGE_RECORDS => after = Some(last.address),
            _ => return Ok(sweep),
        }
    }
}

#[cfg(test)]
#[path = "test/state_gc.rs"]
mod tests;
