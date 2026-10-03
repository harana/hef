//! Learned position index: a compact piecewise linear model that maps a sorted key to its approximate row position.
//!
//! Instead of probing a bucketed range filter or scanning a whole granule, a reader evaluates the model to land near
//! the target row and then confirms the exact boundary with a bounded local search inside the error window. The cost is
//! one model evaluation plus a small local scan, not a full-granule decode.
//!
//! The index is optional and only valid where a sortedness proof covers the column it models; without such a proof the
//! reader falls back to the bucketed range filter or a plain scan.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::layout::footer::SortDirection;

/// Format tag at the front of an encoded [`LearnedPositionIndex`], so a decoder rejects bytes that are not one.
const LEARNED_POSITION_MAGIC: u32 = 0x4C50_5831; // "LPX1"

/// One linear segment of the piecewise model.
///
/// Maps keys in the range `[key_lo, next_segment.key_lo)` to approximate row positions via `intercept + slope * (key -
/// key_lo)`, clamped to `[0, row_count)`.
#[derive(Clone, Debug, PartialEq)]
struct Segment {
    intercept: f64,
    key_lo: i64,
    slope: f64,
}

/// A compact piecewise linear model mapping a sorted key to its approximate row position, with a recorded maximum error
/// bound.
///
/// Given a range predicate `key >= lo AND key <= hi`, the reader evaluates the model for `lo` to find an approximate
/// starting row, then confirms the exact boundary by a bounded local search within `±error_bound` rows of that
/// estimate. Positions are exact after confirmation.
///
/// Built over one sorted projection of one column. Only valid when a `SortednessProof` in the file's footer covers the
/// assumed key order.
#[derive(Clone, Debug, PartialEq)]
pub struct LearnedPositionIndex {
    /// The column whose sorted values this model covers.
    pub column_id: u32,
    /// Sort direction of the covered key.
    pub direction: SortDirection,
    /// Maximum error: the true row position is within `±error_bound` rows of the model's estimate, for present and
    /// absent query keys alike. One row wider than the bound the model was fitted with: the fit guarantee holds at
    /// the fitted keys, and a query key falling between two fitted keys can land one further row away.
    pub error_bound: u64,
    /// The projection this index is built for.
    pub projection_id: u32,
    row_count: u64,
    segments: Vec<Segment>,
}

impl LearnedPositionIndex {
    /// Builds a learned position index from a sorted ascending key column.
    ///
    /// Fits a piecewise linear model to the `(key, row_position)` pairs so that every fitted key's position estimate
    /// is within `error_bound` rows of the true position. The stored `error_bound` is one row wider than the fit's:
    /// the insertion point of a query key absent from the fitted set can sit one row past the fit guarantee (as in a
    /// PGM index), so the extra slack keeps every range boundary inside the confirmation window. Returns an index
    /// ready to answer range seek queries.
    pub fn build(column_id: u32, projection_id: u32, keys: &[i64], error_bound: u64) -> Self {
        let row_count = keys.len() as u64;
        let segments = fit_segments(keys, error_bound);
        Self {
            column_id,
            direction: SortDirection::Ascending,
            error_bound: error_bound.saturating_add(1),
            projection_id,
            row_count,
            segments,
        }
    }

    /// Estimates the row position of `key` and returns the window within which the exact boundary must lie.
    ///
    /// The window `[search_from, search_to]` is guaranteed to contain the true boundary row for `key` — the first row
    /// with key ≥ `key` — whether `key` itself is stored or falls between stored keys, so a reader scanning that
    /// window will find the exact boundary before reading any column data.
    pub fn estimate_position(&self, key: i64) -> PositionWindow {
        let approx = self.model_eval(key);
        let search_from = approx.saturating_sub(self.error_bound);
        let search_to = approx
            .saturating_add(self.error_bound)
            .min(self.row_count.saturating_sub(1));
        PositionWindow {
            approximate_row: approx,
            search_from,
            search_to,
        }
    }

