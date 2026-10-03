//! Decides which columns a stored file holds and how payload fields get lifted into fast typed columns.
//!
//! Every file carries a fixed set of required columns (the event envelope and the internal identity/hash columns). On
//! top of those, frequently queried payload fields are "promoted" into their own strongly typed columns, and frequently
//! accessed fields are "shredded" out of the JSON-like payload into columns of their own; schema-declared free-text
//! fields get the same treatment by declaration. Promoted columns are keyed by schema version so older rows that
//! predate a promotion still resolve through the payload instead of reading as NULL.

use super::error::FormatError;
use super::events::variant::VariantValue;
use super::layout::footer::ColumnKind;
use hashbrown::{HashMap, HashSet};

/// Stable ids for the required physical columns.
pub mod column_ids {
    pub const EPOCH: u32 = 0;
    pub const SEQUENCE: u32 = 1;
    pub const STREAM_ID: u32 = 2;
    pub const STREAM_SEQUENCE: u32 = 3;
    pub const OCCURRED_AT: u32 = 4;
    pub const INGESTED_AT: u32 = 5;
    pub const SOURCE_ID: u32 = 6;
    pub const EVENT_TYPE_ID: u32 = 7;
    pub const ENTITY_TYPE_ID: u32 = 8;
    pub const ENTITY_ID_HASH_LOW: u32 = 9;
    pub const ENTITY_ID_HASH_HIGH: u32 = 10;
    pub const PAYLOAD_REF: u32 = 11;
    pub const FLAGS: u32 = 12;
    pub const SCHEMA_VERSION: u32 = 13;
    pub const EVENT_ID: u32 = 14;
    pub const ENTITY_ID: u32 = 15;
    pub const ACTOR_ID: u32 = 16;
    pub const ACCOUNT_ID: u32 = 17;
    pub const ACTOR_ID_HASH_LOW: u32 = 18;
    pub const ACCOUNT_ID_HASH_LOW: u32 = 19;
    pub const TRACE_ID_HASH_LOW: u32 = 20;
    pub const DEDUPE_HASH_LOW: u32 = 21;
    pub const DEDUPE_HASH_HIGH: u32 = 22;
    pub const PAYLOAD_FLAGS: u32 = 23;

    /// Promoted columns are allocated from here.
    pub const PROMOTED_BASE: u32 = 1000;
    /// Shredded payload-path columns are allocated from here.
    pub const SHREDDED_BASE: u32 = 2000;
    /// Free-text declared columns are allocated from here.
    pub const FREETEXT_BASE: u32 = 3000;
    /// Context projection columns (context_title, context_summary, etc.) are allocated from here.
    pub const CONTEXT_BASE: u32 = 4000;
    /// Internal embedding/vector columns are allocated from here.
    pub const EMBEDDING_BASE: u32 = 5000;
    /// Signed-event provenance columns are allocated from here. Materialized only for streams whose events carry
    /// protocol signatures.
    pub const PROVENANCE_BASE: u32 = 6000;

    pub const AUTHOR_PUBKEY: u32 = PROVENANCE_BASE;
    pub const SIGNATURE: u32 = PROVENANCE_BASE + 1;
    pub const SIGNATURE_SCHEME: u32 = PROVENANCE_BASE + 2;
    pub const PROTOCOL_EVENT_ID: u32 = PROVENANCE_BASE + 3;
    pub const PROTOCOL_KIND: u32 = PROVENANCE_BASE + 4;
    pub const CLAIMED_AT: u32 = PROVENANCE_BASE + 5;

    /// Relationship-reference columns are allocated from here. Materialized only for streams whose events declare
    /// relationships to other events.
    pub const RELATIONSHIP_BASE: u32 = 7000;

    pub const PARENT_REF: u32 = RELATIONSHIP_BASE;
    pub const ROOT_REF: u32 = RELATIONSHIP_BASE + 1;
    pub const LINKED_REFS: u32 = RELATIONSHIP_BASE + 2;
    pub const RELATED_REFS: u32 = RELATIONSHIP_BASE + 3;
}

/// One column's static description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub column_id: u32,
    /// Internal scan columns (`payload_ref`, `epoch`, `sequence`, hashes) never cross the public boundary; public APIs
    /// receive opaque cursors.
    pub internal_only: bool,
    pub kind: ColumnKind,
    pub name: &'static str,
    pub nullable: bool,
}

