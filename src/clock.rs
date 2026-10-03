//! How HEF reads the current time, and the one test clock that stands in for it.
//!
//! The journal stamps frames, the write pipeline decides when to flush, and the simulation scripts time-dependent
//! faults, all through [`Clock`] rather than the operating system directly, so a test can hand in a [`FixedClock`] and
//! check time-dependent behaviour without waiting for the real clock to move.
//!
//! See: hef-core-invariants/spec.md

use chrono::{DateTime, Utc};
use std::sync::atomic::{AtomicI64, Ordering};

/// Reads the current wall-clock time.
///
/// Anything that needs to stamp a row, expire a session or claim, or measure how long a piece of work took reads the
/// time through this trait rather than calling the operating system directly. That one indirection lets a test hand the
/// code a fixed or scripted time, so time-dependent behaviour can be checked without waiting for the real clock to
/// move.
pub trait Clock: Send + Sync {
    /// The current time in nanoseconds since the Unix epoch (UTC).
    ///
    /// Reach for this when comparing against another nanosecond value or measuring an elapsed duration — a lease,
    /// a session expiry, a rate-limit window. For stamping a persisted row's timestamp column, use `now()` instead.
    fn now_nanos(&self) -> i64;

    /// The current time, ready to stamp a persisted row.
    ///
    /// Nanosecond counts outside chrono's representable range (roughly ±262 years from 1970) would panic here, but
    /// nothing in this codebase produces one — every `now_nanos` value comes from the OS clock or from `FixedClock`
    /// seeded with a realistic value.
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_nanos(self.now_nanos())
    }
}

/// A test clock that only moves when the test moves it.
///
/// Shareable across threads, so it can be handed to code whose futures have to be `Send`.
///
/// Hand this to any code that reads time through `Clock` and the time it sees stays frozen at whatever instant you set,
/// so a test can check time-dependent behaviour without waiting for the real clock — call `advance` to jump it forward
/// by a fixed amount. Every service shares this one stand-in rather than writing its own.
#[derive(Debug)]
pub struct FixedClock {
    now_nanos: AtomicI64,
}

impl FixedClock {
    /// A clock frozen at `now_nanos` (nanoseconds since the Unix epoch) until the test moves it.
    pub fn at(now_nanos: i64) -> Self {
        Self {
            now_nanos: AtomicI64::new(now_nanos),
        }
    }

    /// Moves the clock forward by `nanos`.
    pub fn advance(&self, nanos: i64) {
        self.now_nanos
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |now| {
                Some(now.saturating_add(nanos))
            })
            .expect("closure always returns Some");
    }
}

impl Clock for FixedClock {
    fn now_nanos(&self) -> i64 {
        self.now_nanos.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[path = "test/clock.rs"]
mod tests;
