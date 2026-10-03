//! Zone maps: the tiny "smallest and largest value in this block" summaries a reader checks before opening a block of
//! rows.
//!
//! A zone map records the minimum and maximum value a column takes over a stretch of rows. When a query asks for values
//! outside that span, the whole block can be skipped without reading it. This file holds four flavours: one for integer
//! and decimal columns ([`NumericMinMax`]), one for variable-length strings that keeps its size bounded even when the
//! strings are long ([`StringMinMax`]), and two ordered-range summaries for the epoch/sequence and time columns
//! ([`SequenceRange`] and [`TimeRange`]). Each can be built from values, asked whether it lets a query prune a block,
//! and serialized to and from bytes so it can live in a file's skip-index directory.
//!
//! The string zone map is the interesting one: rather than store a possibly-huge string in full, it may keep a
//! shortened lower bound (rounded *down*) and a shortened upper bound (rounded *up*), so a pruning decision stays sound
//! — it never skips a block that could match — while the stored entry stays small. Shortened entries are marked inexact
//! so the query keeps a real filter above the scan.

use super::Exactness;
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};

/// Smallest and largest value seen in an integer or decimal column over a block of rows, plus how many rows were
/// present versus null.
///
/// A query that asks for a value range outside `[min, max]` can skip the whole block. Decimal values are carried in
/// their scaled `i128` representation, the same one the page statistics use, so the comparison is exact. Both bounds
/// are `None` only when every covered row was null (there was no value to bound).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NumericMinMax {
    pub max: Option<i128>,
    pub min: Option<i128>,
    pub null_count: u32,
    pub row_count: u32,
}

impl NumericMinMax {
    /// Builds the zone map from a column's values, where `None` marks a null.
    ///
    /// Scans the values once to find the smallest and largest non-null value and to count nulls. If every value is null
    /// the bounds come back `None`. The result is a pure function of the input, so two builds over the same values
    /// produce the same zone map.
    pub fn build(values: &[Option<i128>]) -> NumericMinMax {
        let mut min: Option<i128> = None;
        let mut max: Option<i128> = None;
        let mut null_count: u32 = 0;
        for value in values {
            match value {
                Some(value) => {
                    min = Some(min.map_or(*value, |current| current.min(*value)));
                    max = Some(max.map_or(*value, |current| current.max(*value)));
                }
                None => null_count = null_count.saturating_add(1),
            }
        }
        NumericMinMax {
            max,
            min,
            null_count,
            row_count: values.len() as u32,
        }
    }

    /// Whether a query asking for values in `[lo, hi]` can skip this block.
    ///
    /// Returns `true` only when the requested range lies entirely outside the stored `[min, max]` span, so no row here
    /// could match. A block with no values at all (both bounds `None`) holds nothing to match, so it is always safe to
    /// prune. This is conservative: when the ranges touch it returns `false` and the block is kept.
    pub fn can_prune_range(&self, lo: i128, hi: i128) -> bool {
        if lo > hi {
            return true;
        }
        match (self.min, self.max) {
            (Some(min), Some(max)) => hi < min || lo > max,
            // No non-null value present: nothing here can match.
            _ => true,
        }
    }

    /// Exactness of this zone map. Numeric min/max bounds are always exact: the stored values are the true extremes, so
    /// a kept block decision is final apart from the predicate check itself.
    pub fn exactness(&self) -> Exactness {
        Exactness::Exact
    }

