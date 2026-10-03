//! Groups the analytical columns into families and decides which ones may be returned to an outside caller.
//!
//! Families are split by when they can be computed. Tier A families are computable at seal time from the single event
//! and live in the base file; Tier B families are cross-event, asynchronous, revisable model outputs and live in a
//! sibling derived-columns file (same format, catalogued as `file_type = derived_columns`), lined up row-for-row with
//! the base. A separate rule blocks raw identity, physical, and internal analytical columns from public output.

use crate::columns::{PROVENANCE_COLUMNS, RELATIONSHIP_COLUMNS, REQUIRED_COLUMNS};

/// Temporal computability tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Computable at seal time from the single event; lives in the base HEF file.
    A,
    /// Cross-event, asynchronous, revisable model output; lives in the sibling derived-columns file with per-column
    /// producer/model lineage.
    B,
}

/// The named groups the analytical columns fall into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColumnFamily {
    ChatInvestigationContextColumns,
    ClassificationLabels,
    ClusterColumns,
    DriverAndCauseColumns,
    EmbeddingColumnsInternal,
    EventEnvelope,
    FreetextColumns,
    PreparedViewLineageColumns,
    /// Optional: present only for events that declare references to other events (parent, thread root, links).
    RelationshipReferences,
    RevenueAnomalyColumns,
    RevenueMetricColumns,
    SafeRetryAndIngestMode,
    /// Optional: present only for events that arrived over a signed protocol.
    SignedEventProvenance,
    SourceTypeEntity,
}

impl ColumnFamily {
    /// Every family, for iterating over the complete set.
    pub const ALL: [ColumnFamily; 14] = [
        ColumnFamily::EventEnvelope,
        ColumnFamily::SourceTypeEntity,
        ColumnFamily::SafeRetryAndIngestMode,
        ColumnFamily::ClassificationLabels,
        ColumnFamily::FreetextColumns,
        ColumnFamily::ClusterColumns,
        ColumnFamily::EmbeddingColumnsInternal,
        ColumnFamily::RevenueAnomalyColumns,
        ColumnFamily::DriverAndCauseColumns,
        ColumnFamily::RevenueMetricColumns,
        ColumnFamily::PreparedViewLineageColumns,
        ColumnFamily::ChatInvestigationContextColumns,
        ColumnFamily::SignedEventProvenance,
        ColumnFamily::RelationshipReferences,
    ];

    /// The temporal-computability split: cluster, revenue-anomaly, and driver/cause columns are model-derived (Tier B);
    /// everything else is seal-time computable (Tier A).
    pub fn tier(self) -> Tier {
        match self {
            ColumnFamily::ClusterColumns
            | ColumnFamily::RevenueAnomalyColumns
            | ColumnFamily::DriverAndCauseColumns => Tier::B,
            _ => Tier::A,
        }
    }

    /// Tier A lives in the base HEF file; Tier B lives in the sibling derived-columns file.
    pub fn lives_in_base_file(self) -> bool {
        matches!(self.tier(), Tier::A)
    }

    /// Internal-only families are blocked from public output unless an owning service defines a public-safe derived
    /// representation. Relationship references qualify: they carry raw internal event identity, and their public-safe
    /// representation is the opaque cursor mapped at the API boundary.
    pub fn internal_only(self) -> bool {
        matches!(
            self,
            ColumnFamily::EmbeddingColumnsInternal
                | ColumnFamily::SafeRetryAndIngestMode
                | ColumnFamily::ClassificationLabels
                | ColumnFamily::RelationshipReferences
        )
    }
}

/// Who is asking at the scan boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Caller {
    /// Owner services operating below the API boundary.
    Internal,
    /// Public routes and tool callers: the public allow-list applies.
    Public,
}

/// The scan-phase column authorization rule: enforced below API serialization, before any row leaves the engine.
///
/// Public output is allow-listed, not deny-listed: a column reaches a public caller only when the required-column
/// schema declares it non-internal. Raw identity, physical, payload-reference, and internal analytical columns are
/// internal-only in that schema and stay blocked, and any column absent from the schema (embedding, analytical, or a
/// newly added internal column) is blocked by default rather than leaking until a deny-list catches up.
pub fn column_allowed(column: &str, caller: Caller) -> bool {
    match caller {
        Caller::Internal => true,
        Caller::Public => REQUIRED_COLUMNS
            .iter()
            .chain(PROVENANCE_COLUMNS)
            .chain(RELATIONSHIP_COLUMNS)
            .any(|spec| spec.name == column && !spec.internal_only),
    }
}

/// Splits a requested projection into allowed and dropped column sets for the caller. Blocked internal columns are
/// dropped, never silently returned.
pub fn authorize_columns<'c>(requested: &[&'c str], caller: Caller) -> (Vec<&'c str>, Vec<&'c str>) {
    let mut allowed = Vec::with_capacity(requested.len());
    let mut dropped = Vec::new();
    for column in requested {
        if column_allowed(column, caller) {
            allowed.push(*column);
        } else {
            dropped.push(*column);
        }
    }
    (allowed, dropped)
}

#[cfg(test)]
#[path = "test/families.rs"]
mod tests;
