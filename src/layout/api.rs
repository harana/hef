//! The questions a caller asks of a subject's event graph: which events an event points back to, which point at it,
//! and everything it descends from, level by level.
//!
//! [`super::RemoteGraph`] answers them from graph-profile files in object storage.
//!
//! See: hef-apis/spec.md

use super::graph::EdgeKind;
use crate::error::FormatError;
use hashbrown::HashSet;

/// Reads the edges between one subject's events, named by sequence number.
///
/// The batch forms, [`parents_of`](Self::parents_of) and [`children_of`](Self::children_of), answer many events at
/// once so an implementation can fetch everything they need together; the single-event forms and
/// [`walk`](Self::walk) are built on them.
///
/// See: hef-apis/spec.md
pub trait EventGraph {
    /// The parents of each event in `sns` along `kind` edges, one list per event in the order asked, each in the
    /// order the writer was given them.
    fn parents_of(&self, subject: &[u8], sns: &[u64], kind: EdgeKind) -> Result<Vec<Vec<u64>>, FormatError>;

    /// The children of each event in `sns` along `kind` edges, one list per event in the order asked. A list holds
    /// the children stored with the parent's file: those in the same file, then those from later files that the
    /// file's last compaction folded in.
    fn children_of(&self, subject: &[u8], sns: &[u64], kind: EdgeKind) -> Result<Vec<Vec<u64>>, FormatError>;

    /// The parents of event `sn` along `kind` edges.
    fn parents(&self, subject: &[u8], sn: u64, kind: EdgeKind) -> Result<Vec<u64>, FormatError> {
        Ok(self.parents_of(subject, &[sn], kind)?.pop().unwrap_or_default())
    }

    /// The children of event `sn` along `kind` edges, as [`children_of`](Self::children_of) describes.
    fn children(&self, subject: &[u8], sn: u64, kind: EdgeKind) -> Result<Vec<u64>, FormatError> {
        Ok(self.children_of(subject, &[sn], kind)?.pop().unwrap_or_default())
    }

    /// Walks back from event `start` along `kind` edges, breadth first: each item is the next level of ancestors not
    /// seen before, in ascending order, fetched only when the caller asks for it. Stops once `limit` ancestors have
    /// been returned (the last level cut short to fit) or no new ancestor remains. `start` itself is not returned.
    fn walk<'a>(&'a self, subject: &'a [u8], start: u64, kind: EdgeKind, limit: usize) -> GraphWalk<'a>
    where
        Self: Sized,
    {
        GraphWalk {
            frontier: vec![start],
            graph: self,
            kind,
            remaining: limit,
            subject,
            visited: HashSet::from_iter([start]),
        }
    }
}

/// A breadth-first walk over an event's ancestors, one level per item. Made by [`EventGraph::walk`].
///
/// See: hef-apis/spec.md
pub struct GraphWalk<'a> {
    frontier: Vec<u64>,
    graph: &'a dyn EventGraph,
    kind: EdgeKind,
    remaining: usize,
    subject: &'a [u8],
    visited: HashSet<u64>,
}

impl Iterator for GraphWalk<'_> {
    type Item = Result<Vec<u64>, FormatError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 || self.frontier.is_empty() {
            return None;
        }
        let parents = match self.graph.parents_of(self.subject, &self.frontier, self.kind) {
            Ok(parents) => parents,
            Err(error) => {
                self.frontier.clear();
                return Some(Err(error));
            }
        };
        let mut level: Vec<u64> = parents
            .into_iter()
            .flatten()
            .filter(|sn| self.visited.insert(*sn))
            .collect();
        level.sort_unstable();
        level.truncate(self.remaining);
        self.remaining -= level.len();
        self.frontier.clone_from(&level);
        (!level.is_empty()).then_some(Ok(level))
    }
}

#[cfg(test)]
#[path = "test/api.rs"]
mod tests;