/// Every HEF file includes column chunks for these.
pub const REQUIRED_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec {
        column_id: column_ids::EPOCH,
        name: "epoch",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::SEQUENCE,
        name: "sequence",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::STREAM_ID,
        name: "stream_id",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::STREAM_SEQUENCE,
        name: "stream_sequence",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::OCCURRED_AT,
        name: "occurred_at",
        kind: ColumnKind::I64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::INGESTED_AT,
        name: "ingested_at",
        kind: ColumnKind::I64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::SOURCE_ID,
        name: "source_id",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::EVENT_TYPE_ID,
        name: "event_type_id",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::ENTITY_TYPE_ID,
        name: "entity_type_id",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::ENTITY_ID_HASH_LOW,
        name: "entity_id_hash_low",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::ENTITY_ID_HASH_HIGH,
        name: "entity_id_hash_high",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::PAYLOAD_REF,
        name: "payload_ref",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::FLAGS,
        name: "flags",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::SCHEMA_VERSION,
        name: "schema_version",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::EVENT_ID,
        name: "event_id",
        kind: ColumnKind::U128,
        nullable: false,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::ENTITY_ID,
        name: "entity_id",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::ACTOR_ID,
        name: "actor_id",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::ACCOUNT_ID,
        name: "account_id",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::ACTOR_ID_HASH_LOW,
        name: "actor_id_hash_low",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::ACCOUNT_ID_HASH_LOW,
        name: "account_id_hash_low",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::TRACE_ID_HASH_LOW,
        name: "trace_id_hash_low",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::DEDUPE_HASH_LOW,
        name: "dedupe_hash_low",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::DEDUPE_HASH_HIGH,
        name: "dedupe_hash_high",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::PAYLOAD_FLAGS,
        name: "payload_flags",
        kind: ColumnKind::U64,
        nullable: false,
        internal_only: true,
    },
];

/// The optional signed-event provenance family: present only in files whose rows carry protocol signatures, absent
/// (materializing nothing) everywhere else. Byte-valued fields are stored in the lowercase hex form the protocol
/// itself uses, so the canonical serialization rebuilds from the stored column verbatim.
///
/// These are ordinary Tier A columns: they encode, promote, and authorize like any other, and they are exactly as
/// visible as the event they attest — never more.
pub const PROVENANCE_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec {
        column_id: column_ids::AUTHOR_PUBKEY,
        name: "author_pubkey",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::SIGNATURE,
        name: "signature",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::SIGNATURE_SCHEME,
        name: "signature_scheme",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::PROTOCOL_EVENT_ID,
        name: "protocol_event_id",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::PROTOCOL_KIND,
        name: "protocol_kind",
        kind: ColumnKind::I64,
        nullable: true,
        internal_only: false,
    },
    ColumnSpec {
        column_id: column_ids::CLAIMED_AT,
        name: "claimed_at",
        kind: ColumnKind::I64,
        nullable: true,
        internal_only: false,
    },
];

/// The optional relationship-references family: present only in files whose rows declare references to other events,
/// absent (materializing nothing) everywhere else. Each value is the reference's canonical text
/// (`<space>:<lowercase hex>`), so a reverse lookup — "children of X", "thread of R" — is a plain string equality
/// filter over an ordinary column; the multi-valued `link`/`related` kinds store their references space-separated.
///
/// The columns are internal-only: a reference in the `event_id` space is raw internal event identity, so public
/// callers receive related-event identity only as opaque cursors mapped at the API boundary, exactly like the event's
/// own identity. A signed protocol's references remain visible with the event through its payload bytes.
pub const RELATIONSHIP_COLUMNS: &[ColumnSpec] = &[
    ColumnSpec {
        column_id: column_ids::PARENT_REF,
        name: "parent_ref",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::ROOT_REF,
        name: "root_ref",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::LINKED_REFS,
        name: "linked_refs",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: true,
    },
    ColumnSpec {
        column_id: column_ids::RELATED_REFS,
        name: "related_refs",
        kind: ColumnKind::String,
        nullable: true,
        internal_only: true,
    },
];

/// One promoted attribute: a strongly typed physical column lifted from the payload. Promotion is workload-aware and
/// automatic at the system level — candidates come from filters, group-bys, rules, metric definitions — and arrives
/// here as a plan; the format mechanism (typed column + presence map) is what this module owns. Money promotes to
/// fixed-scale decimal, never float.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedColumn {
    pub kind: ColumnKind,
    pub name: String,
    /// Top-level payload path the values come from.
    pub path: String,
    /// Presence-map key: granules whose rows predate this schema version fall back to the payload blocks, not NULL.
    pub since_schema_version: u32,
    /// Declares that queries filter this column by substring (`CONTAINS`), so its blocks' token filter also carries
    /// trigrams and a substring predicate prunes granules instead of decoding every one. Off by default: trigrams cost
    /// filter bytes and prune nothing extra for a column filtered only by whole value.
    pub substring_searchable: bool,
}

