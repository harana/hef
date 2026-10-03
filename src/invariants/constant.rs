//! Shared constants for the clock interfaces: the xorshift jitter shifts both `io::SystemClock` and `sim::SimClock`
//! draw from, and the fixed wall-clock start `sim::SimClock` boots at.

/// xorshift64 shift triple (Marsaglia's classic 13/7/17 variant).
pub const JITTER_XORSHIFT_SHIFT_1: u32 = 13;
pub const JITTER_XORSHIFT_SHIFT_2: u32 = 7;
pub const JITTER_XORSHIFT_SHIFT_3: u32 = 17;

/// `SimClock`'s fixed starting wall-clock time (2023-11-14T22:13:20Z) — arbitrary but reproducible.
pub const SIM_CLOCK_START_NANOS: i64 = 1_700_000_000_000_000_000;
