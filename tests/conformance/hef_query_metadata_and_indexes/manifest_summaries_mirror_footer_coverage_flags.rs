//! Checks that a file's late-event coverage flag reaches its manifest summary, so a planner sees the file's
//! `occurred_at` coverage is not implied by its ingest order without ever opening the file.

use crate::support;
use hef::artifacts::batch::EventInput;
use hef::events::TimestampValue;
use hef::indexes::summary::summary_from_footer;
use hef::writer::build::{HefRow, build_hef_file};

fn row_with_occurred(i: u64, occurred_nanos: i64) -> HefRow {
    let mut event: EventInput = support::event(i);
    event.envelope.occurred_at = TimestampValue::from_physical_nanos(occurred_nanos);
    HefRow {
        epoch: 1,
        event,
        sequence: i + 1,
    }
}

/// conformance: hef-query-metadata-and-indexes/manifest-summaries-mirror-footer-coverage-flags/late-event-file-is-flagged-in-its-manifest-summary
#[test]
fn late_event_file_is_flagged_in_its_manifest_summary() {
    // A file whose occurred_at rises with ingest (epoch, sequence) order carries no late event; its manifest summary
    // must have has_late_events clear.
    let in_order: Vec<HefRow> = (0..8)
        .map(|i| row_with_occurred(i, 1_000_000 + i as i64 * 1_000))
        .collect();
    let ordered_file = build_hef_file(in_order.clone(), &support::build_config()).unwrap();
    assert!(
        !summary_from_footer(&ordered_file.footer).has_late_events,
        "a file with occurred_at in ingest order must not be flagged as carrying late events"
    );

    // Replace the last-ingested row with a late event: it still sits at the highest sequence, but its occurred_at
    // predates every earlier row. The manifest summary must surface has_late_events so a planner sees that coverage
    // without opening the file.
    let mut late = in_order;
    late.pop();
    late.push(row_with_occurred(7, 1));
    let late_file = build_hef_file(late, &support::build_config()).unwrap();
    assert!(
        summary_from_footer(&late_file.footer).has_late_events,
        "a row whose occurred_at predates an earlier-ingested row must flag the file as carrying late events"
    );
}