    /// Serializes the zone map to bytes for the skip-index directory.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_u32(self.row_count);
        out.put_u32(self.null_count);
        put_opt_i128(&mut out, self.min);
        put_opt_i128(&mut out, self.max);
        out.into_bytes()
    }

    /// Rebuilds the zone map from bytes written by [`NumericMinMax::encode`], or returns a [`FormatError`] if the bytes
    /// are truncated or internally inconsistent.
    ///
    /// The invariants [`build`](Self::build) upholds are re-checked here rather than trusted, because a corrupt zone map
    /// that decodes to bad bounds fails *open*: [`can_prune_range`](Self::can_prune_range) reads absent bounds as "no
    /// values here" and prunes the block, so a flipped option tag or a mangled `null_count` could hide rows that in fact
    /// match. Decoding therefore rejects a `null_count` above `row_count`, bounds that are not both present or both
    /// absent, a `min` greater than its `max`, and absent bounds over a block that holds non-null rows (only an
    /// all-null block can lack bounds).
    pub fn decode(bytes: &[u8]) -> Result<NumericMinMax, FormatError> {
        let mut reader = Reader::new(bytes);
        let row_count = reader.u32("numeric minmax row count")?;
        let null_count = reader.u32("numeric minmax null count")?;
        let min = read_opt_i128(&mut reader)?;
        let max = read_opt_i128(&mut reader)?;
        if null_count > row_count {
            return Err(FormatError::Structural {
                rule: "numeric minmax null count exceeds row count",
            });
        }
        match (min, max) {
            (Some(_), None) | (None, Some(_)) => {
                return Err(FormatError::Structural {
                    rule: "numeric minmax bounds must be both present or both absent",
                });
            }
            (Some(min), Some(max)) if min > max => {
                return Err(FormatError::Structural {
                    rule: "numeric minmax min exceeds max",
                });
            }
            (None, None) if row_count > null_count => {
                return Err(FormatError::Structural {
                    rule: "numeric minmax absent bounds require an all-null block",
                });
            }
            _ => {}
        }
        Ok(NumericMinMax {
            max,
            min,
            null_count,
            row_count,
        })
    }
}

/// Smallest and largest *string* value in a block, stored so its size stays bounded even when the column holds long,
/// high-cardinality text.
///
/// Plain text columns can be arbitrarily long, so keeping the literal minimum and maximum would blow up the zone-map
/// size. Instead this may store *shortened* bounds: a lower bound rounded **down** (a byte prefix of the true minimum,
/// which is always ≤ it) and an upper bound rounded **up** (the smallest short string that is still ≥ the true
/// maximum). When the upper bound cannot be represented within the length budget — every prefix byte is already `0xFF`
/// — `max` is `None`, meaning "unbounded above". `is_truncated` records whether either bound was shortened; when it is
/// clear the bounds are exact and the entry behaves like an ordinary zone map.
///
/// Building is a pure, deterministic function of the values and the length budget, so every node that builds over the
/// same committed rows gets byte-identical bounds. Pruning with a shortened entry stays sound — it never skips a block
/// that could match — but is reported as inexact so the query keeps a real filter above the scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringMinMax {
    pub is_truncated: bool,
    /// `None` means unbounded above: the true maximum could not be represented within the length budget, so no finite
    /// upper bound is stored.
    pub max: Option<Vec<u8>>,
    pub min: Vec<u8>,
}

impl StringMinMax {
    /// Builds the bounded zone map from a column's values (`None` marks a null), keeping each stored bound at most
    /// `max_stored_len` bytes long.
    ///
    /// Finds the true byte-wise minimum and maximum over the non-null values. If both already fit the budget they are
    /// stored exactly and `is_truncated` is clear. Otherwise the lower bound is rounded down (its first
    /// `max_stored_len` bytes) and the upper bound is rounded up to the smallest short string still ≥ the true maximum;
    /// if no such finite string exists (every leading byte is `0xFF`) the upper bound becomes `None` (unbounded above)
    /// and `is_truncated` is set. With no non-null values the entry is the empty exact range `min = []`, `max =
    /// Some([])`.
    ///
    /// The result is a deterministic function of `values` and `max_stored_len`: two builds over the same inputs produce
    /// byte-identical `min`, `max`, and `is_truncated`.
    pub fn build(values: &[Option<&[u8]>], max_stored_len: usize) -> StringMinMax {
        let mut true_min: Option<&[u8]> = None;
        let mut true_max: Option<&[u8]> = None;
        for value in values.iter().flatten() {
            true_min = Some(match true_min {
                Some(current) if current <= *value => current,
                _ => *value,
            });
            true_max = Some(match true_max {
                Some(current) if current >= *value => current,
                _ => *value,
            });
        }
        let (Some(true_min), Some(true_max)) = (true_min, true_max) else {
            // No non-null values: an empty, exact range that matches nothing.
            return StringMinMax {
                is_truncated: false,
                max: Some(Vec::new()),
                min: Vec::new(),
            };
        };

        let min_fits = true_min.len() <= max_stored_len;
        let max_fits = true_max.len() <= max_stored_len;

        // Each bound is shortened only when it overflows the budget; a bound that already fits is kept exact so pruning
        // stays as tight as possible. The lower bound is rounded down (a byte prefix is lexicographically ≤ the
        // original, so the first `max_stored_len` bytes are a sound lower bound); the upper bound is rounded up to the
        // smallest short string ≥ the true maximum.
        let min = if min_fits {
            true_min.to_vec()
        } else {
            truncate_down(true_min, max_stored_len)
        };
        let max = if max_fits {
            Some(true_max.to_vec())
        } else {
            truncate_up(true_max, max_stored_len)
        };
        StringMinMax {
            is_truncated: !(min_fits && max_fits),
            max,
            min,
        }
    }

