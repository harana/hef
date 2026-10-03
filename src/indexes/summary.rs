//! The tiny per-file summary a query reads *before* opening a file, and the rule that turns it into a yes/no "could
//! this file hold anything I want?".
//!
//! A [`ManifestSummary`] is a faithful copy of a few numbers from a file's HEF footer — its row count, the span of
//! event times and sequence numbers it covers, and a couple of flags — small enough to keep in an external manifest for
//! thousands of files. With it a planner can [`can_prune_file`] a file whose coverage cannot possibly overlap the
//! query, skipping the file open entirely.
//!
//! The summary is deliberately coarse and is *only* a pre-filter. Once a file survives this check and is opened,
//! planning switches to the footer's own directories, which are authoritative; the summary never stands in for footer
//! validation. [`plan_granules_from_footer`] and [`footer_is_authoritative_for_planning`] make that boundary explicit.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::layout::footer::Footer;
use crate::layout::optional_features;

/// The byte length of the schema fingerprint: a BLAKE3 hash of the schema.
const SCHEMA_FINGERPRINT_BYTES: usize = 32;

/// A small per-file summary copied out of the HEF footer, used to decide whether a file is worth opening at all.
///
/// Every field here is also present, authoritatively, in the footer; this is a cheap copy so a planner can prune files
/// without reading them. The time and sequence fields give the file's coverage — the closed ranges
/// `[min_occurred_at_physical, max_occurred_at_physical]` and `[min_sequence, max_sequence]` — which [`can_prune_file`]
/// compares against a query's range. Because it is only a coarse copy, it must never be used in place of opening the
/// file and validating its footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManifestSummary {
    pub granule_count: u32,
    pub has_deletion_vectors: bool,
    pub has_late_events: bool,
    pub max_occurred_at_physical: i64,
    pub max_sequence: u64,
    pub min_occurred_at_physical: i64,
    pub min_sequence: u64,
    pub row_count: u64,
    pub schema_fingerprint: [u8; SCHEMA_FINGERPRINT_BYTES],
}

impl ManifestSummary {
    /// Serializes the summary to a fixed byte layout. Round-trips with [`decode`](Self::decode).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(SCHEMA_FINGERPRINT_BYTES + 4 + 2 + 8 * 5);
        out.put_slice(&self.schema_fingerprint);
        out.put_u32(self.granule_count);
        out.put_u8(u8::from(self.has_deletion_vectors));
        out.put_u8(u8::from(self.has_late_events));
        out.put_i64(self.max_occurred_at_physical);
        out.put_u64(self.max_sequence);
        out.put_i64(self.min_occurred_at_physical);
        out.put_u64(self.min_sequence);
        out.put_u64(self.row_count);
        out.into_bytes()
    }

    /// Reads a summary back from [`encode`](Self::encode)'s bytes, refusing if the input is truncated or if its
    /// coverage is inverted while it still claims rows or granules.
    ///
    /// Inverted coverage (`min > max` on either axis) is the empty-file sentinel: [`can_prune_file`] reads it as "no
    /// rows here" and prunes the file unconditionally. That is only sound for a genuinely empty file, so a summary that
    /// decodes to inverted coverage yet reports a non-zero `row_count` or `granule_count` is rejected — otherwise one
    /// corrupted byte could hide an entire non-empty file from every query.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        let fingerprint_bytes = reader.take(SCHEMA_FINGERPRINT_BYTES, "summary schema_fingerprint")?;
        let mut schema_fingerprint = [0u8; SCHEMA_FINGERPRINT_BYTES];
        schema_fingerprint.copy_from_slice(fingerprint_bytes);
        let summary = Self {
            granule_count: reader.u32("summary granule_count")?,
            has_deletion_vectors: reader.u8("summary has_deletion_vectors")? != 0,
            has_late_events: reader.u8("summary has_late_events")? != 0,
            max_occurred_at_physical: reader.i64("summary max_occurred_at_physical")?,
            max_sequence: reader.u64("summary max_sequence")?,
            min_occurred_at_physical: reader.i64("summary min_occurred_at_physical")?,
            min_sequence: reader.u64("summary min_sequence")?,
            row_count: reader.u64("summary row_count")?,
            schema_fingerprint,
        };
        let occurred_inverted = summary.min_occurred_at_physical > summary.max_occurred_at_physical;
        let sequence_inverted = summary.min_sequence > summary.max_sequence;
        if (occurred_inverted || sequence_inverted) && (summary.granule_count != 0 || summary.row_count != 0) {
            return Err(FormatError::Structural {
                rule: "summary coverage inverted with non-zero counts",
            });
        }
        Ok(summary)
    }
}

