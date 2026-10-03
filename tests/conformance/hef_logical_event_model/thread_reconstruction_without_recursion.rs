//! Checks that a whole chat thread comes back with one equality filter on the denormalized root reference — no
//! walking up parent chains — and that a thread's root is recognized by carrying no references at all.

use crate::support;
use hef::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};
use hef::layout::reader::HefFile;
use hef::writer::build::{HefRow, build_hef_file};

/// A conversation: row 0 is the root (no references); every reply `i > 0` declares its parent (the previous message)
/// and the denormalized root, exactly as a threading protocol's root/reply markers arrive at ingest.
fn conversation(messages: u64) -> HefFile {
    let root_id: u128 = 0xC0FFEE;
    let rows: Vec<HefRow> = (0..messages)
        .map(|i| {
            let mut event = support::event(i);
            if i > 0 {
                event.relationships = Some(
                    EventRelationships::new(vec![
                        RelationshipRef::to_event(RelationshipKind::Parent, root_id + u128::from(i) - 1),
                        RelationshipRef::to_event(RelationshipKind::Root, root_id),
                    ])
                    .unwrap(),
                );
            }
            HefRow {
                epoch: 1,
                sequence: i + 1,
                event,
            }
        })
        .collect();
    HefFile::open(build_hef_file(rows, &support::build_config()).unwrap().bytes, None).unwrap()
}

/// conformance: hef-logical-event-model/thread-reconstruction-without-recursion/whole-conversation-in-one-filter
#[test]
fn whole_conversation_in_one_filter() {
    let file = conversation(24);
    // One equality lookup on the root reference returns every reply, ordered by the envelope — 23 messages without a
    // single parent-chain hop.
    let thread = file
        .thread_rows(TargetIdSpace::EventId, &0xC0FFEEu128.to_be_bytes())
        .unwrap();
    assert_eq!(thread, (1..24).collect::<Vec<u64>>());
    // The parent chain is still there for a reply-to view, but reconstruction never depends on walking it.
    let children_of_root = file
        .rows_referencing(
            RelationshipKind::Parent,
            TargetIdSpace::EventId,
            &0xC0FFEEu128.to_be_bytes(),
        )
        .unwrap();
    assert_eq!(children_of_root, vec![1], "direct replies to the root");
}

/// conformance: hef-logical-event-model/thread-reconstruction-without-recursion/root-marked-by-absence
#[test]
fn root_marked_by_absence() {
    let file = conversation(4);
    // The root declared nothing: it appears in no thread lookup and stores no self-reference.
    let thread = file
        .thread_rows(TargetIdSpace::EventId, &0xC0FFEEu128.to_be_bytes())
        .unwrap();
    assert!(
        !thread.contains(&0),
        "the root is fetched by its own identity, not by a self-reference"
    );
    // Absence is the marker: row 0's relationship slots are simply not present.
    let root_column = file.read_column(hef::columns::column_ids::ROOT_REF, 0).unwrap();
    assert!(
        !root_column.presence.first().is_some_and(|byte| byte & 1 != 0),
        "row 0 carries no root reference"
    );
}
