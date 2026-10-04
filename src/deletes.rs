//! How Harana forgets things without ever rewriting a file: HEF files are immutable, so deletes, corrections, and
//! erasure are all recorded as new, separate metadata layered on top rather than edits to published data.
//!
//! A delete marks row positions (or, for one subject inside a multi-subject event, just their columns) as excluded
//! from future reads. A correction is a new event that supersedes an old one. Right-to-erasure requests are honored
//! by destroying the encryption key for a data subject's payload bytes ("crypto-shredding") rather than mutating
//! them, and by de-identifying — not deleting — business records that a lawful retention obligation still requires.
//! A correction or delete also invalidates any model-derived column values computed from the event it touches, so a
//! query never serves a number that a correction has made stale.

use crate::error::FormatError;
use crate::events::{EventId, SequenceRange};
use crate::file::bytes::{Reader, Writer};
use crate::indexes::bitmap::{RoaringRangeBitmap, RowRange};
use crate::security::{AeadScheme, ContentKey, SealedContentKey};
use hashbrown::HashMap;
use std::collections::{BTreeMap, BTreeSet};

/// Tracks which row ordinals in a file have been deleted.
///
/// Row positions are in the primary-rowset ordinal domain. Applying a `DeletionVector` filters any row list before rows
/// are returned to the caller; deleted ordinals are never served to readers.
///
/// Deleted positions are held as sorted, disjoint, non-adjacent `[start, end)` runs — the same compact form the wire
/// encoding and the row bitmaps use — so a deleted span costs one run rather than one entry per row, and decoding a
/// run list never expands it.
#[derive(Debug, Clone)]
pub struct DeletionVector {
    deleted_count: u64,
    deleted_runs: Vec<RowRange>,
}

impl DeletionVector {
    pub fn new() -> Self {
        Self {
            deleted_count: 0,
            deleted_runs: Vec::new(),
        }
    }

    /// Records `ordinal` as deleted.
    pub fn mark_deleted(&mut self, ordinal: u64) {
        self.mark_range(RowRange {
            start: ordinal,
            end: ordinal + 1,
        });
    }

    /// Records every ordinal in `[range.start, range.end)` as deleted, folding the span into the runs it touches or
    /// overlaps so the runs stay sorted, disjoint, and non-adjacent.
    fn mark_range(&mut self, range: RowRange) {
        if range.end <= range.start {
            return;
        }
        // Runs ending before `start` cannot touch the new span; one ending exactly at `start` is adjacent to it and
        // folds in, which is what keeps the runs non-adjacent.
        let first = self.deleted_runs.partition_point(|run| run.end < range.start);
        let mut merged = range;
        let mut past = first;
        while let Some(run) = self.deleted_runs.get(past) {
            if run.start > range.end {
                break;
            }
            merged.start = merged.start.min(run.start);
            merged.end = merged.end.max(run.end);
            past += 1;
        }
        let absorbed: u64 = self.deleted_runs[first..past]
            .iter()
            .map(|run| run.end - run.start)
            .sum();
        self.deleted_count += (merged.end - merged.start) - absorbed;
        self.deleted_runs.splice(first..past, std::iter::once(merged));
    }

    /// Returns true if `ordinal` has been marked deleted.
    ///
    /// Annotated `#[inline]` because the query crate calls this once per row over a granule's ordinals, where a
    /// non-generic cross-crate call is only inlined if ThinLTO happens to import it.
    #[inline]
    pub fn is_deleted(&self, ordinal: u64) -> bool {
        let index = self.deleted_runs.partition_point(|run| run.end <= ordinal);
        self.deleted_runs.get(index).is_some_and(|run| run.start <= ordinal)
    }

    /// Number of deleted row positions.
    pub fn deleted_count(&self) -> u64 {
        self.deleted_count
    }