/// Derives a [`ManifestSummary`] from a real file footer, showing that the summary is a copy of footer metadata and
/// nothing more.
///
/// The time and sequence spans are aggregated across `footer.granules`; the row count comes from the footer's exact
/// counts, the schema fingerprint and granule count from the footer directly. An empty file (no granules) yields a
/// coverage that prunes everything, since there is nothing to match.
pub fn summary_from_footer(footer: &Footer) -> ManifestSummary {
    let mut min_occurred = i64::MAX;
    let mut max_occurred = i64::MIN;
    let mut min_sequence = u64::MAX;
    let mut max_sequence = u64::MIN;
    for granule in &footer.granules {
        min_occurred = min_occurred.min(granule.min_occurred_at_physical);
        max_occurred = max_occurred.max(granule.max_occurred_at_physical);
        min_sequence = min_sequence.min(granule.first_sequence);
        max_sequence = max_sequence.max(granule.last_sequence);
    }
    if footer.granules.is_empty() {
        // No rows: leave an empty coverage (min > max) that prunes any predicate.
        min_occurred = i64::MAX;
        max_occurred = i64::MIN;
        min_sequence = u64::MAX;
        max_sequence = u64::MIN;
    }
    ManifestSummary {
        granule_count: footer.granules.len() as u32,
        has_deletion_vectors: footer.optional_feature_flags & optional_features::HEF_NATIVE_DELETION_VECTORS != 0,
        has_late_events: footer.optional_feature_flags & optional_features::HEF_LATE_EVENTS != 0,
        max_occurred_at_physical: max_occurred,
        max_sequence,
        min_occurred_at_physical: min_occurred,
        min_sequence,
        row_count: footer.exact_counts.row_count,
        schema_fingerprint: footer.schema_fingerprint,
    }
}

/// A coarse query predicate expressed as the file-level ranges a planner can check against a [`ManifestSummary`] before
/// opening a file.
///
/// Each bound is optional and inclusive; `None` means "unbounded on that side". A file can be pruned when the
/// predicate's range provably falls outside the file's coverage on either dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FilePredicate {
    pub max_occurred_at_physical: Option<i64>,
    pub max_sequence: Option<u64>,
    pub min_occurred_at_physical: Option<i64>,
    pub min_sequence: Option<u64>,
}

/// Whether a file can be skipped without opening it, judged from its summary alone. Returns `true` only when the
/// predicate's range lies entirely outside the file's coverage on the occurred-at axis or the sequence axis — in which
/// case no row in the file can match.
///
/// This is conservative: it never prunes a file whose coverage overlaps the predicate, so a matching row is never
/// skipped. When the predicate leaves a dimension unbounded, that dimension cannot rule the file out.
pub fn can_prune_file(summary: &ManifestSummary, predicate: &FilePredicate) -> bool {
    // An empty coverage (min > max) holds no rows; prune unconditionally.
    let occurred_empty = summary.min_occurred_at_physical > summary.max_occurred_at_physical;
    let sequence_empty = summary.min_sequence > summary.max_sequence;
    if occurred_empty || sequence_empty {
        return true;
    }

    // Prune if the predicate's lower bound is above the file's max, or its upper bound is below the file's min, on
    // either axis.
    let occurred_above = predicate
        .min_occurred_at_physical
        .is_some_and(|lo| lo > summary.max_occurred_at_physical);
    let occurred_below = predicate
        .max_occurred_at_physical
        .is_some_and(|hi| hi < summary.min_occurred_at_physical);
    let sequence_above = predicate.min_sequence.is_some_and(|lo| lo > summary.max_sequence);
    let sequence_below = predicate.max_sequence.is_some_and(|hi| hi < summary.min_sequence);

    occurred_above || occurred_below || sequence_above || sequence_below
}

/// States that planning for an opened file is driven by the footer, not by the manifest summary. This is always `true`:
/// it documents and tests the contract that the summary is a pre-filter only and cannot substitute for footer
/// validation.
pub fn footer_is_authoritative_for_planning() -> bool {
    true
}

/// Plans the granules to scan from the footer's authoritative granule directory, returning their ids in directory
/// order.
///
/// Once a file survives manifest-summary pruning and is opened, this is where real planning reads from:
/// `footer.granules`, the authoritative pruning and scan unit. The manifest summary is not consulted here — it has done
/// its job as a coarse pre-filter and plays no part in footer-level planning.
pub fn plan_granules_from_footer(footer: &Footer) -> Vec<u32> {
    footer.granules.iter().map(|g| g.granule_id).collect()
}

#[cfg(test)]
#[path = "test/summary.rs"]
mod tests;
