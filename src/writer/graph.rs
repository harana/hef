//! Turns the `prev` and `auth` references a subject's events declare into the edges a graph-profile file stores,
//! named by sequence number, by looking each referenced event up in the application's durable record of the sequence
//! numbers it handed out.
//!
//! See: hef-file-layout/spec.md

use crate::error::StorageError;
use crate::events::{RelationshipRef, TargetIdSpace};
use crate::layout::graph::{EdgeKind, GraphInput, RowEdges, UnresolvedEdge};
use crate::writer::build::BuildRow;

/// The application's record of which sequence number each of a subject's events was given, looked up by any identifier
/// a reference may name the event with (a Matrix `$...` id, say).
///
/// Sequence numbers are handed out as events arrive and must survive restarts, so the application keeps the record in
/// its own durable storage and backs this interface with it. The record holds the events of the batch being built as
/// well as earlier ones.
///
/// See: hef-file-layout/spec.md
pub trait EventSequenceNumbers {
    /// The sequence number of the event each of `targets` names within `subject`, one answer per target in the order
    /// asked: `None` for an event the record does not hold yet. Only a target's space and bytes matter, not its kind.
    fn sequence_numbers(&self, subject: &[u8], targets: &[&RelationshipRef]) -> Result<Vec<Option<u64>>, StorageError>;
}

/// The graph input for `rows`, one subject's events in sequence order with the first at `first_sn`.
///
/// Each row's `prev` and `auth` references become its parents, named by sequence number in the order the row declared
/// them. A reference already in the `subject_sequence` space is used as it is; every other is looked up in `numbers`,
/// all in one call, and one the record does not hold stays as declared in the result's unresolved list. Inbound edges
/// are left empty for the caller to fill. Fails when `numbers` fails or answers a different number of targets than it
/// was asked.
pub fn resolve_graph(
    rows: &[BuildRow],
    subject: Vec<u8>,
    first_sn: u64,
    numbers: &dyn EventSequenceNumbers,
) -> Result<GraphInput, StorageError> {
    let lookups: Vec<&RelationshipRef> = rows
        .iter()
        .flat_map(declared)
        .map(|(_, reference)| reference)
        .filter(|reference| reference.space != TargetIdSpace::SubjectSequence)
        .collect();
    let answers = numbers.sequence_numbers(&subject, &lookups)?;
    if answers.len() != lookups.len() {
        return Err(StorageError::Io {
            op: "sequence number lookup",
            detail: format!("asked for {} targets, got {} answers", lookups.len(), answers.len()),
        });
    }
    let mut answers = answers.into_iter();
    let mut graph = GraphInput {
        first_sn,
        subject,
        ..GraphInput::default()
    };
    for (child, row) in (first_sn..).zip(rows) {
        let mut edges = RowEdges::default();
        for (kind, reference) in declared(row) {
            let sn = match reference.space {
                TargetIdSpace::SubjectSequence => <[u8; 8]>::try_from(reference.target_ref.as_slice())
                    .ok()
                    .map(u64::from_be_bytes),
                _ => answers.next().flatten(),
            };
            match (sn, kind) {
                (Some(sn), EdgeKind::Auth) => edges.auth.push(sn),
                (Some(sn), EdgeKind::Prev) => edges.prev.push(sn),
                (None, _) => graph.unresolved.push(UnresolvedEdge {
                    child,
                    parent: reference.clone(),
                }),
            }
        }
        graph.rows.push(edges);
    }
    Ok(graph)
}

/// The `prev` and `auth` references `row` declares, each with the edge kind it records, in declaration order.
fn declared(row: &BuildRow) -> Vec<(EdgeKind, &RelationshipRef)> {
    row.relationships
        .iter()
        .flat_map(|relationships| relationships.refs())
        .filter_map(|reference| Some((EdgeKind::of(reference.kind)?, reference)))
        .collect()
}

#[cfg(test)]
#[path = "test/graph.rs"]
mod tests;
