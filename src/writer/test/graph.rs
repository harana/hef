use super::*;
use crate::events::{EventRelationships, RelationshipKind};
use crate::layout::Profile;
use crate::layout::reader::HefFile;
use crate::writer::build::{HefBuildConfig, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};
use std::collections::HashMap;

/// An in-memory stand-in for the application's durable record of sequence numbers, keyed by target space and bytes.
#[derive(Default)]
struct RecordedNumbers {
    numbers: HashMap<(TargetIdSpace, Vec<u8>), u64>,
}

impl EventSequenceNumbers for RecordedNumbers {
    fn sequence_numbers(&self, subject: &[u8], targets: &[&RelationshipRef]) -> Result<Vec<Option<u64>>, StorageError> {
        assert_eq!(subject, b"!room:example.org");
        Ok(targets
            .iter()
            .map(|target| self.numbers.get(&(target.space, target.target_ref.clone())).copied())
            .collect())
    }
}

fn event_id(sn: u64) -> Vec<u8> {
    format!("$event{sn}:example.org").into_bytes()
}

fn external(kind: RelationshipKind, sn: u64) -> RelationshipRef {
    RelationshipRef::to_external(kind, &event_id(sn)).unwrap()
}

/// Rows for events `first_sn..first_sn + count`, each naming the one before it as `prev` by protocol id and the first
/// event as `auth` by sequence number, with the first naming a `prev` the record has never seen.
fn rows(first_sn: u64, count: u64) -> Vec<BuildRow> {
    (first_sn..first_sn + count)
        .map(|sn| {
            let mut row = plain_row(sn);
            row.external_id = Some(event_id(sn));
            let refs = if sn == first_sn {
                vec![RelationshipRef::to_external(RelationshipKind::Prev, b"$never-seen:example.org").unwrap()]
            } else {
                vec![
                    external(RelationshipKind::Prev, sn - 1),
                    RelationshipRef::to_subject_sequence(RelationshipKind::Auth, first_sn),
                ]
            };
            row.relationships = Some(EventRelationships::new(refs).unwrap());
            row
        })
        .collect()
}

/// The record after events `0..count` were numbered as they arrived.
fn recorded(count: u64) -> RecordedNumbers {
    RecordedNumbers {
        numbers: (0..count)
            .map(|sn| ((TargetIdSpace::ExternalId, event_id(sn)), sn))
            .collect(),
    }
}

fn graph_config(graph: GraphInput) -> HefBuildConfig {
    let mut config = source_form_config(4);
    config.graph = Some(graph);
    config.profile = Profile::Graph;
    config
}

#[test]
fn references_resolve_to_sequence_numbers_and_unknown_ones_stay_unresolved() {
    let graph = resolve_graph(&rows(10, 3), b"!room:example.org".to_vec(), 10, &recorded(13)).unwrap();
    assert_eq!(graph.first_sn, 10);
    assert_eq!(graph.rows[0], RowEdges::default());
    assert_eq!(
        graph.rows[2],
        RowEdges {
            auth: vec![10],
            prev: vec![11],
        }
    );
    assert_eq!(
        graph.unresolved,
        [UnresolvedEdge {
            child: 10,
            parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$never-seen:example.org").unwrap(),
        }]
    );
}

#[test]
fn a_record_that_answers_the_wrong_number_of_targets_fails_the_resolution() {
    struct Short;
    impl EventSequenceNumbers for Short {
        fn sequence_numbers(&self, _: &[u8], _: &[&RelationshipRef]) -> Result<Vec<Option<u64>>, StorageError> {
            Ok(Vec::new())
        }
    }
    assert!(matches!(
        resolve_graph(&rows(0, 3), b"!room:example.org".to_vec(), 0, &Short),
        Err(StorageError::Io { .. })
    ));
}

#[test]
fn sequence_numbers_are_dense_and_gap_free_across_a_subjects_files() {
    let numbers = recorded(20);
    let mut sns = Vec::new();
    for (first_sn, count) in [(0, 9), (9, 11)] {
        let graph = resolve_graph(
            &rows(first_sn, count),
            b"!room:example.org".to_vec(),
            first_sn,
            &numbers,
        )
        .unwrap();
        let built = build_hef_file(rows(first_sn, count), &graph_config(graph)).unwrap();
        let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
        assert_eq!(file.graph_sn_of_row(count), None, "no row past the file's last");
        for row in 0..count {
            let sn = file.graph_sn_of_row(row).unwrap();
            assert_eq!(file.graph_row_of(sn), Some(row));
            sns.push(sn);
        }
    }
    assert_eq!(sns, (0..20).collect::<Vec<_>>());
}

#[test]
fn unresolved_parents_survive_the_round_trip_and_flag_their_rows() {
    let graph = resolve_graph(&rows(10, 6), b"!room:example.org".to_vec(), 10, &recorded(11)).unwrap();
    let built = build_hef_file(rows(10, 6), &graph_config(graph)).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    let never_seen = RelationshipRef::to_external(RelationshipKind::Prev, b"$never-seen:example.org").unwrap();
    // Event 11's prev (event 10) is in the record; events 12 to 15 name prevs the record has not numbered yet.
    let flagged: Vec<u64> = (10..16).filter(|&sn| file.graph_has_unresolved_edges(sn)).collect();
    assert_eq!(flagged, [10, 12, 13, 14, 15]);
    assert_eq!(
        file.graph_unresolved_parents(10),
        [UnresolvedEdge {
            child: 10,
            parent: never_seen,
        }]
    );
    assert_eq!(
        file.graph_unresolved_parents(13),
        [UnresolvedEdge {
            child: 13,
            parent: external(RelationshipKind::Prev, 12),
        }]
    );
    assert!(!file.graph_has_unresolved_edges(16), "the file does not hold event 16");

    let edges = file.granule_edges(file.graph_granule_of(11).unwrap()).unwrap();
    assert_eq!(edges.parents(11, EdgeKind::Prev), [10]);
    assert_eq!(edges.parents(13, EdgeKind::Auth), [10]);
    assert!(edges.parents(13, EdgeKind::Prev).is_empty());
    assert_eq!(
        file.resolve_reference(&RelationshipRef::to_subject_sequence(RelationshipKind::Prev, 12))
            .unwrap(),
        [2]
    );
    assert!(
        file.resolve_reference(&RelationshipRef::to_subject_sequence(RelationshipKind::Prev, 16))
            .unwrap()
            .is_empty()
    );
}
