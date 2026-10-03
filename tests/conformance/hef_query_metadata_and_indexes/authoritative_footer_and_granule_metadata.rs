//! Checks that once a surviving file is opened, planning reads its granule metadata from the authoritative footer
//! directories — not from the coarse manifest summary copy, which never substitutes for footer validation.

use crate::support;
use hef::indexes::summary::{footer_is_authoritative_for_planning, plan_granules_from_footer, summary_from_footer};

/// conformance: hef-query-metadata-and-indexes/authoritative-footer-and-granule-metadata/footer-drives-planning
#[test]
fn footer_drives_planning() {
    // A surviving file is opened: planning uses the FOOTER directories as authoritative (e.g. footer.granules), not the
    // manifest summary copy, and the summary does not substitute for footer validation. Build a real footer via
    // support::built_file, derive a ManifestSummary from it, and assert planning reads granule metadata from the
    // footer.
    let built = support::built_file(8);
    let footer = &built.footer;

    // The coarse summary is derived from the footer but is only a pre-filter.
    let summary = summary_from_footer(footer);
    assert_eq!(summary.granule_count as usize, footer.granules.len());

    // Planning reads the granule ids from the footer's authoritative directory.
    let planned = plan_granules_from_footer(footer);
    assert!(!footer.granules.is_empty(), "fixture should build granules");
    assert!(footer_is_authoritative_for_planning());
    let from_footer: Vec<u32> = footer.granules.iter().map(|g| g.granule_id).collect();
    assert_eq!(planned, from_footer);

    // The summary carries only a count, not the directory; it cannot stand in for the footer's per-granule metadata
    // that planning actually consumes.
    assert_eq!(planned.len(), summary.granule_count as usize);
}
