//! The questions a caller asks of a subject's event graph: which events an event points back to, which point at it,
//! and everything it descends from, level by level; which sequence numbers a list of protocol event ids stand for;
//! where events sit in the auth chain cover; and what the subject's state was after a state event.
//!
//! [`super::RemoteGraph`] answers them from graph-profile files in object storage, and [`super::RemoteState`] adds
//! the state questions on top of it.
//!
//! See: hef-apis/spec.md

use super::graph::{ChainLabel, ChainLink, EdgeKind};
use crate::artifacts::state_chunk::{StateChange, StateKey, StateRoot};
use crate::error::{FormatError, StateChunkError};
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

/// Turns the ids a protocol gives events (a Matrix `$...` id, say) into the sequence numbers the graph names them by.
///
/// See: hef-apis/spec.md
pub trait EventIds {
    /// The sequence number of the event of `subject` whose protocol id is each of `ids`, one answer per id in the order
    /// asked: `None` for an id no event of the subject carries.
    fn resolve_ids(&self, subject: &[u8], ids: &[&[u8]]) -> Result<Vec<Option<u64>>, FormatError>;
}

/// Reads where a subject's events sit in its auth chain cover, the labels and links that answer "does this event
/// reach that one through auth edges?" without walking the graph.
///
/// See: hef-apis/spec.md
pub trait AuthChains {
    /// The chain-cover label of each event in `sns`, one per event in the order asked: `None` for an event whose file
    /// stores no chain cover. Fails when no file holds one of the events.
    fn chain_labels(&self, subject: &[u8], sns: &[u64]) -> Result<Vec<Option<ChainLabel>>, FormatError>;

    /// Every link of `subject`'s chain cover that starts on one of `chains`, sorted and without repeats.
    fn chain_links(&self, subject: &[u8], chains: &[u32]) -> Result<Vec<ChainLink>, FormatError>;
}

/// Reads a subject's state (a chat room's membership, say) as it stood after one of its state events: the event's
/// state root, then one key of that state or every key that differs between two states.
///
/// See: hef-apis/spec.md
pub trait SubjectState {
    /// The subject's state root right after event `sn`, or `None` when `sn` is not a state event. Fails when no file
    /// holds the event.
    fn state_root(&self, subject: &[u8], sn: u64) -> Result<Option<StateRoot>, FormatError>;

    /// The sequence number of the event that set `key` in the state at `root`, or `None` when that state lacks the key.
    fn state_get(&self, root: &StateRoot, key: StateKey) -> Result<Option<u64>, StateChunkError>;

    /// Every key whose value differs between the states at `before` and `after`, in key order.
    fn state_diff(&self, before: &StateRoot, after: &StateRoot) -> Result<Vec<StateChange>, StateChunkError>;
}

#[cfg(test)]
#[path = "test/api.rs"]
mod tests;