/// The promotion plan for one publication.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromotionPlan {
    pub columns: Vec<PromotedColumn>,
}

/// The payload fields an operator has declared as free text, so they are split into their own column family by
/// declaration rather than by usage statistics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FreetextDeclaration {
    pub fields: Vec<String>,
}

/// Internal selection thresholds for statistics-driven shredding. Internal constants, not operator knobs; tunable only
/// through benchmark-gated changes.
pub const SHRED_MIN_PRESENCE_PERCENT: u32 = 50;
pub const SHRED_MIN_KIND_CONSENSUS_PERCENT: u32 = 95;

/// Sparse-tier floor: a type-consistent path present on at least this percent of rows — but under the dense
/// [`SHRED_MIN_PRESENCE_PERCENT`] — stores as a sparse shredded column instead of staying residual.
pub const SPARSE_SHRED_MIN_PRESENCE_PERCENT: u32 = 5;

/// Largest file-wide distinct-value set a string column may have and still store one shared file-scope dictionary
/// alphabet; beyond it every block keeps its own dictionary.
pub const SHARED_DICTIONARY_MAX_VALUES: usize = 1024;

/// Pinned per-file budget on sparse shredded keys, so footer metadata cannot grow without limit; a qualifying path
/// past the budget simply stays residual.
pub const SPARSE_KEYS_PER_FILE_MAX: usize = 16;

const PERCENT_SCALE: u64 = 100;

#[derive(Debug, Default, Clone)]
struct PathObservation {
    /// Counts indexed by `ColumnKind as usize` (6 variants).
    by_kind: [u64; 6],
    present: u64,
}

impl PathObservation {
    fn record(&mut self, kind: ColumnKind) {
        if let Some(slot) = self.by_kind.get_mut(kind as usize) {
            *slot += 1;
        }
        self.present += 1;
    }
}

/// Per-path access/type statistics collected at publication: the input to shredding selection.
#[derive(Debug, Default)]
pub struct PathStatistics {
    paths: HashMap<String, PathObservation>,
    /// The writer's fast path. File-local field ids address this vector directly, so observing a repeated payload
    /// shape neither hashes nor compares its field names. Kept separate from `paths` so the public string-keyed
    /// collector remains useful to planners and conformance tests.
    dense_fields: Vec<PathObservation>,
    /// Very wide schemas do not reserve one observation (seven counters) for every interned name in every Rayon
    /// worker. They retain the stable field identity, but store only ids actually observed by that worker.
    sparse_fields: Option<HashMap<u32, PathObservation>>,
    rows: u64,
}

/// Above this many file-local fields, a per-worker dense observation vector costs more than its cache locality saves.
/// The fallback still hashes only a four-byte identity, never the repeated payload-path string.
pub(crate) const DENSE_FIELD_STATISTICS_LIMIT: usize = 4_096;

fn scalar_kind(value: &VariantValue) -> Option<ColumnKind> {
    // Only kinds that a typed column reproduces byte-identically are shred candidates. `Timestamp` (coerces to `Int`)
    // and `Float` (coerces to `Double`) would lose their logical type on read-back, so a path made only of those never
    // becomes a candidate; any such values on a mixed path stay in the residual instead.
    match value {
        VariantValue::Int(_) => Some(ColumnKind::I64),
        VariantValue::Double(_) => Some(ColumnKind::F64),
        VariantValue::Decimal { .. } => Some(ColumnKind::Decimal),
        VariantValue::String(_) => Some(ColumnKind::String),
        _ => None,
    }
}

impl PathStatistics {
    /// An empty collector, before any rows have been observed.
    pub fn new() -> Self {
        Self::default()
    }

    /// An ID-addressed collector for the HEF writer. `field_count` is the build's immutable intern-table width.
    pub(crate) fn for_field_ids(field_count: usize) -> Self {
        if field_count <= DENSE_FIELD_STATISTICS_LIMIT {
            Self {
                dense_fields: vec![PathObservation::default(); field_count],
                ..Self::default()
            }
        } else {
            Self {
                sparse_fields: Some(HashMap::new()),
                ..Self::default()
            }
        }
    }

