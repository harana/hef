//! Decides when one subject's open graph file is sealed: once it holds enough events, or once its first event has
//! waited long enough, whichever comes first.
//!
//! Each subject (a chat room, say) fills its own graph file, because a graph file holds one subject's events numbered
//! densely in arrival order. A busy subject seals by count; a quiet one seals by age, so its events still reach storage
//! on a schedule. Compaction later merges a subject's small sealed files.
//!
//! See: hef-write-path/spec.md

use super::constant::{DEFAULT_GRAPH_SEAL_AGE_NANOS, DEFAULT_GRAPH_SEAL_EVENTS};
use crate::clock::Clock;

/// When a subject's open graph file is sealed.
///
/// See: hef-write-path/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphSealPolicy {
    /// Nanoseconds after the open file's first event arrived at which the file is sealed however few events it holds.
    pub max_age_nanos: i64,
    /// The file is sealed once it holds this many events.
    pub max_events: u64,
}

impl Default for GraphSealPolicy {
    fn default() -> Self {
        Self {
            max_age_nanos: DEFAULT_GRAPH_SEAL_AGE_NANOS,
            max_events: DEFAULT_GRAPH_SEAL_EVENTS,
        }
    }
}

/// What the application knows about one subject's open graph file. It keeps this beside the events themselves, in its
/// own durable storage, so a restart picks the file up where it left off.
///
/// See: hef-write-path/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenGraphFile {
    pub event_count: u64,
    /// Wall-clock nanoseconds since the Unix epoch, read through [`Clock`], when the file's first event arrived.
    pub first_event_at_nanos: i64,
}

/// Which limit sealed a graph file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphSealTrigger {
    Age,
    EventCount,
}

/// Whether the subject's open file `open` should be sealed now, and why: `EventCount` once it holds `max_events`
/// events, otherwise `Age` once `max_age_nanos` have passed since its first event arrived. `None` for a file that
/// should stay open, and always for an empty one.
///
/// Nothing in HEF calls this: the application checks it after appending events to a subject's open file, and when it
/// fires builds the file with the subject's graph and publishes it like any other file.
pub fn should_seal_graph(
    open: &OpenGraphFile,
    policy: &GraphSealPolicy,
    clock: &dyn Clock,
) -> Option<GraphSealTrigger> {
    if open.event_count == 0 {
        return None;
    }
    if open.event_count >= policy.max_events {
        return Some(GraphSealTrigger::EventCount);
    }
    if clock.now_nanos().saturating_sub(open.first_event_at_nanos) >= policy.max_age_nanos {
        return Some(GraphSealTrigger::Age);
    }
    None
}

#[cfg(test)]
#[path = "test/graph_seal.rs"]
mod tests;