    /// The deleted ordinals, in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        self.deleted_runs.iter().flat_map(|run| run.start..run.end)
    }

    /// Clears the flag of every deleted row in `keep`, where `keep[i]` covers the row at ordinal
    /// `first_ordinal + i` — the contiguous ordinal span a granule occupies. Flags already cleared stay cleared, so
    /// this composes with the other reasons a scan drops a row.
    ///
    /// Costs one binary search plus a walk of the runs that overlap the span, rather than a search per row.
    pub fn exclude_deleted(&self, first_ordinal: u64, keep: &mut [bool]) {
        let past_last_ordinal = first_ordinal + keep.len() as u64;
        let first_run = self.deleted_runs.partition_point(|run| run.end <= first_ordinal);
        for run in &self.deleted_runs[first_run..] {
            if run.start >= past_last_ordinal {
                break;
            }
            let from = (run.start.max(first_ordinal) - first_ordinal) as usize;
            let past = (run.end.min(past_last_ordinal) - first_ordinal) as usize;
            keep[from..past].fill(false);
        }
    }

    /// Filters `ordinals` to exclude any deleted positions. `ordinals` must be sorted ascending — the order a row
    /// scan naturally produces them in — so this is a single merge walk against the deleted runs rather than a
    /// binary search per ordinal.
    pub fn apply(&self, ordinals: &[u64]) -> Vec<u64> {
        let mut kept = Vec::with_capacity(ordinals.len());
        let mut runs = self.deleted_runs.iter().peekable();
        for &ordinal in ordinals {
            while runs.peek().is_some_and(|run| run.end <= ordinal) {
                runs.next();
            }
            if runs.peek().is_none_or(|run| run.start > ordinal) {
                kept.push(ordinal);
            }
        }
        kept
    }
}

impl Default for DeletionVector {
    fn default() -> Self {
        Self::new()
    }
}

/// Redacts specific columns for specific row ordinals, leaving the row itself intact for subjects that share the event.
///
/// Used for multi-subject events where one subject must be forgotten: only that subject's data-class-labeled fields are
/// redacted; the event row survives for co-mentioned subjects.
pub struct FieldDeletionVector {
    affected_ordinals: BTreeSet<u64>,
    redacted_columns: Vec<String>,
}

impl FieldDeletionVector {
    /// Creates a new field-level deletion vector that will redact `columns`.
    pub fn new(columns: Vec<String>) -> Self {
        Self {
            affected_ordinals: BTreeSet::new(),
            redacted_columns: columns,
        }
    }

    /// Records `ordinal` as having field-level redaction applied.
    pub fn mark_affected(&mut self, ordinal: u64) {
        self.affected_ordinals.insert(ordinal);
    }

    /// Returns true if `column` should be redacted at `ordinal`.
    pub fn column_redacted(&self, column: &str, ordinal: u64) -> bool {
        self.affected_ordinals.contains(&ordinal) && self.redacted_columns.iter().any(|c| c == column)
    }

    /// Returns true if the event's raw payload must be withheld at `ordinal`: whenever any of its fields is redacted.
    /// The raw bytes are kept byte-exact for signature checks, so a redacted field cannot be cut out of them; the
    /// whole raw payload is withheld instead, while the redacted canonical payload still serves.
    pub fn withholds_raw_payload(&self, ordinal: u64) -> bool {
        self.affected_ordinals.contains(&ordinal)
    }

    /// The list of columns being redacted.
    pub fn redacted_columns(&self) -> &[String] {
        &self.redacted_columns
    }
}

/// Which ordinal domain the row positions in a deletion vector use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowPositionDomain {
    /// Positions in the primary rowset ordinal domain — the spec-required default. Projection positions must be mapped
    /// through the projection row map before matching.
    PrimaryRowsetOrdinal,
}

/// How deleted row positions are physically stored in a `DeletionVectorRef`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionVectorEncoding {
    /// Row positions stored inline within the ref itself (small deletion sets).
    Inline,
    /// Row positions stored as a HEF-native block (for large deletion sets).
    NativeBlock,
}

/// Stable identity and location metadata for one row-level deletion vector.
///
/// Carries the Iceberg v3-compatible identity fields the spec requires. Fields are written once at publish time and
/// never mutated.
#[derive(Debug, Clone)]
pub struct DeletionVectorRef {
    pub blake3: [u8; 32],
    pub deleted_count: u64,
    pub deletion_vector_generation: u64,
    pub encoding: DeletionVectorEncoding,
    pub row_position_domain: RowPositionDomain,
    pub target_file_id: u128,
    pub target_projection_id: Option<u64>,
    pub target_sequence_range: SequenceRange,
}

