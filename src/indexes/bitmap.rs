//! Exact bitmap indexes for columns that only ever hold a handful of distinct values — a `status`, a `country`, a
//! `source_id`, a boolean flag.
//!
//! For each such value the index keeps the set of rows that carry it, stored as a compressed run of `[start, end)` row
//! ranges rather than one bit per row. The key property is that two of these compressed sets can be intersected
//! *directly*, by walking both range lists at once, without ever expanding them back into individual row ids. That lets
//! a query AND together several low-cardinality conditions (e.g. `status = active AND country = US`) cheaply, and the
//! answer is exact: a row is in the result if and only if it truly matched every condition.
//!
//! A planner only takes this fast path when three things hold together: the encoding really is directly intersectable,
//! the file declares the bitmap feature, and the bitmap block's checksum verifies. [`BitmapBlockInfo`] and
//! [`can_use_bitmap`] capture that contract.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::indexes::Exactness;

/// The encoded size of one run: a `start` and an `end`, each a `u64`.
const ENCODED_ROW_RANGE_BYTES: usize = 16;

/// The encoded size of the run-count prefix at the front of an encoded [`RoaringRangeBitmap`]: a single `u32`.
const RUN_COUNT_PREFIX_BYTES: usize = 4;

/// The encoded size of one [`BitmapIndex`] entry's fixed header: the `u64` value and the `u32` length prefix of its
/// encoded bitmap block (the block bytes themselves are variable length and follow).
const INDEX_ENTRY_HEADER_BYTES: usize = 12;

/// The byte length of the BLAKE3 checksum recorded for a bitmap block.
const CHECKSUM_BYTES: usize = 32;

/// One half-open run of row ids, `[start, end)`, that all share a value. Runs inside a [`RoaringRangeBitmap`] are
/// sorted, never overlap, and never touch (an `end` equal to the next `start` is merged into one run), so a row id
/// appears in at most one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRange {
    pub end: u64,
    pub start: u64,
}

impl RowRange {
    fn is_empty(self) -> bool {
        self.end <= self.start
    }
}

/// A compressed set of row ids that supports intersection while staying compressed.
///
/// Rows are held as sorted, disjoint, non-adjacent `[start, end)` runs, which is compact when set rows cluster (the
/// usual case for a low-cardinality column whose rows arrive in bursts) and lets [`intersect`](Self::intersect) run a
/// two-pointer merge over the runs of both sides — never materializing individual row ids. The result equals the true
/// set intersection, and the representation is exact, so membership questions have no false positives and no false
/// negatives.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoaringRangeBitmap {
    runs: Vec<RowRange>,
}

impl RoaringRangeBitmap {
    /// The exactness of answers this bitmap gives: always [`Exactness::Exact`], because the representation stores the
    /// true set of rows with no approximation.
    pub fn exactness() -> Exactness {
        Exactness::Exact
    }

    /// Builds a bitmap from any sequence of row ids, in any order and with duplicates. Adjacent and overlapping rows
    /// collapse into runs, so the result is the canonical compressed form of the set.
    pub fn from_rows<I: IntoIterator<Item = u64>>(rows: I) -> Self {
        let mut sorted: Vec<u64> = rows.into_iter().collect();
        sorted.sort_unstable();
        sorted.dedup();
        let mut runs: Vec<RowRange> = Vec::new();
        for row in sorted {
            match runs.last_mut() {
                Some(last) if last.end == row => last.end = row + 1,
                _ => runs.push(RowRange {
                    start: row,
                    end: row + 1,
                }),
            }
        }
        Self { runs }
    }

