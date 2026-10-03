//! Decides, for one block of rows at a time, whether a query could possibly find a match there — and skips the block
//! when it provably cannot.
//!
//! HEF splits a file into *granules*, small row ranges that are the unit of "read it or skip it". Before reading a
//! granule's real data, the planner asks one question: *could any row in this granule match the query's filter?* This
//! module answers that question with a single **falsification expression** built from whatever small statistics the
//! granule happens to carry (min/max bounds, a membership filter, a range-emptiness filter, a null count, and so on).
//! Each statistic contributes one [`PruningTerm`]; the terms are evaluated by one uniform loop, never by a tangle of
//! per-statistic special cases. The answer is always conservative: it may keep a granule that turns out to hold no
//! match, but it never drops a granule that holds one.
//!
//! Because the contribution of each statistic is just a term in a list, adding a brand-new statistic kind means adding
//! one variant that knows how to render its own verdict — not a new branch in the planner. The same loop that evaluated
//! yesterday's statistics evaluates tomorrow's.
//!
//! This expression sits *below* HEF's exact aggregate / cube / sketch shortcut tier: a query that an aggregate block
//! can answer outright still takes that faster path ([`plan_query`]); pruning never downgrades such a query to
//! min/max-only granule scanning.

use super::Exactness;

/// What a single statistic concludes about a single filter clause for one granule: either it has *proven* that no row
/// in the granule can satisfy the clause, or it cannot rule the granule out.
///
/// `ProvenAbsent` is the only verdict that lets a granule be dropped, and a statistic returns it only when it is
/// certain — every statistic here answers conservatively, so it never claims `ProvenAbsent` for a granule that might
/// hold a matching row. `CouldMatch` is the safe default: it keeps the granule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatVerdict {
    CouldMatch,
    ProvenAbsent,
}

/// One filter clause and a comparable value the query is testing against it.
///
/// A clause names a column (`column_id`), the shape of the comparison (`predicate`), and the value(s) it compares to as
/// a closed range `[value_lo, value_hi]`. Equality `x = 5` is modelled as the degenerate range `[5, 5]`; a range bound
/// `x >= 5` is modelled as `[5, i128::MAX]`. The whole query filter is a conjunction (AND) of these clauses — a granule
/// must plausibly satisfy *every* clause to be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilterClause {
    pub column_id: u32,
    pub predicate: PredicateKind,
    /// Inclusive upper end of the value range the clause accepts.
    pub value_hi: i128,
    /// Inclusive lower end of the value range the clause accepts.
    pub value_lo: i128,
}

/// The shape of the comparison a clause makes, as far as pruning cares.
///
/// `Equality` asks "is this exact value present?"; `Membership` asks "is any of a set of values present?" (the set is
/// summarised by `[value_lo, value_hi]` for bound purposes); `Presence` asks "is this column/path present at all?"; and
/// `Range` asks "does any value fall in `[value_lo, value_hi]`?". Different statistics can falsify different shapes,
/// which is what each term's verdict encodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateKind {
    Equality,
    Membership,
    Presence,
    Range,
}

/// The whole query filter as a conjunction of clauses that must all hold.
///
/// A granule is dropped only if some clause is *proven* unsatisfiable there; a clause with no statistic able to falsify
/// it simply keeps the granule. The clauses are ANDed: proving any one clause absent is enough to drop the granule,
/// because a row must satisfy all of them to match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Filter {
    pub clauses: Vec<FilterClause>,
}