    /// Serializes the index to bytes (magic, identity fields, then the segment list, floats stored as their exact IEEE
    /// bits). Pairs with [`LearnedPositionIndex::decode`] and round-trips exactly.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(40 + self.segments.len() * 24);
        out.put_u32(LEARNED_POSITION_MAGIC);
        out.put_u32(self.column_id);
        out.put_u32(self.projection_id);
        out.put_u8(match self.direction {
            SortDirection::Ascending => 0,
            SortDirection::Descending => 1,
        });
        out.put_u64(self.error_bound);
        out.put_u64(self.row_count);
        out.put_u32(self.segments.len() as u32);
        for segment in &self.segments {
            out.put_u64(segment.intercept.to_bits());
            out.put_i64(segment.key_lo);
            out.put_u64(segment.slope.to_bits());
        }
        out.into_bytes()
    }

    /// Rebuilds an index from [`LearnedPositionIndex::encode`] output. Refuses on a wrong tag, an unknown sort
    /// direction, truncation, or segment keys out of ascending order (which would break the segment binary search).
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("learned position magic")? != LEARNED_POSITION_MAGIC {
            return Err(FormatError::Structural {
                rule: "learned position index bad magic",
            });
        }
        let column_id = reader.u32("learned position column")?;
        let projection_id = reader.u32("learned position projection")?;
        let direction = match reader.u8("learned position direction")? {
            0 => SortDirection::Ascending,
            1 => SortDirection::Descending,
            _ => {
                return Err(FormatError::Structural {
                    rule: "learned position unknown sort direction",
                });
            }
        };
        let error_bound = reader.u64("learned position error bound")?;
        let row_count = reader.u64("learned position row count")?;
        let segment_count = reader.u32("learned position segment count")? as usize;
        let mut segments = Vec::with_capacity(reader.capacity_hint(segment_count, 24));
        for _ in 0..segment_count {
            let intercept = f64::from_bits(reader.u64("learned position intercept")?);
            let key_lo = reader.i64("learned position key")?;
            let slope = f64::from_bits(reader.u64("learned position slope")?);
            if segments
                .last()
                .is_some_and(|previous: &Segment| key_lo <= previous.key_lo)
            {
                return Err(FormatError::Structural {
                    rule: "learned position segment keys must be strictly ascending",
                });
            }
            segments.push(Segment {
                intercept,
                key_lo,
                slope,
            });
        }
        Ok(Self {
            column_id,
            direction,
            error_bound,
            projection_id,
            row_count,
            segments,
        })
    }

    fn model_eval(&self, key: i64) -> u64 {
        // partition_point on `key_lo < key` lands on the *first* segment whose key_lo == key, so a duplicated key
        // that spans multiple segments resolves to the earliest one, not the last.
        let i = self.segments.partition_point(|s| s.key_lo < key);
        let seg_index = if i < self.segments.len() && self.segments[i].key_lo == key {
            i
        } else if i == 0 {
            return 0;
        } else {
            i - 1
        };
        let Some(seg) = self.segments.get(seg_index) else {
            return 0;
        };
        // Widened to i128 before the subtraction: keys spanning the whole signed 64-bit domain (i64::MIN to i64::MAX)
        // overflow an i64 difference, and this crate aborts on overflow.
        let mut pos = seg.intercept + seg.slope * (key as i128 - seg.key_lo as i128) as f64;
        // A key past the segment's own fitted keys extrapolates its line; every row from the next segment's first key
        // onward is covered by that segment's exact intercept, so cap the estimate there — otherwise a steep segment
        // overshoots the gap between segments and the window misses the true boundary sitting below it.
        if let Some(next) = self.segments.get(seg_index + 1) {
            pos = pos.min(next.intercept);
        }
        (pos.round() as i64).max(0).min(self.row_count.saturating_sub(1) as i64) as u64
    }
}

/// The approximate row position and the local search window the reader must scan to confirm the exact range boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionWindow {
    /// The row the model estimated as the match position.
    pub approximate_row: u64,
    /// First row of the confirmation window (inclusive, clamped to the row count).
    pub search_from: u64,
    /// Last row of the confirmation window (inclusive).
    pub search_to: u64,
}

/// Validates that a learned seek is available for the given column and proof state.
///
/// Returns `Some(LearnedSeek)` when the index covers the column and a sortedness proof is available. Returns `None`
/// when either condition is missing — the reader must fall back to the bucketed range filter or a plain scan.
pub fn plan_learned_seek(index: &LearnedPositionIndex, proof_present: bool, column_id: u32) -> Option<LearnedSeek<'_>> {
    if !proof_present || index.column_id != column_id {
        return None;
    }
    Some(LearnedSeek { index })
}

/// A validated, ready-to-use learned range seek for a sorted column.
///
/// Obtained from `plan_learned_seek` after confirming that a sortedness proof covers the assumed key order. Call
/// `seek_range_start` to get the row window for local confirmation.
pub struct LearnedSeek<'a> {
    index: &'a LearnedPositionIndex,
}

impl<'a> LearnedSeek<'a> {
    /// Returns the row window to search for the exact start of `key_lo`.
    ///
    /// The window is bounded by `±error_bound` rows around the model's estimate, so the scan cost is proportional to
    /// the error budget, not the whole granule.
    pub fn seek_range_start(&self, key_lo: i64) -> PositionWindow {
        self.index.estimate_position(key_lo)
    }
}

/// Fits a minimal piecewise linear model whose maximum per-point error is at most `error_bound` rows, using a greedy
/// cone-intersection algorithm.
///
/// For each segment the algorithm maintains the range of slopes `[slope_lo, slope_hi]` consistent with all points seen
/// so far staying within the error budget. When a new point would make that range empty, a new segment starts.
fn fit_segments(keys: &[i64], error_bound: u64) -> Vec<Segment> {
    if keys.is_empty() {
        return Vec::new();
    }

    let error = error_bound as f64;
    let mut segments: Vec<Segment> = Vec::new();
    let mut seg_start = 0usize;

    while let Some(&key0) = keys.get(seg_start) {
        let pos0 = seg_start as f64;

        let mut slope_lo = f64::NEG_INFINITY;
        let mut slope_hi = f64::INFINITY;
        let mut chosen_slope = 0.0f64;
        let mut seg_end = seg_start;

        for (i, &key) in keys.iter().enumerate().skip(seg_start + 1) {
            let dk = (key as i128 - key0 as i128) as f64;

            if dk == 0.0 {
                // Duplicate key at a different row: only fits if the row distance from pos0 stays within budget (slope
                // is 0 here).
                if (pos0 - i as f64).abs() > error {
                    break;
                }
                seg_end = i;
                continue;
            }

            // Slope range for point i to stay within error_bound of pos_i.
            let s_lo = (i as f64 - error - pos0) / dk;
            let s_hi = (i as f64 + error - pos0) / dk;
            let new_lo = slope_lo.max(s_lo);
            let new_hi = slope_hi.min(s_hi);

            if new_lo > new_hi {
                break;
            }

            slope_lo = new_lo;
            slope_hi = new_hi;
            chosen_slope = (slope_lo + slope_hi) / 2.0;
            seg_end = i;
        }

        segments.push(Segment {
            intercept: pos0,
            key_lo: key0,
            slope: chosen_slope,
        });
        seg_start = seg_end + 1;
    }

    segments
}

#[cfg(test)]
#[path = "test/learned_position.rs"]
mod tests;