    /// Whether a query asking for string values in `[predicate_lo, predicate_hi]` can skip this block.
    ///
    /// Returns `true` only when the predicate's range lies entirely outside the stored `[min, max]` span, treating `max
    /// = None` as "+infinity" (never outside above). This is conservatively correct: because the stored lower bound is
    /// ≤ the true minimum and the stored upper bound is ≥ the true maximum, a range ruled out here cannot contain any
    /// real value, so no matching row is ever skipped. When the ranges touch it returns `false` and the block is kept.
    pub fn can_prune(&self, predicate_lo: &[u8], predicate_hi: &[u8]) -> bool {
        if predicate_lo > predicate_hi {
            return true;
        }
        // Entirely below the stored minimum: predicate_hi < min.
        if predicate_hi < self.min.as_slice() {
            return true;
        }
        // Entirely above the stored maximum: predicate_lo > max (only when a finite upper bound exists; `None` means
        // unbounded, never above).
        match &self.max {
            Some(max) => predicate_lo > max.as_slice(),
            None => false,
        }
    }

    /// Exactness of this entry. Exact when neither bound was shortened, otherwise `InexactNoFalseNegative`: a shortened
    /// entry never drops a matching block, but a kept block may not actually contain a match once the dropped suffix
    /// bytes are considered, so the query must keep a real filter above the scan.
    pub fn exactness(&self) -> Exactness {
        if self.is_truncated {
            Exactness::InexactNoFalseNegative
        } else {
            Exactness::Exact
        }
    }

    /// Serializes the entry to bytes for the skip-index directory.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_u8(u8::from(self.is_truncated));
        out.put_u32(self.min.len() as u32);
        out.put_slice(&self.min);
        match &self.max {
            Some(max) => {
                out.put_u8(1);
                out.put_u32(max.len() as u32);
                out.put_slice(max);
            }
            None => out.put_u8(0),
        }
        out.into_bytes()
    }

    /// Rebuilds the entry from bytes written by [`StringMinMax::encode`], or returns a [`FormatError`] if the bytes are
    /// truncated or internally inconsistent.
    ///
    /// Like [`NumericMinMax::decode`], the invariants [`build`](Self::build) upholds are re-checked rather than
    /// trusted, because a corrupt entry fails *open*: inverted bounds make [`can_prune`](Self::can_prune) skip a block
    /// whose rows match, and a cleared truncation flag over shortened bounds upgrades [`exactness`](Self::exactness)
    /// to `Exact` and drops the residual filter the entry requires. Decoding therefore rejects a `min` above a finite
    /// `max` and an exact entry with no finite upper bound (only truncation ever produces one).
    pub fn decode(bytes: &[u8]) -> Result<StringMinMax, FormatError> {
        let mut reader = Reader::new(bytes);
        let is_truncated = reader.u8("string minmax truncated flag")? != 0;
        let min_len = reader.u32("string minmax min length")? as usize;
        let min = reader.take(min_len, "string minmax min")?.to_vec();
        let max = if reader.u8("string minmax max tag")? == 1 {
            let max_len = reader.u32("string minmax max length")? as usize;
            Some(reader.take(max_len, "string minmax max")?.to_vec())
        } else {
            None
        };
        match &max {
            Some(max) if min > *max => {
                return Err(FormatError::Structural {
                    rule: "string minmax min exceeds max",
                });
            }
            None if !is_truncated => {
                return Err(FormatError::Structural {
                    rule: "string minmax exact entry must have a finite max",
                });
            }
            _ => {}
        }
        Ok(StringMinMax { is_truncated, max, min })
    }
}