/// One statistic a granule carries, together with the summary data it needs to judge a clause — modelled as *data*, so
/// every kind flows through the same evaluation loop.
///
/// Each variant is a different summary the writer may attach to a column: `MinMaxBound` holds the value span;
/// `NullCount` holds how many of the covered rows are null (and the total); `PathPresence` records which payload paths
/// appear; `PointMembership` holds the set of values present (a membership filter); `RangeEmptiness` holds the spans
/// that are non-empty; `SequenceBounds` and `TimeBounds` hold ordered epoch/sequence and time spans; and `TextToken`
/// holds the token hashes a token filter reports as possibly present in this block. To teach pruning a new statistic,
/// add a variant here and one arm to [`Statistic::judge`] — nothing else changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statistic {
    MinMaxBound {
        column_id: u32,
        max: i128,
        min: i128,
    },
    NullCount {
        column_id: u32,
        null_count: u64,
        row_count: u64,
    },
    PathPresence {
        column_id: u32,
        present: bool,
    },
    PointMembership {
        column_id: u32,
        /// Values the membership filter reports as possibly present. A real filter may also report false positives;
        /// this models the membership set the granule advertises.
        members: Vec<i128>,
    },
    RangeEmptiness {
        column_id: u32,
        /// Inclusive `[lo, hi]` spans the granule is known to be non-empty over; a clause whose range misses every
        /// span is proven absent.
        non_empty_spans: Vec<(i128, i128)>,
    },
    SequenceBounds {
        column_id: u32,
        max_sequence: i128,
        min_sequence: i128,
    },
    /// Hashes of tokens a text-token filter reports as possibly present in this block. Built from the actual token set
    /// via a bloom/ribbon filter or compact inverted list; may report false positives, so it is always
    /// `InexactNoFalseNegative`. A query for a token whose hash is absent here can prune the block outright; a hit must
    /// still be confirmed by an exact check over the materialised text. Requirement: "Text-token and path-presence
    /// indexes are produced and used".
    TextToken {
        column_id: u32,
        /// Token hashes the filter reports as possibly present.
        token_hashes: Vec<i128>,
    },
    TimeBounds {
        column_id: u32,
        max_time: i128,
        min_time: i128,
    },
}

impl Statistic {
    /// The column this statistic summarises, used to match it to a clause.
    pub fn column_id(&self) -> u32 {
        match self {
            Statistic::MinMaxBound { column_id, .. }
            | Statistic::NullCount { column_id, .. }
            | Statistic::PathPresence { column_id, .. }
            | Statistic::PointMembership { column_id, .. }
            | Statistic::RangeEmptiness { column_id, .. }
            | Statistic::SequenceBounds { column_id, .. }
            | Statistic::TextToken { column_id, .. }
            | Statistic::TimeBounds { column_id, .. } => *column_id,
        }
    }

    /// Whether this statistic can say anything about the given clause.
    ///
    /// A statistic is relevant only when it covers the clause's column and its summary speaks to the clause's shape —
    /// for example a `RangeEmptiness` filter answers `Range` and `Membership` but not `Presence`. Irrelevant statistics
    /// contribute no term, so the expression holds exactly one term per declared-and-relevant statistic.
    pub fn relevant_to(&self, clause: &FilterClause) -> bool {
        if self.column_id() != clause.column_id {
            return false;
        }
        match self {
            // Ordered bounds answer "is the value in range?" for equality, membership, and range shapes alike.
            Statistic::MinMaxBound { .. } | Statistic::SequenceBounds { .. } | Statistic::TimeBounds { .. } => {
                matches!(
                    clause.predicate,
                    PredicateKind::Equality | PredicateKind::Membership | PredicateKind::Range
                )
            }
            // A null count only proves a column is entirely null over the granule, which falsifies any value lookup but
            // not a presence test.
            Statistic::NullCount { .. } => matches!(
                clause.predicate,
                PredicateKind::Equality | PredicateKind::Membership | PredicateKind::Range
            ),
            // Point-membership filters never answer a range; they answer point and set lookups only (the spec forbids
            // using them for ranges).
            Statistic::PointMembership { .. } => {
                matches!(clause.predicate, PredicateKind::Equality | PredicateKind::Membership)
            }
            // Range-emptiness filters answer range-shaped questions (and a set, treated as the spanning range) but not
            // a bare presence test.
            Statistic::RangeEmptiness { .. } => {
                matches!(clause.predicate, PredicateKind::Membership | PredicateKind::Range)
            }
            // Path presence answers only "is this column/path present?".
            Statistic::PathPresence { .. } => {
                matches!(clause.predicate, PredicateKind::Presence)
            }
            // A token filter answers "does this block contain this token?" — point membership over tokens, not a range.
            Statistic::TextToken { .. } => {
                matches!(clause.predicate, PredicateKind::Equality | PredicateKind::Membership)
            }
        }
    }

