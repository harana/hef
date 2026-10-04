//! The friendly front door to the shared file layer: the one interface durable files are read and written through, plus
//! the small helpers every consumer reuses.
//!
//! Two access shapes share one set of bytes-on-media rules. Append-only files — the HEJ journal today — go through
//! [`BlockStore`]: whole 4096-byte-aligned frames in, a durability barrier, byte-offset reads back out. Whole files —
//! object commits, pointer records, HEF parts — go through the helpers here and the deploying application's own
//! whole-file publish: an atomic create-only publish and a byte-range read, with integrity always proven by
//! [`crate::file::integrity`].
//!
//! Both shapes are synchronous and deterministic; a production backend may own a completion-based runtime internally,
//! and the in-memory simulation owns none.

use super::error::FileError;
use super::model::{Atomicity, BlockTarget, ByteRange};

/// How a backend appends to, reads from, and flushes append-only files, one target at a time.
///
/// The interface is synchronous by design: the commit protocol above it is a deterministic state machine, and a
/// production backend owns any asynchronous edge internally. Every append is a whole 4096-byte-aligned frame.
///
/// An append is visible the instant it returns: it advances the written extent and its bytes are readable before any
/// [`sync`](Self::sync). `sync` governs durability across a crash, not visibility — so [`extent`](Self::extent) counts
/// appended-but-unsynced bytes and [`read`](Self::read) serves them, identically on every backend (the production
/// backend and the simulation oracle alike).
pub trait BlockStore {
    /// Appends one aligned frame to `target`; returns the byte offset it landed at. Durability is claimed only after
    /// [`sync`](Self::sync) (buffered mode) or write completion (direct mode); callers consult
    /// [`atomicity`](Self::atomicity) for the target's mode.
    fn append(&mut self, target: BlockTarget, frame: &[u8]) -> Result<u64, FileError>;

    /// Appends one aligned frame and makes it durable in a single step, returning the byte offset it landed at — the
    /// force-commit a latency-critical caller wants.
    ///
    /// The default is simply an [`append`](Self::append) followed by a [`sync`](Self::sync); a backend may override it
    /// with a cheaper fused path (for example one linked io_uring submission), with the same result.
    fn append_and_sync(&mut self, target: BlockTarget, frame: &[u8]) -> Result<u64, FileError> {
        let offset = self.append(target, frame)?;
        self.sync(target)?;
        Ok(offset)
    }

    /// Makes everything appended to `target` so far durable.
    fn sync(&mut self, target: BlockTarget) -> Result<(), FileError>;

    /// Drops everything past `len` bytes of `target` and makes the shorter extent durable, so the next append starts
    /// at `len`.
    ///
    /// This is how recovery discards a torn tail. The layer here cannot find one on its own: it knows frames are
    /// 4096-byte-aligned but not where one ends, so a torn write that happens to stop on a block boundary looks
    /// perfectly aligned. Only the caller that decodes frames knows the last good boundary — it replays, finds the
    /// first frame that does not verify, and truncates to that offset. Without that the next append lands after the
    /// bad frame and every frame after it becomes unreachable, because replay stops at the first one it cannot read.
    ///
    /// `len` must be 4096-byte-aligned ([`FileError::Unaligned`] otherwise) and no larger than the current extent
    /// ([`FileError::OutOfBounds`] otherwise); truncating to the current extent is a no-op that still syncs.
    fn truncate(&mut self, target: BlockTarget, len: u64) -> Result<(), FileError>;

    /// Reads `len` bytes at `offset` from the target's written extent.
    fn read(&self, target: BlockTarget, offset: u64, len: u32) -> Result<Vec<u8>, FileError>;

    /// The target's written extent in bytes.
    fn extent(&self, target: BlockTarget) -> Result<u64, FileError>;

    /// The target's current untorn-write guarantee: what the startup probe found, unless this target's writes have
    /// since fallen back to the ordinary path, in which case the conservative no-capability answer.
    fn atomicity(&self, target: BlockTarget) -> Result<Atomicity, FileError>;
}

/// How a reader fetches pieces of a stored file it does not hold in memory, such as a file in object storage. The
/// deploying application implements it over its own object store.
///
/// Synchronous by design, like [`BlockStore`]: the application owns any asynchronous edge internally. `object` is the
/// file id the manifest names; the application maps it to its own object key. The bytes are untrusted here; the
/// reader proves every byte it serves against the file's authenticated checksums.
pub trait RangeSource: Send + Sync {
    /// Returns exactly `len` bytes of `object` starting at byte `offset`. A range that runs past the end of the object
    /// is an error, never a short read.
    fn read_range(&self, object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FileError>;

    /// Returns every range in `ranges` of `object`, in the order given, so a caller can hand over several at once and
    /// the application can merge neighbours or fetch them in parallel. The default reads them one at a time.
    fn read_ranges(&self, object: u128, ranges: &[ByteRange]) -> Result<Vec<Vec<u8>>, FileError> {
        ranges
            .iter()
            .map(|range| self.read_range(object, range.offset, range.len))
            .collect()
    }
}

/// Confirms a whole buffer is the bytes the owner committed: its length matches and its authoritative BLAKE3 matches.
/// This is the gate every read through an owning reference passes before any byte is used.
pub fn verify_checksum(
    bytes: &[u8],
    expected_blake3: &[u8; blake3::OUT_LEN],
    expected_size: u64,
) -> Result<(), FileError> {
    if bytes.len() as u64 != expected_size {
        return Err(FileError::ChecksumMismatch);
    }
    if blake3::hash(bytes) != blake3::Hash::from_bytes(*expected_blake3) {
        return Err(FileError::ChecksumMismatch);
    }
    Ok(())
}

/// Takes `length` bytes (or to the end when `None`) from `offset`. An offset past the end is an error; a length past
/// the end is clamped, the way cloud range reads behave.
pub fn slice_range(bytes: &[u8], offset: u64, length: Option<u64>) -> Result<Vec<u8>, FileError> {
    let size = bytes.len() as u64;
    if offset > size {
        return Err(FileError::InvalidRange { offset, size });
    }
    let end = match length {
        Some(len) => offset.saturating_add(len).min(size),
        None => size,
    };
    Ok(bytes.get(offset as usize..end as usize).unwrap_or_default().to_vec())
}

#[cfg(test)]
#[path = "test/api.rs"]
mod tests;
