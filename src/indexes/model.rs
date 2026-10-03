//! The shared vocabulary the planner uses to talk about skip indexes — what one is, which predicates it can answer, and
//! how to pick the right one for a query.
//!
//! A *skip index* is a small summary the writer attaches to a block of rows so a reader can decide, without touching
//! the real data, whether the block could hold a row the query wants. This module does not store any bytes on disk; it
//! is the planner's mental model. A [`SkipIndex`] descriptor records where one index lives and what it covers;
//! [`PredicateShape`] names the broad family a query filter falls into; and the functions here answer the two questions
//! the planner keeps asking: "can this kind of index answer this shape of predicate?" and, given the indexes a column
//! actually has, "which one should I use?" The rules are faithful to the spec: a point-membership filter is never
//! allowed to answer a range predicate, and an inexact index always forces an exact filter to be kept above or inside
//! the scan.

use super::{Exactness, Granularity, SkipIndexKind};

/// The broad family a query filter falls into, as far as a skip index cares.
///
/// A skip index does not see the exact SQL; it only needs to know whether the filter is asking for one value
/// (`Equality`, e.g. `x = 5`), for any of a set of values (`Membership`, e.g. `x IN (…)`), or for a span of values
/// bounded on one or both sides (`RangeBound`, which covers `<`, `<=`, `>`, `>=`, and `BETWEEN`). Different index kinds
/// answer different shapes, and matching the two up is what [`SkipIndexKind::answers`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateShape {
    Equality,
    Membership,
    RangeBound,
}

/// Where one skip index lives in a file and what stretch of rows it summarises.
///
/// This is the planner-side descriptor for the on-disk `SkipIndex<kind, granularity>` entry: it names the column and
/// projection it covers, the kind of summary it is, how coarse a block it describes, how trustworthy its answer is, the
/// (optional) false-positive rate of a probabilistic kind, and the references it needs to find the row range and the
/// index block itself. It carries no values — it is a pointer plus a label.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkipIndex {
    pub block_ref: u64,
    pub column_id: u32,
    pub exactness: Exactness,
    /// Target false-positive rate for probabilistic kinds; `None` for exact kinds, which never give a false positive.
    pub fpr: Option<f64>,
    pub granularity: Granularity,
    pub index_id: u32,
    pub kind: SkipIndexKind,
    pub projection_id: u32,
    pub row_range: RowRange,
}

/// The contiguous span of rows a skip index covers, named by the ordinal of its first row and how many rows follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowRange {
    pub first_row_ordinal: u64,
    pub row_count: u64,
}

impl SkipIndexKind {
    /// Whether an index of this kind can answer a predicate of the given shape.
    ///
    /// This is the planner's gate: it returns `true` only when probing this kind of index can actually contribute to
    /// deciding the predicate. Crucially, a point-membership kind returns `false` for `RangeBound`, so it can never be
    /// pressed into answering a range predicate (only `MinMax`, `RangeFilter`, `SequenceRange`, and `TimeRange` answer
    /// ranges).
    pub fn answers(self, shape: PredicateShape) -> bool {
        match self {
            // Zone maps bound the value span, so they answer equality (is the point inside [min, max]?) and
            // range-emptiness alike.
            SkipIndexKind::MinMax => {
                matches!(shape, PredicateShape::Equality | PredicateShape::RangeBound)
            }
            // Succinct range filter: range-emptiness only. It is not a point index, so it does not claim equality
            // (min/max or a membership filter serves that better).
            SkipIndexKind::RangeFilter => matches!(shape, PredicateShape::RangeBound),
            // Learned position: a range seek accelerator for sorted keys. It narrows a range scan to a bounded window,
            // so it answers range predicates only (not point or membership).
            SkipIndexKind::LearnedPosition => matches!(shape, PredicateShape::RangeBound),
            // Point-membership filters: a single key or a set of keys. They have no notion of order, so they must never
            // answer a range.
            SkipIndexKind::AccountHashFilter
            | SkipIndexKind::BinaryFuseFilter
            | SkipIndexKind::EntityHashFilter
            | SkipIndexKind::OpportunityHashFilter
            | SkipIndexKind::RibbonFilter
            | SkipIndexKind::SplitBlockBloomFilter => {
                matches!(shape, PredicateShape::Equality | PredicateShape::Membership)
            }
            // Epoch/sequence and time spans are ordered ranges.
            SkipIndexKind::SequenceRange | SkipIndexKind::TimeRange => {
                matches!(shape, PredicateShape::RangeBound)
            }
            // An investigation/evidence grouping key is looked up by point.
            SkipIndexKind::ContextLocator => {
                matches!(shape, PredicateShape::Equality | PredicateShape::Membership)
            }
            // Payload-path presence answers "is this field present?" — a point existence test, not a range.
            SkipIndexKind::PathPresence => matches!(shape, PredicateShape::Equality),
            // A token filter answers "does this block contain this token?" — point membership over tokens.
            SkipIndexKind::TextToken => {
                matches!(shape, PredicateShape::Equality | PredicateShape::Membership)
            }
        }
    }