    /// Whether this statistic *keeping* a granule proves that some row actually satisfies the clause, so the row-level
    /// predicate could be elided.
    ///
    /// No granule-level statistic can prove that today, so this is always `false`. A bounds-only statistic (min/max,
    /// sequence, time, null count) only fails to rule the granule out — `x = 5` inside a `[0, 10]` span says nothing
    /// about a row equal to 5 existing. The membership-style filters, path presence included, are backed by Bloom
    /// filters that may over-report. And even a statistic that recorded presence exactly would speak for the whole
    /// granule: "some row carries this path" is not "this row carries it", so the row-level predicate still has to run.
    pub fn keep_confirms_match(&self) -> bool {
        match self {
            Statistic::MinMaxBound { .. }
            | Statistic::NullCount { .. }
            | Statistic::PathPresence { .. }
            | Statistic::PointMembership { .. }
            | Statistic::RangeEmptiness { .. }
            | Statistic::SequenceBounds { .. }
            | Statistic::TextToken { .. }
            | Statistic::TimeBounds { .. } => false,
        }
    }

    /// What this statistic concludes about the clause for its granule.
    ///
    /// This is the heart of the uniform evaluation: every statistic kind renders its own [`StatVerdict`] here, and the
    /// planner's loop never needs to know which kind it is holding. A statistic returns `ProvenAbsent` only when it is
    /// certain no row can match; otherwise it conservatively returns `CouldMatch`. Adding a new statistic kind adds
    /// exactly one arm here.
    pub fn judge(&self, clause: &FilterClause) -> StatVerdict {
        match self {
            // The value span misses the clause's range entirely => no row can match. `[min, max]` and `[value_lo,
            // value_hi]` are disjoint when one ends before the other begins.
            Statistic::MinMaxBound { max, min, .. } => disjoint(*min, *max, clause.value_lo, clause.value_hi),
            Statistic::SequenceBounds {
                max_sequence,
                min_sequence,
                ..
            } => disjoint(*min_sequence, *max_sequence, clause.value_lo, clause.value_hi),
            Statistic::TimeBounds { max_time, min_time, .. } => {
                disjoint(*min_time, *max_time, clause.value_lo, clause.value_hi)
            }
            // Every covered row is null, so no row holds the value being looked up. (A non-fully-null column cannot
            // rule the granule out.)
            Statistic::NullCount {
                null_count, row_count, ..
            } => {
                if *row_count > 0 && null_count >= row_count {
                    StatVerdict::ProvenAbsent
                } else {
                    StatVerdict::CouldMatch
                }
            }
            // No advertised member falls in the clause's value range => absent. A membership filter may over-report
            // (false positives), so a hit is only `CouldMatch`, never a proof of presence.
            Statistic::PointMembership { members, .. } => {
                let any_in_range = members.iter().any(|&m| m >= clause.value_lo && m <= clause.value_hi);
                if any_in_range {
                    StatVerdict::CouldMatch
                } else {
                    StatVerdict::ProvenAbsent
                }
            }
            // None of the non-empty spans overlaps the clause's range => absent.
            Statistic::RangeEmptiness { non_empty_spans, .. } => {
                let overlaps = non_empty_spans
                    .iter()
                    .any(|&(lo, hi)| !is_disjoint(lo, hi, clause.value_lo, clause.value_hi));
                if overlaps {
                    StatVerdict::CouldMatch
                } else {
                    StatVerdict::ProvenAbsent
                }
            }
            // The path is absent from the granule => a presence predicate over it cannot match. If present, the granule
            // survives.
            Statistic::PathPresence { present, .. } => {
                if *present {
                    StatVerdict::CouldMatch
                } else {
                    StatVerdict::ProvenAbsent
                }
            }
            // No reported token hash falls in the clause's value range => the token cannot be present. A hit is only
            // `CouldMatch` because the token filter may report false positives.
            Statistic::TextToken { token_hashes, .. } => {
                let any_match = token_hashes
                    .iter()
                    .any(|&h| h >= clause.value_lo && h <= clause.value_hi);
                if any_match {
                    StatVerdict::CouldMatch
                } else {
                    StatVerdict::ProvenAbsent
                }
            }
        }
    }
}