/// Stable identity and location metadata for one field-level deletion vector.
///
/// Unlike `DeletionVectorRef`, which deletes entire rows, this ref names specific columns to redact, leaving the row
/// available to co-mentioned subjects.
#[derive(Debug, Clone)]
pub struct FieldDeletionVectorRef {
    pub blake3: [u8; 32],
    pub deleted_count: u64,
    pub deletion_vector_generation: u64,
    pub redacted_columns: Vec<String>,
    pub target_file_id: u128,
    pub target_projection_id: Option<u64>,
    pub target_sequence_range: SequenceRange,
}

/// The kind of supersession a correcting event represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrectionType {
    /// A partial field update — the correcting event carries only the changed fields; all others retain their original
    /// values.
    Amendment,
    /// A full supersession — the correcting event completely replaces the original.
    Replacement,
    /// A retraction — the original event is nullified with no replacement.
    Retraction,
}

/// Supersession metadata attached to a correcting event.
///
/// A correction is a new event with this metadata attached; the original event is suppressed via a deletion vector in
/// latest-corrected views while raw-history views keep both visible.
#[derive(Debug, Clone)]
pub struct CorrectionMetadata {
    pub correction_epoch: u64,
    pub correction_generation: u64,
    pub correction_sequence: u64,
    pub correction_type: CorrectionType,
    pub corrects_event_id: EventId,
}

/// Computes the deletion vector a latest-corrected view must apply: one deleted ordinal for every event that
/// `corrections` supersedes. A raw-history view applies no such vector and keeps both the original and the
/// correcting event visible; a latest-corrected view applies it so the superseded event is suppressed and the
/// correction is shown.
pub fn latest_corrected_deletion_vector(
    events: &[(u64, EventId)],
    corrections: &[CorrectionMetadata],
) -> DeletionVector {
    // Index the events once so each correction is a hash lookup, not a scan of every event in the file.
    let mut ordinal_by_event = HashMap::with_capacity(events.len());
    for (ordinal, id) in events {
        ordinal_by_event.entry(*id).or_insert(*ordinal);
    }
    let mut deletion_vector = DeletionVector::new();
    for correction in corrections {
        if let Some(ordinal) = ordinal_by_event.get(&correction.corrects_event_id) {
            deletion_vector.mark_deleted(*ordinal);
        }
    }
    deletion_vector
}

/// Aggregate shortcut adjustment co-published alongside a deletion vector.
///
/// Invertible aggregate shortcuts (SUM, COUNT) subtract this delta rather than rescanning the file. MIN/MAX use the
/// granule-extreme rule; if neither covers the needed aggregate, the reader falls back to a scan.
#[derive(Debug, Clone, Copy)]
pub struct DeletionAggregateDelta {
    pub deleted_count: u64,
    /// Sum of the deleted rows' values for the relevant numeric column; `None` when the column is non-numeric or the
    /// sum was not precomputed.
    pub deleted_sum: Option<i128>,
}

impl DeletionAggregateDelta {
    /// Adjusts a COUNT aggregate by subtracting the number of deleted rows.
    pub fn subtract_count(&self, total_count: u64) -> u64 {
        total_count.saturating_sub(self.deleted_count)
    }

    /// Adjusts a SUM aggregate by subtracting the sum of deleted rows. Returns `None` if no precomputed sum is
    /// available, or if the adjusted result would overflow `i128` — either way, the caller must fall back to a scan.
    pub fn subtract_sum(&self, total_sum: i128) -> Option<i128> {
        total_sum.checked_sub(self.deleted_sum?)
    }
}

/// Materialization state for one model-derived (Tier B) column value on one event.
///
/// A correction or delete invalidates the affected event and every event in its model-neighbor dependency closure
/// (e.g. Hawkes parent/children, co-cluster members, trailing-window co-members): each moves to `Pending` until
/// recompute republishes it. A query never serves a `Pending` event's prior value, and a scan is never used to
/// recompute a model-derived value on the query path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DerivedColumnState {
    Pending,
    Published { value: f64, watermark_sequence: u64 },
}

