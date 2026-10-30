//! The top-k scan: scores every row of one vector block against a query and keeps the best `k`, skipping rows the
//! filter rules out while it scans.

use super::model::{ScoredRow, VectorColumnMeta, VectorElement, VectorMetric};
use super::quantize::{binarize, decode_row, trailer_pair};
use crate::indexes::bitmap::RoaringRangeBitmap;
use multiversion::multiversion;
use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// A query prepared once for one column's element type, so each row costs one kernel call.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone)]
pub struct PreparedQuery {
    bits: Vec<u8>,
    metric: VectorMetric,
    sum: f32,
    values: Vec<f32>,
}

impl PreparedQuery {
    /// Prepares `query` for scoring under `metric`.
    pub fn new(query: &[f32], metric: VectorMetric) -> Self {
        PreparedQuery {
            bits: binarize(query),
            metric,
            sum: query.iter().sum(),
            values: query.to_vec(),
        }
    }

    /// Scores one stored row of `element` type; higher is closer. 1-bit rows score as `dimension − Hamming distance`.
    pub fn score(&self, element: VectorElement, row: &[u8]) -> f32 {
        let dimension = self.values.len();
        match element {
            VectorElement::U1 => {
                let bits = row.get(..self.bits.len()).unwrap_or(row);
                dimension as f32 - hamming(bits, &self.bits) as f32
            }
            VectorElement::I8 if self.metric != VectorMetric::L2 => {
                let (values, trailer) = row.split_at(dimension.min(row.len()));
                let (scale, offset) = trailer_pair(trailer);
                scale * dot_i8(values, &self.values) + offset * self.sum
            }
            VectorElement::F32 if self.metric != VectorMetric::L2 => row
                .chunks_exact(4)
                .zip(&self.values)
                .map(|(bytes, value)| f32::from_le_bytes(bytes.try_into().unwrap_or([0; 4])) * value)
                .sum(),
            _ => {
                let decoded = decode_row(element, dimension as u32, row);
                match self.metric {
                    VectorMetric::L2 => -decoded
                        .iter()
                        .zip(&self.values)
                        .map(|(stored, value)| (stored - value) * (stored - value))
                        .sum::<f32>(),
                    VectorMetric::Cosine | VectorMetric::Dot => decoded
                        .iter()
                        .zip(&self.values)
                        .map(|(stored, value)| stored * value)
                        .sum(),
                }
            }
        }
    }
}

/// Scores every row of `rows` (one block of `meta`'s column, whose first row is file row `first_row`) and returns the
/// best `k`, best first; ties go to the lower row. Rows outside `filter`, when one is given, are never scored or
/// returned. Asking for more than the matching rows returns all of them.
pub fn top_k(
    meta: &VectorColumnMeta,
    rows: &[u8],
    first_row: u64,
    query: &PreparedQuery,
    filter: Option<&RoaringRangeBitmap>,
    k: usize,
) -> Vec<ScoredRow> {
    let mut best = TopK::new(k);
    let width = meta.row_width();
    if width == 0 {
        return Vec::new();
    }
    let block_rows = (rows.len() / width) as u64;
    let mut score_range = |start: u64, end: u64| {
        let bytes = rows
            .get(start as usize * width..end as usize * width)
            .unwrap_or_default();
        for (index, row) in bytes.chunks_exact(width).enumerate() {
            best.offer(ScoredRow {
                row: first_row + start + index as u64,
                score: query.score(meta.element, row),
            });
        }
    };
    match filter {
        None => score_range(0, block_rows),
        Some(filter) => {
            // Walk only the filter's runs that overlap this block, so filtered-out rows cost nothing.
            for range in filter.ranges() {
                let start = range.start.max(first_row);
                let end = range.end.min(first_row + block_rows);
                if start < end {
                    score_range(start - first_row, end - first_row);
                }
            }
        }
    }
    best.into_sorted()
}

#[multiversion(targets(
    "x86_64+avx512f+avx512bw+avx512dq+avx512vl+avx512vpopcntdq",
    "x86_64+avx2+popcnt",
    "aarch64+neon"
))]
fn hamming(left: &[u8], right: &[u8]) -> u32 {
    left.chunks(8)
        .zip(right.chunks(8))
        .map(|(left, right)| {
            let mut a = [0u8; 8];
            let mut b = [0u8; 8];
            a.iter_mut().zip(left).for_each(|(slot, byte)| *slot = *byte);
            b.iter_mut().zip(right).for_each(|(slot, byte)| *slot = *byte);
            (u64::from_le_bytes(a) ^ u64::from_le_bytes(b)).count_ones()
        })
        .sum()
}

#[multiversion(targets("x86_64+avx512f+avx512bw+avx512dq+avx512vl", "x86_64+avx2+fma", "aarch64+neon"))]
fn dot_i8(stored: &[u8], query: &[f32]) -> f32 {
    // Eight independent sums let the compiler keep one vector register per lane instead of a serial add chain.
    let mut lanes = [0f32; 8];
    for (stored, query) in stored.chunks(8).zip(query.chunks(8)) {
        for ((lane, q), value) in lanes.iter_mut().zip(stored).zip(query) {
            *lane += f32::from(*q as i8) * value;
        }
    }
    lanes.iter().sum()
}

/// Keeps the best `k` rows offered so far.
#[derive(Debug)]
pub struct TopK {
    heap: BinaryHeap<Ranked>,
    k: usize,
}

impl TopK {
    /// An empty collector that keeps at most `k` rows.
    pub fn new(k: usize) -> Self {
        TopK {
            heap: BinaryHeap::with_capacity(k.saturating_add(1).min(1 << 16)),
            k,
        }
    }

    /// Offers one scored row; it is kept only if it is among the best `k` so far.
    pub fn offer(&mut self, row: ScoredRow) {
        if self.k == 0 {
            return;
        }
        let ranked = Ranked(row);
        if self.heap.len() < self.k {
            self.heap.push(ranked);
        } else if self.heap.peek().is_some_and(|worst| ranked < *worst) {
            self.heap.pop();
            self.heap.push(ranked);
        }
    }

    /// The kept rows, best first.
    pub fn into_sorted(self) -> Vec<ScoredRow> {
        self.heap.into_sorted_vec().into_iter().map(|ranked| ranked.0).collect()
    }
}

/// Orders rows best first: a higher score, then a lower row, is "less", so the heap's maximum is the worst kept row.
#[derive(Debug)]
struct Ranked(ScoredRow);

impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .0
            .score
            .total_cmp(&self.0.score)
            .then(self.0.row.cmp(&other.0.row))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ranked {}