    /// How trustworthy this kind of index is by default once it is used to skip data.
    ///
    /// `MinMax`, `SequenceRange`, and `TimeRange` bound real values exactly, so they are `Exact`. Every probabilistic
    /// or succinct filter — the hash filters, the Bloom and ribbon and binary-fuse filters, and the range filter — may
    /// admit a block that holds no match, so they are `InexactNoFalseNegative`: they never drop a matching block, but a
    /// query using them must keep an exact filter above the scan. (A bounded *string* `MinMax` can also be inexact when
    /// its bounds were truncated; that is carried on the entry itself, see
    /// [`crate::indexes::minmax::StringMinMax::exactness`].)
    pub fn default_exactness(self) -> Exactness {
        match self {
            // These produce exact keep/drop decisions after confirmation; no residual filter is needed above the scan.
            SkipIndexKind::LearnedPosition
            | SkipIndexKind::MinMax
            | SkipIndexKind::SequenceRange
            | SkipIndexKind::TimeRange => Exactness::Exact,
            SkipIndexKind::AccountHashFilter
            | SkipIndexKind::BinaryFuseFilter
            | SkipIndexKind::ContextLocator
            | SkipIndexKind::EntityHashFilter
            | SkipIndexKind::OpportunityHashFilter
            | SkipIndexKind::PathPresence
            | SkipIndexKind::RangeFilter
            | SkipIndexKind::RibbonFilter
            | SkipIndexKind::SplitBlockBloomFilter
            | SkipIndexKind::TextToken => Exactness::InexactNoFalseNegative,
        }
    }
}

/// Picks a skip index from those a column actually has that can answer the given predicate shape, preferring an exact
/// answer over an inexact one.
///
/// Returns `None` when no available kind answers the shape — the planner then scans the block without skip-index help
/// (always safe, never a false negative). When several kinds qualify, an exact kind wins over an inexact one so the
/// query can avoid keeping a residual filter where possible.
///
/// The range-safety rule is built in: for a `RangeBound` shape this can only ever return a range-capable kind (`MinMax`
/// or `RangeFilter`, or the sequence/time ranges), because a point-membership kind reports `false` from
/// [`SkipIndexKind::answers`] for ranges and is filtered out before the choice is made.
pub fn choose_skip_index(shape: PredicateShape, available: &[SkipIndexKind]) -> Option<SkipIndexKind> {
    let mut best: Option<SkipIndexKind> = None;
    for &kind in available {
        if !kind.answers(shape) {
            continue;
        }
        match best {
            None => best = Some(kind),
            Some(current) => {
                // Prefer exact over inexact; otherwise keep the first match so the result is deterministic in
                // `available` order.
                let current_exact = current.default_exactness() == Exactness::Exact;
                let candidate_exact = kind.default_exactness() == Exactness::Exact;
                if candidate_exact && !current_exact {
                    best = Some(kind);
                }
            }
        }
    }
    best
}

/// Whether a query that pruned with this kind of skip index must keep an exact predicate filter above or inside the
/// scan.
///
/// This is `true` exactly when the kind's default exactness is inexact: an inexact skip index can let through a block
/// that holds no matching row, so the query stays correct only by re-checking the predicate on the real rows. For an
/// exact kind it is `false` — the index's keep/drop decision is final.
pub fn pushdown_residual_required(kind: SkipIndexKind) -> bool {
    kind.default_exactness().requires_residual_filter()
}

#[cfg(test)]
#[path = "test/model.rs"]
mod tests;