    /// Builds a bitmap from a packed LSB-0 bit array (bit `i % 8` of byte `i / 8` is row `i`), emitting runs straight
    /// from the byte scan — no per-row id is ever materialized or sorted, unlike [`from_rows`](Self::from_rows) over
    /// the same bits. All-zero and all-one bytes advance a whole byte at a time, so a mostly-present or mostly-absent
    /// bitmap costs one branch per byte rather than one per row. The result is byte-identical to
    /// `Self::from_rows(<every set row>)`.
    pub fn from_packed_bits(bits: &[u8]) -> Self {
        let mut runs: Vec<RowRange> = Vec::new();
        // Start of the run currently being extended, still open at the scan position.
        let mut open: Option<u64> = None;
        for (byte_index, &byte) in bits.iter().enumerate() {
            let base = byte_index as u64 * 8;
            match byte {
                0x00 => {
                    if let Some(start) = open.take() {
                        runs.push(RowRange { start, end: base });
                    }
                }
                0xFF => {
                    if open.is_none() {
                        open = Some(base);
                    }
                }
                _ => {
                    for bit in 0..8u64 {
                        if byte & (1 << bit) != 0 {
                            if open.is_none() {
                                open = Some(base + bit);
                            }
                        } else if let Some(start) = open.take() {
                            runs.push(RowRange { start, end: base + bit });
                        }
                    }
                }
            }
        }
        if let Some(start) = open {
            runs.push(RowRange {
                start,
                end: bits.len() as u64 * 8,
            });
        }
        Self { runs }
    }

    /// Builds a bitmap directly from `[start, end)` runs, normalising them into the canonical sorted, disjoint,
    /// non-adjacent form first. Empty runs are dropped and overlapping or touching runs are merged, so two inputs that
    /// describe the same set of rows produce byte-identical bitmaps.
    pub fn from_ranges<I: IntoIterator<Item = RowRange>>(ranges: I) -> Self {
        let mut sorted: Vec<RowRange> = ranges.into_iter().filter(|r| !r.is_empty()).collect();
        sorted.sort_unstable_by_key(|r| (r.start, r.end));
        let mut runs: Vec<RowRange> = Vec::with_capacity(sorted.len());
        for range in sorted {
            match runs.last_mut() {
                Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
                _ => runs.push(range),
            }
        }
        Self { runs }
    }

    /// The set of `[start, end)` runs backing this bitmap, in sorted order.
    pub fn ranges(&self) -> &[RowRange] {
        &self.runs
    }

    /// Whether this bitmap holds no rows at all.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// How many rows the bitmap selects, counted from the run widths without expanding them.
    pub fn count(&self) -> u64 {
        self.runs.iter().map(|r| r.end.saturating_sub(r.start)).sum()
    }

    /// Whether a given row id is in the set. Runs are sorted, so this is a binary search over the runs, not a scan of
    /// every row.
    pub fn contains(&self, row: u64) -> bool {
        // Find the last run whose start is <= row; the row is present iff it falls before that run's end.
        let idx = self.runs.partition_point(|r| r.start <= row);
        match idx.checked_sub(1).and_then(|i| self.runs.get(i)) {
            Some(run) => row < run.end,
            None => false,
        }
    }

    /// Removes rows present in `other`, returning those in `self` but not in `other` — the set difference (AND-NOT).
    /// Like [`intersect`], both sides stay compressed throughout: row ids are never materialised. The result equals the
    /// true set difference, exactly.
    ///
    /// The deletion-vector anti-join uses this: `live = all_rows.difference(&deleted)`.
    ///
    /// [`intersect`]: Self::intersect
    pub fn difference(&self, other: &Self) -> Self {
        let mut runs: Vec<RowRange> = Vec::new();
        let mut bi = 0usize;
        for a in &self.runs {
            let mut cur = a.start;
            // Skip other runs that ended before cur.
            while let Some(b) = other.runs.get(bi) {
                if b.end > cur {
                    break;
                }
                bi += 1;
            }
            // Walk other runs that overlap this a-run.
            let mut oi = bi;
            while let Some(b) = other.runs.get(oi) {
                if b.start >= a.end {
                    break;
                }
                // Emit the gap before b.start (capped to a.end).
                let gap_end = b.start.min(a.end);
                if cur < gap_end {
                    runs.push(RowRange {
                        start: cur,
                        end: gap_end,
                    });
                }
                if b.end > cur {
                    cur = b.end;
                }
                oi += 1;
            }
            // Emit the tail of this a-run not covered by any b-run.
            if cur < a.end {
                runs.push(RowRange { start: cur, end: a.end });
            }
        }
        Self { runs }
    }