/// One statistic's contribution to falsifying one filter clause for a granule.
///
/// A term pairs the [`Statistic`] doing the judging with the clause it judges and the `exactness` of the SkipIndex the
/// statistic came from. The exactness matters even when the term keeps the granule: if a granule survives partly on an
/// inexact term, the query must still apply a real filter, because an inexact statistic can let through rows that do
/// not actually match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruningTerm {
    pub clause: FilterClause,
    pub exactness: Exactness,
    pub statistic: Statistic,
}

impl PruningTerm {
    /// What this term concludes for the granule it was built from: it simply asks its statistic to judge its clause.
    pub fn verdict(&self) -> StatVerdict {
        self.statistic.judge(&self.clause)
    }
}

/// The set of statistics one granule declares — possibly several per column, or none at all.
///
/// This is the granule's advertised summary: the planner reads it to build terms and reads it again (for the granule
/// under test) to evaluate them. A column with no statistic here simply contributes no term, which means always-keep —
/// absence of a statistic never drops data.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GranuleStatistics {
    pub statistics: Vec<Statistic>,
}

impl GranuleStatistics {
    /// Finds the statistic of the same shape as `template` (same kind and same column) that this granule declares, if
    /// any.
    ///
    /// Evaluation uses this to look up *this* granule's data for a term that was built from another granule's (or the
    /// directory's) declared shape, so the keep/drop decision reflects the granule actually under test.
    fn matching(&self, template: &Statistic) -> Option<&Statistic> {
        self.statistics
            .iter()
            .find(|s| same_kind(s, template) && s.column_id() == template.column_id())
    }
}

/// The single conservative expression that decides keep-or-drop for a granule, as a flat list of terms plus the filter
/// the terms came from.
///
/// There is exactly one term per declared-and-relevant statistic. A clause that no declared statistic can falsify
/// contributes no term — its always-keep result is implicit in the conjunction. Evaluating the expression is one
/// uniform fold over `terms`; there is deliberately no per-statistic branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruningExpression {
    pub filter: Filter,
    pub terms: Vec<PruningTerm>,
}

impl PruningExpression {
    /// How many terms make up the expression — one per declared-and-relevant statistic.
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }
}

/// The result of evaluating a pruning expression against one granule.
///
/// `drop` is `true` when the granule can be skipped (some term proved a clause unsatisfiable); `false` keeps it.
/// `residual_required` is `true` when any surviving clause was not confirmed outright — because no statistic could
/// speak to it, because the granule declares no statistic of the term's shape, because the statistic is inexact and
/// can let non-matching rows through, or because its keep only fails to rule the granule out (a bounds statistic such
/// as min/max) — meaning the query must keep an exact filter above or inside the scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneOutcome {
    pub drop: bool,
    pub residual_required: bool,
}

/// A query that has been planned against the statistics a granule may carry.
///
/// Either the query is answerable from an exact aggregate block, cube cell, or sketch — in which case it takes the
/// `AggregateShortcut` and skips granule scanning entirely — or it falls through to a `Scan` carrying the pruning
/// expression that will be evaluated per granule. Choosing the shortcut never weakens to min/max-only pruning; the two
/// tiers compose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryPlan {
    AggregateShortcut,
    Scan { pruning: PruningExpression },
}

