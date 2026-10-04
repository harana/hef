//! The real, production versions of the engine's outside-world interfaces that need nothing beyond the operating
//! system: the actual system clock and the thread-pool encode executor.
//!
//! `SystemClock` reads the OS clocks. Production journal storage on disk is a [`JournalStorage`](super::JournalStorage)
//! implementation over a real block store, supplied by the application that deploys HEF, so the engine's state machines
//! stay synchronous and deterministic (design D4) while that store owns the asynchronous edge internally.

use super::constant::{JITTER_XORSHIFT_SHIFT_1, JITTER_XORSHIFT_SHIFT_2, JITTER_XORSHIFT_SHIFT_3};
use super::{Clock, MonotonicClock};
use rayon::prelude::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// OS-backed clock. Jitter uses a per-instance xorshift seeded from the clock itself; production draws need no
/// cryptographic strength, only desynchronisation.
#[derive(Debug)]
pub struct SystemClock {
    rng_state: AtomicU64,
    start: std::time::Instant,
}

impl SystemClock {
    /// A clock reading the OS clocks, with its jitter source seeded from the current time.
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self {
            start: std::time::Instant::now(),
            rng_state: AtomicU64::new(seed | 1),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_nanos(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
            .unwrap_or(0)
    }
}

impl MonotonicClock for SystemClock {
    fn monotonic_nanos(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_nanos()).unwrap_or(u64::MAX)
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

/// The production executor: fans the jobs out across the process-wide rayon pool, which holds one worker per core
/// however many builds are running. Job index is the only ordering contract, so the output of a build driven through
/// this pool is byte-identical to the sequential simulation executor's.
///
/// The pool is shared rather than per-call: a build calls this several times, and spawning a fresh set of OS threads
/// each time made every call pay thread creation and let concurrent builds each claim a whole machine's worth of
/// threads.
#[derive(Debug, Default)]
pub struct ThreadPoolEncodeExecutor;

impl super::EncodeExecutor for ThreadPoolEncodeExecutor {
    fn parallelism(&self) -> usize {
        rayon::current_num_threads().max(1)
    }

    fn run_jobs(&self, job_count: usize, job: &(dyn Fn(usize) + Sync)) {
        (0..job_count).into_par_iter().for_each(job);
    }
}
