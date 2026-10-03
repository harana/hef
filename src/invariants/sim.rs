//! The test versions of the engine's outside-world interfaces: a clock the test advances by hand and storage that can
//! be made to fail on command.
//!
//! Everything here is seed-reproducible and fault-injectable without touching production code paths: tests drive
//! `SimClock` time explicitly, script storage faults (torn tails, failed appends/syncs, crash points, bit rot) into
//! `SimJournalStorage`, and race publishers against `SimulatedPublishedSet`'s real compare-and-swap behaviour.
//!
//! `SimJournalStorage` delegates to the shared file layer's simulation block store, so the journal and the rest of the
//! store share one fault-injecting in-memory backend, and the simulation owns no kernel runtime.

use super::constant::{
    JITTER_XORSHIFT_SHIFT_1, JITTER_XORSHIFT_SHIFT_2, JITTER_XORSHIFT_SHIFT_3, SIM_CLOCK_START_NANOS,
};
use super::{Clock, JournalStorage, MonotonicClock, PublishedSet, ShardAtomicity, ShardId};
use crate::clock::FixedClock;
use crate::error::{PublishError, StorageError};
use crate::file::api::BlockStore;
use crate::file::model::BlockTarget;
use crate::file::sim::{Fault as BlockFault, SimBlockStore};
use crate::lifecycle::ManifestGeneration;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

/// Deterministic clock: time advances only when the test advances it, and jitter comes from a seeded xorshift so a run
/// is reproducible from its seed.
///
/// Wall-clock time is the shared [`FixedClock`]; this type adds only the monotonic counter and seeded jitter the
/// storage engine needs on top.
#[derive(Debug)]
pub struct SimClock {
    auto_advance_nanos: AtomicU64,
    monotonic_nanos: AtomicU64,
    now: FixedClock,
    rng_state: AtomicU64,
}

impl SimClock {
    /// A clock fixed at a starting time, with jitter driven by `seed` so a run is reproducible. Time moves only when
    /// the test advances it.
    pub fn new(seed: u64) -> Self {
        Self {
            auto_advance_nanos: AtomicU64::new(0),
            monotonic_nanos: AtomicU64::new(0),
            now: FixedClock::at(SIM_CLOCK_START_NANOS),
            // xorshift64 must not start at zero.
            rng_state: AtomicU64::new(seed | 1),
        }
    }

    /// Advances both wall and monotonic time.
    pub fn advance(&self, nanos: u64) {
        self.now.advance(nanos as i64);
        self.monotonic_nanos
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
                Some(now.saturating_add(nanos))
            })
            .expect("closure always returns Some");
    }

    /// Makes every later reading of the clock also move it forward by `nanos`, so a test can model time passing
    /// *inside* a call it has no way to interrupt — a publication that stalls halfway, say. `0` turns it off again,
    /// which is how a fresh clock starts.
    pub fn auto_advance(&self, nanos: u64) {
        self.auto_advance_nanos.store(nanos, Ordering::Relaxed);
    }
}

impl Clock for SimClock {
    fn now_nanos(&self) -> i64 {
        let now = self.now.now_nanos();
        self.advance(self.auto_advance_nanos.load(Ordering::Relaxed));
        now
    }
}

impl MonotonicClock for SimClock {
    fn monotonic_nanos(&self) -> u64 {
        self.monotonic_nanos.load(Ordering::Relaxed)
    }

    fn jitter(&self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        let mut x = self.rng_state.load(Ordering::Relaxed);
        x ^= x << JITTER_XORSHIFT_SHIFT_1;
        x ^= x >> JITTER_XORSHIFT_SHIFT_2;
        x ^= x << JITTER_XORSHIFT_SHIFT_3;
        self.rng_state.store(x, Ordering::Relaxed);
        x % bound
    }
}

/// A scripted storage fault, consumed in injection order by the operation it names. Keyed by journal shard; mapped onto
/// the shared block store's own fault on injection.
#[derive(Debug, Clone)]
pub enum Fault {
    /// The next `append` on the shard fails before writing anything.
    FailAppend { shard: ShardId },
    /// The next `sync` on the shard fails; pending bytes stay pending.
    FailSync { shard: ShardId },
    /// At the next crash, the most recent pending append on the shard survives only as its first `keep_bytes` bytes (a
    /// torn tail).
    TornTail { keep_bytes: u32, shard: ShardId },
}

/// One journal shard is one append target in the shared file layer.
fn target_of(shard: ShardId) -> BlockTarget {
    BlockTarget(u64::from(shard.0))
}

impl Fault {
    fn into_block(self) -> BlockFault {
        match self {
            Fault::FailAppend { shard } => BlockFault::FailAppend {
                target: target_of(shard),
            },
            Fault::FailSync { shard } => BlockFault::FailSync {
                target: target_of(shard),
            },
            Fault::TornTail { keep_bytes, shard } => BlockFault::TornTail {
                keep_bytes,
                target: target_of(shard),
            },
        }
    }
}

