//! The metadata HEF keeps so a query can skip over data it will never need to read.
//!
//! Before touching any real column bytes, a reader consults small summaries that answer one question per layer: could
//! this whole file, stripe, granule, or page possibly hold a row the query wants? Each summary is a *skip index* — a
//! compact, rebuildable structure (a min/max bound, a membership filter, a range-emptiness filter, a bitmap) that can
//! rule a block *out* but, crucially, never rules a matching block out by mistake. Skip indexes are pure acceleration:
//! they are always rebuildable from the committed rows and are never the authoritative source of any value.
//!
//! The submodules hold the concrete structures and the planner logic that uses them: [`model`] (the uniform skip-index
//! vocabulary and the choice of which index answers which predicate), [`minmax`] (numeric and bounded-string zone maps
//! plus sequence and time ranges), [`probabilistic`] and [`range_filter`] (point-membership and range-emptiness
//! filters), [`bitmap`] (exact intersectable bitmaps), [`summary`] (the layered manifest → footer → granule hierarchy),
//! and [`pruning`] (the single conservative falsification expression that folds every declared statistic into one
//! keep/drop decision).

pub mod artifact;
pub mod bitmap;
pub mod constant_flags;
pub mod external_id;
pub mod learned_position;
pub mod minmax;
pub mod model;
pub mod ndv;
pub mod path_presence;
pub mod probabilistic;
pub mod pruning;
pub mod range_filter;
pub mod rank_select;
pub mod summary;
pub mod text_token;

/// How trustworthy a skip index's answer is once the planner uses it to skip data. An `Exact` index can both keep and
/// drop blocks with certainty; an `InexactNoFalseNegative` index may keep a block that turns out to hold no match — so
/// the query must still apply a real filter — but never drops a block that does hold one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exactness {
    Exact,
    InexactNoFalseNegative,
}

impl Exactness {
    /// Whether a query that pruned with an index of this exactness must still keep an exact predicate filter above or
    /// inside the scan. Inexact indexes can admit non-matching rows, so the answer is `true` for them and `false` for
    /// exact indexes.
    pub fn requires_residual_filter(self) -> bool {
        matches!(self, Exactness::InexactNoFalseNegative)
    }
}

/// The unit of data a skip index summarises, from a whole file down to one mini-block inside a page. A granule is the
/// file's smallest independently prunable and readable block of rows. Coarser granularities skip more data in a single
/// decision; finer ones skip more precisely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Granularity {
    File,
    Granule,
    MiniBlock,
    Page,
    Stripe,
}

/// The kinds of skip index HEF understands. Each names a structure a writer can build over a column and a reader can
/// probe to rule blocks out: `MinMax` answers ordering and range predicates; `AccountHashFilter`, `BinaryFuseFilter`,
/// `EntityHashFilter`, `OpportunityHashFilter`, `RibbonFilter`, and `SplitBlockBloomFilter` answer point membership;
/// `RangeFilter` answers range-emptiness; `SequenceRange` and `TimeRange` bound the epoch/sequence and time spans;
/// `ContextLocator`, `PathPresence`, and `TextToken` locate investigation context, payload paths, and search tokens;
/// and `LearnedPosition` maps a sorted key to its approximate row position for fast range seeks under a covering
/// sortedness proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipIndexKind {
    AccountHashFilter,
    BinaryFuseFilter,
    ContextLocator,
    EntityHashFilter,
    /// A compact piecewise linear model over a sorted key column; turns a range seek into a model evaluation plus a
    /// bounded local scan.
    LearnedPosition,
    MinMax,
    OpportunityHashFilter,
    PathPresence,
    RangeFilter,
    RibbonFilter,
    SequenceRange,
    SplitBlockBloomFilter,
    TextToken,
    TimeRange,
}

/// Hashes bytes to a `u64` for the membership filters in this module (token and path presence), so different inputs
/// almost always land on different filter slots and every machine computes the same value.
///
/// It folds an FNV-1a-seeded polynomial rolling hash over the bytes, then runs the SplitMix64 finalizer for avalanche.
/// Shared by [`text_token::hash_token`] and [`path_presence::hash_path`] so both filters hash identically.
pub fn stable_hash(bytes: &[u8]) -> u64 {
    stable_hash_of(bytes, |b| b)
}

/// The same hash as [`stable_hash`], but reading each byte as its ASCII-lowercase form, so a caller that hashes text
/// case-insensitively never has to build a lowercased copy of it first. Bytes outside `A`-`Z` pass through untouched:
/// for all-ASCII input this equals `stable_hash` of the `to_lowercase()` form, and a caller with non-ASCII input must
/// lowercase it itself.
pub fn stable_hash_ascii_lowercase(bytes: &[u8]) -> u64 {
    stable_hash_of(bytes, |b| b.to_ascii_lowercase())
}

/// The polynomial's multiplier and starting value: the FNV-1a prime and offset basis. They fix the hash's output, which
/// reaches persisted filter bits, so they can never change.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// The weight each byte of an eight-byte chunk carries: the first byte of the chunk is scaled by [`FNV_PRIME`] to the
/// seventh and the last by one, which is what eight one-byte steps would have applied by the time the chunk is done.
/// The running hash carries [`FNV_PRIME_POW8`] across the same step. Folding a whole chunk with these weights lands on
/// exactly the value the byte-at-a-time loop reaches, with eight independent multiplies in place of a chain of eight
/// dependent ones.
const FNV_CHUNK_WEIGHTS: [u64; 8] = {
    let second = FNV_PRIME.wrapping_mul(FNV_PRIME);
    let third = second.wrapping_mul(FNV_PRIME);
    let fourth = third.wrapping_mul(FNV_PRIME);
    let fifth = fourth.wrapping_mul(FNV_PRIME);
    let sixth = fifth.wrapping_mul(FNV_PRIME);
    let seventh = sixth.wrapping_mul(FNV_PRIME);
    [seventh, sixth, fifth, fourth, third, second, FNV_PRIME, 1]
};
const FNV_PRIME_POW8: u64 = FNV_PRIME.wrapping_mul(FNV_CHUNK_WEIGHTS[0]);

/// Folds `bytes` into the polynomial hash, reading each byte through `byte` (identity, or ASCII-lowercasing), eight
/// bytes per step with the tail done one byte at a time.
fn stable_hash_of(bytes: &[u8], byte: impl Fn(u8) -> u8) -> u64 {
    let mut h = FNV_OFFSET_BASIS;
    let mut rest = bytes;
    while let Some((chunk, tail)) = rest.split_first_chunk::<8>() {
        h = h.wrapping_mul(FNV_PRIME_POW8);
        for (&b, weight) in chunk.iter().zip(FNV_CHUNK_WEIGHTS) {
            h = h.wrapping_add(u64::from(byte(b)).wrapping_mul(weight));
        }
        rest = tail;
    }
    for &b in rest {
        h = h.wrapping_mul(FNV_PRIME).wrapping_add(u64::from(byte(b)));
    }
    // SplitMix64 finalizer.
    let mut z = h.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
#[path = "test/mod.rs"]
mod tests;