/// Tracks per-event materialization state for one model-derived column, and the recompute jobs invalidation has
/// enqueued for it.
#[derive(Debug, Default)]
pub struct DerivedColumnLedger {
    /// Invalidation order: `Some(id)` at a still-pending slot, `None` at a slot a later `republish` tombstoned. A
    /// slot is cleared in place rather than removed, so `republish` never rescans the whole queue.
    recompute_queue: Vec<Option<EventId>>,
    /// Each still-pending event's slot in `recompute_queue`, so `republish` tombstones it directly instead of
    /// searching for it.
    queue_position: HashMap<EventId, usize>,
    states: HashMap<EventId, DerivedColumnState>,
}

impl DerivedColumnLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks `event_id` and every event in its model-neighbor dependency closure `neighbors` as pending, and
    /// enqueues each for recompute. Call this when a correction or delete affects `event_id`.
    pub fn invalidate(&mut self, event_id: EventId, neighbors: &[EventId]) {
        for id in std::iter::once(event_id).chain(neighbors.iter().copied()) {
            let previous = self.states.insert(id, DerivedColumnState::Pending);
            if previous != Some(DerivedColumnState::Pending) {
                let position = self.recompute_queue.len();
                self.recompute_queue.push(Some(id));
                self.queue_position.insert(id, position);
            }
        }
    }

    /// Republishes a recomputed value for `event_id`, clearing its pending state and removing it from the recompute
    /// queue.
    pub fn republish(&mut self, event_id: EventId, value: f64, watermark_sequence: u64) {
        let previous = self.states.insert(
            event_id,
            DerivedColumnState::Published {
                value,
                watermark_sequence,
            },
        );
        if previous == Some(DerivedColumnState::Pending)
            && let Some(position) = self.queue_position.remove(&event_id)
            && let Some(slot) = self.recompute_queue.get_mut(position)
        {
            *slot = None;
        }
    }

    /// The value a query should serve for `event_id`: `None` while pending, so an invalidated event never serves its
    /// stale pre-correction number.
    pub fn published_value(&self, event_id: EventId) -> Option<f64> {
        match self.states.get(&event_id)? {
            DerivedColumnState::Published { value, .. } => Some(*value),
            DerivedColumnState::Pending => None,
        }
    }

    /// Events still awaiting a recompute republish, in the order they were invalidated.
    pub fn pending_recomputes(&self) -> Vec<EventId> {
        self.recompute_queue.iter().filter_map(|slot| *slot).collect()
    }
}

/// Canonical entity classification for erasure purposes: a natural person whose data may be erased, a business or
/// financial record that may reference one, or a legal entity that is never a data subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntityClass {
    BusinessRecord,
    DataSubject,
    LegalEntity,
}

/// The disposition a jurisdiction erasure profile assigns to an entity class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErasureDisposition {
    CryptoShred,
    PhysicalDestruction,
    Retain,
    RestrictSuppress,
}

impl ErasureDisposition {
    /// Protection ranking used to resolve conflicting jurisdictions when no carve-out applies: higher is more
    /// protective (more thoroughly erased or hidden).
    fn protection_rank(self) -> u8 {
        match self {
            ErasureDisposition::Retain => 0,
            ErasureDisposition::RestrictSuppress => 1,
            ErasureDisposition::PhysicalDestruction => 2,
            ErasureDisposition::CryptoShred => 3,
        }
    }
}

/// One jurisdiction's declared disposition for an entity class, including whether it is a non-overridable
/// lawful-retention carve-out (e.g. a tax/accounting retention obligation) that wins regardless of how protective
/// other jurisdictions' dispositions are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityDisposition {
    pub disposition: ErasureDisposition,
    pub retention_carve_out: bool,
}