/// One query plus the one fact pruning needs to know about the shortcut tier: can an exact aggregate / cube / sketch
/// answer it outright?
///
/// The real aggregate blocks live in another capability; here the decision is modelled abstractly as
/// `answerable_from_aggregate`. When that is `true` the planner takes the shortcut; otherwise it builds a
/// granule-pruning scan from `filter`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub answerable_from_aggregate: bool,
    pub filter: Filter,
}

/// Builds the one falsification expression for a filter from the statistics a granule declares.
///
/// For every filter clause, this gathers each declared statistic that covers the clause's column and can speak to its
/// shape, and turns each into a [`PruningTerm`] carrying that statistic's exactness. A clause that no declared
/// statistic can falsify contributes no term — its always-keep result is the default, so absence of a statistic never
/// drops data. The result has exactly one term per declared-and-relevant statistic and is evaluated by the same loop
/// for every granule.
pub fn build_pruning_expression(filter: &Filter, declared: &GranuleStatistics) -> PruningExpression {
    let mut terms = Vec::new();
    for clause in &filter.clauses {
        for statistic in &declared.statistics {
            if !statistic.relevant_to(clause) {
                continue;
            }
            terms.push(PruningTerm {
                clause: *clause,
                exactness: exactness_of(statistic),
                statistic: statistic.clone(),
            });
        }
    }
    PruningExpression {
        filter: filter.clone(),
        terms,
    }
}

/// Evaluates the one expression against one granule to decide keep or drop.
///
/// Every term is judged against the statistic the granule actually carries for it (looked up by kind and column), in
/// one uniform loop. The granule is dropped the moment any term proves a clause unsatisfiable — the conjunction of
/// clauses means one proven-absent clause is enough. If the granule survives and any term that contributed to keeping
/// it is inexact — or is a bounds-only statistic whose keep never proves a row matches — `residual_required` is set so
/// the query keeps an exact filter above or inside the scan.
///
/// The decision equals what per-statistic pruning would produce for the same granule, statistics, and filter, so
/// adopting the single expression changes no query result.
pub fn evaluate(expr: &PruningExpression, granule: &GranuleStatistics) -> PruneOutcome {
    // A clause is only free of its row-level filter when some term confirms it outright. A clause no statistic could
    // speak to contributes no term at all, and a term whose statistic this granule does not declare cannot confirm
    // anything either — both leave the clause unconfirmed, so the exact predicate must still run over the rows.
    let mut confirmed_clauses: Vec<FilterClause> = Vec::new();
    for term in &expr.terms {
        // Judge against this granule's own data for the term's statistic shape; if the granule does not declare it, the
        // term cannot falsify here.
        let Some(stat) = granule.matching(&term.statistic) else {
            continue;
        };
        match stat.judge(&term.clause) {
            StatVerdict::ProvenAbsent => {
                // One clause proven unsatisfiable drops the granule outright; a dropped granule needs no residual
                // filter.
                return PruneOutcome {
                    drop: true,
                    residual_required: false,
                };
            }
            StatVerdict::CouldMatch => {
                // The granule survives this term. It confirms its clause only when the statistic is exact *and* its
                // keep proves a row matches: an inexact statistic can admit non-matching rows, and a bounds-only keep
                // never proves any row satisfies an equality or range clause.
                if !term.exactness.requires_residual_filter() && term.statistic.keep_confirms_match() {
                    confirmed_clauses.push(term.clause);
                }
            }
        }
    }
    PruneOutcome {
        drop: false,
        residual_required: !expr
            .filter
            .clauses
            .iter()
            .all(|clause| confirmed_clauses.contains(clause)),
    }
}

/// Applies page-granularity min/max pruning within a surviving granule, returning the indexes of pages the filter
/// cannot rule out.
///
/// Works the same way as granule-level pruning: each page's declared statistics become a single-granule falsification
/// expression evaluated against the filter. Pages not listed in `page_statistics` carry no statistics and are always
/// kept — absence of a statistic never drops data, so this is conservative and never produces a false negative.
pub fn prune_pages_in_granule(
    filter: &Filter,
    page_statistics: &[(u32, GranuleStatistics)],
    total_page_count: u32,
) -> Vec<u32> {
    let no_stats = GranuleStatistics::default();
    (0..total_page_count)
        .filter(|&page_idx| {
            let stats = page_statistics
                .iter()
                .find(|(pi, _)| *pi == page_idx)
                .map(|(_, s)| s)
                .unwrap_or(&no_stats);
            let expr = build_pruning_expression(filter, stats);
            !evaluate(&expr, stats).drop
        })
        .collect()
}

