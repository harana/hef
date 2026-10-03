//! Checks that a declared reference is stored exactly as declared, with nothing ever validating that its target
//! exists: a reply can arrive before its parent, a target can be erased later, and in both cases resolution simply
//! finds nothing — never an error.

use crate::support;
use hef::artifacts::batch::{build_batch, decode_batch};
use hef::events::EventId;
use hef::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef};
use hef::layout::reader::HefFile;
use hef::typed_id::TypedIdTestExt;
use hef::writer::build::{HefRow, build_hef_file};

/// conformance: hef-logical-event-model/relationship-references-accepted-without-referential-integrity/reply-arrives-before-its-parent
#[test]
fn reply_arrives_before_its_parent() {
    // The reply names a parent that is nowhere in this batch — not yet ingested. The append path performs no lookup,
    // so the batch builds and round-trips with the reference stored byte-exactly as declared.
    let parent_id: u128 = 0xDEAD_BEEF;
    let reply = {
        let mut event = support::event(1);
        event.relationships = Some(
            EventRelationships::new(vec![RelationshipRef::to_event(RelationshipKind::Parent, parent_id)]).unwrap(),
        );
        event
    };
    let payload = build_batch(std::slice::from_ref(&reply), 1, 0).unwrap();
    let decoded = decode_batch(&payload, 1).unwrap();
    assert_eq!(
        decoded.events.first().and_then(|event| event.relationships.clone()),
        reply.relationships,
        "the declared reference commits unchanged, target existence never checked"
    );

    // Sealed into a file, resolving the dangling reference yields empty — an answer, not an error.
    let built = build_hef_file(
        vec![HefRow {
            epoch: 1,
            sequence: 1,
            event: reply,
        }],
        &support::build_config(),
    )
    .unwrap();
    let file = HefFile::open(built.bytes, None).unwrap();
    let reference = RelationshipRef::to_event(RelationshipKind::Parent, parent_id);
    assert_eq!(file.resolve_reference(&reference).unwrap(), Vec::<u64>::new());

    // Once the parent is ingested (a later file), the very same reference resolves to it.
    let parent = {
        let mut event = support::event(0);
        event.envelope.event_id = EventId::new_test_id(parent_id);
        event
    };
    let later = build_hef_file(
        vec![HefRow {
            epoch: 1,
            sequence: 2,
            event: parent,
        }],
        &support::build_config(),
    )
    .unwrap();
    let later = HefFile::open(later.bytes, None).unwrap();
    assert_eq!(later.resolve_reference(&reference).unwrap(), vec![0]);
}

/// conformance: hef-logical-event-model/relationship-references-accepted-without-referential-integrity/target-erased-after-commit
#[test]
fn target_erased_after_commit() {
    // Rows 1 and 2 reference row 0's event id; a file re-sealed without the target (erasure, retention expiry) leaves
    // the referencing rows byte-identical and their references simply unresolvable.
    let target: u128 = 0xC0FFEE;
    let referencing = |i: u64| {
        let mut event = support::event(i);
        event.relationships =
            Some(EventRelationships::new(vec![RelationshipRef::to_event(RelationshipKind::Link, target)]).unwrap());
        event
    };
    let built = build_hef_file(
        vec![
            HefRow {
                epoch: 1,
                sequence: 1,
                event: referencing(1),
            },
            HefRow {
                epoch: 1,
                sequence: 2,
                event: referencing(2),
            },
        ],
        &support::build_config(),
    )
    .unwrap();
    let file = HefFile::open(built.bytes, None).unwrap();
    // The references are still declared and still filterable...
    let reference = RelationshipRef::to_event(RelationshipKind::Link, target);
    assert_eq!(
        file.rows_referencing(RelationshipKind::Link, reference.space, &reference.target_ref)
            .unwrap(),
        vec![0, 1]
    );
    // ...while resolving them in a world without the target returns empty, not an error.
    assert_eq!(file.resolve_reference(&reference).unwrap(), Vec::<u64>::new());
}
