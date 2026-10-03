//! Checks that the manifest summary lets a planner skip a file whose coverage cannot overlap the query, so the file is
//! pruned without ever being opened.

use crate::support;
use hef::indexes::summary::{FilePredicate, can_prune_file, summary_from_footer};

/// conformance: hef-query-metadata-and-indexes/layered-metadata-hierarchy-to-minimize-reads/prune-without-opening-files
#[test]
fn prune_without_opening_files() {
    // A predicate whose range falls outside a file's manifest summary coverage -> can_prune_file(..) is true, so the
    // file is pruned WITHOUT being opened (we model "open" with a counter that stays zero on the pruned path).
    let built = support::built_file(8);
    let summary = summary_from_footer(&built.footer);

    // Choose a predicate strictly above the file's sequence coverage so it cannot match any row.
    let predicate = FilePredicate {
        min_sequence: Some(summary.max_sequence + 1_000),
        ..FilePredicate::default()
    };

    let mut files_opened = 0usize;
    if !can_prune_file(&summary, &predicate) {
        // Only a non-pruned file would be opened; this branch must not run.
        files_opened += 1;
        let _ = &built.bytes;
    }

    assert!(
        can_prune_file(&summary, &predicate),
        "out-of-range predicate must prune the file"
    );
    assert_eq!(files_opened, 0, "pruned file must not be opened");

    // A predicate inside the coverage must NOT prune (the file would be opened).
    let overlapping = FilePredicate {
        min_sequence: Some(summary.min_sequence),
        max_sequence: Some(summary.max_sequence),
        ..FilePredicate::default()
    };
    assert!(!can_prune_file(&summary, &overlapping));
}