    /// Intersects two bitmaps directly in their compressed form, returning the rows present in both. The work is a
    /// single linear merge over the runs of each side — row ids are never materialised — and the result equals the true
    /// set intersection, exactly.
    pub fn intersect(&self, other: &Self) -> Self {
        let mut runs: Vec<RowRange> = Vec::new();
        let mut a = 0usize;
        let mut b = 0usize;
        while let (Some(left), Some(right)) = (self.runs.get(a), other.runs.get(b)) {
            let start = left.start.max(right.start);
            let end = left.end.min(right.end);
            if start < end {
                match runs.last_mut() {
                    Some(last) if start <= last.end => last.end = last.end.max(end),
                    _ => runs.push(RowRange { start, end }),
                }
            }
            // Advance whichever run ends first; the other may still overlap a later run on the opposite side.
            if left.end <= right.end {
                a += 1;
            } else {
                b += 1;
            }
        }
        Self { runs }
    }

    /// Serializes the bitmap to bytes: a run count followed by each run's `start` and `end`. Round-trips exactly with
    /// [`decode`](Self::decode).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(RUN_COUNT_PREFIX_BYTES + self.runs.len() * ENCODED_ROW_RANGE_BYTES);
        out.put_u32(self.runs.len() as u32);
        for run in &self.runs {
            out.put_u64(run.start);
            out.put_u64(run.end);
        }
        out.into_bytes()
    }

    /// Reads a bitmap back from [`encode`](Self::encode)'s bytes. Refuses on truncated input or on runs that are
    /// not in canonical sorted, disjoint, non-adjacent order, so a decoded bitmap is always well-formed.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        let count = reader.u32("bitmap run count")? as usize;
        let mut runs: Vec<RowRange> = Vec::with_capacity(reader.capacity_hint(count, ENCODED_ROW_RANGE_BYTES));
        for _ in 0..count {
            let start = reader.u64("bitmap run start")?;
            let end = reader.u64("bitmap run end")?;
            if end <= start {
                return Err(FormatError::Structural {
                    rule: "bitmap run must be non-empty",
                });
            }
            // Strictly increasing and non-touching keeps the form canonical.
            if runs.last().is_some_and(|last| start <= last.end) {
                return Err(FormatError::Structural {
                    rule: "bitmap runs must be sorted and non-adjacent",
                });
            }
            runs.push(RowRange { start, end });
        }
        Ok(Self { runs })
    }
}

/// A bitmap index for one low-cardinality column: a mapping from each distinct value (encoded as a `u64` code, e.g. a
/// dictionary id or a `status` code) to the set of rows that hold it.
///
/// Looking up one value gives its rows; intersecting two values' bitmaps answers an AND across columns. Because each
/// per-value set is an exact [`RoaringRangeBitmap`], the index is exact end to end.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BitmapIndex {
    entries: Vec<(u64, RoaringRangeBitmap)>,
}

impl BitmapIndex {
    /// Starts an empty index with no values mapped.
    pub fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// Records that `rows` carry `value`. Any rows already mapped to that value are merged in, so calling this
    /// repeatedly for the same value accumulates its row set.
    pub fn insert<I: IntoIterator<Item = u64>>(&mut self, value: u64, rows: I) {
        let added = RoaringRangeBitmap::from_rows(rows);
        match self.entries.binary_search_by_key(&value, |(v, _)| *v) {
            Ok(idx) => {
                if let Some((_, existing)) = self.entries.get_mut(idx) {
                    let merged = RoaringRangeBitmap::from_ranges(
                        existing.ranges().iter().copied().chain(added.ranges().iter().copied()),
                    );
                    *existing = merged;
                }
            }
            Err(idx) => self.entries.insert(idx, (value, added)),
        }
    }

    /// The bitmap of rows holding `value`, or `None` if the value was never recorded (meaning no row in this index
    /// carries it).
    pub fn bitmap(&self, value: u64) -> Option<&RoaringRangeBitmap> {
        self.entries
            .binary_search_by_key(&value, |(v, _)| *v)
            .ok()
            .and_then(|idx| self.entries.get(idx).map(|(_, b)| b))
    }

