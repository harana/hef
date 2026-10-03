//! The standard fields every event carries, plus the time and duration types whose names deliberately hide their units.
//!
//! Logical names stay unit-neutral: `TimestampValue` and `DurationValue` carry no unit suffix, because the physical
//! encoding (signed nanoseconds since the Unix epoch, UTC) is an internal detail recorded here once and never exposed
//! through a public schema name.

crate::typed_id::define_typed_id!(
    /// One customer organisation. Every tenant-scoped row, event, and policy decision carries the `TenantId` it
    /// belongs to.
    TenantId, TenantIdTag, "tenant"
);

crate::typed_id::define_typed_id!(
    /// Public event identity (UUID/ULID-compatible 128-bit value).
    EventId, EventIdTag, "event"
);

/// Stream identity within a tenant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(pub u64);

/// A timestamp-like logical value. Physically encoded as signed nanoseconds since Unix epoch UTC; the logical name
/// stays unit-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct TimestampValue {
    physical_nanos: i64,
}

impl TimestampValue {
    /// Builds the value from its internal physical encoding.
    pub const fn from_physical_nanos(physical_nanos: i64) -> Self {
        Self { physical_nanos }
    }

    /// The internal physical encoding. Not a public schema name.
    pub const fn physical_nanos(self) -> i64 {
        self.physical_nanos
    }
}

/// A duration-like logical value. Physically encoded as signed nanoseconds; the logical name stays unit-neutral.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct DurationValue {
    physical_nanos: i64,
}

impl DurationValue {
    /// Builds the value from its internal physical encoding.
    pub const fn from_physical_nanos(physical_nanos: i64) -> Self {
        Self { physical_nanos }
    }

    /// The internal physical encoding. Not a public schema name.
    pub const fn physical_nanos(self) -> i64 {
        self.physical_nanos
    }
}

/// One internal `(epoch, sequence)` position. Internal identity is `(tenant_id, epoch, sequence)`; none of these are
/// public output — public callers receive opaque cursors at the API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SequencePoint {
    pub epoch: u64,
    pub sequence: u64,
}

impl Ord for SequencePoint {
    /// Epoch dominates sequence: a later epoch always sorts after an earlier one regardless of sequence, so watermark
    /// comparisons stay correct no matter how the fields are declared.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.epoch, self.sequence).cmp(&(other.epoch, other.sequence))
    }
}

impl PartialOrd for SequencePoint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A contiguous internal sequence range within one epoch (inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SequenceRange {
    pub epoch: u64,
    pub first_sequence: u64,
    pub last_sequence: u64,
}

impl Ord for SequenceRange {
    /// Epoch dominates both sequence bounds: ranges sort by epoch first, then by first and last sequence, independent of
    /// field declaration order.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.epoch, self.first_sequence, self.last_sequence).cmp(&(
            other.epoch,
            other.first_sequence,
            other.last_sequence,
        ))
    }
}

impl PartialOrd for SequenceRange {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl SequenceRange {
    /// Number of sequences covered. Ranges are validated non-empty at construction sites; an inverted range reports
    /// zero.
    pub fn count(&self) -> u64 {
        self.last_sequence.saturating_sub(self.first_sequence).saturating_add(
            if self.last_sequence >= self.first_sequence {
                1
            } else {
                0
            },
        )
    }

    /// True when `other` overlaps this range in the same epoch.
    pub fn overlaps(&self, other: &SequenceRange) -> bool {
        self.epoch == other.epoch
            && self.first_sequence <= other.last_sequence
            && other.first_sequence <= self.last_sequence
    }

    /// True when this range fully contains `other` (same epoch).
    pub fn contains(&self, other: &SequenceRange) -> bool {
        self.epoch == other.epoch
            && self.first_sequence <= other.first_sequence
            && other.last_sequence <= self.last_sequence
    }
}

/// Envelope flag bits carried per event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventFlags(pub u32);

/// The fixed logical envelope every event carries. Field names are the stable logical names; hashes are the internal
/// join/filter forms of identity-bearing strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventEnvelope {
    pub account_id: Option<String>,
    pub account_id_hash_low: u64,
    pub actor_id: Option<String>,
    pub actor_id_hash_low: u64,
    pub dedupe_hash_high: u64,
    pub dedupe_hash_low: u64,
    pub entity_id: Option<String>,
    pub entity_id_hash_high: u64,
    pub entity_id_hash_low: u64,
    pub entity_type: String,
    pub event_id: EventId,
    pub event_type: String,
    pub flags: EventFlags,
    pub ingested_at: TimestampValue,
    pub occurred_at: TimestampValue,
    pub schema_version: u32,
    pub source: String,
    pub stream_id: StreamId,
    pub stream_sequence: u64,
    pub tenant_id: TenantId,
    pub trace_id_hash_low: u64,
}

#[cfg(test)]
#[path = "test/envelope.rs"]
mod tests;
