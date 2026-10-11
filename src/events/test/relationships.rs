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

#[test]
fn every_target_space_is_listed_in_alphabetical_order_and_round_trips() {
    // An exhaustive match, so a new space fails to compile here until it is listed.
    let listed = |space: TargetIdSpace| match space {
        TargetIdSpace::EventId
        | TargetIdSpace::ExternalId
        | TargetIdSpace::ProtocolEventId
        | TargetIdSpace::SubjectSequence => TargetIdSpace::ALL.contains(&space),
    };
    let names: Vec<String> = TargetIdSpace::ALL.iter().map(|space| format!("{space:?}")).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
    for space in TargetIdSpace::ALL {
        assert!(listed(space));
        assert_eq!(TargetIdSpace::from_str(space.as_str()), Some(space));
    }
}

#[test]
fn a_subject_sequence_target_is_eight_big_endian_bytes() {
    let reference = RelationshipRef::to_subject_sequence(RelationshipKind::Prev, 0x0102);
    assert_eq!(reference.target_ref, [0, 0, 0, 0, 0, 0, 1, 2]);
    assert_eq!(reference.column_value(), "subject_sequence:0000000000000102");
    assert_eq!(
        RelationshipRef::parse_column_value(RelationshipKind::Prev, &reference.column_value()),
        Ok(reference)
    );
    assert_eq!(
        RelationshipRef::new(RelationshipKind::Prev, TargetIdSpace::SubjectSequence, vec![1; 4]),
        Err(RelationshipError::WrongTargetLength)
    );
}

#[test]
fn prev_and_auth_references_repeat_and_round_trip() {
    let mut refs: Vec<RelationshipRef> = (0..20u8)
        .map(|i| RelationshipRef::to_external(RelationshipKind::Prev, format!("$prev{i}").as_bytes()).unwrap())
        .collect();
    refs.extend(
        (0..10u8)
            .map(|i| RelationshipRef::to_external(RelationshipKind::Auth, format!("$auth{i}").as_bytes()).unwrap()),
    );
    let relationships = EventRelationships::new(refs.clone()).expect("prev and auth repeat freely");
    for kind in [RelationshipKind::Prev, RelationshipKind::Auth] {
        let text = relationships.column_text(kind).expect("declared");
        let parsed = EventRelationships::parse_column_text(kind, &text).expect("round-trips");
        let declared: Vec<RelationshipRef> = refs.iter().filter(|r| r.kind == kind).cloned().collect();
        assert_eq!(parsed, declared);
    }
    assert_eq!(
        relationships
            .column_text(RelationshipKind::Prev)
            .map(|text| text.split(' ').count()),
        Some(20)
    );
    assert_eq!(
        relationships
            .column_text(RelationshipKind::Auth)
            .map(|text| text.split(' ').count()),
        Some(10)
    );
}

#[test]
fn a_room_version_one_event_id_is_an_accepted_external_target() {
    let reference = RelationshipRef::to_external(RelationshipKind::Prev, b"$abc:example.org").expect("fits the space");
    assert_eq!(reference.space, TargetIdSpace::ExternalId);
    let value = reference.column_value();
    assert!(value.starts_with("external_id:"), "{value}");
    assert_eq!(
        RelationshipRef::parse_column_value(RelationshipKind::Prev, &value),
        Ok(reference)
    );
}

#[test]
fn external_targets_are_one_to_255_bytes() {
    assert!(RelationshipRef::to_external(RelationshipKind::Auth, &[b'x'; 255]).is_ok());
    assert_eq!(
        RelationshipRef::to_external(RelationshipKind::Auth, &[b'x'; 256]),
        Err(RelationshipError::WrongTargetLength)
    );
    assert_eq!(
        RelationshipRef::to_external(RelationshipKind::Auth, b""),
        Err(RelationshipError::WrongTargetLength)
    );
    assert_eq!(
        RelationshipRef::parse_column_value(RelationshipKind::Auth, "external_id:abc"),
        Err(RelationshipError::MalformedColumnValue),
        "an odd hex length is not a whole byte string"
    );
}