    /// How many distinct values the index maps.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the index maps no values at all.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The rows that carry every one of `values` at once, as a single compressed bitmap. With no values the result is
    /// empty; with one it is that value's bitmap; otherwise the per-value bitmaps are intersected directly. A value
    /// absent from the index contributes no rows, so the whole intersection is empty.
    pub fn intersect_values(&self, values: &[u64]) -> RoaringRangeBitmap {
        let mut iter = values.iter();
        let Some(first) = iter.next() else {
            return RoaringRangeBitmap::default();
        };
        let Some(acc) = self.bitmap(*first) else {
            return RoaringRangeBitmap::default();
        };
        let mut acc = acc.clone();
        for value in iter {
            match self.bitmap(*value) {
                Some(next) => acc = acc.intersect(next),
                None => return RoaringRangeBitmap::default(),
            }
            if acc.is_empty() {
                break;
            }
        }
        acc
    }

    /// Serializes the whole index: a value count, then each value followed by its encoded bitmap. Round-trips with
    /// [`decode`](Self::decode).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_u32(self.entries.len() as u32);
        for (value, bitmap) in &self.entries {
            out.put_u64(*value);
            let block = bitmap.encode();
            out.put_u32(block.len() as u32);
            out.put_slice(&block);
        }
        out.into_bytes()
    }

    /// Reads an index back from [`encode`](Self::encode)'s bytes. Refuses on truncation or on values that are not
    /// strictly increasing (which would break the lookup invariant).
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        let count = reader.u32("bitmap index value count")? as usize;
        let mut entries: Vec<(u64, RoaringRangeBitmap)> =
            Vec::with_capacity(reader.capacity_hint(count, INDEX_ENTRY_HEADER_BYTES));
        for _ in 0..count {
            let value = reader.u64("bitmap index value")?;
            if entries.last().is_some_and(|(last, _)| value <= *last) {
                return Err(FormatError::Structural {
                    rule: "bitmap index values must be sorted and unique",
                });
            }
            let block_len = reader.u32("bitmap block length")? as usize;
            let block = reader.take(block_len, "bitmap block")?;
            entries.push((value, RoaringRangeBitmap::decode(block)?));
        }
        Ok(Self { entries })
    }
}

/// What a file declares about one bitmap block, so a planner can decide whether it is safe to use that block to
/// accelerate a query.
///
/// All three conditions must be true together (see [`can_use_bitmap`]): the encoding must support direct compressed
/// intersection, the file must declare the bitmap feature, and the block's checksum must have verified. Any one of them
/// false means the planner must fall back to scanning instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitmapBlockInfo {
    pub checksum_verified: bool,
    pub directly_intersectable: bool,
    pub feature_declared: bool,
}

/// Computes the BLAKE3 checksum of an encoded bitmap block, the same value a reader recomputes to verify the block on
/// the way in.
pub fn bitmap_block_checksum(block: &[u8]) -> [u8; CHECKSUM_BYTES] {
    *crate::file::integrity::hash_tree(block).as_bytes()
}

/// Builds a [`BitmapBlockInfo`] for a block by actually verifying its checksum: `checksum_verified` is set only if
/// BLAKE3 over `block` equals `expected`. The caller still supplies whether the encoding is directly intersectable and
/// whether the file declared the feature.
pub fn verify_bitmap_block(
    block: &[u8],
    expected: &[u8; CHECKSUM_BYTES],
    directly_intersectable: bool,
    feature_declared: bool,
) -> BitmapBlockInfo {
    BitmapBlockInfo {
        checksum_verified: &bitmap_block_checksum(block) == expected,
        directly_intersectable,
        feature_declared,
    }
}

/// Whether a planner may use this bitmap block to accelerate a query. Returns `true` only when the block is directly
/// intersectable in compressed form, the file declared the bitmap feature, and its checksum verified — all three. If
/// any is false the bitmap path must not be taken and the planner falls back to a full scan or another index.
pub fn can_use_bitmap(info: &BitmapBlockInfo) -> bool {
    info.checksum_verified && info.directly_intersectable && info.feature_declared
}

#[cfg(test)]
#[path = "test/bitmap.rs"]
mod tests;