/// The span of epoch/sequence positions a block covers, as a pair of (epoch, sequence) endpoints.
///
/// Rows are ordered first by epoch then by sequence, so this records the first and last position in that order. A query
/// restricted to positions outside the `[first, last]` span can skip the block. Both endpoints are exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SequenceRange {
    pub first_epoch: u64,
    pub first_sequence: u64,
    pub last_epoch: u64,
    pub last_sequence: u64,
}

impl SequenceRange {
    /// Builds the range from a block's (epoch, sequence) positions, taking the smallest as the first endpoint and the
    /// largest as the last. Returns `None` when there are no positions to bound.
    pub fn build(positions: &[(u64, u64)]) -> Option<SequenceRange> {
        let mut first: Option<(u64, u64)> = None;
        let mut last: Option<(u64, u64)> = None;
        for &position in positions {
            first = Some(first.map_or(position, |current| current.min(position)));
            last = Some(last.map_or(position, |current| current.max(position)));
        }
        let (first, last) = (first?, last?);
        Some(SequenceRange {
            first_epoch: first.0,
            first_sequence: first.1,
            last_epoch: last.0,
            last_sequence: last.1,
        })
    }

    /// Whether a query restricted to positions in `[lo, hi]` (each an (epoch, sequence) pair) can skip this block, true
    /// when that range lies entirely outside the covered span. Conservative: touching ranges keep the block.
    pub fn can_prune(&self, lo: (u64, u64), hi: (u64, u64)) -> bool {
        if lo > hi {
            return true;
        }
        let first = (self.first_epoch, self.first_sequence);
        let last = (self.last_epoch, self.last_sequence);
        hi < first || lo > last
    }

    /// Exactness of this range. Always exact: the endpoints are the true extremes.
    pub fn exactness(&self) -> Exactness {
        Exactness::Exact
    }

    /// Serializes the range to bytes for the skip-index directory.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_u64(self.first_epoch);
        out.put_u64(self.first_sequence);
        out.put_u64(self.last_epoch);
        out.put_u64(self.last_sequence);
        out.into_bytes()
    }

    /// Rebuilds the range from bytes written by [`SequenceRange::encode`], or returns a [`FormatError`] if the bytes
    /// are truncated or the endpoints are inverted.
    ///
    /// Like [`NumericMinMax::decode`], the ordering invariant is re-checked rather than trusted: a corrupt range whose
    /// first endpoint sorts after its last fails *open* — [`can_prune`](Self::can_prune) reads the empty span as
    /// covering nothing and skips a block whose rows match.
    pub fn decode(bytes: &[u8]) -> Result<SequenceRange, FormatError> {
        let mut reader = Reader::new(bytes);
        let range = SequenceRange {
            first_epoch: reader.u64("sequence range first epoch")?,
            first_sequence: reader.u64("sequence range first sequence")?,
            last_epoch: reader.u64("sequence range last epoch")?,
            last_sequence: reader.u64("sequence range last sequence")?,
        };
        if (range.first_epoch, range.first_sequence) > (range.last_epoch, range.last_sequence) {
            return Err(FormatError::Structural {
                rule: "sequence range first exceeds last",
            });
        }
        Ok(range)
    }
}

/// The span of physical timestamps a block covers, as a smallest and largest value.
///
/// Times are stored as the engine's physical integer instants (the same ones the granule directory uses). A query
/// restricted to a time window outside `[min_physical, max_physical]` can skip the block. Both bounds are exact.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TimeRange {
    pub max_physical: i64,
    pub min_physical: i64,
}