/// Plans a query, keeping the exact aggregate / cube / sketch shortcut tier above granule pruning.
///
/// When the query is answerable from an exact aggregate block, cube cell, or sketch, this returns
/// [`QueryPlan::AggregateShortcut`] and does not build a granule-pruning scan — the more powerful path answers it, and
/// pruning never downgrades it to min/max-only scanning. Otherwise it returns a [`QueryPlan::Scan`] whose pruning
/// expression is built from the declared statistics.
pub fn plan_query(query: &Query, declared: &GranuleStatistics) -> QueryPlan {
    if query.answerable_from_aggregate {
        return QueryPlan::AggregateShortcut;
    }
    QueryPlan::Scan {
        pruning: build_pruning_expression(&query.filter, declared),
    }
}

/// The exactness of the SkipIndex a statistic comes from.
///
/// Ordered bounds (min/max, sequence, time) and an exact null count are `Exact`; membership, range-emptiness, and
/// path-presence filters may over-report, so they are `InexactNoFalseNegative` and force a residual filter when they
/// keep a granule. Path presence is a Bloom filter (see [`PathPresenceIndex`](super::path_presence::PathPresenceIndex)),
/// so a positive answer is a candidate, never a proof.
fn exactness_of(statistic: &Statistic) -> Exactness {
    match statistic {
        Statistic::MinMaxBound { .. }
        | Statistic::NullCount { .. }
        | Statistic::SequenceBounds { .. }
        | Statistic::TimeBounds { .. } => Exactness::Exact,
        Statistic::PathPresence { .. }
        | Statistic::PointMembership { .. }
        | Statistic::RangeEmptiness { .. }
        | Statistic::TextToken { .. } => Exactness::InexactNoFalseNegative,
    }
}

/// `ProvenAbsent` when the two inclusive ranges do not overlap, else `CouldMatch` — the shared falsification test for
/// every ordered-bound statistic.
fn disjoint(a_lo: i128, a_hi: i128, b_lo: i128, b_hi: i128) -> StatVerdict {
    if is_disjoint(a_lo, a_hi, b_lo, b_hi) {
        StatVerdict::ProvenAbsent
    } else {
        StatVerdict::CouldMatch
    }
}

/// Whether two inclusive ranges `[a_lo, a_hi]` and `[b_lo, b_hi]` share no point.
fn is_disjoint(a_lo: i128, a_hi: i128, b_lo: i128, b_hi: i128) -> bool {
    a_hi < b_lo || b_hi < a_lo
}

/// Whether two statistics are the same kind (ignoring their data), used to pair a term's declared statistic with the
/// granule's own statistic of that kind.
fn same_kind(a: &Statistic, b: &Statistic) -> bool {
    matches!(
        (a, b),
        (Statistic::MinMaxBound { .. }, Statistic::MinMaxBound { .. })
            | (Statistic::NullCount { .. }, Statistic::NullCount { .. })
            | (Statistic::PathPresence { .. }, Statistic::PathPresence { .. })
            | (Statistic::PointMembership { .. }, Statistic::PointMembership { .. })
            | (Statistic::RangeEmptiness { .. }, Statistic::RangeEmptiness { .. })
            | (Statistic::SequenceBounds { .. }, Statistic::SequenceBounds { .. })
            | (Statistic::TextToken { .. }, Statistic::TextToken { .. })
            | (Statistic::TimeBounds { .. }, Statistic::TimeBounds { .. })
    )
}

#[cfg(test)]
#[path = "test/pruning.rs"]
mod tests;