/// Resolves the disposition to apply to an entity when a data subject falls under multiple jurisdiction profiles
/// whose per-entity-class dispositions conflict. A non-overridable retention carve-out always wins; otherwise the
/// most-protective disposition wins. Returns `None` if `dispositions` is empty — no jurisdiction applies.
pub fn resolve_conflicting_dispositions(dispositions: &[EntityDisposition]) -> Option<EntityDisposition> {
    dispositions
        .iter()
        .find(|d| d.retention_carve_out)
        .or_else(|| dispositions.iter().max_by_key(|d| d.disposition.protection_rank()))
        .copied()
}

crate::typed_id::define_typed_id!(
    /// Public identity of a data subject (e.g. a `Person`/`Contact`/`Lead`) whose personal data may be subject to
    /// erasure.
    SubjectId,
    SubjectIdTag,
    "subject"
);

/// Marker written over a redacted personal field; distinguishes "redacted" from "was always empty".
const REDACTED_FIELD_MARKER: &str = "[redacted]";

/// A business or financial record (e.g. an invoice) that references a data subject via a personal foreign key.
#[derive(Debug, Clone)]
pub struct BusinessRecord {
    pub amount: i128,
    pub personal_fields: BTreeMap<String, String>,
    pub subject_id: Option<SubjectId>,
}

/// De-identifies `record` in place for a data-subject erasure that a lawful basis (e.g. tax/accounting) requires the
/// record to survive: nulls the personal foreign key and redacts every personal field, while `amount` and the record
/// itself are retained so revenue still aggregates de-identified.
pub fn de_identify_business_record(record: &mut BusinessRecord) {
    record.subject_id = None;
    for value in record.personal_fields.values_mut() {
        *value = REDACTED_FIELD_MARKER.to_owned();
    }
}

/// A per-data-subject content key used to seal that subject's payload bytes in the payload arena.
///
/// Sealing uses the pinned AEAD (AES-256-GCM by default), so a tampered block is rejected on decrypt instead of
/// returning altered bytes. Destroying the key (crypto-shredding) makes every payload sealed under it computationally
/// unrecoverable — everywhere it persists (HEJ, HEF, and backups) — without touching the ciphertext bytes themselves or
/// waiting on an HEF rewrite.
///
/// The nonce-reuse guard is the shared [`SealedContentKey`], which only protects seals made through one live instance:
/// a process that reuses key material must resume from a persisted epoch, either by wrapping a keystore checkout with
/// [`from_sealing_key`](Self::from_sealing_key) or by passing the persisted epoch to
/// [`generate_with`](Self::generate_with) — restarting at epoch 0 with old material can repeat a nonce.
#[derive(Debug, Clone)]
pub struct SubjectContentKey {
    key: Option<SealedContentKey>,
    key_epoch: u64,
    scheme: AeadScheme,
}

impl SubjectContentKey {
    /// Creates a live key from 32 bytes of key material, sealing under the default AEAD at key epoch 0.
    pub fn generate(key_material: [u8; 32]) -> Self {
        Self::generate_with(key_material, 0, AeadScheme::default())
    }

    /// Creates a live key that seals under `scheme` at `key_epoch` — used when a key's nonce space has been rolled to a
    /// fresh epoch, or when a high-volume path selects the extended-nonce cipher.
    pub fn generate_with(key_material: [u8; 32], key_epoch: u64, scheme: AeadScheme) -> Self {
        Self {
            key: Some(SealedContentKey::new(ContentKey::new(key_material), key_epoch, scheme)),
            key_epoch,
            scheme,
        }
    }

    /// Wraps a sealing key checked out from the durable keystore, so sealing resumes past every epoch a previous run
    /// reserved — a block re-sealed after a restart can never repeat a nonce an earlier run used.
    pub fn from_sealing_key(sealing_key: SealedContentKey) -> Self {
        let key_epoch = sealing_key.key_epoch();
        let scheme = sealing_key.scheme();
        Self {
            key: Some(sealing_key),
            key_epoch,
            scheme,
        }
    }

    /// The key epoch this key seals under.
    pub fn key_epoch(&self) -> u64 {
        self.key_epoch
    }

    /// The AEAD scheme this key seals with.
    pub fn scheme(&self) -> AeadScheme {
        self.scheme
    }

