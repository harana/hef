use super::*;
use crate::error::RelationshipError;

#[test]
fn a_reference_round_trips_through_its_column_value() {
    let reference = RelationshipRef::to_event(RelationshipKind::Parent, 0x1234_5678_9abc_def0_1122_3344_5566_7788);
    let value = reference.column_value();
    assert!(
        value.starts_with("event_id:"),
        "space tag leads the stored form: {value}"
    );
    let parsed = RelationshipRef::parse_column_value(RelationshipKind::Parent, &value).expect("canonical form parses");
    assert_eq!(parsed, reference);

    let protocol = RelationshipRef::to_protocol(RelationshipKind::Root, [0xab; 32]);
    let parsed = RelationshipRef::parse_column_value(RelationshipKind::Root, &protocol.column_value()).expect("parses");
    assert_eq!(parsed, protocol);
}

#[test]
fn a_target_of_the_wrong_width_is_refused() {
    assert_eq!(
        RelationshipRef::new(RelationshipKind::Link, TargetIdSpace::EventId, vec![0u8; 3]),
        Err(RelationshipError::WrongTargetLength)
    );
}

#[test]
fn at_most_one_parent_and_one_root() {
    let two_parents = vec![
        RelationshipRef::to_event(RelationshipKind::Parent, 1),
        RelationshipRef::to_event(RelationshipKind::Parent, 2),
    ];
    assert_eq!(
        EventRelationships::new(two_parents),
        Err(RelationshipError::MoreThanOneParent)
    );
    let two_roots = vec![
        RelationshipRef::to_event(RelationshipKind::Root, 1),
        RelationshipRef::to_event(RelationshipKind::Root, 2),
    ];
    assert_eq!(
        EventRelationships::new(two_roots),
        Err(RelationshipError::MoreThanOneRoot)
    );
}

#[test]
fn multi_valued_kinds_join_and_split_in_declaration_order() {
    let relationships = EventRelationships::new(vec![
        RelationshipRef::to_event(RelationshipKind::Link, 7),
        RelationshipRef::to_protocol(RelationshipKind::Link, [1; 32]),
        RelationshipRef::to_event(RelationshipKind::Parent, 9),
    ])
    .expect("one parent is allowed");

    let text = relationships.column_text(RelationshipKind::Link).expect("two links");
    let parsed = EventRelationships::parse_column_text(RelationshipKind::Link, &text).expect("round-trips");
    assert_eq!(parsed.len(), 2);
    assert_eq!(
        parsed.first(),
        Some(&RelationshipRef::to_event(RelationshipKind::Link, 7))
    );
    assert_eq!(relationships.column_text(RelationshipKind::Related), None);
    assert_eq!(
        relationships.parent(),
        Some(&RelationshipRef::to_event(RelationshipKind::Parent, 9))
    );
    assert_eq!(relationships.root(), None, "absence marks a root or unthreaded event");
}

#[test]
fn registry_tags_round_trip_and_unknown_tags_refuse() {
    for kind in RelationshipKind::ALL {
        assert_eq!(RelationshipKind::from_str(kind.as_str()), Some(kind));
    }
    assert_eq!(RelationshipKind::from_str("caused_by"), None);
    assert_eq!(TargetIdSpace::from_str("entity_id"), None);
    assert_eq!(
        RelationshipRef::parse_column_value(RelationshipKind::Link, "entity_id:00"),
        Err(RelationshipError::UnknownSpace)
    );
    assert_eq!(
        RelationshipRef::parse_column_value(RelationshipKind::Link, "no-colon"),
        Err(RelationshipError::MalformedColumnValue)
    );
}