    /// Observes one row's payload. Only top-level scalar object fields are shred candidates in this change.
    pub fn observe(&mut self, payload: Option<&VariantValue>) {
        self.rows += 1;
        let Some(VariantValue::Object(fields)) = payload else {
            return;
        };
        for (path, value) in fields {
            let Some(kind) = scalar_kind(value) else {
                continue;
            };
            // Clone the key only the first time a path is seen; already-tracked paths update in place.
            if let Some(observation) = self.paths.get_mut(path.as_str()) {
                observation.record(kind);
            } else {
                self.paths.entry(path.clone()).or_default().record(kind);
            }
        }
    }

    /// Observes one normalized row through its stable file-local field ids. `field_ids` and `values` are the row's
    /// top-level fields in its own sorted field order, paired position by position. A payload that is absent or is not
    /// an object passes two empty slices and counts only toward the file's row denominator.
    pub(crate) fn observe_field_ids(&mut self, field_ids: &[u32], values: &[VariantValue]) {
        self.rows += 1;
        for (&field_id, value) in field_ids.iter().zip(values) {
            let Some(kind) = scalar_kind(value) else {
                continue;
            };
            if let Some(observation) = self.dense_fields.get_mut(field_id as usize) {
                observation.record(kind);
            } else if let Some(fields) = self.sparse_fields.as_mut() {
                fields.entry(field_id).or_default().record(kind);
            }
        }
    }

    /// Folds another collector's counts into this one.
    ///
    /// Every field is a plain sum and both selections sort their output, so a statistics pass split across threads and
    /// merged selects exactly the paths, kinds and order one sequential pass would.
    pub fn merge(&mut self, other: Self) {
        self.rows += other.rows;
        for (path, observation) in other.paths {
            let slot = self.paths.entry(path).or_default();
            slot.present += observation.present;
            for (into, from) in slot.by_kind.iter_mut().zip(observation.by_kind) {
                *into += from;
            }
        }
        debug_assert_eq!(self.dense_fields.len(), other.dense_fields.len());
        for (slot, observation) in self.dense_fields.iter_mut().zip(other.dense_fields) {
            slot.present += observation.present;
            for (into, from) in slot.by_kind.iter_mut().zip(observation.by_kind) {
                *into += from;
            }
        }
        if let (Some(into), Some(from)) = (self.sparse_fields.as_mut(), other.sparse_fields) {
            for (field_id, observation) in from {
                let slot = into.entry(field_id).or_default();
                slot.present += observation.present;
                for (into, from) in slot.by_kind.iter_mut().zip(observation.by_kind) {
                    *into += from;
                }
            }
        }
    }

