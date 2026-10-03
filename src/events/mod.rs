//! Defines what one event is — its fixed set of standard fields, its flexible payload, and the rules for what may leave
//! the engine.
//!
//! Every event carries the same envelope of standard fields (identity, time, source, type, and so on) plus a payload in
//! one canonical format that all incoming data is translated into at ingest. This module also groups the analytical
//! columns into families and enforces which columns are allowed to reach public callers.

pub mod constant;
pub mod envelope;
pub mod families;
pub mod provenance;
pub mod relationships;
pub mod sim;
pub mod transcode;
pub mod variant;

pub use envelope::{
    DurationValue, EventEnvelope, EventFlags, EventId, SequencePoint, SequenceRange, StreamId, TenantId, TimestampValue,
};
pub use provenance::{SignatureScheme, SignedEventProvenance};
pub use relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};
pub use sim::{SimulatedEventAuthor, signed_event_payload};