impl TimeRange {
    /// Builds the range from a block's physical timestamps, taking the smallest and largest. Returns `None` when there
    /// are no timestamps to bound.
    pub fn build(values: &[i64]) -> Option<TimeRange> {
        let (&first, rest) = values.split_first()?;
        // A plain min/max fold over two running accumulators, rather than re-testing an `Option` on every value: the
        // first value seeds both accumulators, so there is no presence check left inside the loop to block
        // vectorizing it.
        let (min, max) = rest
            .iter()
            .fold((first, first), |(min, max), &value| (min.min(value), max.max(value)));
        Some(TimeRange {
            max_physical: max,
            min_physical: min,
        })
    }

    /// Whether a query restricted to the time window `[lo, hi]` can skip this block, true when that window lies
    /// entirely outside the covered span. Conservative: touching windows keep the block.
    pub fn can_prune(&self, lo: i64, hi: i64) -> bool {
        if lo > hi {
            return true;
        }
        hi < self.min_physical || lo > self.max_physical
    }

    /// Exactness of this range. Always exact: the bounds are the true extremes.
    pub fn exactness(&self) -> Exactness {
        Exactness::Exact
    }

    /// Serializes the range to bytes for the skip-index directory.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_i64(self.min_physical);
        out.put_i64(self.max_physical);
        out.into_bytes()
    }

    /// Rebuilds the range from bytes written by [`TimeRange::encode`], or returns a [`FormatError`] if the bytes are
    /// truncated or the bounds are inverted.
    ///
    /// Like [`NumericMinMax::decode`], the ordering invariant is re-checked rather than trusted: a corrupt range with
    /// `min > max` fails *open* — [`can_prune`](Self::can_prune) reads the empty span as covering nothing and skips a
    /// block whose rows match.
    pub fn decode(bytes: &[u8]) -> Result<TimeRange, FormatError> {
        let mut reader = Reader::new(bytes);
        let min_physical = reader.i64("time range min")?;
        let max_physical = reader.i64("time range max")?;
        if min_physical > max_physical {
            return Err(FormatError::Structural {
                rule: "time range min exceeds max",
            });
        }
        Ok(TimeRange {
            max_physical,
            min_physical,
        })
    }
}

/// Returns the first `max_stored_len` bytes of `value` — a lower bound rounded down, since a byte prefix is
/// lexicographically ≤ the full string.
fn truncate_down(value: &[u8], max_stored_len: usize) -> Vec<u8> {
    value.get(..max_stored_len).unwrap_or(value).to_vec()
}

/// Returns the smallest byte string of length ≤ `max_stored_len` that is still ≥ `value`, or `None` (unbounded above)
/// when no such finite string exists.
///
/// Takes the leading `max_stored_len` bytes, then "rounds up" by finding the last byte below `0xFF`, incrementing it,
/// and dropping everything after it (so e.g. `ab` → `ac`, `az` (with budget after the prefix) → `b`). If every byte in
/// the prefix is already `0xFF` there is no finite string above it, so the bound is unbounded above and the result is
/// `None`.
fn truncate_up(value: &[u8], max_stored_len: usize) -> Option<Vec<u8>> {
    let mut prefix: Vec<u8> = value.get(..max_stored_len).unwrap_or(value).to_vec();
    while let Some(last) = prefix.last().copied() {
        if last < 0xFF {
            let len = prefix.len();
            if let Some(slot) = prefix.get_mut(len - 1) {
                *slot = last + 1;
            }
            return Some(prefix);
        }
        prefix.pop();
    }
    // Every leading byte was 0xFF: no finite upper bound exists.
    None
}

fn put_opt_i128(out: &mut Writer, value: Option<i128>) {
    match value {
        Some(value) => {
            out.put_u8(1);
            out.put_u128(value as u128);
        }
        None => out.put_u8(0),
    }
}

fn read_opt_i128(reader: &mut Reader<'_>) -> Result<Option<i128>, FormatError> {
    match reader.u8("option tag")? {
        0 => Ok(None),
        1 => Ok(Some(reader.u128("i128")? as i128)),
        _ => Err(FormatError::Structural {
            rule: "numeric minmax option tag must be 0 or 1",
        }),
    }
}

#[cfg(test)]
#[path = "test/minmax.rs"]
mod tests;
