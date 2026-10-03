//! A catalog-level dictionary shared across all HEF files in a manifest file-set.
//!
//! Low-cardinality envelope columns (source_id, event_type_id, entity_type_id) use a single stable dictionary so a
//! predicate literal translates to one code that is valid across every file in the set, and code-space skip metadata is
//! directly comparable without per-file re-translation.

use super::predicate::StringPredicate;
use std::collections::BTreeSet;

/// A catalog-level dictionary shared across all HEF files in a manifest file-set.
///
/// Holds the stable sorted list of distinct strings for a coded envelope column (source_id, event_type_id, or
/// entity_type_id), scoped to a manifest version. The manifest references this dictionary by pointer and version rather
/// than embedding it inline, consistent with the metadata-placement discipline.
pub struct GlobalDictionary {
    entries: Vec<String>,
}

/// A predicate translated into global code-space, produced once from a [`GlobalDictionary`] and reused across every
/// file in the manifest file-set without per-file re-translation.
pub struct GlobalCodeFilter {
    /// The verdict for a local value absent from the global dictionary, decided once at translation. `false` only
    /// when absence alone proves the predicate cannot match — every needle is present in the dictionary, so an
    /// absent value can equal none of them. `true` otherwise: the value's string is unknown at match time, so it is
    /// kept rather than dropped (a range, or a needle itself absent, could still match it).
    absent_local_matches: bool,
    kind: FilterKind,
}

enum FilterKind {
    /// NotEquals where the excluded value is absent from the global dictionary: every global code passes (a value
    /// present in the dictionary cannot equal a needle that is not).
    NotEqualsAbsent,
    AcceptsNone,
    ExcludeCode(u64),
    Range {
        hi: u64,
        lo: u64,
    },
    Set(Vec<u64>),
}

impl GlobalDictionary {
    /// Creates a global dictionary. `entries` must already be sorted in byte order and deduplicated — the same order
    /// the file-level dictionary encoding uses, so codes map directly.
    pub fn from_sorted(entries: Vec<String>) -> Self {
        Self { entries }
    }

