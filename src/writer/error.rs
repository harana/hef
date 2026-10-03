//! The ways the commit pipeline fails along the way: a flush cannot complete, a reservation lease cannot be hardened,
//! or the worker's hand-off queue rejects a record.

use crate::error::{FormatError, StorageError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FlushError {
    #[error(transparent)]
    Format(FormatError),
    #[error(transparent)]
    Lease(LeaseError),
    /// A frame contains events for exactly one tenant and one epoch.
    #[error("frame contains events for more than one tenant or epoch")]
    MixedTenantOrEpoch,
    #[error("nothing to flush")]
    NothingToFlush,
    #[error(transparent)]
    Queue(QueueError),
    #[error(transparent)]
    Storage(StorageError),
    /// The durability barrier failed after the frame's bytes were already appended *and* those bytes were read back
    /// intact. They may still be persisted by a later sync, so the caller must not close the frame's range with a void
    /// record. A sync failure whose bytes are not present at all is reported as `Storage` instead, because that range
    /// can safely be voided.
    #[error("durability barrier failed after append: {error}")]
    SyncAfterAppend { error: StorageError, frame_offset: u64 },
}

/// Why a harden attempt was refused.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum LeaseError {
    /// The lease expired: the range was (or will be) closed by a void record; the worker must re-reserve a fresh range
    /// rather than harden under the stale one.
    #[error("lease expired; re-reserve a fresh range and retry")]
    Expired,
    #[error("unknown lease id")]
    Unknown,
}

#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum QueueError {
    #[error("record codec failure")]
    Codec,
    #[error("bounded buffer is full; wait for the head to advance")]
    Full,
    /// The event's envelope names a different tenant than the worker this queue belongs to.
    #[error("event tenant does not match this worker's tenant")]
    TenantMismatch,
}