/// In-memory journal storage with scripted fault injection, delegating to the shared file layer's simulation block
/// store. Production code sees only the `JournalStorage` trait; every fault arrives through the interface, never
/// through patched code.
#[derive(Debug, Default)]
pub struct SimJournalStorage {
    inner: SimBlockStore,
}

impl SimJournalStorage {
    /// Empty in-memory storage with no shards and no faults scripted yet.
    pub fn new() -> Self {
        Self {
            inner: SimBlockStore::new(),
        }
    }

    /// Overrides the probed-atomicity answer reported for every shard.
    pub fn with_atomicity(self, atomicity: ShardAtomicity) -> Self {
        Self {
            inner: self.inner.with_atomicity(atomicity),
        }
    }

    /// Scripts the next fault. Faults are consumed in order by the matching operation.
    pub fn inject(&mut self, fault: Fault) {
        self.inner.inject(fault.into_block());
    }

    /// Simulates power loss: pending (un-synced) appends are lost. An armed `TornTail` fault leaves a prefix of the
    /// newest pending append on media instead of dropping it cleanly.
    pub fn crash(&mut self) {
        self.inner.crash();
    }

    /// Simulates reordered completions at a crash: pending appends whose submission index is in `survivors` reach
    /// media, the rest are lost.
    pub fn crash_with_surviving_pending(&mut self, shard: ShardId, survivors: &BTreeSet<usize>) {
        self.inner.crash_with_surviving_pending(target_of(shard), survivors);
    }

    /// Flips one durable byte (bit rot). BLAKE3 must catch it on replay.
    pub fn corrupt_durable_byte(&mut self, shard: ShardId, offset: u64) {
        self.inner.corrupt_durable_byte(target_of(shard), offset);
    }
}

impl JournalStorage for SimJournalStorage {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, StorageError> {
        Ok(self.inner.append(target_of(shard), frame)?)
    }

    fn sync(&mut self, shard: ShardId) -> Result<(), StorageError> {
        Ok(self.inner.sync(target_of(shard))?)
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), StorageError> {
        Ok(self.inner.truncate(target_of(shard), len)?)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, StorageError> {
        Ok(self.inner.read(target_of(shard), offset, len)?)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, StorageError> {
        Ok(self.inner.extent(target_of(shard))?)
    }

    fn atomicity(&self, shard: ShardId) -> Result<ShardAtomicity, StorageError> {
        Ok(self.inner.atomicity(target_of(shard))?)
    }
}

/// In-memory `PublishedSet` with real conditional-write semantics: generation objects are create-only and the head
/// pointer advances by CAS, so a racing publisher genuinely loses and must rebase.
#[derive(Debug, Default)]
pub struct SimulatedPublishedSet {
    generations: BTreeMap<u64, ManifestGeneration>,
    head: u64,
}

impl SimulatedPublishedSet {
    /// Starts with an empty generation 0 as the published head.
    pub fn new() -> Self {
        let mut generations = BTreeMap::new();
        generations.insert(0, ManifestGeneration::default());
        Self { generations, head: 0 }
    }
}

impl PublishedSet for SimulatedPublishedSet {
    fn head(&self) -> Result<(u64, ManifestGeneration), PublishError> {
        let generation = self
            .generations
            .get(&self.head)
            .cloned()
            .ok_or(PublishError::UnknownGeneration)?;
        Ok((self.head, generation))
    }

    fn put_generation(&mut self, generation: ManifestGeneration) -> Result<(), PublishError> {
        if self.generations.contains_key(&generation.generation) {
            return Err(PublishError::GenerationExists);
        }
        self.generations.insert(generation.generation, generation);
        Ok(())
    }

    fn advance_head(&mut self, expected: u64, next: u64) -> Result<(), PublishError> {
        if !self.generations.contains_key(&next) {
            return Err(PublishError::UnknownGeneration);
        }
        if self.head != expected {
            return Err(PublishError::CasLost {
                current_generation: self.head,
            });
        }
        self.head = next;
        Ok(())
    }

    fn generation(&self, id: u64) -> Result<ManifestGeneration, PublishError> {
        self.generations
            .get(&id)
            .cloned()
            .ok_or(PublishError::UnknownGeneration)
    }
}

/// The deterministic-simulation executor: runs every job on the calling thread, in index order.
///
/// The simulation owns no threads, so a build driven through this executor is a plain sequential loop — and because
/// callers collect results by job index, its output is byte-identical to the production thread pool's.
#[derive(Debug, Default)]
pub struct SerialEncodeExecutor;

impl super::EncodeExecutor for SerialEncodeExecutor {
    fn parallelism(&self) -> usize {
        1
    }

    fn run_jobs(&self, job_count: usize, job: &(dyn Fn(usize) + Sync)) {
        for index in 0..job_count {
            job(index);
        }
    }
}
