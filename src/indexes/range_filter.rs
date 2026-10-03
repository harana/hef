//! A range-emptiness filter: it answers "could this block hold any key in the range [lo, hi]?" without ever wrongly
//! saying no.
//!
//! When a query asks for a span of keys — a window of timestamps, a slice of an entity-hash space, a sub-range of
//! sequence numbers — min/max bounds prune badly if blocks overlap heavily. A range filter does better: it remembers
//! *which parts* of the key space a block actually touched, finely enough to rule the block out for a query range that
//! misses all of them. Like the point-membership filters, it is one-sided — it may keep a block that holds no matching
//! key (a false positive, costing a wasted read) but never drops a block that does (no false negatives) — so its
//! exactness is always [`Exactness::InexactNoFalseNegative`] and a query that prunes with it keeps an exact filter
//! above the scan.
//!
//! The on-disk representation is a *Grafite-style succinct range filter*: the key space is cut into fixed-width buckets
//! and the filter stores the sorted set of buckets that any key fell into. A query range is answered by checking
//! whether any stored bucket overlaps it. This gives a bounded, input-insensitive false-positive rate for
//! range-emptiness, which prefix-Bloom or trie filters do not. A range filter is a *range* structure: it has no
//! `contains_point`, and a point-membership filter must never be used to answer a range predicate (see
//! [`crate::indexes::probabilistic`]).

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::indexes::Exactness;

/// Format tag at the front of an encoded [`RangeFilter`], so a decoder rejects bytes that are not one (refusal).
const RANGE_FILTER_MAGIC: u32 = 0x5247_4631; // "RGF1"

/// Default number of buckets to spread keys across when a builder does not pick one. A few thousand buckets keeps the
/// stored set small while still resolving a query range to a narrow slice of the key space.
pub const DEFAULT_BUCKET_COUNT: u32 = 4096;

/// A succinct summary of which slices of the key space a block touched, used to rule the block out for a range query
/// that misses every touched slice.
///
/// The full `u64` key space is divided into `bucket_count` equal-width buckets. Every key that was built into the
/// filter marks its bucket, and the filter keeps the sorted list of marked bucket ids. To answer `[lo, hi]` the filter
/// maps the endpoints to bucket ids and asks whether any marked bucket lies in that inclusive bucket span; if so the
/// block *might* hold a matching key, and if not the block certainly does not. Over-reporting (a marked bucket overlaps
/// the range but holds no actual key inside `[lo, hi]`) is allowed and only costs a wasted read; under-reporting can
/// never happen because every real key marked its bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RangeFilter {
    bucket_count: u32,
    /// Sorted, de-duplicated ids of the buckets at least one key fell into.
    occupied: Vec<u32>,
}

impl RangeFilter {
    /// Builds a range filter over `keys` using [`DEFAULT_BUCKET_COUNT`] buckets. Convenience wrapper over
    /// [`RangeFilter::build_with_buckets`].
    pub fn build(keys: &[u64]) -> Self {
        Self::build_with_buckets(keys, DEFAULT_BUCKET_COUNT)
    }

    /// Builds a range filter over `keys`, partitioning the key space into `bucket_count` equal-width buckets (clamped
    /// to at least one). The result is a deterministic function of the key set and the bucket count, so any node
    /// building over the same keys produces byte-identical bytes. Duplicate keys are harmless.
    pub fn build_with_buckets(keys: &[u64], bucket_count: u32) -> Self {
        let bucket_count = bucket_count.max(1);
        let mut occupied: Vec<u32> = keys.iter().map(|&key| Self::bucket_of(key, bucket_count)).collect();
        occupied.sort_unstable();
        occupied.dedup();
        RangeFilter { bucket_count, occupied }
    }

    /// Maps a key to its bucket id in `0..bucket_count` via a multiply-shift, so buckets are equal-width over the whole
    /// `u64` space without a division.
    fn bucket_of(key: u64, bucket_count: u32) -> u32 {
        // (key * bucket_count) >> 64, computed in u128 to avoid overflow.
        let product = (key as u128).wrapping_mul(bucket_count as u128);
        (product >> 64) as u32
    }

    /// Reports whether this block might contain a key in the inclusive range `[lo, hi]`. A `true` answer may be a false
    /// positive (re-check the real predicate on the rows); a `false` answer is certain — no key in `[lo, hi]` is
    /// present. An inverted range (`lo > hi`) is empty, so the answer is `false`.
    pub fn range_nonempty(&self, lo: u64, hi: u64) -> bool {
        if lo > hi {
            return false;
        }
        let lo_bucket = Self::bucket_of(lo, self.bucket_count);
        let hi_bucket = Self::bucket_of(hi, self.bucket_count);
        // Any occupied bucket in [lo_bucket, hi_bucket] means the block might hold a matching key. `occupied` is
        // sorted, so binary-search the lower edge and check the neighbour lands within the upper edge.
        match self.occupied.binary_search(&lo_bucket) {
            Ok(_) => true,
            Err(pos) => matches!(self.occupied.get(pos), Some(&bucket) if bucket <= hi_bucket),
        }
    }

    /// How trustworthy this filter is for pruning: always [`Exactness::InexactNoFalseNegative`], since it can
    /// over-report a range as non-empty but never under-reports.
    pub fn exactness(&self) -> Exactness {
        Exactness::InexactNoFalseNegative
    }

    /// Serializes the filter to bytes (magic, bucket count, occupied-bucket count, then the sorted bucket ids). Pairs
    /// with [`RangeFilter::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(12 + self.occupied.len() * 4);
        out.put_u32(RANGE_FILTER_MAGIC);
        out.put_u32(self.bucket_count);
        out.put_u32(self.occupied.len() as u32);
        for &bucket in &self.occupied {
            out.put_u32(bucket);
        }
        out.into_bytes()
    }

    /// Rebuilds a filter from [`RangeFilter::encode`] output. Refuses on a wrong tag, a zero bucket count, a
    /// bucket id outside the bucket range, or a non-ascending bucket list (which would break the binary search) rather
    /// than trusting malformed bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("range filter magic")? != RANGE_FILTER_MAGIC {
            return Err(FormatError::Structural {
                rule: "range filter bad magic",
            });
        }
        let bucket_count = reader.u32("range filter bucket count")?;
        if bucket_count == 0 {
            return Err(FormatError::Structural {
                rule: "range filter has no buckets",
            });
        }
        let occupied_len = reader.u32("range filter occupied count")? as usize;
        let hint = reader.capacity_hint(occupied_len, 4);
        let mut occupied = Vec::with_capacity(hint);
        let mut previous: Option<u32> = None;
        for _ in 0..occupied_len {
            let bucket = reader.u32("range filter occupied bucket")?;
            if bucket >= bucket_count {
                return Err(FormatError::Structural {
                    rule: "range filter bucket id out of range",
                });
            }
            if let Some(prev) = previous
                && bucket <= prev
            {
                return Err(FormatError::Structural {
                    rule: "range filter buckets not strictly ascending",
                });
            }
            previous = Some(bucket);
            occupied.push(bucket);
        }
        Ok(RangeFilter { bucket_count, occupied })
    }
}

#[cfg(test)]
#[path = "test/range_filter.rs"]
mod tests;