    /// Seals `plaintext` as block `block_id` of file `file_id`, or `None` once the key has been destroyed. The returned
    /// blob carries its own nonce, so [`rebuild_payload`] needs nothing but the blob, the key, and the block and file
    /// ids to open it.
    ///
    /// Nonce uniqueness is enforced by the shared [`SealedContentKey`] guard: a re-seal of the same `block_id` rolls
    /// the epoch until the nonce is fresh before sealing, so an AES-GCM nonce reuse — catastrophic for the cipher — is
    /// impossible through this API regardless of what `block_id` the caller supplies. A key built from a keystore
    /// checkout also returns `None` once its reserved epoch range is spent, rather than roll into another checkout's
    /// epochs.
    pub fn encrypt(&mut self, block_id: u64, file_id: u128, plaintext: &[u8]) -> Option<Vec<u8>> {
        let key = self.key.as_mut()?;
        let sealed = key.encrypt(block_id, file_id, plaintext)?;
        self.key_epoch = key.key_epoch();
        Some(sealed)
    }

    /// Destroys the key. Every payload sealed under it becomes permanently unrecoverable; further encrypt calls return
    /// `None` and rebuilds return a tombstone. The key material is scrubbed from memory as it is dropped.
    pub fn destroy(&mut self) {
        self.key = None;
    }

    /// True once `destroy` has been called.
    pub fn is_destroyed(&self) -> bool {
        self.key.is_none()
    }

    /// Opens a blob produced by [`encrypt`](Self::encrypt), checking it really is block `block_id` of file `file_id`:
    /// the payload when the key is live and the block authenticates, a tombstone once the key is destroyed, and a
    /// rejection when a live key meets a tampered block or one replayed into a different slot or a different file.
    fn open(&self, block_id: u64, file_id: u128, sealed: &[u8]) -> RebuiltPayload {
        match &self.key {
            None => RebuiltPayload::Tombstone,
            Some(key) => match key.decrypt(block_id, file_id, sealed) {
                Some(plaintext) => RebuiltPayload::Payload(plaintext),
                None => RebuiltPayload::Rejected,
            },
        }
    }
}

/// The outcome of rebuilding one subject's payload bytes from a sealed block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuiltPayload {
    /// The key is live and the block authenticated to these bytes.
    Payload(Vec<u8>),
    /// The key is live but the block failed authentication — its ciphertext or bound block identity was tampered with.
    /// Rebuild rejects it rather than returning altered bytes.
    Rejected,
    /// The subject's content key has been destroyed: the bytes are unrecoverable. Non-erased structural metadata may
    /// still surround this tombstone; replay is never blocked or corrupted by it.
    Tombstone,
}

/// Rebuilds a subject's payload from a `sealed` block that the caller expects to be block `block_id` of file `file_id`,
/// under `key`. Erasure-aware and tamper-aware: a destroyed key produces a tombstone, a tampered block or one replayed
/// into a different slot or a different file is rejected, and a live key over an intact block returns the payload —
/// never stale bytes, altered bytes, or a blocked replay.
pub fn rebuild_payload(key: &SubjectContentKey, block_id: u64, file_id: u128, sealed: &[u8]) -> RebuiltPayload {
    key.open(block_id, file_id, sealed)
}

/// Deleted count below which a deletion vector's wire form is the sorted ordinal array rather than the positional
/// Roaring bitmap. Correction and erasure vectors are usually a handful of ordinals, where the array is smaller than
/// any bitmap; the value is a starting pin awaiting the measured correction-workload distribution before the
/// native-block feature ships.
pub const DELETION_VECTOR_ORDINAL_ARRAY_MAX: u64 = 4096;

/// Decode-bomb guard on a deletion vector's decoded row count, so a forged run list cannot hand readers a vector whose
/// ordinals no file could hold.
const MAX_DELETION_VECTOR_DECODED_ROWS: u64 = 64 * 1024 * 1024;

