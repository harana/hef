//! Checks that the references an event declares about other events (its parent, thread root, links) are stored as an
//! ordinary optional column family: streams that declare none pay nothing, reverse lookups are plain equality
//! filters, and no relationship ever reads as a causal claim.

use crate::support;
use hef::columns::column_ids;
use hef::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};
use hef::layout::reader::HefFile;
use hef::writer::build::{HefRow, build_hef_file};

/// Rows 0..count from the shared fixture, with `declare(i)` naming each row's references.
fn built_with_relationships(
    count: u64,
    declare: impl Fn(u64) -> Option<EventRelationships>,
) -> hef::writer::build::BuiltHef {
    let rows: Vec<HefRow> = (0..count)
        .map(|i| {
            let mut event = support::event(i);
            event.relationships = declare(i);
            HefRow {
                epoch: 1,
                sequence: i + 1,
                event,
            }
        })
        .collect();
    build_hef_file(rows, &support::build_config()).unwrap()
}

/// The fixture's event id for row `i`, as relationship target bytes.
fn event_target(i: u64) -> u128 {
    0xC0FFEE + u128::from(i)
}

/// conformance: hef-logical-event-model/event-relationship-references-column-family/unrelated-stream-pays-nothing
#[test]
fn unrelated_stream_pays_nothing() {
    // No row declares a relationship, so the family is absent: no relationship column is declared at all, and the
    // envelope columns are exactly those of a file built before the family existed.
    let built = built_with_relationships(12, |_| None);
    let file = HefFile::open(built.bytes, None).unwrap();
    assert!(
        !file.footer().columns.iter().any(
            |column| (column_ids::RELATIONSHIP_BASE..column_ids::RELATIONSHIP_BASE + 6).contains(&column.column_id)
        ),
        "a stream without relationships materializes no relationship columns"
    );
    // And a lookup over the absent family finds nothing rather than failing.
    assert_eq!(
        file.rows_referencing(
            RelationshipKind::Parent,
            TargetIdSpace::EventId,
            &event_target(0).to_be_bytes()
        )
        .unwrap(),
        Vec::<u64>::new()
    );
}

/// conformance: hef-logical-event-model/event-relationship-references-column-family/children-of-an-event-resolve-by-filter
#[test]
fn children_of_an_event_resolve_by_filter() {
    // Rows 3 and 7 declare row 0 as their parent; row 5 declares row 1. "Children of 0" is a pushdown equality
    // lookup on the parent-reference column — no traversal, and row 0's own stored bytes are never consulted.
    let built = built_with_relationships(10, |i| match i {
        3 | 7 => Some(
            EventRelationships::new(vec![RelationshipRef::to_event(
                RelationshipKind::Parent,
                event_target(0),
            )])
            .unwrap(),
        ),
        5 => Some(
            EventRelationships::new(vec![RelationshipRef::to_event(
                RelationshipKind::Parent,
                event_target(1),
            )])
            .unwrap(),
        ),
        _ => None,
    });
    let file = HefFile::open(built.bytes, None).unwrap();
    let children = file
        .rows_referencing(
            RelationshipKind::Parent,
            TargetIdSpace::EventId,
            &event_target(0).to_be_bytes(),
        )
        .unwrap();
    assert_eq!(children, vec![3, 7], "exactly the declaring rows, in file order");
    // The parent itself declared nothing: its slot in the relationship columns is absent, proving the edge lives on
    // the referencing event alone.
    let parent_column = file.read_column(column_ids::PARENT_REF, 0).unwrap();
    assert!(
        !parent_column.presence.first().is_some_and(|byte| byte & 1 != 0),
        "row 0 (the parent) carries no relationship value of its own"
    );
}

/// conformance: hef-logical-event-model/event-relationship-references-column-family/relationship-never-licenses-causal-claims
#[test]
fn relationship_never_licenses_causal_claims() {
    // The kind registry asserts structure only: reply, grouping, reference. No causal kind exists to store, so no
    // stored relationship can ever read back as "X caused Y" — causal phrasing stays licensed solely by the Revenue
    // Intelligence corroboration paths.
    let structural: Vec<&str> = RelationshipKind::ALL.iter().map(|kind| kind.as_str()).collect();
    assert_eq!(structural, vec!["auth", "link", "parent", "prev", "related", "root"]);
    for causal in ["caused_by", "causes", "driver_of"] {
        assert_eq!(
            RelationshipKind::from_str(causal),
            None,
            "{causal} is not in the registry"
        );
    }
}
