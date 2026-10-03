//! Constant-time rank and select over a compressed run-list bitmap.
//!
//! `RankSelect` wraps a `RoaringRangeBitmap` with one precomputed cumulative-count array so each query costs a single
//! binary search over the (typically small) run list — never a linear scan over individual rows.
//!
//! - **rank(pos)**: how many set bits appear before position `pos`.
//! - **select(k)**: physical position of the k-th set bit (0-indexed).
//!
//! Both answers are exact and identical to what a full materialisation of the bitmap into a bit-vector and a naive scan
//! would produce. The structure is derivable from and always consistent with the bitmap it indexes.
//!
//! The scan uses this to translate filtered logical positions into physical row offsets for late-materialisation
//! gather, and to apply the deletion-vector anti-join as bitmap operations rather than a row-by-row walk.

use super::bitmap::{RoaringRangeBitmap, RowRange};

/// Rank and select acceleration over a `RoaringRangeBitmap`.
///
/// Built once from a bitmap with `from_bitmap`; each query (`rank`, `select`) then costs one binary search over the
/// precomputed cumulative-count array. The structure is exact: it is derivable from the bitmap and every answer equals
/// what a full materialisation would produce.
#[derive(Debug, Clone)]
pub struct RankSelect {
    /// `cumulative[i]` = total set rows in `runs[0..i]`.
    cumulative: Vec<u64>,
    runs: Vec<RowRange>,
    total: u64,
}

impl RankSelect {
    /// Builds the rank/select index from a bitmap. Precomputes one cumulative-count entry per run; subsequent queries
    /// cost O(log runs).
    pub fn from_bitmap(bitmap: &RoaringRangeBitmap) -> Self {
        let runs = bitmap.ranges().to_vec();
        let mut cumulative = Vec::with_capacity(runs.len());
        let mut acc = 0u64;
        for run in &runs {
            cumulative.push(acc);
            acc += run.end.saturating_sub(run.start);
        }
        Self {
            cumulative,
            runs,
            total: acc,
        }
    }

    /// Builds the rank/select index straight from a packed LSB-0 presence bitmap, deriving the run list from the byte
    /// scan ([`RoaringRangeBitmap::from_packed_bits`]) without ever materializing one row id per set bit. Every answer
    /// is identical to building over `RoaringRangeBitmap::from_rows(<every set row>)` of the same bits — the raw path
    /// stays the equivalence oracle in tests.
    pub fn from_packed_bits(bits: &[u8]) -> Self {
        Self::from_bitmap(&RoaringRangeBitmap::from_packed_bits(bits))
    }

    /// How many set bits (live or non-null rows) appear strictly before `position`. Returns 0 when `position` is 0 or
    /// the bitmap is empty.
    ///
    /// Exact: identical to counting set bits in `[0, position)` over the materialized bitmap.
    pub fn rank(&self, position: u64) -> u64 {
        if position == 0 || self.runs.is_empty() {
            return 0;
        }
        // Runs whose end <= position are entirely before position.
        let full_idx = self.runs.partition_point(|r| r.end <= position);
        let full_count = self.cumulative.get(full_idx).copied().unwrap_or(self.total);
        // A partial overlap: runs[full_idx] may start before position.
        let partial = self
            .runs
            .get(full_idx)
            .filter(|r| r.start < position)
            .map_or(0, |r| position - r.start);
        full_count + partial
    }

    /// Physical position of the `k`-th set bit (0-indexed). Returns `None` when `k >= total`.
    ///
    /// Exact: same position a linear scan of the materialised bitmap returns.
    pub fn select(&self, k: u64) -> Option<u64> {
        if k >= self.total {
            return None;
        }
        // Find the last run whose cumulative prefix count is <= k; the k-th bit lives in that run.
        let idx = self.cumulative.partition_point(|&c| c <= k);
        let run_idx = idx.saturating_sub(1);
        let run = self.runs.get(run_idx)?;
        let offset = k - self.cumulative.get(run_idx).copied().unwrap_or(0);
        Some(run.start + offset)
    }

    /// Total number of set bits across all runs.
    pub fn total(&self) -> u64 {
        self.total
    }

    /// `(rank(position), contains(position))` from one binary search instead of two: how many set bits appear
    /// strictly before `position`, together with whether `position` itself is set. Exact and identical to calling
    /// [`Self::rank`] at `position` and `position + 1` and comparing — the answer a caller needing both from a single
    /// bit position would otherwise pay two searches for.
    pub fn rank_and_contains(&self, position: u64) -> (u64, bool) {
        if self.runs.is_empty() {
            return (0, false);
        }
        let full_idx = self.runs.partition_point(|r| r.end <= position);
        let full_count = self.cumulative.get(full_idx).copied().unwrap_or(self.total);
        let run = self.runs.get(full_idx);
        let contains = run.is_some_and(|r| r.start <= position);
        let partial = run.filter(|r| r.start < position).map_or(0, |r| position - r.start);
        (full_count + partial, contains)
    }
}

#[cfg(test)]
#[path = "test/rank_select.rs"]
mod tests;
