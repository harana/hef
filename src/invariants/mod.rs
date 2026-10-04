//! The swappable interfaces through which the engine touches the outside world — the clock, storage I/O, and publishing
//! — so the exact same engine code can run against real hardware or a deterministic test double.
//!
//! Engine production code reads wall-clock time, randomness, storage, and catalogue publication only through the traits
//! here. `sim` supplies the deterministic, fault-injecting test implementations; `io` supplies the production ones on
//! compio. Nothing in the engine calls `std::time` or the filesystem directly.

pub mod constant;
pub mod io;
pub mod sim;

use super::error::{PublishError, StorageError};
use super::lifecycle::ManifestGeneration;
pub use crate::clock::Clock;
pub use crate::file::model::{Atomicity as ShardAtomicity, DurabilityMode};

/// Identifies one journal shard (one append target owned by one worker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(pub u32);

/// Extends the shared wall-clock interface with the extras the storage engine needs: a monotonic counter for lease
/// deadlines and idle detection, and a seedable entropy source for jitter so simulation can drive both
/// deterministically from a seed.
///
/// Every implementor must also implement [`Clock`] (which supplies `now_nanos`); this trait adds only
/// what the engine adds on top.
pub trait MonotonicClock: Clock {
    /// Monotonic nanoseconds for deadlines and idle detection.
    fn monotonic_nanos(&self) -> u64;
    /// Uniform entropy in `[0, bound)`; `bound == 0` returns 0. Jittered triggers draw from here so simulation controls
    /// them via the seed.
    fn jitter(&self, bound: u64) -> u64;
}

/// How the engine appends to, reads from, and flushes the durable journal, one shard at a time.
///
/// The trait is synchronous by design (design D4): the commit protocol is a deterministic state machine, and the
/// compio-backed production implementation owns the async edge internally. All appends must be 4096-byte aligned whole
/// frames.
pub trait JournalStorage {
    /// Appends one aligned frame; returns the byte offset it was written at. Durability is claimed only after `sync`
    /// (buffered mode) or completion (direct mode); callers consult `atomicity` for the shard's mode.
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, StorageError>;
    /// Durability barrier for everything appended to the shard so far.
    fn sync(&mut self, shard: ShardId) -> Result<(), StorageError>;
    /// Drops everything past `len` bytes of the shard and makes the shorter extent durable, so the next append starts
    /// at `len`. This is how recovery discards a torn tail: the storage layer sees 4096-byte-aligned blocks and cannot
    /// tell where a frame ends, so only replay knows the last good boundary. `len` must be 4096-byte aligned and no
    /// larger than the shard's current extent.
    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), StorageError>;
    /// Reads `len` bytes at `offset` from the shard's written extent.
    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, StorageError>;
    /// The shard's written extent in bytes.
    fn extent(&self, shard: ShardId) -> Result<u64, StorageError>;
    /// Probed atomicity metadata recorded for the shard at startup.
    fn atomicity(&self, shard: ShardId) -> Result<ShardAtomicity, StorageError>;
}

/// Runs the independent pieces of a batch job — such as encoding a file's column blocks — possibly in parallel.
///
/// Storage-core code reaches scheduling only through this interface, so the identical build code runs on a thread
/// pool in production and strictly sequentially under deterministic simulation. An implementation runs `job(index)`
/// exactly once for every `index` in `0..job_count` and returns only when all of them have finished. It promises
/// nothing about ordering or interleaving, so callers collect results into index-addressed slots and their output is
/// byte-identical however the jobs were scheduled.
///
/// See: hef-core-invariants/spec.md
pub trait EncodeExecutor: Sync {
    /// Number of jobs this executor can make progress on at once. Callers use the hint to turn collections of tiny
    /// metadata operations into roughly one batch per worker instead of submitting one task per entry. Executors
    /// that do not expose parallel workers retain the conservative serial default.
    fn parallelism(&self) -> usize {
        1
    }

    /// Runs `job(0)` through `job(job_count - 1)` to completion.
    fn run_jobs(&self, job_count: usize, job: &(dyn Fn(usize) + Sync));
}

/// How a new version of the file catalogue is published, modelled on object-store conditional writes — create-only
/// generation objects plus an If-Match compare-and-swap on the head pointer.
///
/// Production uses [`crate::object_store::LivePublishedSet`] over the application's object store; tests use the
/// in-memory [`sim::SimulatedPublishedSet`], which keeps the same rebase-and-retry, never-overwrite discipline.
///
/// Generation ids are plain numbers, but a real store compares the head pointer by its ETag, not by the id it holds.
/// An implementation may therefore remember the ETag it read in `head` and use it for the next `advance_head`; it must
/// then fail that `advance_head` with `CasLost` whenever the pointer changed since that read, even if the caller's
/// `expected` id is right, and never fall back to an unconditional write.
pub trait PublishedSet {
    /// Current head generation id and its manifest contents. May remember the pointer's ETag for `advance_head`.
    fn head(&self) -> Result<(u64, ManifestGeneration), PublishError>;
    /// Create-only write of a new generation object. Fails with `GenerationExists` when the id was already written by
    /// anyone.
    fn put_generation(&mut self, generation: ManifestGeneration) -> Result<(), PublishError>;
    /// If-Match CAS advance of the head pointer from `expected` to `next`. A lost race returns `CasLost {
    /// current_generation }`.
    fn advance_head(&mut self, expected: u64, next: u64) -> Result<(), PublishError>;
    /// Reads one published generation object by id.
    fn generation(&self, id: u64) -> Result<ManifestGeneration, PublishError>;
}
