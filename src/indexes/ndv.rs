//! Counts how many distinct values a column holds — exactly while the count is small, and as a HyperLogLog-class
//! estimate beyond that — in one pass over immutable input, with no sampling machinery.
//!
//! The build computes one sketch per promoted column per stripe and records the result in the footer, so the planner
//! can estimate join and grouping cardinalities without scanning a column. Sketches merge (per-column, across
//! granules of a stripe) and are deterministic: the same values always produce the same count on any node. Only
//! counts leave the sketch — no value bytes are ever persisted, so nothing here crosses the per-subject encryption or
//! public-output boundaries.
//!
//! See: hef-aggregation-metadata/spec.md

use crate::encoding::ColumnData;
use hashbrown::HashSet;

/// Distinct values counted exactly before a sketch degrades to its HyperLogLog estimate.
pub const NDV_EXACT_MAX: usize = 1024;

/// HyperLogLog register count (2^8): at 256 registers the standard error is about 6.5%, comfortably inside what a
/// planner's cardinality estimate needs, for 256 bytes per (column, stripe).
const NDV_REGISTERS: usize = 256;

/// One column's distinct-count accumulator over some rows: exact while at most [`NDV_EXACT_MAX`] distinct values have
/// been seen, HyperLogLog beyond. The registers always accumulate alongside the exact set, so degrading loses no
/// history and merging two sketches in either mode stays correct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NdvSketch {
    exact: Option<HashSet<u64>>,
    registers: [u8; NDV_REGISTERS],
}

impl NdvSketch {
    pub fn new() -> Self {
        Self {
            exact: Some(HashSet::new()),
            registers: [0u8; NDV_REGISTERS],
        }
    }

    /// Folds one value's 64-bit hash into the sketch.
    pub fn observe_hash(&mut self, hash: u64) {
        if let Some(exact) = &mut self.exact {
            exact.insert(hash);
            if exact.len() > NDV_EXACT_MAX {
                self.exact = None;
            }
        }
        let register = (hash >> 56) as usize;
        let rank = ((hash << 8) | 1).leading_zeros() as u8 + 1;
        if let Some(slot) = self.registers.get_mut(register)
            && *slot < rank
        {
            *slot = rank;
        }
    }

    /// Folds every value of a decoded block into the sketch, hashing each value's canonical bytes. Null strings are
    /// not values and are skipped.
    pub fn observe_column(&mut self, data: &ColumnData) {
        match data {
            ColumnData::U64(values) => {
                for value in values {
                    self.observe_hash(mix(*value));
                }
            }
            ColumnData::I64(values) => {
                for value in values {
                    self.observe_hash(mix(*value as u64));
                }
            }
            ColumnData::F64(values) => {
                for value in values {
                    self.observe_hash(mix(value.to_bits()));
                }
            }
            ColumnData::U128(values) => {
                for value in values {
                    self.observe_hash(mix((*value as u64) ^ mix((*value >> 64) as u64)));
                }
            }
            ColumnData::Decimal { values, .. } => {
                for value in values {
                    self.observe_hash(mix((*value as u64) ^ mix((*value >> 64) as u64)));
                }
            }
            ColumnData::Strings(values) => {
                for value in values.iter().flatten() {
                    self.observe_hash(hash_bytes(value.as_bytes()));
                }
            }
        }
    }

    /// Folds another sketch of the same column into this one. Exactness survives only while both sides are exact and
    /// the union stays within [`NDV_EXACT_MAX`]; registers merge by maximum either way, so the estimate never loses
    /// values.
    pub fn merge(&mut self, other: &NdvSketch) {
        match (&mut self.exact, &other.exact) {
            (Some(mine), Some(theirs)) => {
                mine.extend(theirs.iter().copied());
                if mine.len() > NDV_EXACT_MAX {
                    self.exact = None;
                }
            }
            _ => self.exact = None,
        }
        for (mine, theirs) in self.registers.iter_mut().zip(other.registers.iter()) {
            *mine = (*mine).max(*theirs);
        }
    }

    /// The distinct count and whether it is exact: the true count while the sketch never left exact mode, the
    /// HyperLogLog estimate (with the standard small-range correction) after.
    pub fn distinct(&self) -> (u64, bool) {
        if let Some(exact) = &self.exact {
            return (exact.len() as u64, true);
        }
        let m = NDV_REGISTERS as f64;
        let sum: f64 = self.registers.iter().map(|r| 2.0f64.powi(-i32::from(*r))).sum();
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let mut estimate = alpha * m * m / sum;
        let zeros = self.registers.iter().filter(|r| **r == 0).count();
        if estimate <= 2.5 * m && zeros > 0 {
            estimate = m * (m / zeros as f64).ln();
        }
        (estimate.round() as u64, false)
    }
}

impl Default for NdvSketch {
    fn default() -> Self {
        Self::new()
    }
}

/// SplitMix64's finalizer: a cheap, deterministic 64-bit avalanche so nearby integer values land in unrelated
/// registers.
fn mix(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9E37_79B9_7F4A_7C15);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^ (value >> 31)
}

/// Hashes the bytes for register-quality avalanche. Reuses [`super::stable_hash`]'s chunked FNV-1a-then-SplitMix64
/// rather than a byte-at-a-time fold: the sketch's hash is never persisted (only the distinct count is), so it carries
/// no format commitment and is free to share the faster implementation.
fn hash_bytes(bytes: &[u8]) -> u64 {
    super::stable_hash(bytes)
}

#[cfg(test)]
#[path = "test/ndv.rs"]
mod tests;