/// The two pinned wire encodings a deletion vector may be stored in. Both decode to the identical set of deleted row
/// positions — the form changes bytes, never semantics — and the stored bytes carry the form in their leading tag so a
/// reader decodes without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionVectorWireForm {
    /// A sorted `u32` ordinal array: smaller than any bitmap for the sparse vectors corrections and erasure usually
    /// produce. Chosen below [`DELETION_VECTOR_ORDINAL_ARRAY_MAX`] deleted rows when every ordinal fits `u32`.
    OrdinalArray,
    /// The positional Roaring run form, for dense vectors (or any ordinal beyond `u32`).
    PositionalRoaring,
}

impl DeletionVectorWireForm {
    fn tag(self) -> u8 {
        match self {
            DeletionVectorWireForm::OrdinalArray => 1,
            DeletionVectorWireForm::PositionalRoaring => 2,
        }
    }
}

/// Serializes a deletion vector in the wire form its density selects: the sorted ordinal array below
/// [`DELETION_VECTOR_ORDINAL_ARRAY_MAX`] deleted rows (when every ordinal fits `u32`), the positional Roaring form
/// otherwise. Selection is deterministic — the same deleted set always produces the same form and the same bytes — and
/// [`decode_deletion_vector`] round-trips either form to the identical set.
pub fn encode_deletion_vector(vector: &DeletionVector) -> (DeletionVectorWireForm, Vec<u8>) {
    let fits_ordinals = vector.deleted_count() < DELETION_VECTOR_ORDINAL_ARRAY_MAX
        && vector.iter().all(|ordinal| ordinal <= u64::from(u32::MAX));
    if fits_ordinals {
        let mut out = Writer::with_capacity(5 + vector.deleted_count() as usize * 4);
        out.put_u8(DeletionVectorWireForm::OrdinalArray.tag());
        out.put_u32(vector.deleted_count() as u32);
        for ordinal in vector.iter() {
            out.put_u32(ordinal as u32);
        }
        (DeletionVectorWireForm::OrdinalArray, out.into_bytes())
    } else {
        let bitmap = RoaringRangeBitmap::from_ranges(vector.deleted_runs.iter().copied());
        let encoded = bitmap.encode();
        let mut out = Writer::with_capacity(1 + encoded.len());
        out.put_u8(DeletionVectorWireForm::PositionalRoaring.tag());
        out.put_slice(&encoded);
        (DeletionVectorWireForm::PositionalRoaring, out.into_bytes())
    }
}

/// Reads a deletion vector back from either wire form, dispatching on the leading tag. Whichever form the bytes carry,
/// the decoded set of deleted row positions is identical to what was encoded; malformed input — an unknown tag,
/// unsorted ordinals, or a forged bitmap amplifying past the decode bound — refuses instead of decoding wrong.
pub fn decode_deletion_vector(bytes: &[u8]) -> Result<DeletionVector, FormatError> {
    let mut reader = Reader::new(bytes);
    let tag = reader.u8("deletion vector wire form")?;
    let mut vector = DeletionVector::new();
    if tag == DeletionVectorWireForm::OrdinalArray.tag() {
        let count = reader.u32("deletion vector ordinal count")? as usize;
        let mut previous: Option<u32> = None;
        for _ in 0..count {
            let ordinal = reader.u32("deletion vector ordinal")?;
            if previous.is_some_and(|previous| ordinal <= previous) {
                return Err(FormatError::Structural {
                    rule: "deletion vector ordinals must be strictly ascending",
                });
            }
            previous = Some(ordinal);
            vector.mark_deleted(u64::from(ordinal));
        }
        return Ok(vector);
    }
    if tag == DeletionVectorWireForm::PositionalRoaring.tag() {
        let bitmap = RoaringRangeBitmap::decode(reader.take(reader.remaining(), "deletion vector bitmap")?)?;
        if bitmap.count() > MAX_DELETION_VECTOR_DECODED_ROWS {
            return Err(FormatError::Structural {
                rule: "deletion vector decodes past the row-count bound",
            });
        }
        for range in bitmap.ranges() {
            vector.mark_range(*range);
        }
        return Ok(vector);
    }
    Err(FormatError::Structural {
        rule: "unknown deletion vector wire form",
    })
}

#[cfg(test)]
#[path = "test/deletes.rs"]
mod tests;