    /// Builds the dictionary for a manifest file-set out of the file-scope alphabets its files already carry.
    ///
    /// Each file's footer stores its shared alphabet strictly ascending, and this merges them into the one ascending,
    /// deduplicated list [`from_sorted`](Self::from_sorted) requires — so the code a value gets here is its position
    /// in the merged order, which is what makes a range predicate translatable once for the whole set. A file whose
    /// own alphabet predates the merge maps through [`local_to_global_map`](Self::local_to_global_map).
    pub fn from_file_alphabets<'a>(alphabets: impl IntoIterator<Item = &'a [String]>) -> Self {
        let merged: BTreeSet<&str> = alphabets.into_iter().flatten().map(String::as_str).collect();
        Self::from_sorted(merged.into_iter().map(str::to_owned).collect())
    }

    /// Returns the global code for a string, or `None` if the value is absent from this dictionary.
    pub fn code_for(&self, value: &str) -> Option<u64> {
        self.entries
            .binary_search_by_key(&value, |e| e.as_str())
            .ok()
            .map(|i| i as u64)
    }

    /// Translates a predicate into global code-space. Call this once per query and hand the resulting
    /// [`GlobalCodeFilter`] to every file in the set — no per-file re-translation is needed.
    pub fn translate(&self, predicate: &StringPredicate) -> GlobalCodeFilter {
        let kind = match predicate {
            // Substring order does not follow the dictionary's, so every entry is tested — one search per distinct
            // value in the whole file set, not per row.
            StringPredicate::Contains(matcher) => FilterKind::Set(
                self.entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| matcher.matches(entry))
                    .map(|(index, _)| index as u64)
                    .collect(),
            ),
            // `code_for` already binary-searches `self.entries` directly, so no `Vec<&str>` copy of the dictionary
            // is needed to resolve a needle to its code.
            StringPredicate::Equals(needle) => match self.code_for(needle) {
                Some(i) => FilterKind::Range { lo: i, hi: i + 1 },
                None => FilterKind::AcceptsNone,
            },
            StringPredicate::NotEquals(needle) => match self.code_for(needle) {
                Some(i) => FilterKind::ExcludeCode(i),
                None => FilterKind::NotEqualsAbsent,
            },
            StringPredicate::InSet(needles) => {
                let mut codes: Vec<u64> = needles.iter().filter_map(|n| self.code_for(n)).collect();
                codes.sort_unstable();
                codes.dedup();
                FilterKind::Set(codes)
            }
            StringPredicate::Range { lower, upper } => {
                let lo = match lower {
                    Some(b) if b.inclusive => self.entries.partition_point(|e| e.as_str() < b.value.as_str()),
                    Some(b) => self.entries.partition_point(|e| e.as_str() <= b.value.as_str()),
                    None => 0,
                };
                let hi = match upper {
                    Some(b) if b.inclusive => self.entries.partition_point(|e| e.as_str() <= b.value.as_str()),
                    Some(b) => self.entries.partition_point(|e| e.as_str() < b.value.as_str()),
                    None => self.entries.len(),
                };
                if lo >= hi {
                    FilterKind::AcceptsNone
                } else {
                    FilterKind::Range {
                        lo: lo as u64,
                        hi: hi as u64,
                    }
                }
            }
        };
        // A local value absent from the dictionary can only be ruled out when every needle is present; a needle
        // that is itself absent, or a range, could still match a string this dictionary has never seen.
        let absent_local_matches = match predicate {
            // A string this dictionary has never seen could still contain the needle.
            StringPredicate::Contains(_) => true,
            StringPredicate::Equals(needle) => self.code_for(needle).is_none(),
            StringPredicate::InSet(needles) => needles.iter().any(|needle| self.code_for(needle).is_none()),
            StringPredicate::NotEquals(_) => true,
            StringPredicate::Range { .. } => true,
        };
        GlobalCodeFilter {
            absent_local_matches,
            kind,
        }
    }

    /// Builds a local-to-global code map for a file whose local dictionary predates the shared dictionary or was
    /// written against an older version.
    ///
    /// Returns one slot per local code index: `Some(global_code)` when the string at that index appears in the global
    /// dictionary, `None` when absent. A `None` slot gets the pruning-safe verdict described on
    /// [`GlobalCodeFilter::matches_local_code`].
    pub fn local_to_global_map(&self, local_entries: &[String]) -> Vec<Option<u64>> {
        local_entries.iter().map(|e| self.code_for(e.as_str())).collect()
    }
}

impl GlobalCodeFilter {
    /// Whether a global code passes this filter.
    pub fn matches_global_code(&self, code: u64) -> bool {
        match &self.kind {
            FilterKind::NotEqualsAbsent => true,
            FilterKind::AcceptsNone => false,
            FilterKind::ExcludeCode(excluded) => code != *excluded,
            FilterKind::Range { hi, lo } => code >= *lo && code < *hi,
            FilterKind::Set(codes) => codes.binary_search(&code).is_ok(),
        }
    }

    /// Whether a local code from a pre-dictionary file passes this filter after remapping through the local-to-global
    /// map. A local code that maps to `None` is a value absent from the global dictionary: its string is unknown
    /// here, so the verdict is pruning-safe rather than exact — it is dropped only when every needle is present in
    /// the dictionary (then an absent value can equal none of them) and kept whenever the predicate could still
    /// match a string outside the dictionary. A local code beyond the map never matches.
    pub fn matches_local_code(&self, local_code: u64, local_to_global: &[Option<u64>]) -> bool {
        match local_to_global.get(local_code as usize) {
            Some(&Some(global_code)) => self.matches_global_code(global_code),
            Some(&None) => self.absent_local_matches,
            _ => false,
        }
    }
}

#[cfg(test)]
#[path = "test/global_dict.rs"]
mod tests;