    /// ID-addressed counterpart of [`Self::shred_candidates`]. Names are consulted only for selected output and the
    /// final deterministic sort, never in the per-row observation loop.
    pub(crate) fn shred_candidates_by_id(
        &self,
        field_names: &[String],
        excluded: &HashSet<u32>,
    ) -> Vec<(String, ColumnKind)> {
        if self.rows == 0 {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for (field_id, observation) in self.field_observations() {
            if excluded.contains(&field_id)
                || observation.present * PERCENT_SCALE < self.rows * u64::from(SHRED_MIN_PRESENCE_PERCENT)
            {
                continue;
            }
            if let (Some(path), Some(kind)) = (field_names.get(field_id as usize), consensus_kind(observation)) {
                candidates.push((path.clone(), kind));
            }
        }
        candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
        candidates
    }

    /// ID-addressed counterpart of [`Self::sparse_candidates`].
    pub(crate) fn sparse_candidates_by_id(
        &self,
        field_names: &[String],
        excluded: &HashSet<u32>,
    ) -> Vec<(String, ColumnKind)> {
        if self.rows == 0 {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, String, ColumnKind)> = Vec::new();
        for (field_id, observation) in self.field_observations() {
            let Some(path) = field_names.get(field_id as usize) else {
                continue;
            };
            if excluded.contains(&field_id) || path.len() > u16::MAX as usize {
                continue;
            }
            let present = observation.present * PERCENT_SCALE;
            if present < self.rows * u64::from(SPARSE_SHRED_MIN_PRESENCE_PERCENT)
                || present >= self.rows * u64::from(SHRED_MIN_PRESENCE_PERCENT)
            {
                continue;
            }
            if let Some(kind) = consensus_kind(observation) {
                candidates.push((observation.present, path.clone(), kind));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        candidates.truncate(SPARSE_KEYS_PER_FILE_MAX);
        candidates.into_iter().map(|(_, path, kind)| (path, kind)).collect()
    }

    fn field_observations(&self) -> Box<dyn Iterator<Item = (u32, &PathObservation)> + '_> {
        if let Some(fields) = &self.sparse_fields {
            Box::new(fields.iter().map(|(&field_id, observation)| (field_id, observation)))
        } else {
            Box::new(
                self.dense_fields
                    .iter()
                    .enumerate()
                    .map(|(field_id, observation)| (field_id as u32, observation)),
            )
        }
    }

    /// Selects frequently accessed, type-consistent paths for shredding. `excluded` paths (promoted columns, declared
    /// free-text fields) stay out of the stats-driven plan.
    pub fn shred_candidates(&self, excluded: &[&str]) -> Vec<(String, ColumnKind)> {
        if self.rows == 0 {
            return Vec::new();
        }
        let mut candidates = Vec::new();
        for (path, observation) in &self.paths {
            if excluded.contains(&path.as_str()) {
                continue;
            }
            if observation.present * PERCENT_SCALE < self.rows * u64::from(SHRED_MIN_PRESENCE_PERCENT) {
                continue;
            }
            if let Some(kind) = consensus_kind(observation) {
                candidates.push((path.clone(), kind));
            }
        }
        // The paths map is unordered; sort so the shred plan (and the column ids it drives) stays deterministic.
        candidates.sort_by(|(a, _), (b, _)| a.cmp(b));
        candidates
    }

    /// Selects paths for the sparse shredded tier: type-consistent like dense candidates, present on at least the
    /// sparse floor but under the dense threshold. Ordered densest-first (ties by path) and truncated to the pinned
    /// per-file key budget, so selection is deterministic and the footer stays bounded — a qualifying path past the
    /// budget simply stays residual.
    pub fn sparse_candidates(&self, excluded: &[&str]) -> Vec<(String, ColumnKind)> {
        if self.rows == 0 {
            return Vec::new();
        }
        let mut candidates: Vec<(u64, String, ColumnKind)> = Vec::new();
        for (path, observation) in &self.paths {
            // The footer's key-set section stores each path behind a u16 length prefix; a longer path cannot be
            // represented, so it stays residual.
            if excluded.contains(&path.as_str()) || path.len() > u16::MAX as usize {
                continue;
            }
            let present = observation.present * PERCENT_SCALE;
            if present < self.rows * u64::from(SPARSE_SHRED_MIN_PRESENCE_PERCENT)
                || present >= self.rows * u64::from(SHRED_MIN_PRESENCE_PERCENT)
            {
                continue;
            }
            if let Some(kind) = consensus_kind(observation) {
                candidates.push((observation.present, path.clone(), kind));
            }
        }
        candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        candidates.truncate(SPARSE_KEYS_PER_FILE_MAX);
        candidates.into_iter().map(|(_, path, kind)| (path, kind)).collect()
    }
}

/// The single kind a path's observations agree on past the consensus bar, or `None` when the path is too mixed to
/// reproduce byte-identically from a typed column.
fn consensus_kind(observation: &PathObservation) -> Option<ColumnKind> {
    let (kind_index, kind_count) = observation
        .by_kind
        .iter()
        .enumerate()
        .max_by_key(|(_, count)| **count)
        .map(|(index, count)| (index, *count))?;
    if kind_count * PERCENT_SCALE < observation.present * u64::from(SHRED_MIN_KIND_CONSENSUS_PERCENT) {
        return None;
    }
    ColumnKind::from_u8(kind_index as u8).ok()
}

/// The schema-version-keyed presence rule: a promoted column is materialized for a row only when the row's schema
/// version is at or past the column's promotion version; earlier rows resolve through the payload blocks (never
/// silently NULL).
pub fn promoted_present(column: &PromotedColumn, row_schema_version: u32) -> bool {
    row_schema_version >= column.since_schema_version
}

/// Validates a promotion plan: strongly typed only, money paths must be decimal (fixed-scale), never float, and only a
/// string column may declare substring search.
pub fn validate_promotion_plan(plan: &PromotionPlan) -> Result<(), FormatError> {
    for column in &plan.columns {
        if column.kind == ColumnKind::F64 && (column.name.contains("amount") || column.name.contains("money")) {
            return Err(FormatError::Structural {
                rule: "money promotes to fixed-scale decimal, never float",
            });
        }
        // Only a string column gets a text-token filter, so a substring declaration on any other kind would be
        // silently ignored; refuse it instead of dropping it.
        if column.substring_searchable && column.kind != ColumnKind::String {
            return Err(FormatError::Structural {
                rule: "only a string promoted column may be declared substring-searchable",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/columns.rs"]
mod tests;
