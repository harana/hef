//! Turns a run of events into one finished, self-describing column file: events become columns, columns are grouped
//! into granules and stripes, and a footer ties it all together.
//!
//! The builder is deterministic: the same rows and configuration produce byte-identical files, which is what makes
//! publication idempotent (the file identity is derived from its content).

use crate::artifacts::batch::{
    EncodedPayload, EventInput, PAYLOAD_FLAG_EXTERNAL_REF, PayloadInput, encode_variant_dictionary,
};
use crate::columns::{
    FreetextDeclaration, PROVENANCE_COLUMNS, PathStatistics, PromotionPlan, RELATIONSHIP_COLUMNS, REQUIRED_COLUMNS,
    SHARED_DICTIONARY_MAX_VALUES, column_ids, promoted_present, validate_promotion_plan,
};
use crate::encoding::{
    BlockStats, CascadeStrategy, ColumnData, FsstTable, ReplayCapture, SideStream, StringColumn, ValueKind,
    count_set_bits, encode_block_replayed, encode_block_with_shared_dictionary, seekable_zstd,
};
use crate::error::FormatError;
use crate::events::provenance::{SignedEventProvenance, hex_lower_into};
use crate::events::relationships::{EventRelationships, RelationshipKind};
use crate::events::variant::{
    EncodeScratch, KeyDictionary, VariantRef, VariantValue, encode_object_fields_into_with_scratch,
    encode_value_into_with_scratch,
};
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::file::bytes::Writer;
use crate::file::constant::{CHUNK_GROUP_BYTES, TREE_TRAILER_MAGIC};
use crate::indexes::ndv::NdvSketch;
use crate::indexes::probabilistic::SplitBlockBloomFilter;
use crate::indexes::text_token::TextTokenIndex;
use crate::invariants::EncodeExecutor;
use crate::invariants::sim::SerialEncodeExecutor;
use crate::layout::footer::{
    ClusteringEntry, ColumnDescriptor, ColumnKind, ColumnMark, EntityHashFilterEntry, ExactCounts, FileDictionaries,
    Footer, FreetextEntry, FreetextRowOffsets, GranuleEntry, MarksPageEntry, PageDirectoryEntry, PageMinMax, PageStats,
    PayloadGranule, PresenceEntry, ResidualCompression, SharedDictionaryEntry, ShredEntry, SortDirection,
    SortednessProof, StripeEntry, StripeNdvEntry, StripeProofEntry, TextTokenEntry, TextTokenOffsetsEntry,
    encode_stripe_marks_pages,
};
use crate::layout::reader::{HefFile, PayloadRead};
use crate::layout::{
    EMPTY_VALUE_ROW_OFFSET, HEADER_BLOCK_LEN, HefHeader, LayoutClass, LayoutTargets, MAX_PAGE_BYTES, encode_header,
    optional_features, required_features,
};
use crate::security::{AeadScheme, FooterEncryption};
use crate::writer::projection::{RowKey, RowOrder};
use hashbrown::{HashMap, HashSet};
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Stable writer phases exposed to benchmark tooling without coupling the storage engine to a reporting format.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BuildPhase {
    SourceDecoding,
    Normalization,
    DictionaryConstruction,
    PathStatistics,
    GranuleConstruction,
    BlockEncoding,
    Layout,
    FooterConstruction,
    Integrity,
    FileAssembly,
    RowDestruction,
}

/// Wall-clock phase samples from one HEF build. Collection is opt-in; the ordinary build functions do not read a
/// clock or allocate this structure.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BuildProfile {
    pub phases: Vec<(BuildPhase, u64)>,
}

struct PhaseClock<'a> {
    phase: BuildPhase,
    profile: Option<&'a mut BuildProfile>,
    started: Option<Instant>,
}

impl<'a> PhaseClock<'a> {
    fn new(profile: Option<&'a mut BuildProfile>, phase: BuildPhase) -> Self {
        let started = profile.as_ref().map(|_| Instant::now());
        Self {
            phase,
            profile,
            started,
        }
    }

    fn transition(&mut self, next: BuildPhase) {
        self.finish_current();
        self.phase = next;
        self.started = self.profile.as_ref().map(|_| Instant::now());
    }

    fn enabled(&self) -> bool {
        self.profile.is_some()
    }

    fn finish(mut self) {
        self.finish_current();
    }

    /// Closes the running phase and hands the profile back, for a build that starts over with a fresh clock.
    fn into_profile(mut self) -> Option<&'a mut BuildProfile> {
        self.finish_current();
        self.profile.take()
    }

    fn finish_current(&mut self) {
        if let (Some(profile), Some(started)) = (self.profile.as_deref_mut(), self.started.take()) {
            let elapsed = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            if let Some((_, total)) = profile.phases.iter_mut().find(|(phase, _)| *phase == self.phase) {
                *total = total.saturating_add(elapsed);
            } else {
                profile.phases.push((self.phase, elapsed));
            }
        }
    }
}

/// Stable worker frames retained in optimized builds, so sampling-profiler trees can group Rayon work by HEF phase.
#[inline(never)]
fn profile_granule_construction<T>(work: impl FnOnce() -> T) -> T {
    std::hint::black_box("hef_writer_granule_construction");
    let result = work();
    std::hint::black_box("hef_writer_granule_construction");
    result
}

#[inline(never)]
fn profile_block_encoding<T>(work: impl FnOnce() -> T) -> T {
    std::hint::black_box("hef_writer_block_encoding");
    let result = work();
    std::hint::black_box("hef_writer_block_encoding");
    result
}

macro_rules! worker_phase_signpost {
    ($function:ident, $name:literal) => {
        #[inline(never)]
        fn $function<T>(work: impl FnOnce() -> T) -> T {
            std::hint::black_box($name);
            let result = work();
            std::hint::black_box($name);
            result
        }
    };
}

worker_phase_signpost!(profile_integrity, "hef_writer_integrity");
worker_phase_signpost!(profile_layout, "hef_writer_layout");
worker_phase_signpost!(profile_footer_metadata, "hef_writer_footer_metadata");
worker_phase_signpost!(profile_path_statistics, "hef_writer_path_statistics");

#[inline(always)]
fn maybe_profile_granule_construction<T>(enabled: bool, work: impl FnOnce() -> T) -> T {
    if enabled {
        profile_granule_construction(work)
    } else {
        work()
    }
}

#[inline(always)]
fn maybe_profile_block_encoding<T>(enabled: bool, work: impl FnOnce() -> T) -> T {
    if enabled { profile_block_encoding(work) } else { work() }
}

/// One row entering a file: its assigned internal identity plus the event. Converted into a [`BuildRow`] on the way
/// into a build; a caller that can produce rows in that shape directly spares the conversion.
#[derive(Debug, Clone, PartialEq)]
pub struct HefRow {
    pub epoch: u64,
    pub event: EventInput,
    pub sequence: u64,
}

/// One authoritative row replacement supplied by the update transaction, in either row shape. Carrying both versions
/// lets the writer derive affected columns itself; callers cannot declare a stale block reusable by naming columns
/// manually.
#[derive(Debug, Clone, PartialEq)]
pub struct HefRowChange<R = HefRow> {
    pub after: R,
    pub before: R,
    pub row_ordinal: u64,
}

/// A payload field's name as the writer takes it: shared, so a name that repeats across rows — nearly every one
/// does — is one allocation however many rows carry it.
pub type FieldName = Arc<str>;

/// One row's payload as the writer takes it: the forms [`PayloadInput`] carries, with an object laid out as a sorted
/// field list instead of a tree, so a row costs one allocation for its fields rather than one per field.
#[derive(Clone, Debug, PartialEq)]
pub enum BuildPayload {
    Encoded(EncodedPayload),
    ExternalRef(String),
    None,
    /// A top-level object as its fields in ascending key order with no key repeated — the order a `BTreeMap` yields
    /// them in. The build refuses any other order.
    Object(Vec<(FieldName, VariantValue)>),
    /// A payload that is not an object.
    Whole(VariantValue),
}

/// The event envelope as the writer takes it: [`EventEnvelope`] field for field, with the three dictionary-coded
/// strings shared so the rows of one source or type carry one allocation between them instead of a copy each.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildEnvelope {
    pub account_id: Option<String>,
    pub account_id_hash_low: u64,
    pub actor_id: Option<String>,
    pub actor_id_hash_low: u64,
    pub dedupe_hash_high: u64,
    pub dedupe_hash_low: u64,
    pub entity_id: Option<String>,
    pub entity_id_hash_high: u64,
    pub entity_id_hash_low: u64,
    pub entity_type: Arc<str>,
    pub event_id: EventId,
    pub event_type: Arc<str>,
    pub flags: EventFlags,
    pub ingested_at: TimestampValue,
    pub occurred_at: TimestampValue,
    pub schema_version: u32,
    pub source: Arc<str>,
    pub stream_id: StreamId,
    pub stream_sequence: u64,
    pub tenant_id: TenantId,
    pub trace_id_hash_low: u64,
}

/// One row as the writer takes it: what a [`HefRow`] carries that the file stores, in the shape the build reads.
/// A caller producing rows in this shape hands the build nothing to convert; [`HefRow`]s are converted on the way in.
#[derive(Clone, Debug, PartialEq)]
pub struct BuildRow {
    pub envelope: BuildEnvelope,
    pub epoch: u64,
    pub payload: BuildPayload,
    /// Boxed because it is a couple of hundred bytes that only a signed event carries, and every pass over the rows
    /// streams the whole row.
    pub provenance: Option<Box<SignedEventProvenance>>,
    pub relationships: Option<EventRelationships>,
    pub sequence: u64,
}

/// A row shape a build takes: [`BuildRow`] as it is, or [`HefRow`] converted on the way in.
pub trait BuildInput: Clone + Send {
    /// The rows in the writer's own shape. Event rows are converted on the worker pool, each worker sharing the
    /// envelope strings and field names it has already seen across the rows it converts.
    fn into_build_rows(rows: Vec<Self>) -> Vec<BuildRow>;

    /// The change set in the writer's own shape; borrowed as it is when it already is.
    fn build_changes(changes: &[HefRowChange<Self>]) -> Cow<'_, [HefRowChange<BuildRow>]>;
}

impl BuildInput for BuildRow {
    fn into_build_rows(rows: Vec<Self>) -> Vec<BuildRow> {
        rows
    }

    fn build_changes(changes: &[HefRowChange<Self>]) -> Cow<'_, [HefRowChange<BuildRow>]> {
        Cow::Borrowed(changes)
    }
}

impl BuildInput for HefRow {
    fn into_build_rows(rows: Vec<Self>) -> Vec<BuildRow> {
        rows.into_par_iter()
            .map_init(SharedStrings::default, |shared, row| shared.build_row(row))
            .collect()
    }

    fn build_changes(changes: &[HefRowChange<Self>]) -> Cow<'_, [HefRowChange<BuildRow>]> {
        let mut shared = SharedStrings::default();
        Cow::Owned(
            changes
                .iter()
                .map(|change| HefRowChange {
                    after: shared.build_row(change.after.clone()),
                    before: shared.build_row(change.before.clone()),
                    row_ordinal: change.row_ordinal,
                })
                .collect(),
        )
    }
}

/// Hands out one shared copy of each distinct string it is asked for, so the rows converted through it share their
/// envelope strings and field names instead of each carrying a copy.
#[derive(Default)]
pub struct SharedStrings {
    strings: HashSet<Arc<str>>,
}

impl SharedStrings {
    /// The shared copy of `text`: the one handed out for it before, or a fresh one on first sight.
    pub fn share(&mut self, text: &str) -> Arc<str> {
        if let Some(shared) = self.strings.get(text) {
            return shared.clone();
        }
        let shared: Arc<str> = Arc::from(text);
        self.strings.insert(shared.clone());
        shared
    }

    /// One event row in the writer's shape, its strings shared through this set.
    pub fn build_row(&mut self, row: HefRow) -> BuildRow {
        let HefRow { epoch, event, sequence } = row;
        let EventInput {
            envelope,
            payload,
            provenance,
            relationships,
            ..
        } = event;
        BuildRow {
            envelope: BuildEnvelope {
                account_id: envelope.account_id,
                account_id_hash_low: envelope.account_id_hash_low,
                actor_id: envelope.actor_id,
                actor_id_hash_low: envelope.actor_id_hash_low,
                dedupe_hash_high: envelope.dedupe_hash_high,
                dedupe_hash_low: envelope.dedupe_hash_low,
                entity_id: envelope.entity_id,
                entity_id_hash_high: envelope.entity_id_hash_high,
                entity_id_hash_low: envelope.entity_id_hash_low,
                entity_type: self.share(&envelope.entity_type),
                event_id: envelope.event_id,
                event_type: self.share(&envelope.event_type),
                flags: envelope.flags,
                ingested_at: envelope.ingested_at,
                occurred_at: envelope.occurred_at,
                schema_version: envelope.schema_version,
                source: self.share(&envelope.source),
                stream_id: envelope.stream_id,
                stream_sequence: envelope.stream_sequence,
                tenant_id: envelope.tenant_id,
                trace_id_hash_low: envelope.trace_id_hash_low,
            },
            epoch,
            payload: match payload {
                PayloadInput::Encoded(encoded) => BuildPayload::Encoded(encoded),
                PayloadInput::ExternalRef(reference) => BuildPayload::ExternalRef(reference),
                PayloadInput::None => BuildPayload::None,
                PayloadInput::Variant(value) => self.build_payload(value),
            },
            provenance: provenance.map(Box::new),
            relationships,
            sequence,
        }
    }

    /// A decoded payload in the writer's shape: an object as its sorted field list, anything else whole.
    fn build_payload(&mut self, value: VariantValue) -> BuildPayload {
        match value {
            VariantValue::Object(fields) => BuildPayload::Object(
                fields
                    .into_iter()
                    .map(|(name, value)| (self.share(&name), value))
                    .collect(),
            ),
            whole => BuildPayload::Whole(whole),
        }
    }
}

/// A pre-computed analytical column — context projection or embedding — to include in the published file alongside the
/// required columns. The caller supplies one value per row, derived from the event payload or an ML pipeline; the
/// builder writes each as an independent column block.
#[derive(Debug, Clone)]
pub struct AnalyticalColumn {
    /// Stable column id; use `column_ids::CONTEXT_BASE` for context projection columns and
    /// `column_ids::EMBEDDING_BASE` for embeddings.
    pub column_id: u32,
    /// Pre-computed values, one per row in the same order as the input rows.
    pub data: ColumnData,
    /// True for embedding/vector columns, which are blocked from public output at the scan boundary; false for context
    /// projection columns, which are public-safe when the caller is authorized.
    pub internal_only: bool,
    pub kind: ColumnKind,
    pub name: String,
    /// Declares that queries filter this column by substring (`CONTAINS`), so its blocks' token filter also carries
    /// trigrams and a substring predicate prunes granules instead of decoding every one. Off by default: trigrams cost
    /// filter bytes and prune nothing extra for a column filtered only by whole value. Only a public
    /// (`internal_only == false`) string column may declare it; the build refuses any other.
    pub substring_searchable: bool,
}

/// Where the file being built sits in its lifecycle. A fresh publication biases its cascades for decode speed and
/// keeps residual arenas uncompressed for offset-jump point access; a rewrite or compaction biases for smaller stored
/// bytes and may compress a granule's residual arena into seekable Zstd-3 frames when that shrinks it. Selected by
/// the writing path's
/// lifecycle stage, never by a caller-facing query knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildLifecycle {
    FreshPublication,
    /// A fresh publication whose column blocks take no trailing compression stage at all. A measurement control for
    /// the benchmark harness, so the stage's read cost can be put against the same rows stored without it; no
    /// production path selects it.
    FreshPublicationWithoutTrailing,
    RewriteOrCompaction,
}

impl BuildLifecycle {
    fn cascade_strategy(self) -> CascadeStrategy {
        match self {
            BuildLifecycle::FreshPublication => CascadeStrategy::for_fresh_publication(),
            BuildLifecycle::FreshPublicationWithoutTrailing => CascadeStrategy::NoTrailing,
            BuildLifecycle::RewriteOrCompaction => CascadeStrategy::for_rewrite_or_compaction(),
        }
    }
}

/// Build configuration for one publication.
#[derive(Debug, Clone)]
pub struct HefBuildConfig {
    /// Pre-computed analytical columns (context projections, embeddings) to include alongside the required columns.
    /// Empty by default.
    pub analytical_columns: Vec<AnalyticalColumn>,
    pub created_at_physical: i64,
    /// The 32-byte file DEK to seal the footer under when `footer_encryption` is `Encrypted`. The caller supplies it;
    /// the writer derives no keys of its own. Ignored (and normally `None`) for a plaintext footer.
    pub footer_dek: Option<[u8; 32]>,
    /// Whether to seal the footer (column directory, dictionaries, layout metadata) under the file DEK. `Plaintext` by
    /// default; a tenant whose policy marks the column schema sensitive sets `Encrypted` and supplies `footer_dek`.
    pub footer_encryption: FooterEncryption,
    pub freetext: FreetextDeclaration,
    /// Whether to also store every declared free-text value a second time, raw and uncompressed, in a per-granule
    /// arena with an `(offset, len)` entry per row, so a point read jumps straight to one row's bytes. Off by
    /// default: a free-text column the encoder stored as FSST or a dictionary is already addressable per value
    /// through the block's own offsets, and on measured data the arena costs about as much as the whole rest of the
    /// file. Turn it on where cold single-row free-text reads are measured hot enough to want the offset jump — or
    /// where the values are long enough that the encoder stores them raw, which the reader does not address per
    /// value.
    pub freetext_row_offset_index: bool,
    pub generation_id: u64,
    /// IO alignment for column block boundaries in bytes. When non-zero, the writer pads each column block to a
    /// multiple of this value and records it in the footer, enabling aligned `O_DIRECT` reads of individual pages.
    /// Zero disables alignment recording and uses the default 64-byte padding.
    pub io_alignment_bytes: u32,
    /// Which lifecycle stage this build serves; it selects the cascade strategy and whether residual arenas may be
    /// compressed. Fresh publication for the write path; rewrite/compaction passes cover cold data.
    pub lifecycle: BuildLifecycle,
    /// Maximum rows per page within a granule. When non-zero and a granule has more rows, the writer splits the column
    /// data into independently decodable pages and records per-page byte ranges in the PAGE_DIRECTORY footer section.
    /// Zero means one page per granule (current default).
    pub page_size_rows: usize,
    pub promotion: PromotionPlan,
    pub targets: LayoutTargets,
    pub tenant_id: TenantId,
}

/// Granules per replay segment: the first granule of each segment runs full encoding selection and later granules of
/// the same column replay its captured transform (re-armed by the encoder's trip-wire), so selection cost is paid once
/// per segment instead of once per block on distribution-stable columns. A pinned code parameter, never operator
/// config.
const REPLAY_SEGMENT_GRANULES: usize = 16;

/// A sealed in-memory file plus its identity metadata.
#[derive(Debug)]
pub struct BuiltHef {
    /// Bytes the build did not store twice because a byte-identical block in the same stripe was aliased instead — a
    /// build statistic; the file's decode semantics are unchanged by aliasing.
    pub aliased_block_bytes: u64,
    /// Number of column blocks encoded by this build. For an update, this excludes blocks borrowed from the source.
    pub encoded_blocks: u64,
    /// Encoded source bytes reused by an update without decoding or re-encoding their column blocks.
    pub reused_source_block_bytes: u64,
    /// Number of source column blocks reused by an update. Zero for an ordinary build or a conservative fallback.
    pub reused_source_blocks: u64,
    pub bytes: Vec<u8>,
    /// Authoritative domain-separated segment seal recorded in the manifest entry.
    pub file_seal: [u8; 32],
    /// Whole-file CRC-64/NVME, computed while the finished ranges are streamed. Handed to a CRC-64/NVME-capable object
    /// provider as the upload checksum so it can reject a corrupted commit; a non-authoritative precheck only.
    pub file_crc64_nvme: u64,
    /// Content-derived file identity: identical input produces the same identity, making re-publication idempotent.
    pub file_id: u128,
    pub footer: Footer,
    /// Byte length of the trailing footer region (footer blob + length word + `"HEF1"` magic), recorded in the manifest
    /// entry as the tail geometry for one-request cold opens.
    pub footer_len: u64,
    pub header: HefHeader,
    /// Total streamed/assembled file length in bytes.
    pub total_len: u64,
    /// Byte length of the complete verified-streaming appendix appended after the footer, including its length word
    /// and `"HEFT"` magic, or `None` when every stripe is small enough to verify with a whole-stripe read.
    pub tree_len: Option<u64>,
}

/// Reuse decisions derived from complete before/after rows. The source block list is never caller-supplied: deriving
/// it here prevents an incomplete change declaration from publishing stale column bytes.
#[derive(Clone)]
struct UpdateReuse<'a> {
    changed_columns_by_granule: HashMap<u32, HashSet<u32>>,
    changed_payload_granules: HashSet<u32>,
    path_statistics_changed: bool,
    /// Whether every change left its row's envelope, identity, provenance and relationships alone, touching only the
    /// payload and dedupe hash — what lets a build rebuild nothing but the touched granules.
    payload_only: bool,
    source: &'a BuiltHef,
    sparse: Option<SparsePlan>,
}

impl UpdateReuse<'_> {
    fn block_changed(&self, granule_id: u32, column_id: u32) -> bool {
        self.changed_columns_by_granule
            .get(&granule_id)
            .is_some_and(|columns| columns.contains(&column_id))
    }
}

/// What a sparse update builds: only the granules whose blocks or payload arena changed, and within each only the
/// columns that did — plus, in a replay segment with a changed tail block, the head granule's block of that column,
/// whose values the tail's encode replays. Every other granule's blocks, payload arena, filters and statistics are
/// borrowed from the source without its rows being normalized or even looked at.
#[derive(Clone)]
struct SparsePlan {
    /// Per source granule, in granule order: the columns to build typed pieces for, or `None` for a granule the build
    /// never touches. A granule whose payload arena changed rebuilds it whatever the set holds.
    build: Vec<Option<HashSet<u32>>>,
    /// Whether the rows handed to the build are the whole file, or only the built granules' rows in granule order.
    rows_cover_file: bool,
}

impl SparsePlan {
    fn builds(&self, granule_index: usize) -> Option<&HashSet<u32>> {
        self.build.get(granule_index).and_then(Option::as_ref)
    }
}

/// Whether the source file's promoted, shredded, free-text and analytical routing is exactly what `config` asks for,
/// so its shredding plan can be taken as this build's without re-deriving the path statistics.
fn static_routes_compatible(source: &Footer, config: &HefBuildConfig) -> bool {
    source
        .freetext
        .iter()
        .map(|entry| entry.declared_field.as_str())
        .eq(config.freetext.fields.iter().map(String::as_str))
        && source.presence.len() == config.promotion.columns.len()
        && config.promotion.columns.iter().enumerate().all(|(index, promoted)| {
            let column_id = column_ids::PROMOTED_BASE + index as u32;
            source.presence.iter().any(|entry| {
                entry.column_id == column_id && entry.since_schema_version == promoted.since_schema_version
            }) && source.columns.iter().any(|column| {
                column.column_id == column_id && column.name == promoted.name && column.kind == promoted.kind
            })
        })
        && source
            .columns
            .iter()
            .filter(|column| (column_ids::CONTEXT_BASE..column_ids::PROVENANCE_BASE).contains(&column.column_id))
            .count()
            == config.analytical_columns.len()
        && config.analytical_columns.iter().all(|analytical| {
            source.columns.iter().any(|column| {
                column.column_id == analytical.column_id
                    && column.name == analytical.name
                    && column.kind == analytical.kind
                    && column.internal_only == analytical.internal_only
            })
        })
}

/// Whether a string column of this id may share one file-scope alphabet: the envelope, promoted/shredded and context
/// families, whose alphabets are closed value sets.
fn shareable_column(column_id: u32) -> bool {
    column_id < column_ids::FREETEXT_BASE || (column_ids::CONTEXT_BASE..column_ids::EMBEDDING_BASE).contains(&column_id)
}

/// Plans a sparse update, or `None` when the change set needs the ordinary build over every row: it changes more
/// than payloads and dedupe hashes, alters the shredding plan, changes a promoted or a shared-alphabet string column
/// (whose file-wide statistics and alphabets are derived from every granule), carries analytical columns, or asks
/// for a layout the source was not built with. A granule the source cut by payload bytes is taken to have been cut
/// with these targets; the build still checks the cut of every granule it rebuilds.
fn plan_sparse_update(
    reuse: &UpdateReuse<'_>,
    config: &HefBuildConfig,
    carries_provenance: bool,
    carries_relationships: bool,
    rows_cover_file: bool,
) -> Option<SparsePlan> {
    let footer = &reuse.source.footer;
    let has_column = |column_id: u32| footer.columns.iter().any(|column| column.column_id == column_id);
    let hot_columns = column_ids::PROMOTED_BASE..column_ids::PROMOTED_BASE + config.promotion.columns.len() as u32;
    let file_wide_column = |column_id: u32| {
        hot_columns.contains(&column_id)
            || (shareable_column(column_id)
                && footer
                    .columns
                    .iter()
                    .any(|column| column.column_id == column_id && column.kind == ColumnKind::String))
    };
    if !reuse.payload_only
        || reuse.path_statistics_changed
        || !config.analytical_columns.is_empty()
        || !static_routes_compatible(footer, config)
        || carries_provenance != has_column(column_ids::AUTHOR_PUBKEY)
        || carries_relationships != has_column(column_ids::PARENT_REF)
        || footer.io_alignment_bytes != config.io_alignment_bytes
        || (!config.freetext.fields.is_empty()
            && config.freetext_row_offset_index != !footer.freetext_row_offsets.is_empty())
        || reuse
            .changed_columns_by_granule
            .values()
            .flatten()
            .any(|&column_id| file_wide_column(column_id))
    {
        return None;
    }
    // Every source granule must cut where it did under these targets — no granule may exceed the row target, and a
    // cut neither the row target, an epoch change nor the end of the file explains must be a byte cut — and every
    // source block must be borrowable as stored under this page size.
    let granule_count = footer.granules.len();
    let mut expected_pages: HashMap<u32, u32> = HashMap::with_capacity(granule_count);
    for (index, granule) in footer.granules.iter().enumerate() {
        let row_count = granule.row_count as usize;
        let forced = row_count == config.targets.index_granularity
            || index + 1 == granule_count
            || footer
                .granules
                .get(index + 1)
                .is_some_and(|next| next.first_epoch != granule.last_epoch);
        if row_count > config.targets.index_granularity
            || (!forced && config.targets.index_granularity_bytes == usize::MAX)
        {
            return None;
        }
        let pages = if config.page_size_rows > 0 && row_count > config.page_size_rows {
            row_count.div_ceil(config.page_size_rows) as u32
        } else {
            1
        };
        expected_pages.insert(granule.granule_id, pages);
    }
    if footer
        .marks
        .iter()
        .any(|mark| mark.projection_id == 0 && expected_pages.get(&mark.granule_id).copied() != Some(mark.page_count))
    {
        return None;
    }
    let mut build: Vec<Option<HashSet<u32>>> = vec![None; granule_count];
    for (granule_id, changed) in &reuse.changed_columns_by_granule {
        let index = footer
            .granules
            .iter()
            .position(|granule| granule.granule_id == *granule_id)?;
        let head = index - index % REPLAY_SEGMENT_GRANULES;
        for target in [index, head] {
            if let Some(slot) = build.get_mut(target) {
                slot.get_or_insert_default().extend(changed.iter().copied());
            }
        }
    }
    Some(SparsePlan { build, rows_cover_file })
}

/// The stored bytes at `offset..offset + len` within the stripe holding source granule `granule_id` — how the footer
/// addresses every per-granule extent.
fn source_extent(source: &BuiltHef, granule_id: u32, offset: u64, len: u64) -> Option<&[u8]> {
    let granule = source
        .footer
        .granules
        .iter()
        .find(|granule| granule.granule_id == granule_id)?;
    let stripe = source
        .footer
        .stripes
        .iter()
        .find(|stripe| stripe.stripe_id == granule.stripe_id)?;
    let start = usize::try_from(stripe.file_offset.checked_add(offset)?).ok()?;
    let end = start.checked_add(usize::try_from(len).ok()?)?;
    source.bytes.get(start..end)
}

/// Rough byte size of a variant's *values* — string/binary contents plus fixed-width scalars — for the granule byte
/// cut. Keys are costed separately by the caller. Counting only keys would let rows carrying large string bodies
/// accumulate into a granule whose real arena exceeds the stripe clamp, making the range permanently unpublishable.
fn variant_value_bytes(value: &VariantValue) -> usize {
    match value {
        VariantValue::Array(items) => items.iter().map(variant_value_bytes).sum(),
        VariantValue::Binary(bytes) => bytes.len(),
        VariantValue::Object(fields) => fields.values().map(variant_value_bytes).sum(),
        VariantValue::String(text) => text.len(),
        VariantValue::Decimal { .. } | VariantValue::Uuid(_) => 16,
        _ => 8,
    }
}

/// Collects every object key in `value`, nested keys included, borrowed rather than cloned. The caller sorts and
/// dedups, so the resulting count equals the distinct-key set [`VariantValue::collect_keys`] would produce.
fn collect_key_names<'a>(value: &'a VariantValue, out: &mut Vec<&'a str>) {
    match value {
        VariantValue::Array(items) => {
            for item in items {
                collect_key_names(item, out);
            }
        }
        VariantValue::Object(fields) => {
            for (key, field_value) in fields {
                out.push(key.as_str());
                collect_key_names(field_value, out);
            }
        }
        _ => {}
    }
}

const NON_OBJECT_FIELD_SHAPE: u32 = u32::MAX;

/// How many recently interned payload shapes [`FieldIdentityCatalog::intern_object`] remembers. Enough that rows
/// alternating among the shapes a few optional fields make still find theirs without hashing a key, and few enough
/// that a genuinely new shape costs only that many length comparisons before it is interned.
const RECENT_SHAPES: usize = 4;

/// Bit budget per distinct entity hash in a granule's identity-hash membership filter, and with it the whole size of
/// the point index: about 1.25 stored bytes per distinct entity. Eight bits is the split-block Bloom filter's own
/// reference point (~2% false positives); ten buys about a percent back for a quarter more bytes, which is the right
/// trade when a false positive costs reading a whole granule's hash block.
const ENTITY_HASH_FILTER_BITS_PER_KEY: u32 = 10;

/// A generous ceiling on what one column block adds to its stripe's co-located marks page: its mark's eight
/// bit-packed fields and optional first-value offset, its page-directory entry's eleven fields and four optional
/// bounds, and its share of the framing each of those field arrays carries. Used only to size the data buffer, so an
/// over-estimate costs a fraction of a percent of it and an under-estimate costs one growth, never a wrong offset.
const MARKS_PAGE_BYTES_PER_BLOCK: usize = 512;

/// What the proof appendix appends after the stripe trees: the total tree length, then [`TREE_TRAILER_MAGIC`].
const PROOF_APPENDIX_TRAILER_BYTES: usize = 8 + TREE_TRAILER_MAGIC.len();

/// How many separately aligned regions each granule contributes to the data area: its dictionary, its offsets table,
/// its residual arena, and its marks page blob. Used only to bound the alignment padding when the data buffer is
/// sized, so an over-count costs one reservation and never a wrong offset.
const PADDED_REGIONS_PER_GRANULE: usize = 4;

/// What one row's residual object costs beyond its field values when the granule's arena is sized: the object's
/// metadata byte, its field count, and the terminal length that closes its offset table. The per-field id and offset
/// entries are not added on top: [`variant_value_bytes`] already charges a flat eight bytes for scalars the encoder
/// stores in two to five, and that slack covers them.
const RESIDUAL_ROW_FRAMING_BYTES: usize = 4;

/// Footer work is much smaller than block encoding, so submitting one job per granule loses to executor and lock
/// overhead on ordinary files. Keep at least this many granules in a metadata batch while still exposing enough
/// batches for large files to occupy the injected executor.
const MIN_METADATA_GRANULES_PER_JOB: usize = 4;

/// File-local identities for top-level payload fields plus the distinct row shapes made from those identities. The
/// first row with a shape interns its names once; consecutive rows with the same shape prove that by comparing against
/// interned names and then carry only a four-byte shape id through statistics and granule construction. Nested names
/// are deliberately absent: no promotion, shredding, or free-text route currently addresses them.
#[derive(Default)]
struct FieldIdentityCatalog {
    ids: HashMap<FieldName, u32>,
    names: Vec<FieldName>,
    /// The shapes of the rows interned most recently, most recent first and at most [`RECENT_SHAPES`] long. A row
    /// whose shape is here proves it by comparing its keys against that shape's names, which is what spares the
    /// steady state a hash lookup per key; see [`Self::intern_object`].
    recent_shapes: Vec<u32>,
    /// The field ids of the shape currently being interned, refilled per row rather than allocated per row. A payload
    /// with an optional field changes shape between neighbouring rows constantly while naming only a handful of
    /// distinct shapes overall, so almost every one of those rows would otherwise allocate a vector, look its shape up
    /// as already known, and drop it again.
    scratch_field_ids: Vec<u32>,
    shape_ids: HashMap<Box<[u32]>, u32>,
    shapes: Vec<Box<[u32]>>,
}

impl FieldIdentityCatalog {
    fn field_id(&self, path: &str) -> Option<u32> {
        self.ids.get(path).copied()
    }

    fn field_names(&self) -> &[FieldName] {
        &self.names
    }

    fn shape(&self, shape_id: u32) -> &[u32] {
        self.shapes.get(shape_id as usize).map(Box::as_ref).unwrap_or_default()
    }

    /// Whether `fields` spell shape `shape_id` name for name. A name shared with the interned one is known equal
    /// from its address alone; any other is compared by text.
    fn same_shape(&self, shape_id: u32, fields: &[(FieldName, VariantValue)]) -> bool {
        let ids = self.shape(shape_id);
        ids.len() == fields.len()
            && ids.iter().zip(fields).all(|(&field_id, (path, _))| {
                self.names
                    .get(field_id as usize)
                    .is_some_and(|known| Arc::ptr_eq(known, path) || **known == **path)
            })
    }

    /// The id of the shape `fields` spells, interning it on first sight.
    ///
    /// The recently seen shapes are tried first, most recent first, and the one that matches moves to the front. Rows
    /// of a stream with an optional field alternate between two shapes, and a few optional fields between a few more,
    /// so remembering only the previous row's shape missed on every alternation and re-hashed every key of the row.
    fn intern_object(&mut self, fields: &[(FieldName, VariantValue)]) -> Result<u32, FormatError> {
        if let Some(position) = self
            .recent_shapes
            .iter()
            .position(|&shape_id| self.same_shape(shape_id, fields))
        {
            let shape_id = self.recent_shapes.remove(position);
            self.recent_shapes.insert(0, shape_id);
            return Ok(shape_id);
        }

        // Borrowed out and handed straight back, so the buffer keeps the capacity the widest row so far needed and a
        // failed intern does not lose it.
        let mut field_ids = std::mem::take(&mut self.scratch_field_ids);
        let interned = self.intern_field_ids(fields, &mut field_ids);
        self.scratch_field_ids = field_ids;
        let shape_id = interned?;
        self.recent_shapes.truncate(RECENT_SHAPES - 1);
        self.recent_shapes.insert(0, shape_id);
        Ok(shape_id)
    }

    /// Resolves every top-level key of `fields` to its file-local id in `field_ids`, then returns the id of the shape
    /// those ids spell — interning it if this is the first row to carry it. Only a shape seen for the first time
    /// comes through here, so this is where a field list's key order is checked: a row that matched a recent shape
    /// spelled, name for name, a list checked before.
    fn intern_field_ids(
        &mut self,
        fields: &[(FieldName, VariantValue)],
        field_ids: &mut Vec<u32>,
    ) -> Result<u32, FormatError> {
        field_ids.clear();
        field_ids.reserve(fields.len());
        let mut previous: Option<&str> = None;
        for (path, _) in fields {
            if previous.is_some_and(|previous| previous >= &**path) {
                return Err(FormatError::Structural {
                    rule: "a payload object's fields are in ascending key order with no key repeated",
                });
            }
            previous = Some(path);
            let field_id = self.intern_name(path)?;
            field_ids.push(field_id);
        }
        self.intern_shape(field_ids)
    }

    /// The id `name` goes by in this catalogue, interning it if this is the first sight of it. Ids are handed out in
    /// first-appearance order, which is what lets a catalogue built in batches be merged back into the order one
    /// sequential pass would have produced.
    fn intern_name(&mut self, name: &FieldName) -> Result<u32, FormatError> {
        if let Some(&field_id) = self.ids.get(&**name) {
            return Ok(field_id);
        }
        let field_id = u32::try_from(self.names.len()).map_err(|_| FormatError::Structural {
            rule: "a HEF file cannot intern more than u32::MAX payload fields",
        })?;
        self.names.push(name.clone());
        self.ids.insert(name.clone(), field_id);
        Ok(field_id)
    }

    /// The id the ordered field set `field_ids` goes by, interning it on first sight. Ids are handed out in
    /// first-appearance order, as [`Self::intern_name`]'s are.
    fn intern_shape(&mut self, field_ids: &[u32]) -> Result<u32, FormatError> {
        if let Some(&shape_id) = self.shape_ids.get(field_ids) {
            return Ok(shape_id);
        }
        let shape_id = u32::try_from(self.shapes.len()).map_err(|_| FormatError::Structural {
            rule: "a HEF file cannot intern more than u32::MAX payload shapes",
        })?;
        // Only a shape seen for the first time takes an allocation of its own; the caller's buffer keeps its capacity.
        let shape: Box<[u32]> = field_ids.into();
        self.shape_ids.insert(shape.clone(), shape_id);
        self.shapes.push(shape);
        Ok(shape_id)
    }
}

/// The three file dictionaries' values indexed by string for a one-hash-lookup [`dictionary_id`], built once per file
/// instead of every row re-running a binary search over each dictionary.
struct DictionaryIndex<'a> {
    entity_type: HashMap<&'a str, u64>,
    event_type: HashMap<&'a str, u64>,
    source: HashMap<&'a str, u64>,
}

fn index_dictionary(values: &[String]) -> HashMap<&str, u64> {
    values
        .iter()
        .enumerate()
        .map(|(id, value)| (value.as_str(), id as u64))
        .collect()
}

/// Rows the file dictionaries sample before choosing how to deduplicate: hashed per worker when every column's
/// sample repeats at least [`DICTIONARY_SAMPLE_MIN_REPEAT`]-fold, sorted whole otherwise.
const DICTIONARY_SAMPLE_ROWS: usize = 1_024;

/// Rows per distinct sampled value below which the columns go straight to the sort, so a column that is distinct
/// row to row never pays the hashing on top of the sort it needs anyway.
const DICTIONARY_SAMPLE_MIN_REPEAT: usize = 2;

/// Adds one row's envelope strings to the entity-type, event-type, and source sets, in that order.
fn observe_dictionary_values<'a>(sets: &mut [HashSet<&'a str>; 3], row: &'a BuildRow) {
    let [entity_type, event_type, source] = sets;
    let envelope = &row.envelope;
    entity_type.insert(&envelope.entity_type);
    event_type.insert(&envelope.event_type);
    source.insert(&envelope.source);
}

/// The file dictionaries: the sorted distinct values of the three envelope string columns. When a sample of the rows
/// already repeats, one pass over the rows deduplicates all three columns through a small hash set per Rayon worker,
/// so only the distinct values reach the sort and a column with a few dozen values never sorts every row. Unstable
/// sort: equal strings are indistinguishable, so the result is identical however the pool split the rows.
fn build_file_dictionaries(rows: &[BuildRow]) -> FileDictionaries {
    let sample_rows = rows.len().min(DICTIONARY_SAMPLE_ROWS);
    let mut sample = <[HashSet<&str>; 3]>::default();
    for row in rows.iter().take(sample_rows) {
        observe_dictionary_values(&mut sample, row);
    }
    let lists: [Vec<&str>; 3] = if sample
        .iter()
        .all(|set| set.len() * DICTIONARY_SAMPLE_MIN_REPEAT <= sample_rows)
    {
        rows.par_iter()
            .fold(<[HashSet<&str>; 3]>::default, |mut sets, row| {
                observe_dictionary_values(&mut sets, row);
                sets
            })
            .reduce(<[HashSet<&str>; 3]>::default, |mut merged, sets| {
                for (merged, set) in merged.iter_mut().zip(sets) {
                    merged.extend(set);
                }
                merged
            })
            .map(|set| set.into_iter().collect())
    } else {
        [
            rows.iter().map(|row| &*row.envelope.entity_type).collect(),
            rows.iter().map(|row| &*row.envelope.event_type).collect(),
            rows.iter().map(|row| &*row.envelope.source).collect(),
        ]
    };
    let [entity_type, event_type, source] = lists.map(|mut list| {
        list.par_sort_unstable();
        list.dedup();
        list.into_iter().map(str::to_owned).collect()
    });
    FileDictionaries {
        entity_type,
        event_type,
        source,
    }
}

fn build_dictionary_index(dictionaries: &FileDictionaries) -> DictionaryIndex<'_> {
    DictionaryIndex {
        entity_type: index_dictionary(&dictionaries.entity_type),
        event_type: index_dictionary(&dictionaries.event_type),
        source: index_dictionary(&dictionaries.source),
    }
}

fn dictionary_id(index: &HashMap<&str, u64>, value: &str) -> u64 {
    index.get(value).copied().unwrap_or(0)
}

/// A column block under construction for one granule.
struct PendingBlock {
    column_id: u32,
    data: ColumnData,
    /// Presence bitmap for shredded/free-text columns (one bit per row); empty for plain columns.
    presence: Vec<u8>,
    /// Shredded scan-path and free-text columns must preserve random access in compressed form.
    random_access: bool,
}

/// Rows per normalization batch. Batches are what makes the pass parallel and what keeps it deterministic: the size
/// is fixed, so the same rows fall in the same batch however many threads run, and merging the batches back in order
/// hands out exactly the field and shape ids one sequential pass would. Two thousand rows is large enough that the
/// per-batch catalogue and merge are noise beside the work, and small enough that a file of a few granules still
/// fills the pool.
const NORMALIZE_BATCH_ROWS: usize = 2_048;

/// One row's payload as the build reads it, after normalization has taken it out of the row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NormalizedPayload {
    /// The row names an immutable external body instead of carrying one. That reference text is the payload, and the
    /// row it arrived in stays the only place it is read from.
    ExternalRef,
    None,
    /// A top-level object: its values sit contiguously from `start` in batch `batch`'s arena, one per field of shape
    /// `shape_id`, in that shape's field order.
    Object {
        batch: u32,
        shape_id: u32,
        start: usize,
    },
    /// A payload that is not an object, and so has no fields to lay out: the whole value sits at `start` in batch
    /// `batch`'s arena.
    Whole {
        batch: u32,
        start: usize,
    },
}

/// Every row's payload laid out the way the build walks it: contiguous arenas of top-level values, plus a small
/// fixed-size entry per row saying where that row's values start and which interned shape names them.
///
/// Payloads arrive as one field list per row, and the build walks them over and over: path statistics, the granule
/// byte cut, granule construction, and finally destruction. Normalizing once at the start turns each of those walks
/// into a scan of one contiguous slice, and turns every key comparison into the field id the row's shape already
/// carries.
///
/// The values are *moved* out of the rows rather than copied, so no payload is ever held twice: what an arena adds is
/// one slot per top-level field, and what it takes away is the tree the row used to keep. After normalization a row's
/// own `payload` is empty and only the entries here describe it — external references excepted, whose text stays put.
///
/// One arena per batch rather than one for the file, so the batches never have to be copied into a single buffer
/// after the parallel pass builds them.
struct NormalizedPayloads {
    identities: FieldIdentityCatalog,
    rows: Vec<NormalizedPayload>,
    values: Vec<Vec<VariantValue>>,
}

/// What one batch of rows normalizes to on its own thread: ids and shapes numbered within the batch, to be renumbered
/// against the file's catalogue when the batches are merged back in order.
struct NormalizedBatch {
    identities: FieldIdentityCatalog,
    rows: Vec<NormalizedPayload>,
    values: Vec<VariantValue>,
}

impl NormalizedPayloads {
    /// Decodes any payload that arrived encoded, gives every top-level field and every distinct row shape its
    /// file-local identity, and moves the payload values out of `rows` into per-batch arenas — for the rows in
    /// `ranges` (ascending, disjoint index ranges into `rows`); a row outside every range keeps its payload where it
    /// is and gets an empty entry here.
    ///
    /// The per-row work — decoding, comparing keys, moving values, and releasing the tree each row arrived in — runs
    /// on the worker pool in [`NORMALIZE_BATCH_ROWS`]-row batches. Only the merge below is sequential, and it touches
    /// one entry per distinct name and shape a batch saw rather than one per row.
    fn normalize(rows: &mut [BuildRow], ranges: &[(usize, usize)]) -> Result<Self, FormatError> {
        let row_count = rows.len();
        // The batches, each tagged with the index of its first row. Taken range by range, so the batch boundaries
        // within a range are the same however many threads run, and the rows outside every range are never read.
        let mut batch_rows: Vec<(usize, &mut [BuildRow])> = Vec::new();
        let mut remaining: &mut [BuildRow] = rows;
        let mut consumed = 0usize;
        for &(start, end) in ranges {
            let rest = std::mem::take(&mut remaining);
            let (_, rest) = rest.split_at_mut(start.saturating_sub(consumed).min(rest.len()));
            let (range_rows, rest) = rest.split_at_mut(end.saturating_sub(start).min(rest.len()));
            remaining = rest;
            consumed = end;
            for (offset, chunk) in range_rows.chunks_mut(NORMALIZE_BATCH_ROWS).enumerate() {
                batch_rows.push((start + offset * NORMALIZE_BATCH_ROWS, chunk));
            }
        }
        let batches: Vec<(usize, Result<NormalizedBatch, FormatError>)> = batch_rows
            .into_par_iter()
            .map(|(first_row, chunk)| (first_row, NormalizedBatch::normalize(chunk)))
            .collect();
        let mut normalized = Self {
            identities: FieldIdentityCatalog::default(),
            rows: Vec::with_capacity(row_count),
            values: Vec::with_capacity(batches.len()),
        };
        // Merged in batch order, and each batch's names and shapes in the order that batch first saw them, so the ids
        // are the ones a single sequential pass would have handed out. Nothing here depends on how the pool scheduled
        // the batches, which is what keeps the file bytes identical run to run.
        let mut batch_field_ids: Vec<u32> = Vec::new();
        let mut shape_field_ids: Vec<u32> = Vec::new();
        for (index, (first_row, batch)) in batches.into_iter().enumerate() {
            let batch = batch?;
            let batch_index = u32::try_from(index).map_err(|_| FormatError::Structural {
                rule: "a HEF file cannot normalize more than u32::MAX row batches",
            })?;
            batch_field_ids.clear();
            for name in batch.identities.field_names() {
                batch_field_ids.push(normalized.identities.intern_name(name)?);
            }
            let mut batch_shape_ids: Vec<u32> = Vec::with_capacity(batch.identities.shapes.len());
            for shape in &batch.identities.shapes {
                shape_field_ids.clear();
                shape_field_ids.extend(
                    shape
                        .iter()
                        .map(|&field_id| batch_field_ids.get(field_id as usize).copied().unwrap_or_default()),
                );
                batch_shape_ids.push(normalized.identities.intern_shape(&shape_field_ids)?);
            }
            normalized.rows.resize(first_row, NormalizedPayload::None);
            normalized.rows.extend(batch.rows.iter().map(|entry| match *entry {
                NormalizedPayload::Object { shape_id, start, .. } => NormalizedPayload::Object {
                    batch: batch_index,
                    shape_id: batch_shape_ids.get(shape_id as usize).copied().unwrap_or_default(),
                    start,
                },
                NormalizedPayload::Whole { start, .. } => NormalizedPayload::Whole {
                    batch: batch_index,
                    start,
                },
                carried => carried,
            }));
            normalized.values.push(batch.values);
        }
        normalized.rows.resize(row_count, NormalizedPayload::None);
        Ok(normalized)
    }

    /// Moves every normalized value back into the row it came from — an object as the same field list, a payload
    /// that arrived encoded as its decoded value, which the build treats alike — so the rows can be normalized again
    /// from scratch by a build that needs all of them.
    fn restore(self, rows: &mut [BuildRow]) {
        let Self {
            identities,
            rows: entries,
            values,
        } = self;
        // A batch's values sit in its arena in row order, so walking the rows in order consumes each arena front to
        // back.
        let mut arenas: Vec<std::vec::IntoIter<VariantValue>> = values.into_iter().map(Vec::into_iter).collect();
        for (row, entry) in rows.iter_mut().zip(entries) {
            let payload = match entry {
                NormalizedPayload::Object { batch, shape_id, .. } => {
                    let Some(arena) = arenas.get_mut(batch as usize) else {
                        continue;
                    };
                    let fields = identities
                        .shape(shape_id)
                        .iter()
                        .map(|&field_id| identities.names.get(field_id as usize).cloned().unwrap_or_default())
                        .zip(arena.by_ref())
                        .collect();
                    BuildPayload::Object(fields)
                }
                NormalizedPayload::Whole { batch, .. } => {
                    let Some(value) = arenas.get_mut(batch as usize).and_then(Iterator::next) else {
                        continue;
                    };
                    BuildPayload::Whole(value)
                }
                NormalizedPayload::ExternalRef | NormalizedPayload::None => continue,
            };
            row.payload = payload;
        }
    }

    fn identities(&self) -> &FieldIdentityCatalog {
        &self.identities
    }

    fn payload(&self, row_index: usize) -> NormalizedPayload {
        self.rows.get(row_index).copied().unwrap_or(NormalizedPayload::None)
    }

    /// The shape id one row's fields are named by, or [`NON_OBJECT_FIELD_SHAPE`] for a payload that is not an object.
    fn shape_id(&self, payload: NormalizedPayload) -> u32 {
        match payload {
            NormalizedPayload::Object { shape_id, .. } => shape_id,
            _ => NON_OBJECT_FIELD_SHAPE,
        }
    }

    /// The interned field ids naming one row's top-level fields, in the order its values are laid out. Empty for a
    /// payload that is not an object.
    fn field_ids(&self, payload: NormalizedPayload) -> &[u32] {
        self.identities.shape(self.shape_id(payload))
    }

    /// The names of one row's top-level fields, in the order its values are laid out, so a caller can pair them with
    /// [`Self::values`] position by position.
    fn field_names(&self, payload: NormalizedPayload) -> impl Iterator<Item = &str> {
        self.field_ids(payload)
            .iter()
            .map(|&field_id| self.field_name(field_id))
    }

    /// The name behind one field id. Every id in a shape was interned from a name, so the empty fallback stands for an
    /// id no catalogue entry answers — which would mean a shape from another file.
    fn field_name(&self, field_id: u32) -> &str {
        self.identities
            .field_names()
            .get(field_id as usize)
            .map_or("", |name| name)
    }

    /// One row's normalized values: one per top-level field for an object, the single whole value for a payload that
    /// is not one, and none at all for an absent payload or an external reference.
    fn values(&self, payload: NormalizedPayload) -> &[VariantValue] {
        let (batch, start, len) = match payload {
            NormalizedPayload::ExternalRef | NormalizedPayload::None => return &[],
            NormalizedPayload::Object { batch, shape_id, start } => {
                (batch, start, self.identities.shape(shape_id).len())
            }
            NormalizedPayload::Whole { batch, start } => (batch, start, 1),
        };
        self.values
            .get(batch as usize)
            .and_then(|arena| arena.get(start..start + len))
            .unwrap_or_default()
    }

    /// The payload entries of the `row_count` rows starting at index `start`: one granule's slice of the rows.
    fn range(&self, start: usize, row_count: usize) -> &[NormalizedPayload] {
        self.rows.get(start..start + row_count).unwrap_or_default()
    }

    /// Appends every key name one row's payload carries — its own field names and every nested one — in the order the
    /// payload's own keys arrived in, which is what the granule byte cut compares row against row.
    fn collect_key_names_into<'a>(&'a self, payload: NormalizedPayload, out: &mut Vec<&'a str>) {
        let mut names = self.field_names(payload);
        for value in self.values(payload) {
            if let Some(name) = names.next() {
                out.push(name);
            }
            collect_key_names(value, out);
        }
    }

    /// Releases every normalized value across the worker pool, one batch arena per job, so a build does not end with
    /// one thread walking a file's worth of payload trees.
    fn release(self) {
        self.values.into_par_iter().for_each(drop);
    }
}

impl NormalizedBatch {
    /// Normalizes one batch of rows against its own catalogue, numbering fields and shapes within the batch.
    fn normalize(rows: &mut [BuildRow]) -> Result<Self, FormatError> {
        // The arena's exact size is one read of each row's field count, which is what saves it doubling its way up
        // through a few copies of itself per batch. An encoded payload's count is known only once decoded; it opens
        // at one.
        let value_count: usize = rows
            .iter()
            .map(|row| match &row.payload {
                BuildPayload::Object(fields) => fields.len(),
                BuildPayload::Encoded(_) | BuildPayload::Whole(_) => 1,
                BuildPayload::ExternalRef(_) | BuildPayload::None => 0,
            })
            .sum();
        let mut batch = Self {
            identities: FieldIdentityCatalog::default(),
            rows: Vec::with_capacity(rows.len()),
            values: Vec::with_capacity(value_count),
        };
        let mut shared = SharedStrings::default();
        for row in rows {
            let payload = match std::mem::replace(&mut row.payload, BuildPayload::None) {
                BuildPayload::Encoded(encoded) => {
                    shared.build_payload(VariantRef::new(&encoded.bytes).decode(&encoded.dictionary)?)
                }
                payload => payload,
            };
            let start = batch.values.len();
            let entry = match payload {
                BuildPayload::Object(fields) => {
                    let shape_id = batch.identities.intern_object(&fields)?;
                    // Consuming the list here is what keeps its buffer off the build's sequential tail: it is
                    // released on this worker, alongside the values it moves out.
                    batch.values.extend(fields.into_iter().map(|(_, value)| value));
                    NormalizedPayload::Object {
                        batch: 0,
                        shape_id,
                        start,
                    }
                }
                BuildPayload::Whole(whole) => {
                    batch.values.push(whole);
                    NormalizedPayload::Whole { batch: 0, start }
                }
                BuildPayload::ExternalRef(reference) => {
                    // The reference text is this row's whole payload and the file writes it verbatim, so it stays
                    // where it is instead of being copied into the arena.
                    row.payload = BuildPayload::ExternalRef(reference);
                    NormalizedPayload::ExternalRef
                }
                // An encoded payload was decoded into one of the value forms above.
                BuildPayload::Encoded(_) | BuildPayload::None => NormalizedPayload::None,
            };
            batch.rows.push(entry);
        }
        Ok(batch)
    }
}

/// What one row leaves behind once its shredded and free-text fields have moved out: either the fields that stayed,
/// as a slice of the granule's shared field list, or a whole payload that was not an object and so had nothing to
/// move out of it. Both borrow the normalized payloads rather than copying them.
enum Residual<'a> {
    Fields(Range<usize>),
    Whole(&'a VariantValue),
}

/// Where one payload path routes, precomputed once per file instead of re-derived by a linear scan for every field
/// of every row: which promoted columns copy it (a path may be promoted more than once, under different
/// `since_schema_version`s), and which single shredded or free-text column moves it, if any.
#[derive(Default)]
struct FieldRoute {
    freetext_index: Option<usize>,
    promoted_indices: Vec<usize>,
    shred_index: Option<usize>,
}

/// Builds the field-id → route table once per file. Every field of every row then routes with one array lookup: path
/// strings are hashed only here, once per configured route, rather than in granule workers.
fn build_field_routes<'a>(
    identities: &FieldIdentityCatalog,
    promotion: &'a PromotionPlan,
    shred_plan: &'a [ShredEntry],
    freetext: &'a [FreetextEntry],
) -> Vec<FieldRoute> {
    let mut routes: Vec<FieldRoute> = (0..identities.names.len()).map(|_| FieldRoute::default()).collect();
    for (index, promoted) in promotion.columns.iter().enumerate() {
        if let Some(route) = identities
            .field_id(promoted.path.as_str())
            .and_then(|field_id| routes.get_mut(field_id as usize))
        {
            route.promoted_indices.push(index);
        }
    }
    for (index, entry) in shred_plan.iter().enumerate() {
        if let Some(route) = identities
            .field_id(entry.path.as_str())
            .and_then(|field_id| routes.get_mut(field_id as usize))
        {
            route.shred_index.get_or_insert(index);
        }
    }
    for (index, entry) in freetext.iter().enumerate() {
        if let Some(route) = identities
            .field_id(entry.declared_field.as_str())
            .and_then(|field_id| routes.get_mut(field_id as usize))
        {
            route.freetext_index.get_or_insert(index);
        }
    }
    routes
}

impl FieldRoute {
    /// Whether this path routes anywhere at all. A path that routes nowhere stays in the residual.
    fn routes_anywhere(&self) -> bool {
        self.freetext_index.is_some() || self.shred_index.is_some() || !self.promoted_indices.is_empty()
    }
}

/// Stands in a shape's route list for a field the file's plans never named, so it stays whole in the residual. It is
/// deliberately past the last field identity, so the row loop's single lookup into the routes answers "nothing to do"
/// for it without a route to inspect.
const FIELD_STAYS_RESIDUAL: u32 = u32::MAX;

/// Where each field of a payload shape routes, in that shape's own field order.
///
/// Rows carrying the same ordered field set already share one shape id, so this is resolved once per file — after the
/// promotion, shred and free-text plans are fixed, before the first granule starts — and then read by every granule
/// worker through a shared reference, with no lock and no per-granule copy. A granule reads a row's routes straight
/// off its shape, so the fields that stay in the residual — usually most of a payload — cost one lookup that misses
/// rather than a route to look at.
///
/// One four-byte entry per field of each distinct shape, which is what the shape catalogue already spends itself, so
/// no schema, however varied, makes this outgrow the catalogue it is built from.
struct ShapeRoutes {
    by_shape: Vec<Box<[u32]>>,
}

impl ShapeRoutes {
    /// One entry per field of `shape_id`, in field order: the field identity to route by, or [`FIELD_STAYS_RESIDUAL`].
    /// Empty for a payload that is not an object.
    fn field_routes(&self, shape_id: u32) -> &[u32] {
        self.by_shape
            .get(shape_id as usize)
            .map(Box::as_ref)
            .unwrap_or_default()
    }
}

/// Resolves every interned shape against the file's field-id routes, once per file.
fn build_shape_routes(identities: &FieldIdentityCatalog, routes: &[FieldRoute]) -> ShapeRoutes {
    ShapeRoutes {
        by_shape: identities
            .shapes
            .iter()
            .map(|shape| {
                shape
                    .iter()
                    .map(|&field_id| {
                        if routes.get(field_id as usize).is_some_and(FieldRoute::routes_anywhere) {
                            field_id
                        } else {
                            FIELD_STAYS_RESIDUAL
                        }
                    })
                    .collect()
            })
            .collect(),
    }
}

struct GranulePieces {
    blocks: Vec<PendingBlock>,
    /// Per-dictionary-id row counts for this granule, dense over `dictionaries.entity_type`/`event_type`/`source`:
    /// the granule build already resolves each row's dictionary id to fill `entity_type_id`/`event_type_id`/
    /// `source_id`, so counting alongside that avoids a second file-wide pass re-deriving the same ids.
    by_entity_type: Vec<u64>,
    by_event_type: Vec<u64>,
    by_source: Vec<u64>,
    dictionary_bytes: Vec<u8>,
    entry: GranuleEntry,
    /// Per indexed free-text column, this granule's per-row byte-offset index: the column id, the raw concatenated row
    /// bytes, and the row-indexed `(offset, len)` offsets table over them (same shape as `offsets_bytes`/`residual_bytes`
    /// below, scoped to one free-text column instead of the whole granule). Empty unless the build opted into the index.
    freetext_row_offsets: Vec<(u32, Vec<u8>, Vec<u8>)>,
    /// Whether this granule alone carries a late event — see `build_granule`'s computation of the same name.
    has_internal_late_event: bool,
    offsets_bytes: Vec<u8>,
    residual_bytes: Vec<u8>,
}

impl GranulePieces {
    /// The stand-in for a granule a sparse update never builds: its entry is the source's, and every block, arena
    /// and filter it would otherwise hold is borrowed from the source where the file is laid out.
    fn borrowed(entry: GranuleEntry) -> Self {
        Self {
            blocks: Vec::new(),
            by_entity_type: Vec::new(),
            by_event_type: Vec::new(),
            by_source: Vec::new(),
            dictionary_bytes: Vec::new(),
            entry,
            freetext_row_offsets: Vec::new(),
            has_internal_late_event: false,
            offsets_bytes: Vec::new(),
            residual_bytes: Vec::new(),
        }
    }
}

/// The payload byte estimate the granule cut accumulates row by row, with the scratch that makes a run of same-shaped
/// rows cost one comparison each instead of a sort and a deduplication per row.
#[derive(Default)]
struct PayloadByteEstimator<'a> {
    key_scratch: Vec<&'a str>,
    previous_distinct_keys: usize,
    previous_key_names: Vec<&'a str>,
}

impl<'a> PayloadByteEstimator<'a> {
    /// Rough byte size of one row's payload as the granule cut counts it: a fixed overhead, its distinct keys and its
    /// value bytes for a value, the reference text for an external reference, nothing for an absent payload.
    fn estimate(&mut self, payloads: &'a NormalizedPayloads, entry: NormalizedPayload, row: &BuildRow) -> usize {
        match entry {
            NormalizedPayload::ExternalRef => match &row.payload {
                BuildPayload::ExternalRef(reference) => reference.len(),
                _ => 0,
            },
            NormalizedPayload::None => 0,
            NormalizedPayload::Object { .. } | NormalizedPayload::Whole { .. } => {
                self.key_scratch.clear();
                payloads.collect_key_names_into(entry, &mut self.key_scratch);
                if self.key_scratch != self.previous_key_names {
                    self.previous_key_names.clear();
                    self.previous_key_names.extend_from_slice(&self.key_scratch);
                    self.key_scratch.sort_unstable();
                    self.key_scratch.dedup();
                    self.previous_distinct_keys = self.key_scratch.len();
                }
                64 + self.previous_distinct_keys * 16
                    + payloads.values(entry).iter().map(variant_value_bytes).sum::<usize>()
            }
        }
    }
}

/// Whether one rebuilt granule of a sparse update cuts where the source cut it: no row before its last reaches the
/// byte target, and the last one does when nothing else — the row target, an epoch change, the end of the file —
/// forced the source's cut there (`byte_cut`). `rows` are the granule's own and `first` is their index into
/// `payloads`.
fn granule_cut_reproduces(
    rows: &[BuildRow],
    payloads: &NormalizedPayloads,
    first: usize,
    targets: &LayoutTargets,
    byte_cut: bool,
) -> bool {
    if targets.index_granularity_bytes == usize::MAX {
        return true;
    }
    let mut estimator = PayloadByteEstimator::default();
    let mut payload_bytes = 0usize;
    for (index, row) in rows.iter().enumerate() {
        payload_bytes =
            payload_bytes.saturating_add(estimator.estimate(payloads, payloads.payload(first + index), row));
        let reached = payload_bytes >= targets.index_granularity_bytes;
        let last = index + 1 == rows.len();
        if reached != last && (reached || byte_cut) {
            return false;
        }
    }
    true
}

/// One promoted, shredded, free-text, or provenance column's values as they accumulate through a granule, held in the
/// shape the encoder finally wants: numbers in a plain vector, text in one shared arena rather than a heap string per
/// row.
///
/// A column takes only the values its own type can carry; everything else is refused and stays in the residual
/// payload. Absent rows are not recorded here — the caller's presence bitmap marks them — so the values are dense.
///
/// See: hef-write-path/spec.md
#[derive(Debug, Clone)]
enum TypedColumn {
    /// Each value with the scale it arrived at; the column's single stored scale is settled when the block is packed.
    Decimal(Vec<(i128, u8)>),
    /// A column whose declared kind no payload value converts into. It takes nothing and packs as an empty string
    /// block — what the writer has always produced for such a column.
    Empty,
    F64(Vec<f64>),
    I64(Vec<i64>),
    Strings(StringColumn),
}

impl TypedColumn {
    /// An empty column for values of `kind`.
    fn for_kind(kind: ColumnKind) -> Self {
        match kind {
            ColumnKind::Decimal => TypedColumn::Decimal(Vec::new()),
            ColumnKind::F64 => TypedColumn::F64(Vec::new()),
            ColumnKind::I64 => TypedColumn::I64(Vec::new()),
            ColumnKind::String => TypedColumn::Strings(StringColumn::new()),
            ColumnKind::U128 | ColumnKind::U64 => TypedColumn::Empty,
        }
    }

    /// Appends `value` when this column can carry it, reporting whether it was taken. Lenient, because a promoted
    /// column is only an acceleration copy: the authoritative value stays in the residual either way, so a timestamp
    /// may land in an integer column and a float in a double column.
    fn push_variant(&mut self, value: &VariantValue) -> bool {
        match (self, value) {
            (TypedColumn::I64(values), VariantValue::Int(v)) => values.push(*v),
            (TypedColumn::I64(values), VariantValue::Timestamp(v)) => values.push(v.physical_nanos()),
            (TypedColumn::F64(values), VariantValue::Double(v)) => values.push(*v),
            (TypedColumn::F64(values), VariantValue::Float(v)) => values.push(f64::from(*v)),
            (TypedColumn::Decimal(values), VariantValue::Decimal { unscaled, scale }) => {
                values.push((*unscaled, *scale))
            }
            (TypedColumn::Strings(values), VariantValue::String(v)) => values.push(Some(v)),
            _ => return false,
        }
        true
    }

    /// Appends `value` to a *shredded* column, admitting it only when the typed column would reproduce the exact same
    /// `VariantValue` on read-back. Shredding moves a value out of the residual, so a lossy conversion would corrupt
    /// the payload: a `Timestamp` reads back as an `Int`, a `Float` as a `Double`, and a `Decimal` at the wrong scale
    /// reads back rescaled. Those are refused and stay in the residual. The first `Decimal` admitted fixes the
    /// column's scale (via `column_scale`); a later decimal at a different scale is refused.
    ///
    /// This is the strict counterpart to [`push_variant`](Self::push_variant).
    fn push_variant_shred(&mut self, value: &VariantValue, column_scale: &mut Option<u8>) -> bool {
        match (self, value) {
            (TypedColumn::I64(values), VariantValue::Int(v)) => values.push(*v),
            (TypedColumn::F64(values), VariantValue::Double(v)) => values.push(*v),
            (TypedColumn::Decimal(values), VariantValue::Decimal { unscaled, scale }) => match column_scale {
                Some(established) if *established != *scale => return false,
                _ => {
                    *column_scale = Some(*scale);
                    values.push((*unscaled, *scale));
                }
            },
            (TypedColumn::Strings(values), VariantValue::String(v)) => values.push(Some(v)),
            _ => return false,
        }
        true
    }

    /// Packs the accumulated values into the block the encoder writes.
    fn into_column_data(self) -> Result<ColumnData, FormatError> {
        match self {
            TypedColumn::Decimal(values) => {
                // A decimal column stores every value at one fixed scale, but individual values may arrive at
                // different scales. Pick the largest scale present and rescale each value up to it, so `1.5` (scale 1)
                // and `1.50` (scale 2) both round-trip correctly instead of the second being read back with the
                // first's scale. The rescale is checked: a value whose magnitude cannot survive the lift is a
                // structural error, not a silent clamp to `i128::MAX/MIN` that would corrupt the stored value.
                let scale = values.iter().map(|(_, scale)| *scale).max().unwrap_or(0);
                let rescaled = values
                    .iter()
                    .map(|(unscaled, value_scale)| {
                        let lift = u32::from(scale.saturating_sub(*value_scale));
                        10i128
                            .checked_pow(lift)
                            .and_then(|factor| unscaled.checked_mul(factor))
                            .ok_or(FormatError::Structural {
                                rule: "decimal value overflows i128 when rescaled to the column scale",
                            })
                    })
                    .collect::<Result<Vec<i128>, FormatError>>()?;
                Ok(ColumnData::Decimal {
                    values: rescaled,
                    scale,
                })
            }
            TypedColumn::Empty => Ok(ColumnData::Strings(StringColumn::new())),
            TypedColumn::F64(values) => Ok(ColumnData::F64(values)),
            TypedColumn::I64(values) => Ok(ColumnData::I64(values)),
            TypedColumn::Strings(values) => Ok(ColumnData::Strings(values)),
        }
    }

    /// Packs a *shredded* column. A shredded decimal column stores every value at one fixed scale: the strict shred
    /// admission only takes decimals already at the column's established scale, so every value here agrees and each is
    /// stored as-is with no rescaling — which is what lets a shredded decimal round-trip exactly. If a scale mismatch
    /// is ever seen at packing time (a value bypassed that guard), this reports a structural error rather than
    /// silently rescaling and clamping. Non-decimal columns pack exactly as [`into_column_data`](Self::into_column_data).
    fn into_shredded_column_data(self) -> Result<ColumnData, FormatError> {
        let TypedColumn::Decimal(values) = self else {
            return self.into_column_data();
        };
        let mut scale: Option<u8> = None;
        let unscaled = values
            .iter()
            .map(|(unscaled, value_scale)| match scale {
                Some(established) if established != *value_scale => Err(FormatError::Structural {
                    rule: "a shredded decimal column stores every value at one scale",
                }),
                _ => {
                    scale = Some(*value_scale);
                    Ok(*unscaled)
                }
            })
            .collect::<Result<Vec<i128>, FormatError>>()?;
        Ok(ColumnData::Decimal {
            scale: scale.unwrap_or(0),
            values: unscaled,
        })
    }
}

/// Extracts `len` bits starting at bit `start` from a packed presence bitmap and repacks them into a fresh byte Vec
/// (bit 0 of byte 0 = first bit).
fn slice_presence_bits(full: &[u8], start: usize, len: usize) -> Vec<u8> {
    let byte_count = len.div_ceil(8);
    let mut result = vec![0u8; byte_count];
    for i in 0..len {
        let src_idx = start + i;
        if full.get(src_idx / 8).is_some_and(|b| b & (1 << (src_idx % 8)) != 0)
            && let Some(byte) = result.get_mut(i / 8)
        {
            *byte |= 1 << (i % 8);
        }
    }
    result
}

/// Aggregates per-page `BlockStats` into one granule-level summary. Takes an iterator rather than a slice so the
/// caller does not have to collect its per-page stats into a throwaway `Vec` first.
fn aggregate_stats(stats: impl IntoIterator<Item = BlockStats>) -> BlockStats {
    let mut null_count = 0u32;
    let mut row_count = 0u32;
    let mut min_i128: Option<i128> = None;
    let mut max_i128: Option<i128> = None;
    let mut min_f64: Option<f64> = None;
    let mut max_f64: Option<f64> = None;
    for s in stats {
        null_count += s.null_count;
        row_count += s.row_count;
        min_i128 = match (min_i128, s.min_i128) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (None, v) | (v, None) => v,
        };
        max_i128 = match (max_i128, s.max_i128) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, v) | (v, None) => v,
        };
        min_f64 = match (min_f64, s.min_f64) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (None, v) | (v, None) => v,
        };
        max_f64 = match (max_f64, s.max_f64) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (None, v) | (v, None) => v,
        };
    }
    BlockStats {
        max_f64,
        max_i128,
        min_f64,
        min_i128,
        null_count,
        row_count,
    }
}

/// Seals the plaintext `footer_blob` under the caller-supplied file DEK with the pinned AEAD (AES-256-GCM), binding the
/// file identity as associated data. Returns the nonce followed by the ciphertext — the bytes written where the
/// plaintext footer blob would otherwise go, and the form the reader parses the nonce back out of.
///
/// The nonce is derived from the footer's own content rather than drawn at random, because the whole builder is
/// deterministic: publication retries and crash recovery rebuild the same range and compare the rebuilt BLAKE3 against
/// the manifest entry, so a random nonce would make an identical range hash differently on every attempt and strand the
/// retry as "different content".
///
/// Derivation is a BLAKE3 keyed hash under the file DEK over `file_id` and the plaintext footer, truncated to the AEAD
/// nonce length. The DEK arrives in a cloneable configuration the caller owns, so the writer cannot assume a key seals
/// only one file; keying the derivation by content means two different footers under the same key get different nonces.
/// The one case that repeats a `(key, nonce)` pair is the same footer for the same file — which produces the identical
/// ciphertext the retry path is asking for, and reveals nothing new.
fn seal_footer(dek: &[u8; 32], file_id: u128, footer_blob: &[u8]) -> Result<Vec<u8>, FormatError> {
    let scheme = AeadScheme::AesGcm256;
    let mut hasher = blake3::Hasher::new_keyed(dek);
    hasher.update(b"hef-footer-nonce-v1");
    hasher.update(&file_id.to_le_bytes());
    hasher.update_rayon(footer_blob);
    let nonce = hasher.finalize().as_bytes()[..scheme.nonce_len()].to_vec();
    let ciphertext = scheme
        .seal(dek, &nonce, &file_id.to_le_bytes(), footer_blob)
        .ok_or(FormatError::Structural {
            rule: "footer seal failed",
        })?;
    let mut sealed = nonce;
    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// Builds one identity-hash membership filter per granule, so a reader looking for a single entity id fetches only
/// the granules whose filter admits its hash. Returns the encoded filter bytes keyed by granule, for the stripe loop
/// to place in the data area.
///
/// Each filter covers the *distinct* `entity_id_hash_low` values of one granule, which is what the filter is asked
/// about and what sizes it: a stream where one entity emits thousands of events pays for the entities, not the rows.
/// That, with [`ENTITY_HASH_FILTER_BITS_PER_KEY`], is the whole metadata budget — about 1.25 bytes per distinct
/// entity, against the tens of bytes per row a granule's own envelope columns cost, so the filters stay a small
/// single-digit percentage of any file and need no size-dependent cut-off. A granule's filter is derived from that
/// granule's rows alone, so an unchanged stripe stays byte-identical across a rewrite and is still reusable.
///
/// The filters are acceleration and nothing more — they never rule out a granule that holds the id, and a caller must
/// still confirm a candidate against the real entity id, since two ids can share a hash. A granule carrying no
/// identity-hash column gets none, and a granule with no filter is simply always a candidate.
fn build_entity_hash_filters(granules: &[GranulePieces]) -> BTreeMap<u32, Vec<u8>> {
    let mut distinct: HashSet<u64> = HashSet::default();
    let mut filters = BTreeMap::new();
    for granule in granules {
        let Some(hashes) = granule.blocks.iter().find_map(|block| {
            (block.column_id == column_ids::ENTITY_ID_HASH_LOW)
                .then(|| match &block.data {
                    ColumnData::U64(values) => Some(values.as_slice()),
                    _ => None,
                })
                .flatten()
        }) else {
            continue;
        };
        distinct.clear();
        distinct.extend(hashes.iter().copied());
        let filter = SplitBlockBloomFilter::build_from_iter(
            distinct.iter().copied(),
            distinct.len(),
            ENTITY_HASH_FILTER_BITS_PER_KEY,
        )
        .encode();
        filters.insert(granule.entry.granule_id, filter);
    }
    filters
}

/// The footer metadata one worker derives for a contiguous granule batch. Keeping all of these small products in one
/// job amortizes executor scheduling and walks each granule once; batches are merged in granule order afterwards, so
/// no scheduling decision can affect encoded bytes.
struct FooterMetadataBatch {
    by_entity_type: Vec<u64>,
    by_event_type: Vec<u64>,
    by_source: Vec<u64>,
    clustering: Vec<ClusteringEntry>,
    entity_hash_filters: BTreeMap<u32, Vec<u8>>,
    ndv_by_column_stripe: BTreeMap<(u32, u32), NdvSketch>,
}

/// Metadata products whose inputs are final as soon as block encoding finishes. They are deliberately built before
/// layout starts: the encoder has released its peak scratch memory, while the pending column values needed by NDV and
/// point-filter construction are still available.
struct ScheduledFooterMetadata {
    clustering: Vec<ClusteringEntry>,
    entity_hash_filters: BTreeMap<u32, Vec<u8>>,
    exact_counts: ExactCounts,
    stripe_ndv: Vec<StripeNdvEntry>,
}

fn metadata_job_count(granule_count: usize, parallelism: usize) -> usize {
    if granule_count == 0 {
        return 0;
    }
    let useful_batches = granule_count.div_ceil(MIN_METADATA_GRANULES_PER_JOB);
    useful_batches.min(parallelism.max(1)).max(1)
}

fn batch_range(item_count: usize, job_count: usize, job_index: usize) -> Range<usize> {
    let start = item_count.saturating_mul(job_index) / job_count.max(1);
    let end = item_count.saturating_mul(job_index.saturating_add(1)) / job_count.max(1);
    start..end
}

fn build_footer_metadata_batch(
    granules: &[GranulePieces],
    entity_type_count: usize,
    event_type_count: usize,
    source_count: usize,
    hot_columns: &Range<u32>,
) -> FooterMetadataBatch {
    let mut batch = FooterMetadataBatch {
        by_entity_type: vec![0; entity_type_count],
        by_event_type: vec![0; event_type_count],
        by_source: vec![0; source_count],
        clustering: Vec::with_capacity(granules.len()),
        entity_hash_filters: build_entity_hash_filters(granules),
        ndv_by_column_stripe: BTreeMap::new(),
    };
    for granule in granules {
        for (target, source) in [
            (&mut batch.by_entity_type, &granule.by_entity_type),
            (&mut batch.by_event_type, &granule.by_event_type),
            (&mut batch.by_source, &granule.by_source),
        ] {
            for (total, count) in target.iter_mut().zip(source) {
                *total += count;
            }
        }
        batch.clustering.push(ClusteringEntry {
            clustering_quality: 1.0,
            granule_id: granule.entry.granule_id,
            projection_id: 0,
            sortedness_proof: Some(SortednessProof {
                column_names: vec!["epoch".to_owned(), "sequence".to_owned()],
                direction: SortDirection::Ascending,
            }),
        });
        for block in &granule.blocks {
            if hot_columns.contains(&block.column_id) {
                batch
                    .ndv_by_column_stripe
                    .entry((block.column_id, granule.entry.stripe_id))
                    .or_default()
                    .observe_column(&block.data);
            }
        }
    }
    batch
}

fn finish_footer_metadata(batches: Vec<FooterMetadataBatch>, row_count: u64) -> ScheduledFooterMetadata {
    let mut batches = batches.into_iter();
    let mut merged = batches.next().unwrap_or_else(|| FooterMetadataBatch {
        by_entity_type: Vec::new(),
        by_event_type: Vec::new(),
        by_source: Vec::new(),
        clustering: Vec::new(),
        entity_hash_filters: BTreeMap::new(),
        ndv_by_column_stripe: BTreeMap::new(),
    });
    for batch in batches {
        for (target, source) in [
            (&mut merged.by_entity_type, batch.by_entity_type),
            (&mut merged.by_event_type, batch.by_event_type),
            (&mut merged.by_source, batch.by_source),
        ] {
            for (total, count) in target.iter_mut().zip(source) {
                *total += count;
            }
        }
        merged.clustering.extend(batch.clustering);
        merged.entity_hash_filters.extend(batch.entity_hash_filters);
        for (key, sketch) in batch.ndv_by_column_stripe {
            merged.ndv_by_column_stripe.entry(key).or_default().merge(&sketch);
        }
    }
    let indexed_counts = |counts: Vec<u64>| {
        counts
            .into_iter()
            .enumerate()
            .map(|(index, count)| (index as u32, count))
            .collect()
    };
    let stripe_ndv = merged
        .ndv_by_column_stripe
        .into_iter()
        .map(|((column_id, stripe_id), sketch)| {
            let (distinct_count, exact) = sketch.distinct();
            StripeNdvEntry {
                column_id,
                distinct_count,
                exact,
                stripe_id,
            }
        })
        .collect();
    ScheduledFooterMetadata {
        clustering: merged.clustering,
        entity_hash_filters: merged.entity_hash_filters,
        exact_counts: ExactCounts {
            row_count,
            by_source: indexed_counts(merged.by_source),
            by_event_type: indexed_counts(merged.by_event_type),
            by_entity_type: indexed_counts(merged.by_entity_type),
        },
        stripe_ndv,
    }
}

/// The footer products a sparse update takes from the source instead of deriving from every granule's values: the
/// exact counts and stripe distinct-count estimates are unchanged because no envelope, dictionary or promoted column
/// changed, the clustering entries are per-granule constants, and each granule's identity-hash filter is a pure
/// function of hashes no change touched.
fn borrow_footer_metadata(source: &BuiltHef) -> Result<ScheduledFooterMetadata, FormatError> {
    let mut entity_hash_filters = BTreeMap::new();
    for entry in &source.footer.entity_hash_filters {
        let filter = source_extent(source, entry.granule_id, entry.index_offset, entry.index_len).ok_or(
            FormatError::Structural {
                rule: "a source identity-hash filter must lie inside its stripe",
            },
        )?;
        entity_hash_filters.insert(entry.granule_id, filter.to_vec());
    }
    Ok(ScheduledFooterMetadata {
        clustering: source.footer.clustering.clone(),
        entity_hash_filters,
        exact_counts: source.footer.exact_counts.clone(),
        stripe_ndv: source.footer.stripe_ndv.clone(),
    })
}

fn build_scheduled_footer_metadata(
    granules: &[GranulePieces],
    dictionaries: &FileDictionaries,
    hot_columns: &Range<u32>,
    row_count: u64,
    encode: &dyn EncodeExecutor,
    profile_workers: bool,
) -> ScheduledFooterMetadata {
    let job_count = metadata_job_count(granules.len(), encode.parallelism());
    let slots: Vec<Mutex<Option<FooterMetadataBatch>>> = (0..job_count).map(|_| Mutex::new(None)).collect();
    encode.run_jobs(job_count, &|job_index| {
        let work = || {
            let range = batch_range(granules.len(), job_count, job_index);
            build_footer_metadata_batch(
                granules.get(range).unwrap_or_default(),
                dictionaries.entity_type.len(),
                dictionaries.event_type.len(),
                dictionaries.source.len(),
                hot_columns,
            )
        };
        let batch = if profile_workers {
            profile_footer_metadata(work)
        } else {
            work()
        };
        if let Some(slot) = slots.get(job_index) {
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(batch);
        }
    });
    let batches = slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("the executor runs every metadata batch")
        })
        .collect();
    finish_footer_metadata(batches, row_count)
}

/// Reports whether the file carries at least one late event: a row whose `occurred_at` is older than the `occurred_at`
/// of a row ingested before it. Folds over the per-granule bounds `build_granule` already computed — each granule's
/// own internal inversion (see its `has_internal_late_event`), plus an inversion across a granule boundary, caught by
/// comparing a granule's minimum against the running maximum of every earlier granule. That comparison is safe
/// because a granule with no internal late event is non-decreasing in ingest order, so its first (and therefore
/// minimum) `occurred_at` is exactly what a boundary inversion would land below. Equivalent to a single forward scan
/// over every row's `occurred_at` tracking the running maximum, but paid for once per granule instead of once per row.
fn granules_have_late_events(granules: &[GranulePieces]) -> bool {
    let mut max_occurred_so_far = i64::MIN;
    for granule in granules {
        if granule.has_internal_late_event || granule.entry.min_occurred_at_physical < max_occurred_so_far {
            return true;
        }
        max_occurred_so_far = max_occurred_so_far.max(granule.entry.max_occurred_at_physical);
    }
    false
}

/// Compresses a cold (rewritten/compacted) granule's residual arena with Zstd-3 when that saves bytes, into
/// independently decompressible frames with a seek table (see [`seekable_zstd`]) so a point read into the granule
/// inflates one frame instead of the whole arena. An arena the compression does not shrink (or an empty one) stays
/// uncompressed, and the granule keeps offset-jump point access with no inflation step.
fn compress_residual(residual: Vec<u8>) -> (Vec<u8>, ResidualCompression) {
    if residual.is_empty() {
        return (residual, ResidualCompression::None);
    }
    let Some(compressed) = seekable_zstd::compress(&residual) else {
        return (residual, ResidualCompression::None);
    };
    if compressed.len() + 4 >= residual.len() {
        return (residual, ResidualCompression::None);
    }
    (compressed, ResidualCompression::ZstdSeekable)
}

/// Finds the source granule containing `ordinal` without touching any column bytes.
fn granule_for_ordinal(footer: &Footer, ordinal: u64) -> Option<u32> {
    let after = footer
        .granules
        .partition_point(|granule| granule.first_row_ordinal <= ordinal);
    let granule = footer.granules.get(after.checked_sub(1)?)?;
    let end = granule.first_row_ordinal.checked_add(u64::from(granule.row_count))?;
    (ordinal < end).then_some(granule.granule_id)
}

fn mark_all_columns(source: &BuiltHef, changed: &mut HashSet<u32>) {
    changed.extend(source.footer.columns.iter().map(|column| column.column_id));
}

/// Marks the physical columns and payload arena affected by one payload edit. Top-level object paths are sufficient:
/// promotion, shredding, and free-text routing are all defined on those same paths. A non-object transition is
/// conservatively treated as changing every payload-derived block and the residual arena.
fn mark_payload_change(
    source: &BuiltHef,
    config: &HefBuildConfig,
    before: &BuildPayload,
    after: &BuildPayload,
    changed: &mut HashSet<u32>,
    path_statistics_changed: &mut bool,
) -> bool {
    if before == after {
        return false;
    }
    let (BuildPayload::Object(before), BuildPayload::Object(after)) = (before, after) else {
        *path_statistics_changed = true;
        changed.insert(column_ids::PAYLOAD_FLAGS);
        changed.extend(
            source
                .footer
                .columns
                .iter()
                .filter(|column| column.column_id >= column_ids::PROMOTED_BASE)
                .map(|column| column.column_id),
        );
        return true;
    };

    let mut paths: HashSet<&str> = before.iter().map(|(name, _)| &**name).collect();
    paths.extend(after.iter().map(|(name, _)| &**name));
    let mut residual_changed = false;
    for path in paths {
        let before_value = field_value(before, path);
        let after_value = field_value(after, path);
        if before_value == after_value {
            continue;
        }
        *path_statistics_changed |= before_value.is_none()
            || after_value.is_none()
            || before_value.zip(after_value).is_some_and(|(before, after)| {
                std::mem::discriminant(before) != std::mem::discriminant(after)
                    || matches!(before, VariantValue::Array(_) | VariantValue::Object(_))
            });
        for (index, promoted) in config.promotion.columns.iter().enumerate() {
            if promoted.path == path {
                changed.insert(column_ids::PROMOTED_BASE + index as u32);
            }
        }
        let shredded = source.footer.shredded.iter().find(|entry| entry.path == path);
        let freetext = source.footer.freetext.iter().find(|entry| entry.declared_field == path);
        if let Some(entry) = shredded {
            changed.insert(entry.column_id);
        }
        if let Some(entry) = freetext {
            changed.insert(entry.column_id);
        }
        // Shredded and free-text values move out of the residual. Promotion alone is only a copy, so its source value
        // remains in the residual and changes that arena too.
        residual_changed |= shredded.is_none() && freetext.is_none();
    }
    // Analytical columns are supplied as whole-file arrays. Without the prior arrays there is no proof that a payload
    // edit left their derived values alone, so conservatively rebuild their affected-granule blocks.
    if !config.analytical_columns.is_empty() {
        changed.extend(config.analytical_columns.iter().map(|column| column.column_id));
    }
    residual_changed
}

/// The value under `path` in a sorted field list.
fn field_value<'a>(fields: &'a [(FieldName, VariantValue)], path: &str) -> Option<&'a VariantValue> {
    fields
        .binary_search_by(|(name, _)| (**name).cmp(path))
        .ok()
        .and_then(|index| fields.get(index))
        .map(|(_, value)| value)
}

fn plan_update_reuse_from<'a, 'b>(
    source: &'a BuiltHef,
    before_len: usize,
    after_len: usize,
    changes: impl Iterator<Item = (u64, &'b BuildRow, &'b BuildRow)>,
    config: &HefBuildConfig,
) -> Option<UpdateReuse<'a>> {
    if config.footer_encryption != FooterEncryption::Plaintext
        || source.header.feature_flags & required_features::FOOTER_ENCRYPTED != 0
        || source.header.tenant_id != config.tenant_id
        || source.header.row_count != before_len as u64
        || before_len != after_len
        || source.footer.format_version != (1, 0)
    {
        return None;
    }

    let mut changed_columns_by_granule: HashMap<u32, HashSet<u32>> = HashMap::default();
    let mut changed_payload_granules: HashSet<u32> = HashSet::default();
    let mut path_statistics_changed = false;
    let mut payload_only = true;
    for (ordinal, before, after) in changes {
        if before == after {
            continue;
        }
        let granule_id = granule_for_ordinal(&source.footer, ordinal)?;
        let changed = changed_columns_by_granule.entry(granule_id).or_default();

        // The common amendment case changes payload fields and their derived dedupe hash. Derive those columns
        // exactly. Any other envelope/identity/provenance change takes the conservative all-blocks path for this
        // granule; unchanged granules can still be reused.
        let mut normalized_envelope = before.envelope.clone();
        normalized_envelope.dedupe_hash_low = after.envelope.dedupe_hash_low;
        normalized_envelope.dedupe_hash_high = after.envelope.dedupe_hash_high;
        let only_payload_and_dedupe = before.epoch == after.epoch
            && before.sequence == after.sequence
            && normalized_envelope == after.envelope
            && before.provenance == after.provenance
            && before.relationships == after.relationships;
        if !only_payload_and_dedupe {
            mark_all_columns(source, changed);
            changed_payload_granules.insert(granule_id);
            payload_only = false;
            continue;
        }
        if before.envelope.dedupe_hash_low != after.envelope.dedupe_hash_low {
            changed.insert(column_ids::DEDUPE_HASH_LOW);
        }
        if before.envelope.dedupe_hash_high != after.envelope.dedupe_hash_high {
            changed.insert(column_ids::DEDUPE_HASH_HIGH);
        }
        if mark_payload_change(
            source,
            config,
            &before.payload,
            &after.payload,
            changed,
            &mut path_statistics_changed,
        ) {
            changed_payload_granules.insert(granule_id);
        }
    }
    Some(UpdateReuse {
        changed_columns_by_granule,
        changed_payload_granules,
        path_statistics_changed,
        payload_only,
        source,
        sparse: None,
    })
}

/// Adds the sparse build plan to a reuse plan made from the complete replacement rows, where the change set allows
/// one.
fn with_sparse_plan<'a>(mut reuse: UpdateReuse<'a>, after: &[BuildRow], config: &HefBuildConfig) -> UpdateReuse<'a> {
    let carries_provenance = after.iter().any(|row| row.provenance.is_some());
    let carries_relationships = after.iter().any(|row| row.relationships.is_some());
    reuse.sparse = plan_sparse_update(&reuse, config, carries_provenance, carries_relationships, true);
    reuse
}

/// The reuse plan a sparse change set yields, or `None` when the set has to be built from the complete rows.
fn plan_change_set_reuse<'a>(
    source: &'a BuiltHef,
    after: &[BuildRow],
    changes: &[HefRowChange<BuildRow>],
    config: &HefBuildConfig,
) -> Option<UpdateReuse<'a>> {
    // A dense change set pays to retain nearly a second copy of the input rows and leaves too few blocks reusable.
    // At one quarter of the file, take the established full rewrite; this also bounds sparse-plan memory.
    let sparse_enough = changes.len().saturating_mul(4) < after.len();
    let changes_match = sparse_enough
        && changes
            .iter()
            .all(|change| after.get(change.row_ordinal as usize) == Some(&change.after));
    changes_match
        .then(|| {
            plan_update_reuse_from(
                source,
                source.header.row_count as usize,
                after.len(),
                changes
                    .iter()
                    .map(|change| (change.row_ordinal, &change.before, &change.after)),
                config,
            )
        })
        .flatten()
        .map(|reuse| with_sparse_plan(reuse, after, config))
}

/// The rows a changes-only update builds from, and its plan: for every granule the plan builds, the granule's rows
/// read back from the source with the changed ones replaced by their `after` versions, in granule order. Only what
/// the sparse build reads is reconstructed — sequence, dedupe hashes, schema version and payload; every other
/// envelope field is left empty, because every block that stores one is borrowed from the source unchanged.
fn plan_source_update<'a>(
    source: &'a BuiltHef,
    changes: &[HefRowChange<BuildRow>],
    config: &HefBuildConfig,
) -> Result<(Vec<BuildRow>, UpdateReuse<'a>), FormatError> {
    let row_count = source.header.row_count as usize;
    let has_column = |column_id: u32| source.footer.columns.iter().any(|column| column.column_id == column_id);
    let plan = plan_update_reuse_from(
        source,
        row_count,
        row_count,
        changes
            .iter()
            .map(|change| (change.row_ordinal, &change.before, &change.after)),
        config,
    )
    .and_then(|reuse| {
        let sparse = plan_sparse_update(
            &reuse,
            config,
            has_column(column_ids::AUTHOR_PUBKEY),
            has_column(column_ids::PARENT_REF),
            false,
        )?;
        Some(UpdateReuse {
            sparse: Some(sparse),
            ..reuse
        })
    });
    let Some(reuse) = plan else {
        return Err(FormatError::Structural {
            rule: "the change set cannot be applied from the source alone; update from the full replacement rows",
        });
    };
    let Some(plan) = reuse.sparse.as_ref() else {
        return Err(FormatError::Structural {
            rule: "a changes-only update carries its sparse plan",
        });
    };

    let file = HefFile::open(source.bytes.clone(), Some(&source.file_seal))?;
    let u64_column = |column_id: u32, granule_id: u32| -> Result<Vec<u64>, FormatError> {
        match file.read_column(column_id, granule_id)?.data {
            ColumnData::U64(values) => Ok(values),
            _ => Err(FormatError::Structural {
                rule: "a source envelope column decodes as u64 values",
            }),
        }
    };
    let mut rows: Vec<BuildRow> = Vec::new();
    let mut built: Vec<(GranuleEntry, usize)> = Vec::new();
    let mut shared = SharedStrings::default();
    for (granule, build) in source.footer.granules.iter().zip(&plan.build) {
        if build.is_none() {
            continue;
        }
        built.push((*granule, rows.len()));
        // Row by row through the reader's own reconstruction, which decodes each of the granule's blocks once into
        // its cache; the batched read extracts values one row at a time out of the encoded block instead, which is
        // right for a few rows and quadratic over a whole granule.
        let payloads = (0..u64::from(granule.row_count))
            .map(|row| file.payload(granule.first_row_ordinal + row))
            .collect::<Result<Vec<PayloadRead>, FormatError>>()?;
        let sequence = u64_column(column_ids::SEQUENCE, granule.granule_id)?;
        let dedupe_low = u64_column(column_ids::DEDUPE_HASH_LOW, granule.granule_id)?;
        let dedupe_high = u64_column(column_ids::DEDUPE_HASH_HIGH, granule.granule_id)?;
        let schema_version = u64_column(column_ids::SCHEMA_VERSION, granule.granule_id)?;
        if [&sequence, &dedupe_low, &dedupe_high, &schema_version]
            .iter()
            .any(|values| values.len() != payloads.len())
        {
            return Err(FormatError::Structural {
                rule: "a source granule's columns hold one value per row",
            });
        }
        for (((payload, &sequence), (&dedupe_hash_low, &dedupe_hash_high)), &schema_version) in payloads
            .into_iter()
            .zip(&sequence)
            .zip(dedupe_low.iter().zip(&dedupe_high))
            .zip(&schema_version)
        {
            let schema_version = u32::try_from(schema_version).map_err(|_| FormatError::Structural {
                rule: "a stored schema version fits in u32",
            })?;
            rows.push(BuildRow {
                envelope: BuildEnvelope {
                    account_id: None,
                    account_id_hash_low: 0,
                    actor_id: None,
                    actor_id_hash_low: 0,
                    dedupe_hash_high,
                    dedupe_hash_low,
                    entity_id: None,
                    entity_id_hash_high: 0,
                    entity_id_hash_low: 0,
                    entity_type: shared.share(""),
                    event_id: EventId::from_uuid(uuid::Uuid::nil()),
                    event_type: shared.share(""),
                    flags: EventFlags(0),
                    ingested_at: TimestampValue::default(),
                    occurred_at: TimestampValue::default(),
                    schema_version,
                    source: shared.share(""),
                    stream_id: StreamId(0),
                    stream_sequence: 0,
                    tenant_id: config.tenant_id,
                    trace_id_hash_low: 0,
                },
                epoch: granule.first_epoch,
                payload: match payload {
                    PayloadRead::External(reference) => BuildPayload::ExternalRef(reference),
                    PayloadRead::None => BuildPayload::None,
                    PayloadRead::Value(value) => shared.build_payload(value),
                },
                provenance: None,
                relationships: None,
                sequence,
            });
        }
    }
    for change in changes {
        let local = built.iter().find_map(|(granule, local_start)| {
            let offset = change.row_ordinal.checked_sub(granule.first_row_ordinal)?;
            (offset < u64::from(granule.row_count)).then_some(local_start + offset as usize)
        });
        match local.and_then(|local| rows.get_mut(local)) {
            Some(row) => *row = change.after.clone(),
            None => {
                return Err(FormatError::Structural {
                    rule: "every change lands in a granule the sparse plan builds",
                });
            }
        }
    }
    Ok((rows, reuse))
}

fn plan_update_reuse<'a>(
    source: &'a BuiltHef,
    before: &[BuildRow],
    after: &[BuildRow],
    config: &HefBuildConfig,
) -> Option<UpdateReuse<'a>> {
    plan_update_reuse_from(
        source,
        before.len(),
        after.len(),
        before
            .iter()
            .zip(after)
            .enumerate()
            .map(|(ordinal, (before, after))| (ordinal as u64, before, after)),
        config,
    )
}

/// Builds one immutable HEF file from sequence-ordered rows, encoding its column blocks sequentially.
///
/// Takes the rows by value so the builder can move each row's identity strings straight into their column blocks
/// instead of cloning them per row — the envelope-column clone cost was measurable on large publications. The rows
/// may be [`HefRow`]s, converted on the way in, or [`BuildRow`]s, taken as they are. Callers with a thread pool to
/// spend hand it to [`build_hef_file_with_executor`] instead; the bytes are identical.
pub fn build_hef_file<R: BuildInput>(rows: Vec<R>, config: &HefBuildConfig) -> Result<BuiltHef, FormatError> {
    build_hef_file_with_executor(rows, config, &SerialEncodeExecutor)
}

/// Builds one immutable HEF file from sequence-ordered rows, fanning the block-encode work out through `encode`.
///
/// The executor is the injectable scheduling interface deterministic simulation requires: production passes a thread
/// pool, simulation passes the sequential executor, and the built bytes are byte-identical either way because the
/// encoded pieces are reassembled in a fixed order.
pub fn build_hef_file_with_executor<R: BuildInput>(
    rows: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<BuiltHef, FormatError> {
    build_hef_file_sealed(R::into_build_rows(rows), config, encode, None, None, None)
}

/// Rebuilds an immutable file while reusing source column encodings that the complete before/after row comparison
/// proves unchanged. Row ordinals are mapped to source granules before any column data is decoded. Incompatible
/// schema, layout, dictionary, encryption, or row geometry automatically takes the ordinary full-rewrite path.
pub fn update_hef_file_with_executor<R: BuildInput>(
    source: &BuiltHef,
    before: &[R],
    after: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<BuiltHef, FormatError> {
    let before = R::into_build_rows(before.to_vec());
    let after = R::into_build_rows(after);
    let reuse = plan_update_reuse(source, &before, &after, config).map(|reuse| with_sparse_plan(reuse, &after, config));
    build_hef_file_sealed(after, config, encode, None, None, reuse.as_ref())
}

/// Update entry point for a transaction that already has an authoritative sparse change set. Only changed rows are
/// duplicated for before/after comparison; the complete replacement rows are consumed once by the builder. A change
/// whose `after` value does not match the replacement row disables reuse and falls back to a full rewrite.
pub fn update_hef_file_from_changes_with_executor<R: BuildInput>(
    source: &BuiltHef,
    after: Vec<R>,
    changes: &[HefRowChange<R>],
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<BuiltHef, FormatError> {
    let after = R::into_build_rows(after);
    let reuse = plan_change_set_reuse(source, &after, &R::build_changes(changes), config);
    build_hef_file_sealed(after, config, encode, None, None, reuse.as_ref())
}

/// Updates a file from its authoritative change set alone, without the caller materializing every replacement row:
/// the rows of the granules the changes touch are read back from the source, and only those granules are rebuilt.
///
/// Refuses, with a structural error and nothing built, a change set it cannot apply that way — one that changes a
/// row's envelope, identity, provenance or relationships, alters which payload paths the file shreds, changes a
/// promoted column's values, or moves a granule or stripe boundary. Update from the full replacement rows
/// ([`update_hef_file_from_changes_with_executor`]) for those. The layout targets must be the ones the source was
/// built with.
pub fn update_hef_file_from_source_changes_with_executor<R: BuildInput>(
    source: &BuiltHef,
    changes: &[HefRowChange<R>],
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<BuiltHef, FormatError> {
    let (rows, reuse) = plan_source_update(source, &R::build_changes(changes), config)?;
    build_hef_file_sealed(rows, config, encode, None, None, Some(&reuse))
}

/// Profiled counterpart of [`update_hef_file_with_executor`].
pub fn update_hef_file_profiled_with_executor<R: BuildInput>(
    source: &BuiltHef,
    before: &[R],
    after: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<(BuiltHef, BuildProfile), FormatError> {
    let before = R::into_build_rows(before.to_vec());
    let after = R::into_build_rows(after);
    let reuse = plan_update_reuse(source, &before, &after, config).map(|reuse| with_sparse_plan(reuse, &after, config));
    let mut profile = BuildProfile::default();
    let built = build_hef_file_sealed(after, config, encode, None, Some(&mut profile), reuse.as_ref())?;
    Ok((built, profile))
}

/// Profiled counterpart of [`update_hef_file_from_changes_with_executor`].
pub fn update_hef_file_from_changes_profiled_with_executor<R: BuildInput>(
    source: &BuiltHef,
    after: Vec<R>,
    changes: &[HefRowChange<R>],
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<(BuiltHef, BuildProfile), FormatError> {
    let after = R::into_build_rows(after);
    let reuse = plan_change_set_reuse(source, &after, &R::build_changes(changes), config);
    let mut profile = BuildProfile::default();
    let built = build_hef_file_sealed(after, config, encode, None, Some(&mut profile), reuse.as_ref())?;
    Ok((built, profile))
}

/// Profiled counterpart of [`update_hef_file_from_source_changes_with_executor`]; reading the touched granules back
/// from the source is reported as [`BuildPhase::SourceDecoding`].
pub fn update_hef_file_from_source_changes_profiled_with_executor<R: BuildInput>(
    source: &BuiltHef,
    changes: &[HefRowChange<R>],
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<(BuiltHef, BuildProfile), FormatError> {
    let mut profile = BuildProfile::default();
    let decoding = PhaseClock::new(Some(&mut profile), BuildPhase::SourceDecoding);
    let planned = plan_source_update(source, &R::build_changes(changes), config);
    decoding.finish();
    let (rows, reuse) = planned?;
    let built = build_hef_file_sealed(rows, config, encode, None, Some(&mut profile), Some(&reuse))?;
    Ok((built, profile))
}

/// The first and last sort keys of `rows` in `order` once they prove strictly ordered and all of `tenant_id` - `None`
/// for no rows — or the rule the first row to break one breaks.
fn check_rows(
    rows: &[BuildRow],
    tenant_id: TenantId,
    order: RowOrder,
) -> Result<Option<(RowKey, RowKey)>, &'static str> {
    let mut previous: Option<RowKey> = None;
    for row in rows {
        let key = order.key(row);
        if previous.is_some_and(|previous| previous >= key) {
            return Err(order.rule());
        }
        previous = Some(key);
        if row.envelope.tenant_id != tenant_id {
            return Err("a HEF file carries one tenant");
        }
    }
    Ok(rows.first().map(|row| order.key(row)).zip(previous))
}

/// Abandons a sparse update whose rebuilt granules no longer reproduce the source's granule or stripe geometry and
/// builds the file the ordinary way from every row, exactly as an update without a sparse plan would. Only a build
/// holding every row can; one holding only the touched granules' rows has nothing to fall back to.
fn sparse_fallback(
    mut rows: Vec<BuildRow>,
    payloads: NormalizedPayloads,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
    sink: Option<&mut dyn FnMut(u64, Vec<u8>)>,
    phase_clock: PhaseClock<'_>,
    reuse: &UpdateReuse<'_>,
) -> Result<BuiltHef, FormatError> {
    if !reuse.sparse.as_ref().is_some_and(|plan| plan.rows_cover_file) {
        return Err(FormatError::Structural {
            rule: "the change set moves a granule or stripe boundary; update from the full replacement rows",
        });
    }
    payloads.restore(&mut rows);
    let profile = phase_clock.into_profile();
    let full = UpdateReuse {
        sparse: None,
        ..reuse.clone()
    };
    build_hef_file_sealed(rows, config, encode, sink, profile, Some(&full))
}

/// The profiled counterpart of [`build_hef_file_with_executor`]. This is deliberately a separate entry point so the
/// publication path pays no clock reads, synchronization, or sample allocation when profiling is disabled.
pub fn build_hef_file_profiled_with_executor<R: BuildInput>(
    rows: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
) -> Result<(BuiltHef, BuildProfile), FormatError> {
    let mut profile = BuildProfile::default();
    let built = build_hef_file_sealed(R::into_build_rows(rows), config, encode, None, Some(&mut profile), None)?;
    Ok((built, profile))
}

/// What a streamed build hands back once every byte range has gone to the sink: the same identity, seal, and footer a
/// materialized build produces — everything except the file bytes themselves, which the sink already has.
#[derive(Debug)]
pub struct StreamedHef {
    pub file_seal: [u8; 32],
    pub file_crc64_nvme: u64,
    pub file_id: u128,
    pub footer: Footer,
    pub footer_len: u64,
    pub header: HefHeader,
    pub total_len: u64,
}

impl From<BuiltHef> for StreamedHef {
    fn from(built: BuiltHef) -> Self {
        Self {
            file_seal: built.file_seal,
            file_crc64_nvme: built.file_crc64_nvme,
            file_id: built.file_id,
            footer_len: built.footer_len,
            total_len: built.total_len,
            footer: built.footer,
            header: built.header,
        }
    }
}

/// Builds one file and hands each of its byte ranges to `sink`, with the file offset it belongs at, as soon as that
/// range is final: every stripe right after layout, before the footer is even built; the footer region and trailers
/// after them; and the header last, once the footer pointer and file identity it carries are known. A sink writing
/// to a file therefore writes positionally, and can do so on another thread — it owns each range it is handed. The
/// ranges partition the file exactly, and the file is byte-for-byte the one [`build_hef_file`] produces: same
/// `file_id`, stripe checksums, segment seal, and CRC — without the concatenated file ever being materialized.
pub fn build_hef_file_streamed<R: BuildInput>(
    rows: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
    sink: &mut dyn FnMut(u64, Vec<u8>),
) -> Result<StreamedHef, FormatError> {
    build_hef_file_sealed(R::into_build_rows(rows), config, encode, Some(sink), None, None).map(StreamedHef::from)
}

/// The profiled counterpart of [`build_hef_file_streamed`].
pub fn build_hef_file_streamed_profiled<R: BuildInput>(
    rows: Vec<R>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
    sink: &mut dyn FnMut(u64, Vec<u8>),
) -> Result<(StreamedHef, BuildProfile), FormatError> {
    let mut profile = BuildProfile::default();
    let built = build_hef_file_sealed(
        R::into_build_rows(rows),
        config,
        encode,
        Some(sink),
        Some(&mut profile),
        None,
    )?;
    Ok((StreamedHef::from(built), profile))
}

fn build_hef_file_sealed(
    rows: Vec<BuildRow>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
    sink: Option<&mut dyn FnMut(u64, Vec<u8>)>,
    profile: Option<&mut BuildProfile>,
    update_reuse: Option<&UpdateReuse<'_>>,
) -> Result<BuiltHef, FormatError> {
    build_hef_file_sealed_in_order(rows, config, encode, sink, profile, update_reuse, RowOrder::Sequence)
}

/// Builds one file from rows already sorted in `order`: the primary `(epoch, sequence)` order, or the entity order
/// only the entity projection is built in.
pub(crate) fn build_hef_file_in_order(
    rows: Vec<BuildRow>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
    order: RowOrder,
) -> Result<BuiltHef, FormatError> {
    build_hef_file_sealed_in_order(rows, config, encode, None, None, None, order)
}

fn build_hef_file_sealed_in_order(
    mut rows: Vec<BuildRow>,
    config: &HefBuildConfig,
    encode: &dyn EncodeExecutor,
    mut sink: Option<&mut dyn FnMut(u64, Vec<u8>)>,
    profile: Option<&mut BuildProfile>,
    update_reuse: Option<&UpdateReuse<'_>>,
    order: RowOrder,
) -> Result<BuiltHef, FormatError> {
    let mut phase_clock = PhaseClock::new(profile, BuildPhase::Normalization);
    let profile_workers = phase_clock.enabled();
    // A sparse update builds only the granules its plan names and borrows the rest of the file from the source, so
    // the file's row count and bounds are the source's rather than those of `rows`, which may hold only the built
    // granules' rows.
    let sparse_reuse = update_reuse.filter(|reuse| reuse.sparse.is_some());
    let sparse: Option<&SparsePlan> = sparse_reuse.and_then(|reuse| reuse.sparse.as_ref());
    let source = sparse_reuse.map(|reuse| reuse.source);
    let (row_count, first_key, last_key) = match source {
        Some(source) => {
            let (Some(first), Some(last)) = (source.footer.granules.first(), source.footer.granules.last()) else {
                return Err(FormatError::Structural {
                    rule: "a HEF file must carry at least one row",
                });
            };
            (
                source.header.row_count as usize,
                (first.first_epoch, first.first_sequence),
                (last.last_epoch, last.last_sequence),
            )
        }
        None => {
            let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
                return Err(FormatError::Structural {
                    rule: "a HEF file must carry at least one row",
                });
            };
            match order {
                RowOrder::Entity => {
                    let points = rows.iter().map(|row| (row.epoch, row.sequence));
                    let first = points.clone().min().unwrap_or((first.epoch, first.sequence));
                    let last = points.max().unwrap_or((last.epoch, last.sequence));
                    (rows.len(), first, last)
                }
                RowOrder::Sequence => (rows.len(), (first.epoch, first.sequence), (last.epoch, last.sequence)),
            }
        }
    };
    // A row is a few hundred bytes, so a pass over a file's worth of them streams tens of megabytes: the order and
    // tenant checks make one pass, on the worker pool, and only the chunk boundaries are compared in sequence.
    let checked: Vec<Result<Option<(RowKey, RowKey)>, &'static str>> = rows
        .par_chunks(NORMALIZE_BATCH_ROWS)
        .map(|chunk| check_rows(chunk, config.tenant_id, order))
        .collect();
    let mut previous: Option<RowKey> = None;
    for chunk in checked {
        let Some((first, last)) = chunk.map_err(|rule| FormatError::Structural { rule })? else {
            continue;
        };
        if previous.is_some_and(|previous| previous >= first) {
            return Err(FormatError::Structural { rule: order.rule() });
        }
        previous = Some(last);
    }
    // The rows of each source granule as this build holds them: every granule's for a build over the whole file,
    // only the built granules' — the rest empty — for one handed just those.
    let sparse_ranges: Option<Vec<(usize, usize)>> = sparse.zip(source).map(|(plan, source)| {
        let mut cursor = 0usize;
        source
            .footer
            .granules
            .iter()
            .enumerate()
            .map(|(index, granule)| {
                let start = cursor;
                if plan.rows_cover_file || plan.builds(index).is_some() {
                    cursor += granule.row_count as usize;
                }
                (start, cursor)
            })
            .collect()
    });
    if let Some(ranges) = &sparse_ranges
        && ranges.last().map_or(0, |range| range.1) != rows.len()
    {
        return Err(FormatError::Structural {
            rule: "a sparse update's rows are its built granules' rows in granule order",
        });
    }
    // The columnar build reshapes a payload field by field and walks the rows several times over, so a payload that
    // arrived still encoded is decoded once here rather than on every pass, every top-level field and every distinct
    // row shape is given its file-local identity, and the values move out of their per-row trees into one contiguous
    // arena. Later passes read that arena and never hash a field name. A sparse update normalizes only the granules
    // it builds.
    let normalized_ranges: Vec<(usize, usize)> = match (sparse, &sparse_ranges) {
        (Some(plan), Some(ranges)) => ranges
            .iter()
            .enumerate()
            .filter(|(index, _)| plan.builds(*index).is_some())
            .map(|(_, range)| *range)
            .collect(),
        _ => vec![(0, rows.len())],
    };
    let payloads = NormalizedPayloads::normalize(&mut rows, &normalized_ranges)?;
    let field_identities = payloads.identities();
    // A rebuilt granule must still cut where the source cut it, or the file's geometry — and with it every borrowed
    // block's place — is no longer the source's; a build holding every row then starts over the ordinary way.
    if let (Some(plan), Some(source), Some(ranges), Some(reuse)) = (sparse, source, &sparse_ranges, sparse_reuse) {
        let granule_count = source.footer.granules.len();
        let reproduces =
            source
                .footer
                .granules
                .iter()
                .zip(ranges)
                .enumerate()
                .all(|(index, (granule, &(start, end)))| {
                    plan.builds(index).is_none() || {
                        let forced = granule.row_count as usize == config.targets.index_granularity
                            || index + 1 == granule_count
                            || source
                                .footer
                                .granules
                                .get(index + 1)
                                .is_some_and(|next| next.first_epoch != granule.last_epoch);
                        granule_cut_reproduces(
                            rows.get(start..end).unwrap_or_default(),
                            &payloads,
                            start,
                            &config.targets,
                            !forced,
                        )
                    }
                });
        if !reproduces {
            return sparse_fallback(rows, payloads, config, encode, sink, phase_clock, reuse);
        }
    }
    // A recorded IO alignment must be a power of two no larger than the header block. The header block is the alignment
    // origin for every absolute block offset, so a non-power-of-two or oversized value leaves those offsets incongruent
    // with the alignment the footer advertises, and an `O_DIRECT` reader slices pages at the wrong boundary.
    if config.io_alignment_bytes > 0
        && (!config.io_alignment_bytes.is_power_of_two() || config.io_alignment_bytes as usize > HEADER_BLOCK_LEN)
    {
        return Err(FormatError::Structural {
            rule: "io_alignment_bytes must be a power of two no larger than the header block",
        });
    }
    validate_promotion_plan(&config.promotion)?;

    // Each analytical column must carry exactly one value per row (the `AnalyticalColumn::data` contract). A short column
    // would slice to empty or partial granule blocks that still advertise the full granule row count — a silent
    // corruption — so reject the mismatch here, before anything is encoded.
    if config
        .analytical_columns
        .iter()
        .any(|col| col.data.row_count() != row_count)
    {
        return Err(FormatError::Structural {
            rule: "each analytical column must carry exactly one value per row",
        });
    }

    // A substring declaration only reaches a column that gets a text-token filter at all, which is the public string
    // ones. Declaring it anywhere else would be silently ignored — the operator would pay nothing and get nothing —
    // so the mismatch is refused here rather than dropped.
    if config
        .analytical_columns
        .iter()
        .any(|col| col.substring_searchable && (col.kind != ColumnKind::String || col.internal_only))
    {
        return Err(FormatError::Structural {
            rule: "only a public string analytical column may be declared substring-searchable",
        });
    }

    phase_clock.transition(BuildPhase::DictionaryConstruction);
    // File dictionaries: sorted unique source/type/entity strings. Collect borrowed slices and only own the surviving
    // unique values, so a duplicate string is never copied.
    let dictionaries_unchanged = update_reuse.is_some_and(|reuse| {
        !reuse.changed_columns_by_granule.values().any(|columns| {
            [
                column_ids::SOURCE_ID,
                column_ids::EVENT_TYPE_ID,
                column_ids::ENTITY_TYPE_ID,
            ]
            .iter()
            .any(|column_id| columns.contains(column_id))
        })
    });
    let dictionaries = if dictionaries_unchanged {
        update_reuse
            .map(|reuse| reuse.source.footer.dictionaries.clone())
            .unwrap_or_default()
    } else {
        build_file_dictionaries(&rows)
    };
    let dictionary_index = build_dictionary_index(&dictionaries);

    phase_clock.transition(BuildPhase::PathStatistics);
    // Statistics-driven shredding selection over the canonical payloads. Each row's values come off the normalized
    // arena named by its shape's integer field ids, so this steady-state loop chases no tree pointers and hashes no
    // strings.
    let observe_rows = |entries: &[NormalizedPayload]| {
        let mut statistics = PathStatistics::for_field_ids(field_identities.names.len());
        for &entry in entries {
            statistics.observe_field_ids(payloads.field_ids(entry), payloads.values(entry));
        }
        statistics
    };
    let merge_statistics = |mut left: PathStatistics, right: PathStatistics| {
        left.merge(right);
        left
    };
    let reuse_path_plan = update_reuse
        .is_some_and(|reuse| !reuse.path_statistics_changed && static_routes_compatible(&reuse.source.footer, config));
    let statistics = if reuse_path_plan {
        None
    } else if profile_workers {
        // A bounded chunk leaves the signpost frame on each Rayon worker long enough for Time Profiler to sample it,
        // without paying one signpost call per row. PathStatistics reduction is deterministic and associative.
        payloads
            .rows
            .par_chunks(1_024)
            .map(|entries| profile_path_statistics(|| observe_rows(entries)))
            .reduce(
                || PathStatistics::for_field_ids(field_identities.names.len()),
                merge_statistics,
            )
            .into()
    } else {
        Some(
            payloads
                .rows
                .par_iter()
                .fold(
                    || PathStatistics::for_field_ids(field_identities.names.len()),
                    |mut statistics, &entry| {
                        statistics.observe_field_ids(payloads.field_ids(entry), payloads.values(entry));
                        statistics
                    },
                )
                .reduce(
                    || PathStatistics::for_field_ids(field_identities.names.len()),
                    merge_statistics,
                ),
        )
    };
    let promoted_paths: Vec<&str> = config
        .promotion
        .columns
        .iter()
        .map(|column| column.path.as_str())
        .collect();
    let freetext_paths: Vec<&str> = config.freetext.fields.iter().map(String::as_str).collect();
    let excluded: Vec<&str> = promoted_paths.iter().chain(freetext_paths.iter()).copied().collect();
    let excluded_field_ids: HashSet<u32> = excluded
        .iter()
        .filter_map(|path| field_identities.field_id(path))
        .collect();
    let (shred_plan, sparse_keys, shred_kinds) = if reuse_path_plan {
        let source = &update_reuse.expect("reuse path plan has a source").source.footer;
        let shred_plan = source.shredded.clone();
        let sparse_keys = source.sparse_keys.clone();
        let shred_kinds = shred_plan
            .iter()
            .filter_map(|entry| {
                source
                    .columns
                    .iter()
                    .find(|column| column.column_id == entry.column_id)
                    .map(|column| (entry.path.clone(), column.kind))
            })
            .collect();
        (shred_plan, sparse_keys, shred_kinds)
    } else {
        let statistics = statistics.expect("full path analysis produces statistics");
        // One owned copy per distinct field name, for the candidate selection's own name lists.
        let field_names: Vec<String> = field_identities
            .field_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        let candidates = statistics.shred_candidates_by_id(&field_names, &excluded_field_ids);
        // The sparse tier rides the same shredded machinery: its paths join the shred plan (so blocks, marks, presence,
        // and the payload merge treat them exactly like dense shredded columns) and are additionally declared in the
        // footer's sparse key set, closing the sparse governance per file.
        let sparse = statistics.sparse_candidates_by_id(&field_names, &excluded_field_ids);
        let shred_plan: Vec<ShredEntry> = candidates
            .iter()
            .chain(sparse.iter())
            .enumerate()
            .map(|(index, (path, _kind))| ShredEntry {
                column_id: column_ids::SHREDDED_BASE + index as u32,
                path: path.clone(),
            })
            .collect();
        let sparse_keys: Vec<ShredEntry> = shred_plan.iter().skip(candidates.len()).cloned().collect();
        let shred_kinds: HashMap<String, ColumnKind> = candidates.into_iter().chain(sparse).collect();
        (shred_plan, sparse_keys, shred_kinds)
    };

    // Column directory: required + promoted + shredded + free-text.
    let mut columns: Vec<ColumnDescriptor> = REQUIRED_COLUMNS
        .iter()
        .map(|spec| ColumnDescriptor {
            column_id: spec.column_id,
            name: spec.name.to_owned(),
            kind: spec.kind,
            nullable: spec.nullable,
            internal_only: spec.internal_only,
        })
        .collect();
    // Every remaining descriptor is already countable: one per promoted, shredded, free-text and analytical column,
    // plus the two optional families this file's rows turn out to declare.
    columns.reserve(
        config.promotion.columns.len()
            + shred_plan.len()
            + config.freetext.fields.len()
            + config.analytical_columns.len()
            + PROVENANCE_COLUMNS.len()
            + RELATIONSHIP_COLUMNS.len(),
    );
    let mut presence_entries = Vec::with_capacity(config.promotion.columns.len());
    for (index, promoted) in config.promotion.columns.iter().enumerate() {
        let column_id = column_ids::PROMOTED_BASE + index as u32;
        columns.push(ColumnDescriptor {
            column_id,
            name: promoted.name.clone(),
            kind: promoted.kind,
            nullable: true,
            internal_only: false,
        });
        presence_entries.push(PresenceEntry {
            column_id,
            since_schema_version: promoted.since_schema_version,
        });
    }
    for entry in &shred_plan {
        columns.push(ColumnDescriptor {
            column_id: entry.column_id,
            name: format!("payload.{}", entry.path),
            kind: shred_kinds
                .get(entry.path.as_str())
                .copied()
                .unwrap_or(ColumnKind::String),
            nullable: true,
            internal_only: false,
        });
    }
    // The provenance family is materialized only when this file actually carries signed events; a file of unsigned
    // events declares none of its columns and pays nothing for them. A sparse update carries what its source does:
    // its rows may be only the touched granules', and its plan already required the two to agree.
    let source_has_column = |column_id: u32| {
        source.is_some_and(|source| source.footer.columns.iter().any(|column| column.column_id == column_id))
    };
    let carries_provenance = match source {
        Some(_) => source_has_column(column_ids::AUTHOR_PUBKEY),
        None => rows.iter().any(|row| row.provenance.is_some()),
    };
    if carries_provenance {
        for spec in PROVENANCE_COLUMNS {
            columns.push(ColumnDescriptor {
                column_id: spec.column_id,
                name: spec.name.to_owned(),
                kind: spec.kind,
                nullable: spec.nullable,
                internal_only: spec.internal_only,
            });
        }
    }
    // Same absence rule for the relationship family: only a file whose rows declare references pays for the columns.
    let carries_relationships = match source {
        Some(_) => source_has_column(column_ids::PARENT_REF),
        None => rows.iter().any(|row| row.relationships.is_some()),
    };
    if carries_relationships {
        for spec in RELATIONSHIP_COLUMNS {
            columns.push(ColumnDescriptor {
                column_id: spec.column_id,
                name: spec.name.to_owned(),
                kind: spec.kind,
                nullable: spec.nullable,
                internal_only: spec.internal_only,
            });
        }
    }
    let freetext_entries: Vec<FreetextEntry> = config
        .freetext
        .fields
        .iter()
        .enumerate()
        .map(|(index, field)| FreetextEntry {
            column_id: column_ids::FREETEXT_BASE + index as u32,
            declared_field: field.clone(),
        })
        .collect();
    for entry in &freetext_entries {
        columns.push(ColumnDescriptor {
            column_id: entry.column_id,
            name: format!("freetext.{}", entry.declared_field),
            kind: ColumnKind::String,
            nullable: true,
            internal_only: false,
        });
    }
    for col in &config.analytical_columns {
        columns.push(ColumnDescriptor {
            column_id: col.column_id,
            internal_only: col.internal_only,
            kind: col.kind,
            name: col.name.clone(),
            nullable: true,
        });
    }

    // Every column block is addressed by `(column_id, granule_id)`, and the reader collects marks into a map on that
    // key — so two columns sharing an id do not conflict loudly, one silently replaces the other. A caller-supplied
    // analytical column id that lands on a required column (`payload_flags`) or on a generated promoted, shredded, or
    // free-text id would take that block's place and corrupt every read of it, so the collision is refused here,
    // before anything is encoded.
    let mut seen_column_ids = HashSet::with_capacity(columns.len());
    if !columns.iter().all(|column| seen_column_ids.insert(column.column_id)) {
        return Err(FormatError::Structural {
            rule: "every column in a HEF file must have a distinct column id",
        });
    }

    phase_clock.transition(BuildPhase::GranuleConstruction);
    // Granule partition: row target plus payload byte cap. A granule never spans more than one epoch — sequence numbers
    // reset per epoch, so a granule that straddled an epoch boundary would carry a `(first_sequence, last_sequence)`
    // range that mixes two independent sequence spaces, and sequence pruning (`granules_for_sequence`) would read it as
    // one interval. Because rows are `(epoch, sequence)`-ordered, an epoch change is a clean cut point.
    // A sparse update keeps the source's granules, whose cuts were checked above.
    let mut granule_row_ranges: Vec<(usize, usize)> = sparse_ranges.unwrap_or_default();
    let mut start = 0usize;
    let mut payload_bytes = 0usize;
    // Rows in one run share a shape, so the key names usually arrive in the same order row after row. The estimator
    // holds on to the last row's names and its distinct count, which turns the repeat rows into one comparison each
    // instead of sorting and deduplicating a fresh list per row.
    let mut estimator = PayloadByteEstimator::default();
    let byte_limit_enabled = config.targets.index_granularity_bytes != usize::MAX;
    for (index, row) in rows
        .iter()
        .enumerate()
        .take(if sparse.is_some() { 0 } else { rows.len() })
    {
        if byte_limit_enabled {
            let estimate = estimator.estimate(&payloads, payloads.payload(index), row);
            payload_bytes = payload_bytes.saturating_add(estimate);
        }
        let rows_in_granule = index - start + 1;
        // Every row accumulated into the current granule shares one epoch (this same rule cut the granule at the last
        // epoch change), so the current `row`'s epoch is the granule's epoch — cut here when the next row leaves it.
        let epoch_boundary = rows.get(index + 1).is_some_and(|next| next.epoch != row.epoch);
        if rows_in_granule >= config.targets.index_granularity
            || (byte_limit_enabled && payload_bytes >= config.targets.index_granularity_bytes)
            || epoch_boundary
        {
            granule_row_ranges.push((start, index + 1));
            start = index + 1;
            payload_bytes = 0;
        }
    }
    if sparse.is_none() && start < rows.len() {
        granule_row_ranges.push((start, rows.len()));
    }

    // Build per-granule pieces, one job per granule fanned out through the injected executor. The ranges partition the
    // rows, so peeling each granule's slice off the front hands every job a disjoint view no other job can see, and a
    // granule's pieces are a pure function of that slice plus the shared plan. Products land in index-addressed slots
    // and are reassembled in granule order below, so the file bytes are identical however the jobs were scheduled.
    let mut row_slices: Vec<Mutex<&mut [BuildRow]>> = Vec::with_capacity(granule_row_ranges.len());
    let mut remaining: &mut [BuildRow] = &mut rows;
    for (start, end) in &granule_row_ranges {
        let row_count = end.saturating_sub(*start).min(remaining.len());
        let (slice, rest) = std::mem::take(&mut remaining).split_at_mut(row_count);
        remaining = rest;
        row_slices.push(Mutex::new(slice));
    }
    let granule_slots: Vec<Mutex<Option<Result<GranulePieces, FormatError>>>> =
        row_slices.iter().map(|_| Mutex::new(None)).collect();
    let field_routes = build_field_routes(field_identities, &config.promotion, &shred_plan, &freetext_entries);
    let shape_routes = build_shape_routes(field_identities, &field_routes);
    encode.run_jobs(row_slices.len(), &|granule_index| {
        maybe_profile_granule_construction(profile_workers, || {
            let (Some(granule_rows), Some(slot), Some(&(start, _))) = (
                row_slices.get(granule_index),
                granule_slots.get(granule_index),
                granule_row_ranges.get(granule_index),
            ) else {
                return;
            };
            let mut granule_rows = granule_rows.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut build = |granule_id: u32, first_row_ordinal: u64, wanted: Option<&HashSet<u32>>, residual| {
                let granule_payloads = payloads.range(start, granule_rows.len());
                build_granule(
                    granule_id,
                    first_row_ordinal,
                    &mut granule_rows,
                    granule_payloads,
                    &dictionaries,
                    &dictionary_index,
                    &config.promotion,
                    &shred_plan,
                    &shred_kinds,
                    &freetext_entries,
                    &field_routes,
                    &shape_routes,
                    &payloads,
                    config.freetext_row_offset_index,
                    carries_provenance,
                    carries_relationships,
                    wanted,
                    residual,
                )
            };
            let pieces = match sparse.zip(source.and_then(|source| source.footer.granules.get(granule_index))) {
                Some((plan, source_entry)) => match plan.builds(granule_index) {
                    None => Ok(GranulePieces::borrowed(*source_entry)),
                    Some(columns) => {
                        let rebuild_residual = sparse_reuse
                            .is_some_and(|reuse| reuse.changed_payload_granules.contains(&source_entry.granule_id));
                        build(
                            source_entry.granule_id,
                            source_entry.first_row_ordinal,
                            Some(columns),
                            rebuild_residual,
                        )
                        .map(|mut pieces| {
                            // No change touched an envelope, so the granule's bounds are the source's — and so is
                            // its residual size unless the arena was rebuilt.
                            pieces.entry = GranuleEntry {
                                compressed_bytes_estimate: if rebuild_residual {
                                    pieces.entry.compressed_bytes_estimate
                                } else {
                                    source_entry.compressed_bytes_estimate
                                },
                                ..*source_entry
                            };
                            pieces
                        })
                    }
                },
                None => build(granule_index as u32, start as u64, None, true),
            };
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(pieces);
        });
    });
    // The routes borrow `shred_plan` and `freetext_entries`, which the footer below moves; releasing them here ends
    // that borrow at the last use rather than at the end of the function. `dictionary_index` borrows `dictionaries`,
    // which the footer also moves, for the same reason. The row slices borrow `rows`, which a sparse update that
    // has to start over needs back.
    drop(field_routes);
    drop(dictionary_index);
    drop(row_slices);
    let mut granules: Vec<GranulePieces> = Vec::with_capacity(granule_slots.len());
    for slot in granule_slots {
        granules.push(
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("the executor runs every job")?,
        );
    }

    phase_clock.transition(BuildPhase::BlockEncoding);

    // The columns every granule's blocks cover, in the order `build_granule` emits them: one encode job per
    // (granule, column) below, whether the granule built that block or borrows it from the source.
    let block_columns: Vec<u32> = REQUIRED_COLUMNS
        .iter()
        .map(|spec| spec.column_id)
        .chain((0..config.promotion.columns.len()).map(|index| column_ids::PROMOTED_BASE + index as u32))
        .chain(shred_plan.iter().map(|entry| entry.column_id))
        .chain(freetext_entries.iter().map(|entry| entry.column_id))
        .chain(
            carries_provenance
                .then_some(PROVENANCE_COLUMNS)
                .into_iter()
                .flatten()
                .map(|spec| spec.column_id),
        )
        .chain(
            carries_relationships
                .then_some(RELATIONSHIP_COLUMN_IDS)
                .into_iter()
                .flatten(),
        )
        .collect();
    // The values each source block holds, for the stripe estimate of a block a sparse update did not build.
    let source_block_values: HashMap<(u32, u32), usize> = source
        .into_iter()
        .flat_map(|source| &source.footer.page_stats)
        .map(|stats| ((stats.column_id, stats.granule_id), stats.row_count as usize))
        .collect();
    // Each granule's per-row free-text offset indexes in declared order: the ones the granule built, and for a
    // sparse update the ones it left to the source because that column's values did not change.
    let mut freetext_row_offsets_by_granule: Vec<Vec<(u32, Cow<'_, [u8]>, Cow<'_, [u8]>)>> =
        Vec::with_capacity(granules.len());
    for granule in granules.iter_mut() {
        let mut built = std::mem::take(&mut granule.freetext_row_offsets).into_iter().peekable();
        let mut indexes = Vec::new();
        for entry in &freetext_entries {
            if built
                .peek()
                .is_some_and(|(column_id, _, _)| *column_id == entry.column_id)
            {
                if let Some((column_id, bytes, offsets)) = built.next() {
                    indexes.push((column_id, Cow::Owned(bytes), Cow::Owned(offsets)));
                }
                continue;
            }
            let Some(source) = source.filter(|_| config.freetext_row_offset_index) else {
                continue;
            };
            let (bytes, offsets) = source
                .footer
                .freetext_row_offsets
                .iter()
                .find(|index| index.column_id == entry.column_id && index.granule_id == granule.entry.granule_id)
                .and_then(|index| {
                    Some((
                        source_extent(source, index.granule_id, index.bytes_offset, index.bytes_len)?,
                        source_extent(source, index.granule_id, index.offsets_offset, index.offsets_len)?,
                    ))
                })
                .ok_or(FormatError::Structural {
                    rule: "a sparse update borrows free-text row indexes the source stores",
                })?;
            indexes.push((entry.column_id, Cow::Borrowed(bytes), Cow::Borrowed(offsets)));
        }
        freetext_row_offsets_by_granule.push(indexes);
    }

    // Stripe assignment by accumulated byte estimate.
    let mut stripe_of_granule: Vec<u32> = Vec::with_capacity(granules.len());
    let mut stripe_id = 0u32;
    let mut stripe_bytes = 0usize;
    for (granule_index, granule) in granules.iter().enumerate() {
        // The free-text arenas are written inside the stripe like the residual, so they count toward its size: a
        // granule whose free text alone is oversized must hit the clamp below instead of silently overflowing the
        // u32 per-row offsets written in `build_granule`. The residual counts at its uncompressed size, which the
        // entry records.
        let granule_bytes: usize = block_columns
            .iter()
            .map(|&column_id| {
                let values = match granule.blocks.iter().find(|block| block.column_id == column_id) {
                    Some(block) => block.data.row_count(),
                    None => source_block_values
                        .get(&(column_id, granule.entry.granule_id))
                        .copied()
                        .unwrap_or_default(),
                };
                values * 8
            })
            .sum::<usize>()
            + granule.entry.compressed_bytes_estimate as usize
            + freetext_row_offsets_by_granule
                .get(granule_index)
                .map(|indexes| {
                    indexes
                        .iter()
                        .map(|(_, bytes, offsets)| bytes.len() + offsets.len())
                        .sum::<usize>()
                })
                .unwrap_or_default();
        if stripe_bytes + granule_bytes > config.targets.stripe_target_bytes && stripe_bytes > 0 {
            stripe_id += 1;
            stripe_bytes = 0;
        }
        if granule_bytes > config.targets.max_stripe_bytes {
            return Err(FormatError::Structural {
                rule: "stripe clamp: a single granule exceeds the maximum stripe size",
            });
        }
        stripe_bytes += granule_bytes;
        stripe_of_granule.push(stripe_id);
    }
    for (granule, assigned) in granules.iter_mut().zip(stripe_of_granule.iter()) {
        granule.entry.stripe_id = *assigned;
    }

    // Encode all blocks, then lay out the data area per layout class.
    struct EncodedPiece<'a> {
        /// The encoded block itself, exactly as the encoder produced it — never copied behind the presence prefix, so
        /// the encoder's own buffer is what the data area later reads from.
        body: Cow<'a, [u8]>,
        column_id: u32,
        granule_id: u32,
        /// Per-page (size, pipeline, stats, first_row_in_granule, row_count) when page_size_rows splits this piece
        /// into multiple independent blocks.
        pages: Vec<(u64, crate::encoding::PipelineId, crate::encoding::BlockStats, u32, u32)>,
        pipeline: crate::encoding::PipelineId,
        /// The framing the data area writes ahead of `body`: the presence form for a single-block piece, empty for a
        /// paged one (whose per-page framing already sits inside `body`).
        prefix: Cow<'a, [u8]>,
        row_count: u32,
        stats: crate::encoding::BlockStats,
        stripe_id: u32,
        /// Total decoded byte length of this piece — the framing plus each block's decoded length, so the `ColumnMark`
        /// records the true uncompressed size rather than a copy of the compressed length.
        uncompressed_len: u64,
    }

    impl EncodedPiece<'_> {
        /// How many bytes this piece occupies in the data area.
        fn len(&self) -> usize {
            self.prefix.len() + self.body.len()
        }

        /// The piece's first byte — the presence form tag — or `None` when the piece is empty.
        fn first_byte(&self) -> Option<u8> {
            self.prefix.first().or_else(|| self.body.first()).copied()
        }

        /// Appends the piece's bytes to `out`, framing first.
        fn write_to(&self, out: &mut Writer) {
            out.put_slice(self.prefix.as_ref());
            out.put_slice(self.body.as_ref());
        }

        /// A fast non-cryptographic key for the block-alias prefilter — the confirm step that follows an alias hit
        /// already compares the extents byte-for-byte, so this only needs to bucket candidates cheaply, not resist
        /// collisions.
        fn alias_key(&self) -> u64 {
            use std::hash::Hasher;
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            hasher.write(self.prefix.as_ref());
            hasher.write(self.body.as_ref());
            hasher.finish()
        }
    }

    struct PayloadGeometry<'a> {
        dictionary: Cow<'a, [u8]>,
        /// Per indexed free-text column, this granule's per-row byte-offset index: `(column_id, raw row bytes,
        /// offsets table)`, borrowed from the source when an update left the column alone. Empty when the file
        /// declares no free-text fields, and when the build did not opt into the index.
        freetext_row_offsets: Vec<(u32, Cow<'a, [u8]>, Cow<'a, [u8]>)>,
        granule_id: u32,
        offsets: Cow<'a, [u8]>,
        residual: Cow<'a, [u8]>,
        residual_compression: ResidualCompression,
    }

    /// Encodes one block and returns its presence framing separately from the encoded body, so the body travels to the
    /// data area in the buffer the encoder already filled.
    fn encode_mini_block(
        data: &ColumnData,
        presence: &[u8],
        random_access: bool,
        strategy: CascadeStrategy,
        shared: Option<&[String]>,
    ) -> (
        Vec<u8>,
        Vec<u8>,
        crate::encoding::PipelineId,
        crate::encoding::BlockStats,
        u64,
    ) {
        let encoded = encode_block_with_shared_dictionary(data, random_access, strategy, shared);
        let mut prefix = Writer::with_capacity(presence.len() + 5);
        crate::layout::encode_presence(presence, data.row_count() as u32, &mut prefix);
        // Decoded length: the presence form tag and the in-memory bitmap pass through uncompressed, plus the block's
        // own decoded length.
        let uncompressed_len = 1 + presence.len() as u64 + encoded.uncompressed_len;
        (
            prefix.into_bytes(),
            encoded.bytes,
            encoded.pipeline,
            encoded.stats,
            uncompressed_len,
        )
    }

    fn slice_column_data(data: &ColumnData, start: usize, end: usize) -> ColumnData {
        fn span<T: Clone>(values: &[T], start: usize, end: usize) -> Vec<T> {
            values.get(start..end).map(<[T]>::to_vec).unwrap_or_default()
        }
        match data {
            ColumnData::Decimal { values, scale } => ColumnData::Decimal {
                values: span(values, start, end),
                scale: *scale,
            },
            ColumnData::F64(v) => ColumnData::F64(span(v, start, end)),
            ColumnData::I64(v) => ColumnData::I64(span(v, start, end)),
            ColumnData::Strings(v) => ColumnData::Strings(v.slice(start, end)),
            ColumnData::U128(v) => ColumnData::U128(span(v, start, end)),
            ColumnData::U64(v) => ColumnData::U64(span(v, start, end)),
        }
    }

    /// The encoded token-membership filter over a string block's present values, or `None` for non-string blocks.
    ///
    /// With `include_ngrams` the filter also carries the trigrams of every value, which is what lets the scan prune a
    /// substring (`CONTAINS`/`LIKE`) predicate; without it the filter answers whole-token equality and IN only.
    fn encoded_text_tokens(data: &ColumnData, include_ngrams: bool) -> Option<Vec<u8>> {
        let ColumnData::Strings(values) = data else {
            return None;
        };
        Some(TextTokenIndex::build_from_iter(values.iter_present(), include_ngrams).encode())
    }

    // The columns whose blocks carry a text-token filter, each mapped to whether its filter also carries trigrams:
    // declared free-text fields, promoted string columns, and public context-projection string columns — the declared
    // searchable string fields a scan filters by equality. A free-text field always carries trigrams; a promoted or
    // context string column carries them only where it is declared `substring_searchable`, because trigrams cost
    // filter bytes and prune nothing extra for a column filtered by whole value — but a `CONTAINS` against a column
    // without them keeps every granule and reads the whole column. Identity and statistics-shredded string columns
    // carry no filter at all: no declaration marks them searchable, and a filter over a unique-per-row id column
    // prunes nothing. The filter bytes live in the data area (only their byte ranges reach the footer), so a filter
    // costs a cold open nothing.
    let searchable_text_columns: HashMap<u32, bool> = config
        .promotion
        .columns
        .iter()
        .enumerate()
        .filter(|(_, promoted)| promoted.kind == ColumnKind::String)
        .map(|(index, promoted)| (column_ids::PROMOTED_BASE + index as u32, promoted.substring_searchable))
        .chain(freetext_entries.iter().map(|entry| (entry.column_id, true)))
        .chain(
            config
                .analytical_columns
                .iter()
                .filter(|col| col.kind == ColumnKind::String && !col.internal_only)
                .map(|col| (col.column_id, col.substring_searchable)),
        )
        .collect();

    /// Everything one block's encode job produces: its finished piece plus any text-token filters its pages carry.
    struct BlockProduct<'a> {
        /// The table a freshly encoded FSST block trained (its first page's, for a paged block), for the column's
        /// replay capture.
        fsst: Option<FsstTable>,
        piece: EncodedPiece<'a>,
        reused: bool,
        text_tokens: Vec<TextTokenEntry>,
    }

    /// Encodes one pending block into its piece. Pure per-block work — no shared state — so the executor can run one
    /// job per block concurrently and the products only need reassembling in job order.
    fn encode_block_piece(
        block: &PendingBlock,
        entry: &GranuleEntry,
        stripe: u32,
        page_size: usize,
        strategy: CascadeStrategy,
        searchable_text_columns: &HashMap<u32, bool>,
        replay: Option<&ReplayCapture>,
        shared: Option<&[String]>,
    ) -> BlockProduct<'static> {
        let mut text_tokens: Vec<TextTokenEntry> = Vec::new();
        let mut fsst = None;
        let granule_row_count = entry.row_count as usize;

        let piece = if page_size > 0 && granule_row_count > page_size && block.presence.is_empty() {
            // Plain (non-presence-gated) column: split into independently decodable page blocks and record their
            // sizes for the directory.
            let page_count = granule_row_count.div_ceil(page_size);
            let mut all_bytes = Writer::with_capacity(page_count * 5);
            let mut page_info = Vec::with_capacity(page_count);
            let mut uncompressed_total = 0u64;
            for page_idx in 0..page_count {
                let page_start = page_idx * page_size;
                let page_len = (page_start + page_size).min(granule_row_count) - page_start;
                let page_data = slice_column_data(&block.data, page_start, page_start + page_len);
                let mut encoded = encode_block_replayed(&page_data, block.random_access, strategy, replay, shared).0;
                if fsst.is_none() {
                    fsst = encoded.fsst.take();
                }
                let before = all_bytes.len() as u64;
                crate::layout::encode_presence(&[], page_len as u32, &mut all_bytes);
                all_bytes.put_u32(encoded.pipeline.0);
                all_bytes.put_slice(&encoded.bytes);
                let page_bytes = all_bytes.len() as u64 - before;
                // Decoded length: the presence form tag, the 4-byte page pipeline id, plus the block's decoded
                // length.
                uncompressed_total += 5 + encoded.uncompressed_len;
                if let Some(&include_ngrams) = searchable_text_columns.get(&block.column_id)
                    && let Some(index_bytes) = encoded_text_tokens(&page_data, include_ngrams)
                {
                    text_tokens.push(TextTokenEntry {
                        column_id: block.column_id,
                        granule_id: entry.granule_id,
                        index_bytes,
                        page_index: page_idx as u32,
                    });
                }
                page_info.push((
                    page_bytes,
                    encoded.pipeline,
                    encoded.stats,
                    page_start as u32,
                    page_len as u32,
                ));
            }
            let first_pipeline = page_info
                .first()
                .map(|(_, p, _, _, _)| *p)
                .unwrap_or(crate::encoding::PipelineId(0));
            let agg_stats = aggregate_stats(page_info.iter().map(|(_, _, s, _, _)| *s));
            EncodedPiece {
                column_id: block.column_id,
                granule_id: entry.granule_id,
                stripe_id: stripe,
                body: Cow::Owned(all_bytes.into_bytes()),
                prefix: Cow::Borrowed(&[]),
                pipeline: first_pipeline,
                stats: agg_stats,
                row_count: entry.row_count,
                pages: page_info,
                uncompressed_len: uncompressed_total,
            }
        } else if page_size > 0 && granule_row_count > page_size && !block.presence.is_empty() {
            // Presence-gated column: split presence bitmap and dense values.
            let page_count = granule_row_count.div_ceil(page_size);
            let mut all_bytes = Writer::with_capacity(block.presence.len() + page_count * 5);
            let mut page_info = Vec::with_capacity(page_count);
            let mut value_offset = 0usize;
            let mut uncompressed_total = 0u64;
            for page_idx in 0..page_count {
                let page_start = page_idx * page_size;
                let page_len = (page_start + page_size).min(granule_row_count) - page_start;
                let page_presence = slice_presence_bits(&block.presence, page_start, page_len);
                let present_count = count_set_bits(&page_presence, page_len);
                let page_data = slice_column_data(&block.data, value_offset, value_offset + present_count);
                value_offset += present_count;
                let mut encoded = encode_block_replayed(&page_data, block.random_access, strategy, replay, shared).0;
                if fsst.is_none() {
                    fsst = encoded.fsst.take();
                }
                let before = all_bytes.len() as u64;
                crate::layout::encode_presence(&page_presence, page_len as u32, &mut all_bytes);
                all_bytes.put_u32(encoded.pipeline.0);
                all_bytes.put_slice(&encoded.bytes);
                let page_bytes = all_bytes.len() as u64 - before;
                // Decoded length: the presence form tag and the in-memory bitmap pass through uncompressed, followed
                // by the 4-byte page pipeline id, plus the block's decoded length.
                uncompressed_total += 5 + page_presence.len() as u64 + encoded.uncompressed_len;
                if let Some(&include_ngrams) = searchable_text_columns.get(&block.column_id)
                    && let Some(index_bytes) = encoded_text_tokens(&page_data, include_ngrams)
                {
                    text_tokens.push(TextTokenEntry {
                        column_id: block.column_id,
                        granule_id: entry.granule_id,
                        index_bytes,
                        page_index: page_idx as u32,
                    });
                }
                page_info.push((
                    page_bytes,
                    encoded.pipeline,
                    encoded.stats,
                    page_start as u32,
                    page_len as u32,
                ));
            }
            let first_pipeline = page_info
                .first()
                .map(|(_, p, _, _, _)| *p)
                .unwrap_or(crate::encoding::PipelineId(0));
            let agg_stats = aggregate_stats(page_info.iter().map(|(_, _, s, _, _)| *s));
            EncodedPiece {
                column_id: block.column_id,
                granule_id: entry.granule_id,
                stripe_id: stripe,
                body: Cow::Owned(all_bytes.into_bytes()),
                prefix: Cow::Borrowed(&[]),
                pipeline: first_pipeline,
                stats: agg_stats,
                row_count: entry.row_count,
                pages: page_info,
                uncompressed_len: uncompressed_total,
            }
        } else {
            let mut encoded = encode_block_replayed(&block.data, block.random_access, strategy, replay, shared).0;
            fsst = encoded.fsst.take();
            let mut prefix = Writer::with_capacity(block.presence.len() + 8);
            crate::layout::encode_presence(&block.presence, entry.row_count, &mut prefix);
            // Decoded length: the presence form tag and the in-memory bitmap pass through uncompressed, plus the
            // block's decoded length.
            let uncompressed_len = 1 + block.presence.len() as u64 + encoded.uncompressed_len;
            if let Some(&include_ngrams) = searchable_text_columns.get(&block.column_id)
                && let Some(index_bytes) = encoded_text_tokens(&block.data, include_ngrams)
            {
                text_tokens.push(TextTokenEntry {
                    column_id: block.column_id,
                    granule_id: entry.granule_id,
                    index_bytes,
                    page_index: 0,
                });
            }
            EncodedPiece {
                column_id: block.column_id,
                granule_id: entry.granule_id,
                stripe_id: stripe,
                body: Cow::Owned(encoded.bytes),
                prefix: Cow::Owned(prefix.into_bytes()),
                pipeline: encoded.pipeline,
                stats: encoded.stats,
                row_count: entry.row_count,
                pages: Vec::new(),
                uncompressed_len,
            }
        };
        BlockProduct {
            fsst,
            piece,
            reused: false,
            text_tokens,
        }
    }

    /// Borrows one source block's exact stored extent and reconstructs the metadata the ordinary layout loop consumes.
    /// No presence frame or value stream is decoded; offsets are rewritten when the borrowed bytes are laid out.
    fn reuse_block_piece<'a>(
        source: &'a BuiltHef,
        entry: &GranuleEntry,
        column_id: u32,
        stripe_id: u32,
        page_size_rows: usize,
    ) -> Option<BlockProduct<'a>> {
        let source_granule = source.footer.granules.iter().find(|granule| {
            granule.granule_id == entry.granule_id
                && granule.first_row_ordinal == entry.first_row_ordinal
                && granule.row_count == entry.row_count
        })?;
        let mark = source.footer.marks.iter().find(|mark| {
            mark.projection_id == 0 && mark.column_id == column_id && mark.granule_id == entry.granule_id
        })?;
        let expected_pages = if page_size_rows > 0 && entry.row_count as usize > page_size_rows {
            (entry.row_count as usize).div_ceil(page_size_rows) as u32
        } else {
            1
        };
        if mark.row_count != entry.row_count || mark.page_count != expected_pages {
            return None;
        }
        let stats = source
            .footer
            .page_stats
            .iter()
            .find(|stats| stats.column_id == column_id && stats.granule_id == entry.granule_id)?;
        let stats = BlockStats {
            max_f64: stats.max_f64,
            max_i128: stats.max_i128,
            min_f64: stats.min_f64,
            min_i128: stats.min_i128,
            null_count: stats.null_count,
            row_count: stats.row_count,
        };
        let source_stripe = source
            .footer
            .stripes
            .iter()
            .find(|stripe| stripe.stripe_id == source_granule.stripe_id)?;
        let start = usize::try_from(source_stripe.file_offset.checked_add(mark.compressed_offset)?).ok()?;
        let end = start.checked_add(usize::try_from(mark.compressed_size).ok()?)?;
        let stored = source.bytes.get(start..end)?;
        let (body, prefix) = if mark.compressed_size == 0 {
            (
                Cow::Borrowed(&[][..]),
                Cow::Owned(vec![crate::layout::presence_forms::EMPTY]),
            )
        } else {
            (Cow::Borrowed(stored), Cow::Borrowed(&[][..]))
        };
        let mut page_entries: Vec<_> = source
            .footer
            .page_directory
            .iter()
            .filter(|page| {
                page.projection_id == 0 && page.column_id == column_id && page.granule_id == entry.granule_id
            })
            .collect();
        page_entries.sort_by_key(|page| page.page_index);
        let pages = if mark.page_count > 1 {
            if page_entries.len() != mark.page_count as usize {
                return None;
            }
            page_entries
                .into_iter()
                .map(|page| {
                    Some((
                        page.compressed_len,
                        mark.codec_pipeline_id,
                        BlockStats {
                            max_f64: page.max_f64,
                            max_i128: page.max_i128,
                            min_f64: page.min_f64,
                            min_i128: page.min_i128,
                            null_count: page.null_count,
                            row_count: page.row_count,
                        },
                        u32::try_from(page.first_row_ordinal.checked_sub(entry.first_row_ordinal)?).ok()?,
                        page.row_count,
                    ))
                })
                .collect::<Option<Vec<_>>>()?
        } else {
            Vec::new()
        };

        let mut text_tokens: Vec<TextTokenEntry> = source
            .footer
            .text_token_indexes
            .iter()
            .filter(|token| token.column_id == column_id && token.granule_id == entry.granule_id)
            .cloned()
            .collect();
        for offsets in source
            .footer
            .text_token_offsets
            .iter()
            .filter(|token| token.column_id == column_id && token.granule_id == entry.granule_id)
        {
            let start = usize::try_from(source_stripe.file_offset.checked_add(offsets.index_offset)?).ok()?;
            let end = start.checked_add(usize::try_from(offsets.index_len).ok()?)?;
            text_tokens.push(TextTokenEntry {
                column_id,
                granule_id: entry.granule_id,
                index_bytes: source.bytes.get(start..end)?.to_vec(),
                page_index: offsets.page_index,
            });
        }
        Some(BlockProduct {
            fsst: None,
            piece: EncodedPiece {
                body,
                column_id,
                granule_id: entry.granule_id,
                pages,
                pipeline: mark.codec_pipeline_id,
                prefix,
                row_count: entry.row_count,
                stats,
                stripe_id,
                uncompressed_len: mark.uncompressed_size,
            },
            reused: true,
            text_tokens,
        })
    }

    // File-scope shared dictionary alphabets: a string column shares one alphabet only when every granule repeats
    // the same distinct value set (the status-like case), it fits the pinned budget, and the file has more than one
    // granule. Uniformity is what keeps stripes byte-stable: a block's codes then depend only on its own rows, so an
    // edit elsewhere in the file never re-encodes an untouched stripe and splice reuse keeps working.
    let reuse_source_alphabets = reuse_path_plan
        && update_reuse.is_some_and(|reuse| {
            !reuse.changed_columns_by_granule.values().any(|changed| {
                changed.iter().any(|column_id| {
                    shareable_column(*column_id)
                        && reuse
                            .source
                            .footer
                            .columns
                            .iter()
                            .any(|column| column.column_id == *column_id && column.kind == ColumnKind::String)
                })
            })
        });
    let mut shared_alphabets: BTreeMap<u32, Vec<String>> = if reuse_source_alphabets {
        update_reuse
            .into_iter()
            .flat_map(|reuse| &reuse.source.footer.shared_dictionaries)
            .map(|entry| (entry.column_id, entry.values.clone()))
            .collect()
    } else {
        BTreeMap::new()
    };
    if granules.len() > 1 && !reuse_source_alphabets {
        // Only the envelope, promoted/shredded, and context families share: their alphabets are closed value sets.
        // Free-text, embedding, and provenance columns carry prose or per-event material whose sets shift with any
        // edit, which would toggle sharing between otherwise-identical files and re-encode untouched stripes.
        // Borrows straight into the granule/analytical column data instead of cloning every distinct value: the
        // winning alphabet (if any) is cloned exactly once, below, instead of every observed value being cloned up
        // front. A column whose second granule already disagrees with its first is dropped from `granule_sets`
        // after that one comparison, so a file-wide non-shareable column (the common case for a high-cardinality
        // string) does not keep collecting sets from every remaining granule.
        fn observe_alphabet<'a>(
            column_id: u32,
            values: impl Iterator<Item = &'a str>,
            granule_sets: &mut BTreeMap<u32, Vec<std::collections::BTreeSet<&'a str>>>,
            diverged: &mut HashSet<u32>,
        ) {
            if diverged.contains(&column_id) {
                return;
            }
            let mut set: std::collections::BTreeSet<&'a str> = std::collections::BTreeSet::new();
            for value in values {
                if set.len() > SHARED_DICTIONARY_MAX_VALUES {
                    break;
                }
                set.insert(value);
            }
            let sets = granule_sets.entry(column_id).or_default();
            if sets.first().is_some_and(|first| *first != set) {
                diverged.insert(column_id);
                return;
            }
            sets.push(set);
        }
        let mut granule_sets: BTreeMap<u32, Vec<std::collections::BTreeSet<&str>>> = BTreeMap::new();
        let mut diverged: HashSet<u32> = HashSet::default();
        for granule in &granules {
            for block in &granule.blocks {
                if shareable_column(block.column_id)
                    && let ColumnData::Strings(values) = &block.data
                {
                    observe_alphabet(block.column_id, values.iter_present(), &mut granule_sets, &mut diverged);
                }
            }
        }
        for col in &config.analytical_columns {
            if shareable_column(col.column_id)
                && let ColumnData::Strings(values) = &col.data
            {
                for (g_start, g_end) in &granule_row_ranges {
                    let present = (*g_start..*g_end).filter_map(|index| values.get(index).flatten());
                    observe_alphabet(col.column_id, present, &mut granule_sets, &mut diverged);
                }
            }
        }
        for (column_id, sets) in granule_sets {
            let Some(first) = sets.first() else { continue };
            if sets.len() > 1
                && !first.is_empty()
                && first.len() <= SHARED_DICTIONARY_MAX_VALUES
                // The footer stores each value behind a u16 length prefix; a longer value cannot be represented,
                // so its column keeps block-local dictionaries.
                && first.iter().all(|value| value.len() <= u16::MAX as usize)
                && sets.iter().all(|set| set == first)
            {
                shared_alphabets.insert(column_id, first.iter().map(|value| (*value).to_owned()).collect());
            }
        }
    }

    // File-scope metadata is part of a block's decoding domain. Reuse is enabled only after the candidate has derived
    // the same schema, routing, dictionaries, granule geometry, and shared alphabets as the source. Any mismatch turns
    // this build into the ordinary full rewrite before one source extent is borrowed.
    let source_shared: BTreeMap<u32, Vec<String>> = update_reuse
        .map(|reuse| {
            reuse
                .source
                .footer
                .shared_dictionaries
                .iter()
                .map(|entry| (entry.column_id, entry.values.clone()))
                .collect()
        })
        .unwrap_or_default();
    let update_reuse = update_reuse.filter(|reuse| {
        reuse.source.footer.columns == columns
            && reuse.source.footer.dictionaries == dictionaries
            && reuse.source.footer.freetext == freetext_entries
            && reuse.source.footer.io_alignment_bytes == config.io_alignment_bytes
            && reuse.source.footer.presence == presence_entries
            && reuse.source.footer.shredded == shred_plan
            // `shared_alphabets` contains every eligible alphabet; the footer contains only alphabets an encoded
            // source block actually selected. Every source-used decoding domain must still exist byte-identically,
            // while a candidate-only eligible alphabet is irrelevant until one of its newly encoded blocks selects it.
            && source_shared
                .iter()
                .all(|(column_id, values)| shared_alphabets.get(column_id) == Some(values))
            && reuse.source.footer.granules.len() == granules.len()
            && reuse.source.footer.granules.iter().zip(&granules).all(|(source, candidate)| {
                source.granule_id == candidate.entry.granule_id
                    && source.first_row_ordinal == candidate.entry.first_row_ordinal
                    && source.row_count == candidate.entry.row_count
                    && source.stripe_id == candidate.entry.stripe_id
            })
    });
    // A sparse update that no longer matches its source — a rebuilt granule moved a stripe boundary — has no
    // block data for the granules it borrowed; a build holding every row starts over the ordinary way.
    if let Some(reuse) = sparse_reuse
        && update_reuse.is_none()
    {
        return sparse_fallback(rows, payloads, config, encode, sink, phase_clock, reuse);
    }

    // One encode job per (granule, block), fanned out through the injected executor: production hands the jobs to a
    // thread pool, deterministic simulation runs them sequentially, and either way the products are reassembled in
    // job order below, so the file bytes are identical however the jobs were scheduled.
    let job_list: Vec<(usize, u32)> = (0..granules.len())
        .flat_map(|granule_idx| block_columns.iter().map(move |&column_id| (granule_idx, column_id)))
        .collect();
    let slots: Vec<Mutex<Option<Result<BlockProduct, FormatError>>>> =
        job_list.iter().map(|_| Mutex::new(None)).collect();
    // Blocks encode in two phases so a distribution-stable column stops re-running candidate selection on every
    // block: segment-head granules (every REPLAY_SEGMENT_GRANULES-th) run full selection first, then the remaining
    // blocks replay their column's head capture, each re-arming full selection on the encoder's trip-wire. The phase
    // split is by granule index and every capture is a pure function of its head block, so any executor — serial or
    // threaded — produces byte-identical files. Requirement: "Encoding selection may capture and replay a winning
    // pipeline".
    let is_head = |granule_idx: usize| granule_idx % REPLAY_SEGMENT_GRANULES == 0;
    let (head_jobs, tail_jobs): (Vec<usize>, Vec<usize>) =
        (0..job_list.len()).partition(|idx| job_list.get(*idx).is_some_and(|(granule_idx, _)| is_head(*granule_idx)));
    let run_phase = |jobs: &[usize], captures: &HashMap<(u32, usize), ReplayCapture>| {
        encode.run_jobs(jobs.len(), &|phase_idx| {
            maybe_profile_block_encoding(profile_workers, || {
                let (Some(&(granule_idx, column_id)), Some(slot)) = (
                    jobs.get(phase_idx).and_then(|job_idx| job_list.get(*job_idx)),
                    jobs.get(phase_idx).and_then(|job_idx| slots.get(*job_idx)),
                ) else {
                    return;
                };
                let Some(granule) = granules.get(granule_idx) else {
                    return;
                };
                if let Some(product) = update_reuse.and_then(|reuse| {
                    (!reuse.block_changed(granule.entry.granule_id, column_id))
                        .then(|| {
                            reuse_block_piece(
                                reuse.source,
                                &granule.entry,
                                column_id,
                                granule.entry.stripe_id,
                                config.page_size_rows,
                            )
                        })
                        .flatten()
                }) {
                    *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Ok(product));
                    return;
                }
                // A sparse update built pieces only for the blocks its plan knew would be encoded.
                let product = match granule.blocks.iter().find(|block| block.column_id == column_id) {
                    Some(block) => Ok(encode_block_piece(
                        block,
                        &granule.entry,
                        granule.entry.stripe_id,
                        config.page_size_rows,
                        config.lifecycle.cascade_strategy(),
                        &searchable_text_columns,
                        captures.get(&(column_id, granule_idx / REPLAY_SEGMENT_GRANULES)),
                        shared_alphabets.get(&column_id).map(Vec::as_slice),
                    )),
                    None => Err(FormatError::Structural {
                        rule: "a sparse update encodes only blocks it built",
                    }),
                };
                *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(product);
            });
        });
    };
    run_phase(&head_jobs, &HashMap::default());
    // Captures from the head pieces: the head's chosen transform, the FSST table it trained, and its piece-level
    // decoded/raw sizes as the trip-wire reference. (Piece lengths include the presence prefix and page headers; head
    // and replayed blocks of one column frame alike, so the ratios compare like for like.)
    let mut captures: HashMap<(u32, usize), ReplayCapture> = HashMap::default();
    for &job_idx in &head_jobs {
        let Some(&(granule_idx, column_id)) = job_list.get(job_idx) else {
            continue;
        };
        let Some(block) = granules
            .get(granule_idx)
            .and_then(|granule| granule.blocks.iter().find(|block| block.column_id == column_id))
        else {
            continue;
        };
        let head = slots.get(job_idx).and_then(|slot| {
            let mut guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.as_mut().and_then(|product| product.as_mut().ok()).map(|product| {
                (
                    product.piece.pipeline,
                    product.piece.uncompressed_len,
                    product.fsst.take(),
                )
            })
        });
        if let Some((pipeline, decoded_len, fsst)) = head
            && let Some(capture) = ReplayCapture::from_head(&block.data, pipeline, decoded_len, fsst)
        {
            captures.insert((block.column_id, granule_idx / REPLAY_SEGMENT_GRANULES), capture);
        }
    }
    run_phase(&tail_jobs, &captures);

    let mut pieces: Vec<EncodedPiece<'_>> = Vec::with_capacity(slots.len());
    // One filter per searchable string column per granule, before any of them is split into pages.
    let mut text_token_indexes: Vec<TextTokenEntry> =
        Vec::with_capacity(searchable_text_columns.len().saturating_mul(granules.len()));
    let mut reused_source_block_bytes = 0u64;
    let mut reused_source_blocks = 0u64;
    for slot in slots {
        let product = slot
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expect("the executor runs every job")?;
        text_token_indexes.extend(product.text_tokens);
        if product.reused {
            reused_source_block_bytes += product.piece.len() as u64;
            reused_source_blocks += 1;
        }
        pieces.push(product.piece);
    }

    // The arenas are moved, not cloned: nothing reads them from `granules` again (only `entry` is used below),
    // and a payload-heavy build would otherwise hold a second copy of the whole payload region during layout.
    let mut payload_geometry: Vec<PayloadGeometry<'_>> = Vec::with_capacity(granules.len());
    for (granule_index, granule) in granules.iter_mut().enumerate() {
        let freetext_row_offsets = freetext_row_offsets_by_granule
            .get_mut(granule_index)
            .map(std::mem::take)
            .unwrap_or_default();
        let reused_geometry = update_reuse.and_then(|reuse| {
            if reuse.changed_payload_granules.contains(&granule.entry.granule_id) {
                return None;
            }
            let payload = reuse
                .source
                .footer
                .payload_granules
                .iter()
                .find(|payload| payload.granule_id == granule.entry.granule_id)?;
            let extent = |offset: u64, len: u64| source_extent(reuse.source, granule.entry.granule_id, offset, len);
            Some((
                extent(payload.dictionary_offset, payload.dictionary_len)?,
                extent(payload.offsets_offset, payload.offsets_len)?,
                extent(payload.residual_offset, payload.residual_len)?,
                payload.residual_compression,
            ))
        });
        if let Some((dictionary, offsets, residual, residual_compression)) = reused_geometry {
            payload_geometry.push(PayloadGeometry {
                granule_id: granule.entry.granule_id,
                dictionary: Cow::Borrowed(dictionary),
                freetext_row_offsets,
                offsets: Cow::Borrowed(offsets),
                residual: Cow::Borrowed(residual),
                residual_compression,
            });
            continue;
        }
        // A rewritten/compacted granule's residual arena is cold: compress it into seekable Zstd-3 frames when that
        // shrinks it, recording the choice per granule. Fresh publications keep residuals uncompressed for
        // offset-jump access.
        let (residual, residual_compression) = match config.lifecycle {
            BuildLifecycle::FreshPublication | BuildLifecycle::FreshPublicationWithoutTrailing => {
                (std::mem::take(&mut granule.residual_bytes), ResidualCompression::None)
            }
            BuildLifecycle::RewriteOrCompaction => compress_residual(std::mem::take(&mut granule.residual_bytes)),
        };
        payload_geometry.push(PayloadGeometry {
            granule_id: granule.entry.granule_id,
            dictionary: Cow::Owned(std::mem::take(&mut granule.dictionary_bytes)),
            freetext_row_offsets,
            offsets: Cow::Owned(std::mem::take(&mut granule.offsets_bytes)),
            residual: Cow::Owned(residual),
            residual_compression,
        });
    }

    // Analytical columns (context projections, embeddings): written as independent column blocks alongside the required
    // columns, one block per granule, using the same encoding pipeline. Fanned out through the same injected executor
    // as the required-column blocks above instead of encoding serially on the caller's thread.
    struct AnalyticalProduct {
        piece: EncodedPiece<'static>,
        text_token: Option<TextTokenEntry>,
    }
    let analytical_jobs: Vec<(usize, usize)> = (0..config.analytical_columns.len())
        .flat_map(|col_idx| (0..granules.len()).map(move |granule_idx| (col_idx, granule_idx)))
        .collect();
    let analytical_slots: Vec<Mutex<Option<AnalyticalProduct>>> =
        analytical_jobs.iter().map(|_| Mutex::new(None)).collect();
    encode.run_jobs(analytical_jobs.len(), &|job_idx| {
        maybe_profile_block_encoding(profile_workers, || {
            let (Some(&(col_idx, granule_idx)), Some(slot)) =
                (analytical_jobs.get(job_idx), analytical_slots.get(job_idx))
            else {
                return;
            };
            let (Some(col), Some(&(g_start, g_end)), Some(granule)) = (
                config.analytical_columns.get(col_idx),
                granule_row_ranges.get(granule_idx),
                granules.get(granule_idx),
            ) else {
                return;
            };
            let granule_data = slice_column_data(&col.data, g_start, g_end);
            let (prefix, body, pipeline, stats, uncompressed_len) = encode_mini_block(
                &granule_data,
                &[],
                false,
                config.lifecycle.cascade_strategy(),
                shared_alphabets.get(&col.column_id).map(Vec::as_slice),
            );
            let text_token = searchable_text_columns
                .get(&col.column_id)
                .and_then(|&include_ngrams| encoded_text_tokens(&granule_data, include_ngrams))
                .map(|index_bytes| TextTokenEntry {
                    column_id: col.column_id,
                    granule_id: granule.entry.granule_id,
                    index_bytes,
                    page_index: 0,
                });
            let piece = EncodedPiece {
                body: Cow::Owned(body),
                column_id: col.column_id,
                granule_id: granule.entry.granule_id,
                pages: Vec::new(),
                pipeline,
                prefix: Cow::Owned(prefix),
                row_count: granule.entry.row_count,
                stats,
                stripe_id: stripe_of_granule.get(granule_idx).copied().unwrap_or_default(),
                uncompressed_len,
            };
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some(AnalyticalProduct { piece, text_token });
        });
    });
    for slot in analytical_slots {
        let product = slot
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .expect("the executor runs every job");
        if let Some(text_token) = product.text_token {
            text_token_indexes.push(text_token);
        }
        pieces.push(product.piece);
    }
    let encoded_blocks = pieces.len() as u64 - reused_source_blocks;

    // The promotion plan is the column-heat measurement: promoted columns are precisely the columns whose planner
    // metadata justifies the NDV pass. Once block encoding has released its scratch, build NDV sketches, exact-count
    // reductions, clustering entries, and entity filters together in coarse granule batches on the same injected
    // executor. The results are held until layout/footer assembly consumes them.
    let hot_columns = column_ids::PROMOTED_BASE..column_ids::PROMOTED_BASE + config.promotion.columns.len() as u32;
    phase_clock.transition(BuildPhase::FooterConstruction);
    let mut scheduled_metadata = match source {
        Some(source) => borrow_footer_metadata(source)?,
        None => build_scheduled_footer_metadata(
            &granules,
            &dictionaries,
            &hot_columns,
            row_count as u64,
            encode,
            profile_workers,
        ),
    };
    if order == RowOrder::Entity {
        for entry in &mut scheduled_metadata.clustering {
            entry.sortedness_proof = Some(order.sortedness_proof());
        }
    }
    phase_clock.transition(BuildPhase::Layout);

    // The filters' bytes go into the data area inside each granule's owning stripe (below, beside the payload
    // arenas), with only their byte ranges recorded in the footer — so a cold open, which fetches the footer alone,
    // never pays for filter bytes. Group them per granule for the stripe loop.
    let mut text_tokens_by_granule: BTreeMap<u32, Vec<TextTokenEntry>> = BTreeMap::new();
    for entry in text_token_indexes {
        text_tokens_by_granule.entry(entry.granule_id).or_default().push(entry);
    }

    // No emitted column block may exceed the reader's per-page read bound: a single-page block is bounded by its own
    // byte length, a paged block by each page's byte length — the reader bounds `ColumnMark::compressed_size` and each
    // `PageDirectoryEntry::compressed_len` the same way and rejects the whole file refuse otherwise. Rejecting here
    // fails the build (recoverable) rather than publishing a file the reader cannot open after the backing journal has
    // been recycled (unrecoverable).
    for piece in &pieces {
        let oversize = if piece.pages.is_empty() {
            piece.len() as u64 > MAX_PAGE_BYTES
        } else {
            piece.pages.iter().any(|(page_bytes, ..)| *page_bytes > MAX_PAGE_BYTES)
        };
        if oversize {
            return Err(FormatError::Structural {
                rule: "column block exceeds the maximum page size",
            });
        }
    }

    // Layout class: automatic compact/wide crossover on estimated size.
    let estimated: usize = pieces.iter().map(EncodedPiece::len).sum::<usize>()
        + payload_geometry
            .iter()
            .map(|g| g.dictionary.len() + g.offsets.len() + g.residual.len())
            .sum::<usize>();
    let layout_class = if estimated < config.targets.min_bytes_for_wide {
        LayoutClass::Compact
    } else {
        LayoutClass::Wide
    };

    // One identity-hash membership filter per granule, so an entity-id point lookup probes small filters instead of
    // reading every granule's hash block. Their bytes join the text-token filters at each stripe's tail below; only
    // the byte ranges reach the footer, so a cold open never pays for a point index it is not using.
    let entity_hash_filters_by_granule = &scheduled_metadata.entity_hash_filters;
    match layout_class {
        // Compact: interleaved, granule-major. Wide: column-major sections.
        LayoutClass::Compact => {
            pieces.sort_by_key(|piece| (piece.stripe_id, piece.granule_id, piece.column_id));
        }
        LayoutClass::Wide => {
            pieces.sort_by_key(|piece| (piece.stripe_id, piece.column_id, piece.granule_id));
        }
    }

    // Granule entry lookup for per-page sequence/time stats.
    let granule_by_id: BTreeMap<u32, &GranuleEntry> = granules.iter().map(|g| (g.entry.granule_id, &g.entry)).collect();

    // Block boundaries pad to the declared IO granularity when the caller set one (so a reader can take an aligned
    // `O_DIRECT` read of a page), else to the default 64. Every stripe begins on such a boundary and is laid out in a
    // buffer of its own that opens at the stripe's base, so padding the buffer's length aligns the file-absolute
    // offset recorded in the footer just the same.
    let block_alignment = if config.io_alignment_bytes > 0 {
        (config.io_alignment_bytes as usize).max(64)
    } else {
        64
    };
    // Each granule's payload arena is written inside its owning stripe's byte range (see `lay_out` below), so the
    // stripe's BLAKE3 covers it. `payload_geometry[idx]` is the arena for the granule at position `idx`, whose stripe is
    // `stripe_of_granule[idx]`; this groups those indices per stripe.
    let mut payload_idx_by_stripe: BTreeMap<u32, Vec<usize>> = BTreeMap::new();
    for (idx, stripe) in stripe_of_granule.iter().enumerate() {
        payload_idx_by_stripe.entry(*stripe).or_default().push(idx);
    }
    // Pieces are sorted stripe-first, so a stripe's column blocks are one contiguous run.
    let mut stripe_piece_ranges: Vec<(u32, Range<usize>)> = Vec::new();
    for (idx, piece) in pieces.iter().enumerate() {
        match stripe_piece_ranges.last_mut() {
            Some((stripe_id, range)) if *stripe_id == piece.stripe_id => range.end = idx + 1,
            _ => stripe_piece_ranges.push((piece.stripe_id, idx..idx + 1)),
        }
    }

    /// One stripe's data-area bytes and every footer entry they gave rise to. Offsets inside are stripe-relative, as
    /// the file stores them, which is what lets the stripes be laid out side by side.
    struct LaidOutStripe {
        aliased_block_bytes: u64,
        /// The stripe's bytes, then the alignment gap in front of the next stripe (nothing after the last stripe), so
        /// the buffers concatenate into the data area exactly.
        bytes: Vec<u8>,
        crc: crate::file::integrity::StreamingCrc64Nvme,
        entity_hash_filters: Vec<EntityHashFilterEntry>,
        freetext_row_offsets: Vec<FreetextRowOffsets>,
        integrity: crate::file::integrity::BuiltOutboard,
        marks: Vec<ColumnMark>,
        marks_page_offsets: Vec<MarksPageEntry>,
        page_directory: Vec<PageDirectoryEntry>,
        page_minmax: Vec<PageMinMax>,
        page_stats: Vec<PageStats>,
        payload_granules: Vec<PayloadGranule>,
        /// Byte length of the stripe proper, without the trailing gap.
        stripe_len: u64,
        text_token_offsets: Vec<TextTokenOffsetsEntry>,
    }

    /// What every stripe's layout reads, shared read-only by the layout jobs.
    struct StripeLayout<'a, 'p> {
        block_alignment: usize,
        entity_hash_filters_by_granule: &'a BTreeMap<u32, Vec<u8>>,
        granule_by_id: &'a BTreeMap<u32, &'a GranuleEntry>,
        hot_columns: Range<u32>,
        payload_geometry: &'a [PayloadGeometry<'p>],
        payload_idx_by_stripe: &'a BTreeMap<u32, Vec<usize>>,
        pieces: &'a [EncodedPiece<'p>],
        profile_workers: bool,
        stripe_piece_ranges: &'a [(u32, Range<usize>)],
        text_tokens_by_granule: &'a BTreeMap<u32, Vec<TextTokenEntry>>,
    }

    impl StripeLayout<'_, '_> {
        /// Writes one stripe's column blocks, then its payload arenas, filters, and co-located marks pages, into a
        /// buffer of its own — keeping the whole stripe one contiguous, relocatable byte range — and hashes it while
        /// the bytes are still hot.
        fn lay_out(&self, index: usize) -> LaidOutStripe {
            let (stripe_id, piece_range) = self.stripe_piece_ranges.get(index).cloned().unwrap_or_default();
            let pieces = self.pieces.get(piece_range).unwrap_or_default();
            let geometry: Vec<&PayloadGeometry<'_>> = self
                .payload_idx_by_stripe
                .get(&stripe_id)
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .filter_map(|&idx| self.payload_geometry.get(idx))
                .collect();
            let text_tokens = |granule_id: u32| {
                self.text_tokens_by_granule
                    .get(&granule_id)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
            };
            let text_token_count: usize = geometry.iter().map(|g| text_tokens(g.granule_id).len()).sum();
            let entity_filter_count = geometry
                .iter()
                .filter(|g| self.entity_hash_filters_by_granule.contains_key(&g.granule_id))
                .count();
            // What the stripe holds: its blocks and arenas, every filter's bytes, at most one alignment gap in front
            // of each separately padded region, and its marks pages. Saturating throughout, so an implausible total
            // clamps rather than wrapping into a small reservation the writer would then grow out of.
            let estimated: usize = pieces.iter().map(EncodedPiece::len).sum::<usize>().saturating_add(
                geometry
                    .iter()
                    .map(|g| g.dictionary.len() + g.offsets.len() + g.residual.len())
                    .sum::<usize>(),
            );
            let filter_bytes: usize = geometry
                .iter()
                .map(|g| {
                    text_tokens(g.granule_id)
                        .iter()
                        .map(|entry| entry.index_bytes.len())
                        .sum::<usize>()
                        .saturating_add(
                            self.entity_hash_filters_by_granule
                                .get(&g.granule_id)
                                .map_or(0, Vec::len),
                        )
                })
                .sum();
            let padded_regions = pieces
                .len()
                .saturating_add(geometry.len().saturating_mul(PADDED_REGIONS_PER_GRANULE))
                .saturating_add(text_token_count)
                .saturating_add(entity_filter_count);
            // One mark and one granule-level statistics entry per piece; one page-directory entry per page, or one for
            // a piece that was not split into pages; per-page bounds only for the promoted columns.
            let page_entries: usize = pieces.iter().map(|piece| piece.pages.len().max(1)).sum();
            let hot_page_entries: usize = pieces
                .iter()
                .filter(|piece| self.hot_columns.contains(&piece.column_id))
                .map(|piece| piece.pages.len())
                .sum();
            let capacity = estimated
                .saturating_add(filter_bytes)
                .saturating_add(padded_regions.saturating_mul(self.block_alignment))
                .saturating_add(page_entries.saturating_mul(MARKS_PAGE_BYTES_PER_BLOCK));
            let mut data = Writer::with_capacity(capacity);
            let mut marks: Vec<ColumnMark> = Vec::with_capacity(pieces.len());
            let mut page_directory: Vec<PageDirectoryEntry> = Vec::with_capacity(page_entries);
            let mut page_minmax: Vec<PageMinMax> = Vec::with_capacity(hot_page_entries);
            let mut page_stats: Vec<PageStats> = Vec::with_capacity(pieces.len());
            let mut payload_granules: Vec<PayloadGranule> = Vec::with_capacity(geometry.len());
            let mut freetext_row_offsets: Vec<FreetextRowOffsets> =
                Vec::with_capacity(geometry.iter().map(|g| g.freetext_row_offsets.len()).sum());
            let mut text_token_offsets: Vec<TextTokenOffsetsEntry> = Vec::with_capacity(text_token_count);
            let mut entity_hash_filters: Vec<EntityHashFilterEntry> = Vec::with_capacity(entity_filter_count);
            let mut aliased_block_bytes = 0u64;
            // Byte-identical blocks within one stripe are stored once: later marks alias the first extent (marks are
            // `(offset, len)` pairs, so aliasing is already representable). Keyed by a fast non-cryptographic hash,
            // confirmed byte-exact before aliasing — a hash collision alone never aliases, so the key only needs to
            // bucket candidates cheaply. Deterministic, so identical rows still build identical bytes. Requirement:
            // "Marks may alias identical extents".
            let mut block_extent_by_hash: HashMap<u64, (u64, u64)> = HashMap::default();
            for piece in pieces {
                // Constant elision, governed by the `ELIDED_CONSTANT_BLOCKS` required bit: a dense single-page integer
                // block whose exact stats prove one value for every row stores no bytes — its zero-length mark plus the
                // block's stats reconstruct it. The leading zero presence length proves density; strings, floats,
                // decimals, sparse and multi-page blocks keep their bodies.
                let elided = piece.pages.is_empty()
                    && piece.row_count > 0
                    && piece.stats.null_count == 0
                    && piece.stats.row_count == piece.row_count
                    && matches!(piece.pipeline.value_kind(), Ok(ValueKind::I64 | ValueKind::U64))
                    && piece.stats.min_i128.is_some()
                    && piece.stats.min_i128 == piece.stats.max_i128
                    && piece.first_byte() == Some(crate::layout::presence_forms::EMPTY);
                let compressed_len = if elided { 0 } else { piece.len() as u64 };
                let relative_offset = if elided {
                    0
                } else {
                    let hash = piece.alias_key();
                    let alias = block_extent_by_hash.get(&hash).copied().filter(|&(rel, len)| {
                        let at = rel as usize;
                        let split = at + piece.prefix.len();
                        len == compressed_len
                            && data.bytes().get(at..split) == Some(piece.prefix.as_ref())
                            && data.bytes().get(split..at + len as usize) == Some(piece.body.as_ref())
                    });
                    if let Some((rel, _)) = alias {
                        aliased_block_bytes += compressed_len;
                        rel
                    } else {
                        data.pad_to(self.block_alignment);
                        // Stored offsets are stripe-relative under `STRIPE_RELATIVE_MARKS` (declared for every file
                        // this writer emits); the reader adds the stripe base back before slicing. The stripe directory
                        // and checksums keep the file-absolute base.
                        let rel = data.len() as u64;
                        piece.write_to(&mut data);
                        block_extent_by_hash.insert(hash, (rel, compressed_len));
                        rel
                    }
                };
                let page_count = if piece.pages.is_empty() {
                    1
                } else {
                    piece.pages.len() as u32
                };
                marks.push(ColumnMark {
                    codec_pipeline_id: piece.pipeline,
                    column_id: piece.column_id,
                    projection_id: 0,
                    granule_id: piece.granule_id,
                    compressed_offset: relative_offset,
                    compressed_size: compressed_len,
                    uncompressed_offset: 0,
                    uncompressed_size: piece.uncompressed_len,
                    row_count: piece.row_count,
                    page_count,
                    first_value_offset: None,
                });
                page_stats.push(PageStats {
                    column_id: piece.column_id,
                    granule_id: piece.granule_id,
                    max_f64: piece.stats.max_f64,
                    max_i128: piece.stats.max_i128,
                    min_f64: piece.stats.min_f64,
                    min_i128: piece.stats.min_i128,
                    null_count: piece.stats.null_count,
                    row_count: piece.stats.row_count,
                });
                if let Some(ge) = self.granule_by_id.get(&piece.granule_id) {
                    if piece.pages.is_empty() {
                        page_directory.push(PageDirectoryEntry {
                            column_id: piece.column_id,
                            compressed_len,
                            compressed_offset: relative_offset,
                            first_row_ordinal: ge.first_row_ordinal,
                            granule_id: piece.granule_id,
                            max_f64: piece.stats.max_f64,
                            max_i128: piece.stats.max_i128,
                            max_occurred_at_physical: ge.max_occurred_at_physical,
                            max_sequence: ge.last_sequence,
                            min_f64: piece.stats.min_f64,
                            min_i128: piece.stats.min_i128,
                            min_occurred_at_physical: ge.min_occurred_at_physical,
                            min_sequence: ge.first_sequence,
                            null_count: piece.stats.null_count,
                            page_index: 0,
                            projection_id: 0,
                            row_count: piece.row_count,
                        });
                    } else {
                        let mut page_offset = 0u64;
                        for (page_index, (page_len, _, stats, first_row_in_granule, row_count)) in
                            piece.pages.iter().enumerate()
                        {
                            page_directory.push(PageDirectoryEntry {
                                column_id: piece.column_id,
                                compressed_len: *page_len,
                                compressed_offset: relative_offset + page_offset,
                                first_row_ordinal: ge.first_row_ordinal + u64::from(*first_row_in_granule),
                                granule_id: piece.granule_id,
                                max_f64: stats.max_f64,
                                max_i128: stats.max_i128,
                                max_occurred_at_physical: ge.max_occurred_at_physical,
                                max_sequence: ge.last_sequence,
                                min_f64: stats.min_f64,
                                min_i128: stats.min_i128,
                                min_occurred_at_physical: ge.min_occurred_at_physical,
                                min_sequence: ge.first_sequence,
                                null_count: stats.null_count,
                                page_index: page_index as u32,
                                projection_id: 0,
                                row_count: *row_count,
                            });
                            if self.hot_columns.contains(&piece.column_id) {
                                page_minmax.push(PageMinMax {
                                    column_id: piece.column_id,
                                    granule_id: piece.granule_id,
                                    max_i128: stats.max_i128,
                                    min_i128: stats.min_i128,
                                    null_count: stats.null_count,
                                    page_index: page_index as u32,
                                    row_count: *row_count,
                                });
                            }
                            page_offset += *page_len;
                        }
                    }
                }
            }

            // This stripe's payload arenas (dictionary / offsets / residual per granule), written right after its
            // column blocks so they sit inside the stripe. Their offsets are stripe-relative like the marks, so
            // relocating the stripe rewrites only its one base-offset entry and no payload offset is touched.
            for geometry in &geometry {
                data.pad_to(self.block_alignment);
                let dictionary_offset = data.len() as u64;
                data.put_slice(&geometry.dictionary);
                data.pad_to(self.block_alignment);
                let offsets_offset = data.len() as u64;
                data.put_slice(&geometry.offsets);
                data.pad_to(self.block_alignment);
                let residual_offset = data.len() as u64;
                data.put_slice(&geometry.residual);
                payload_granules.push(PayloadGranule {
                    granule_id: geometry.granule_id,
                    dictionary_offset,
                    dictionary_len: geometry.dictionary.len() as u64,
                    offsets_offset,
                    offsets_len: geometry.offsets.len() as u64,
                    residual_offset,
                    residual_len: geometry.residual.len() as u64,
                    residual_compression: geometry.residual_compression,
                });
                // Per-row byte-offset index for declared free-text columns, written right after the residual arena so
                // it stays inside the same contiguous, checksummed stripe range. Stripe-relative like every other
                // payload-arena offset above.
                for (column_id, bytes, offsets) in &geometry.freetext_row_offsets {
                    data.pad_to(self.block_alignment);
                    let offsets_offset = data.len() as u64;
                    data.put_slice(offsets);
                    data.pad_to(self.block_alignment);
                    let bytes_offset = data.len() as u64;
                    data.put_slice(bytes);
                    freetext_row_offsets.push(FreetextRowOffsets {
                        bytes_len: bytes.len() as u64,
                        bytes_offset,
                        column_id: *column_id,
                        granule_id: geometry.granule_id,
                        offsets_len: offsets.len() as u64,
                        offsets_offset,
                    });
                }
            }

            // The stripe's text-token filters, grouped at the stripe tail — not scattered per granule between the
            // payload arenas — immediately followed by the stripe's co-located marks pages, so a surviving stripe's
            // filters and marks form one contiguous extent a single ranged IO fetches (requirement: "Granule directory
            // and authoritative marks", the permitted co-location). Both sit inside the stripe's byte range, so its
            // checksum covers them; only their stripe-relative byte ranges reach the footer.
            for geometry in &geometry {
                let granule_id = geometry.granule_id;
                for entry in text_tokens(granule_id) {
                    data.pad_to(self.block_alignment);
                    let index_offset = data.len() as u64;
                    data.put_slice(&entry.index_bytes);
                    text_token_offsets.push(TextTokenOffsetsEntry {
                        column_id: entry.column_id,
                        granule_id: entry.granule_id,
                        index_len: entry.index_bytes.len() as u64,
                        index_offset,
                        page_index: entry.page_index,
                    });
                }
                if let Some(filter) = self.entity_hash_filters_by_granule.get(&granule_id) {
                    data.pad_to(self.block_alignment);
                    let index_offset = data.len() as u64;
                    data.put_slice(filter);
                    entity_hash_filters.push(EntityHashFilterEntry {
                        granule_id,
                        index_len: filter.len() as u64,
                        index_offset,
                    });
                }
            }
            let (stripe_marks_directory, stripe_marks_blob) =
                encode_stripe_marks_pages(stripe_id, &marks, &page_directory);
            data.pad_to(self.block_alignment);
            let blob_offset = data.len() as u64;
            data.put_slice(&stripe_marks_blob);
            let marks_page_offsets = stripe_marks_directory
                .into_iter()
                .map(|entry| MarksPageEntry {
                    page_offset: blob_offset + entry.page_offset,
                    ..entry
                })
                .collect();

            let stripe_len = data.len() as u64;
            if index + 1 < self.stripe_piece_ranges.len() {
                data.pad_to(self.block_alignment);
            }
            let bytes = data.into_bytes();
            // One traversal produces both the stripe's ordinary BLAKE3 checksum and its Bao-style outboard proof nodes.
            // No later identity, seal, or proof-building pass reads these large bytes again.
            let hash = || {
                crate::file::integrity::build_outboard_tree_and_root(
                    bytes.get(..stripe_len as usize).unwrap_or_default(),
                    CHUNK_GROUP_BYTES,
                )
            };
            let integrity = if self.profile_workers {
                profile_integrity(hash)
            } else {
                hash()
            };
            // Absorbed while the bytes are still hot; composed behind the late-bound header once that is final.
            let mut crc = crate::file::integrity::StreamingCrc64Nvme::new();
            crc.update(&bytes);
            LaidOutStripe {
                aliased_block_bytes,
                bytes,
                crc,
                entity_hash_filters,
                freetext_row_offsets,
                integrity,
                marks,
                marks_page_offsets,
                page_directory,
                page_minmax,
                page_stats,
                payload_granules,
                stripe_len,
                text_token_offsets,
            }
        }
    }

    // One job per stripe on the injected executor: stripes are independent once their offsets are stripe-relative,
    // so each is laid out, its marks pages encoded, and its checksum taken beside the others instead of one after
    // another on this thread. The serial executor runs the same jobs in turn and builds the same bytes.
    let layout = StripeLayout {
        block_alignment,
        entity_hash_filters_by_granule,
        granule_by_id: &granule_by_id,
        hot_columns: hot_columns.clone(),
        payload_geometry: &payload_geometry,
        payload_idx_by_stripe: &payload_idx_by_stripe,
        pieces: &pieces,
        profile_workers,
        stripe_piece_ranges: &stripe_piece_ranges,
        text_tokens_by_granule: &text_tokens_by_granule,
    };
    let slots: Vec<Mutex<Option<LaidOutStripe>>> = stripe_piece_ranges.iter().map(|_| Mutex::new(None)).collect();
    encode.run_jobs(slots.len(), &|job_index| {
        let work = || layout.lay_out(job_index);
        let laid = if profile_workers { profile_layout(work) } else { work() };
        if let Some(slot) = slots.get(job_index) {
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(laid);
        }
    });
    let laid_out: Vec<LaidOutStripe> = slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .expect("the executor runs every layout job")
        })
        .collect();

    // Emit an alphabet only for columns whose pieces actually recorded the file scope; an unused alphabet would be
    // dead footer weight.
    let mut file_scope_columns: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for piece in &pieces {
        let recorded =
            std::iter::once(piece.pipeline).chain(piece.pages.iter().map(|(_, pipeline, _, _, _)| *pipeline));
        for pipeline in recorded {
            if pipeline.side_stream() == Ok(SideStream::FileScopeDictionary) {
                file_scope_columns.insert(piece.column_id);
            }
        }
    }
    // The stripes now hold every block byte, so the encoded pieces go before the file is assembled: the assembled
    // copy would otherwise sit beside them at the build's peak.
    drop(pieces);

    // Reduce granule geometry once, rather than rescanning every granule for every stripe. The number of stripes grows
    // with file size, so the old nested scan made this otherwise tiny metadata step quadratic on deliberately small
    // stripe targets.
    let mut stripe_rows: BTreeMap<u32, (u64, u64)> = BTreeMap::new();
    for (granule, stripe_id) in granules.iter().zip(&stripe_of_granule) {
        let entry = stripe_rows.entry(*stripe_id).or_insert((u64::MAX, 0));
        entry.0 = entry.0.min(granule.entry.first_row_ordinal);
        entry.1 += u64::from(granule.entry.row_count);
    }

    // Place the stripes: each lands where the one before it ended (that buffer's trailing gap included), which is the
    // block boundary the one-buffer layout padded to, so the concatenated buffers are the data area byte for byte.
    // The stripe directory, checksums, proof directory, and every footer entry follow in stripe order.
    let stripe_count = laid_out.len();
    let mut stripes: Vec<StripeEntry> = Vec::with_capacity(stripe_count);
    let mut stripe_checksums: Vec<[u8; 32]> = Vec::with_capacity(stripe_count);
    let mut stripe_proofs = Vec::with_capacity(stripe_count);
    let mut proof_trees: Vec<Vec<u8>> = Vec::with_capacity(stripe_count);
    let mut proof_tree_bytes_len = 0usize;
    let mut stripe_bytes: Vec<Vec<u8>> = Vec::with_capacity(stripe_count);
    let mut assembled_data_crc = crate::file::integrity::StreamingCrc64Nvme::new();
    let mut data_end = HEADER_BLOCK_LEN as u64;
    let mut aliased_block_bytes = 0u64;
    let mut marks: Vec<ColumnMark> = Vec::with_capacity(laid_out.iter().map(|laid| laid.marks.len()).sum());
    let mut page_directory: Vec<PageDirectoryEntry> =
        Vec::with_capacity(laid_out.iter().map(|laid| laid.page_directory.len()).sum());
    let mut page_minmax: Vec<PageMinMax> = Vec::with_capacity(laid_out.iter().map(|laid| laid.page_minmax.len()).sum());
    let mut page_stats: Vec<PageStats> = Vec::with_capacity(laid_out.iter().map(|laid| laid.page_stats.len()).sum());
    let mut payload_granules: Vec<PayloadGranule> = Vec::with_capacity(payload_geometry.len());
    let mut freetext_row_offsets: Vec<FreetextRowOffsets> =
        Vec::with_capacity(laid_out.iter().map(|laid| laid.freetext_row_offsets.len()).sum());
    let mut text_token_offsets: Vec<TextTokenOffsetsEntry> =
        Vec::with_capacity(laid_out.iter().map(|laid| laid.text_token_offsets.len()).sum());
    let mut entity_hash_filters: Vec<EntityHashFilterEntry> = Vec::with_capacity(entity_hash_filters_by_granule.len());
    let mut marks_page_offsets: Vec<MarksPageEntry> =
        Vec::with_capacity(laid_out.iter().map(|laid| laid.marks_page_offsets.len()).sum());
    for ((stripe_id, _), laid) in stripe_piece_ranges.iter().zip(laid_out) {
        let (first_row, row_count) = stripe_rows.get(stripe_id).copied().unwrap_or((u64::MAX, 0));
        stripes.push(StripeEntry {
            stripe_id: *stripe_id,
            file_offset: data_end,
            byte_len: laid.stripe_len,
            first_row_ordinal: if first_row == u64::MAX { 0 } else { first_row },
            row_count,
        });
        stripe_checksums.push(laid.integrity.root);
        let tree_len = laid.integrity.tree.as_deref().map_or(0, <[u8]>::len);
        stripe_proofs.push(StripeProofEntry {
            chunk_group_bytes: CHUNK_GROUP_BYTES as u32,
            stripe_id: *stripe_id,
            tree_len: tree_len as u64,
            tree_offset: proof_tree_bytes_len as u64,
        });
        proof_tree_bytes_len = proof_tree_bytes_len.saturating_add(tree_len);
        proof_trees.extend(laid.integrity.tree);
        assembled_data_crc.combine(&laid.crc);
        data_end += laid.bytes.len() as u64;
        aliased_block_bytes += laid.aliased_block_bytes;
        marks.extend(laid.marks);
        page_directory.extend(laid.page_directory);
        page_minmax.extend(laid.page_minmax);
        page_stats.extend(laid.page_stats);
        payload_granules.extend(laid.payload_granules);
        freetext_row_offsets.extend(laid.freetext_row_offsets);
        text_token_offsets.extend(laid.text_token_offsets);
        entity_hash_filters.extend(laid.entity_hash_filters);
        marks_page_offsets.extend(laid.marks_page_offsets);
        stripe_bytes.push(laid.bytes);
    }

    phase_clock.transition(BuildPhase::Integrity);
    // Alignment gaps are small but still authoritative. Compute them once now and record their leaves in the footer,
    // allowing a remote header+footer open to reconstruct the exact data partition without fetching padding bytes.
    // Each gap is the tail of the stripe buffer before it.
    let gaps: BTreeMap<usize, &[u8]> = stripes
        .iter()
        .zip(&stripe_bytes)
        .filter_map(|(stripe, bytes)| {
            let gap = bytes.get(stripe.byte_len as usize..)?;
            (!gap.is_empty()).then_some(((stripe.file_offset + stripe.byte_len) as usize, gap))
        })
        .collect();
    let data_commitments =
        crate::integrity::data_commitments_with(data_end as usize, &stripes, &stripe_checksums, |range| {
            gaps.get(&range.start).copied().filter(|gap| gap.len() == range.len())
        })?;
    let integrity_gaps = crate::integrity::integrity_gaps(&data_commitments);

    phase_clock.transition(BuildPhase::FileAssembly);
    // Streamed: every stripe is final, so its bytes leave now — before the footer is built — and the sink's write
    // overlaps the footer, seal, and trailer work below. The gaps were hashed above, so nothing reads them again.
    if let Some(sink) = sink.as_deref_mut() {
        for (stripe, bytes) in stripes.iter().zip(stripe_bytes.drain(..)) {
            sink(stripe.file_offset, bytes);
        }
    }

    phase_clock.transition(BuildPhase::FooterConstruction);
    let ScheduledFooterMetadata {
        clustering,
        entity_hash_filters: _,
        exact_counts,
        stripe_ndv,
    } = scheduled_metadata;

    let mut optional_flags = 0u64;
    if !shred_plan.is_empty() {
        optional_flags |= optional_features::VARIANT_SHREDDED_FIELD_BLOCKS;
    }
    if !freetext_entries.is_empty() {
        optional_flags |= optional_features::FREETEXT_COLUMNS;
    }
    if !freetext_row_offsets.is_empty() {
        optional_flags |= optional_features::TYPED_COLUMN_ROW_OFFSETS;
    }
    if !page_directory.is_empty() {
        optional_flags |= optional_features::PER_PAGE_MARKS;
    }
    if config.io_alignment_bytes > 0 {
        optional_flags |= optional_features::PAGE_IO_ALIGNMENT;
    }
    // No change of a sparse update touched an `occurred_at`, so whether the file carries a late event is whatever
    // the source recorded.
    let late_events = match source {
        Some(source) => source.footer.optional_feature_flags & optional_features::HEF_LATE_EVENTS != 0,
        None => granules_have_late_events(&granules),
    };
    if late_events {
        optional_flags |= optional_features::HEF_LATE_EVENTS;
    }
    if !text_token_offsets.is_empty() {
        optional_flags |= optional_features::TEXT_TOKEN_FILTER_OFFSETS;
    }
    if !entity_hash_filters.is_empty() {
        optional_flags |= optional_features::ENTITY_HASH_POINT_FILTERS;
    }

    // Schema fingerprint over the column directory.
    let mut schema_hash = blake3::Hasher::new();
    for column in &columns {
        schema_hash.update(&column.column_id.to_le_bytes());
        schema_hash.update(column.name.as_bytes());
        schema_hash.update(&[column.kind as u8, u8::from(column.nullable)]);
    }

    // Consumes `shared_alphabets` — no block encodes against it past this point — so each alphabet moves into the
    // footer entry rather than being duplicated string by string.
    let shared_dictionaries: Vec<SharedDictionaryEntry> = shared_alphabets
        .into_iter()
        .filter(|(column_id, _)| file_scope_columns.contains(column_id))
        .map(|(column_id, values)| SharedDictionaryEntry { column_id, values })
        .collect();

    let footer = Footer {
        clustering,
        columns,
        dictionaries,
        // The writer does not yet emit internal embedding/vector column blocks, so there is no per-row index to carry.
        embedding_row_offsets: Vec::new(),
        entity_hash_filters,
        // The writer emits only natively-known encodings, so no optional block needs a forward-compatibility escape
        // hatch.
        escape_hatches: Vec::new(),
        exact_counts,
        format_version: (1, 0),
        freetext: freetext_entries,
        freetext_row_offsets,
        granules: granules.iter().map(|granule| granule.entry).collect(),
        integrity_gaps,
        // The real applied alignment: the IO granularity the caller declared (and the writer padded every block to),
        // or zero when none was requested — in which case readers fall back to unaligned reads.
        io_alignment_bytes: config.io_alignment_bytes,
        marks,
        // `stripe_marks_pages` is declared (see `required_features::ALL`): each stripe's columnar marks pages sit in
        // the data area beside its filter bytes, so the footer carries only their directory; the in-footer columnar
        // pages fields stay empty.
        marks_directory: Vec::new(),
        marks_page_offsets,
        marks_pages: Vec::new(),
        optional_feature_flags: optional_flags,
        page_directory,
        page_minmax,
        page_stats,
        payload_granules,
        presence: presence_entries,
        required_feature_flags: required_features::ALL,
        schema_fingerprint: *schema_hash.finalize().as_bytes(),
        shared_dictionaries,
        shredded: shred_plan,
        sparse_keys,
        stripe_checksums,
        stripe_proofs,
        stripe_ndv,
        stripes,
        // Filter bytes live in the data area; the footer records only their byte ranges below.
        text_token_indexes: Vec::new(),
        text_token_offsets,
    };

    // The checksum/proof pass above produced the large-byte leaves shared by deterministic identity, the authoritative
    // seal, and verified streaming. None of these roots re-hashes a stripe payload.
    let file_id = crate::integrity::derive_file_id(
        config.tenant_id.uuid().as_u128().to_le_bytes(),
        first_key.0,
        last_key.0,
        first_key.1,
        last_key.1,
        row_count as u64,
        &footer.schema_fingerprint,
        layout_class as u8,
        &data_commitments,
    );

    let header = HefHeader {
        version_major: 1,
        version_minor: 0,
        file_id,
        tenant_id: config.tenant_id,
        generation_id: config.generation_id,
        layout_class,
        projection_count: 1,
        created_at_physical: config.created_at_physical,
        // Folded over the per-granule bounds `build_granule` already computed, instead of a fresh min/max pass over
        // every row for each of the four fields.
        min_occurred_at_physical: granules
            .iter()
            .map(|granule| granule.entry.min_occurred_at_physical)
            .min()
            .unwrap_or(0),
        max_occurred_at_physical: granules
            .iter()
            .map(|granule| granule.entry.max_occurred_at_physical)
            .max()
            .unwrap_or(0),
        min_ingested_at_physical: granules
            .iter()
            .map(|granule| granule.entry.min_ingested_at_physical)
            .min()
            .unwrap_or(0),
        max_ingested_at_physical: granules
            .iter()
            .map(|granule| granule.entry.max_ingested_at_physical)
            .max()
            .unwrap_or(0),
        min_epoch: first_key.0,
        max_epoch: last_key.0,
        min_sequence: first_key.1,
        max_sequence: last_key.1,
        row_count: row_count as u64,
        // The footer-encryption bit rides the header's required features so a reader gates it before decoding the
        // (now ciphertext) footer; kept out of `required_features::ALL`, it is set only for an encrypted footer.
        feature_flags: match config.footer_encryption {
            FooterEncryption::Encrypted => required_features::ALL | required_features::FOOTER_ENCRYPTED,
            FooterEncryption::Plaintext => required_features::ALL,
        },
        footer_pointer_hint: data_end,
    };

    let header_bytes = encode_header(&header);
    let footer_blob = crate::layout::footer::encode_footer(&footer);
    // Seal the footer under the caller's file DEK when the schema is sensitive, so the column directory and dictionaries
    // are opaque at rest. The sealed region (nonce + ciphertext) replaces the plaintext blob, and the length word below
    // records its encrypted length so a reader slices the right byte count.
    let footer_region = match config.footer_encryption {
        FooterEncryption::Encrypted => {
            let dek = config.footer_dek.ok_or(FormatError::Structural {
                rule: "encrypted footer requires a file DEK",
            })?;
            seal_footer(&dek, file_id, &footer_blob)?
        }
        FooterEncryption::Plaintext => footer_blob,
    };
    let mut trailer = (footer_region.len() as u64).to_le_bytes().to_vec();
    trailer.extend_from_slice(b"HEF1");
    // Tail geometry keeps the authoritative footer and non-authoritative proof appendix separately sized: a cold open
    // fetches both in one exact final-range request.
    let footer_len = footer_region.len() as u64 + 12;

    phase_clock.transition(BuildPhase::Integrity);
    let sealed_content_len = data_end + footer_region.len() as u64 + trailer.len() as u64;
    let file_seal = crate::integrity::derive_file_seal(
        &header_bytes,
        &footer_region,
        &trailer,
        sealed_content_len,
        &data_commitments,
    );

    // Proof nodes are non-authoritative bytes derived from authenticated stripe roots. They follow the HEF content in
    // one appendix located by the existing HEFT trailer; footer entries split that appendix back into per-stripe trees.
    let proof_appendix_len = if proof_tree_bytes_len == 0 {
        0
    } else {
        proof_tree_bytes_len.saturating_add(PROOF_APPENDIX_TRAILER_BYTES)
    };
    let proof_tree_len_bytes = (proof_tree_bytes_len as u64).to_le_bytes();
    let tree_len = (proof_appendix_len != 0).then_some(proof_appendix_len as u64);
    let total_len = sealed_content_len + proof_appendix_len as u64;

    // CRC-64/NVME remains the exact provider upload precheck. Each stripe absorbed its own bytes as it was laid out;
    // compose those behind the now-final header, then absorb each small tail buffer once. BLAKE3 stays authoritative
    // and domain-separated in `derive_file_seal` above.
    let mut crc = crate::file::integrity::StreamingCrc64Nvme::new();
    crc.update(&header_bytes);
    crc.combine(&assembled_data_crc);
    crc.update(&footer_region);
    crc.update(&trailer);
    for tree in &proof_trees {
        crc.update(tree);
    }
    if proof_appendix_len != 0 {
        crc.update(&proof_tree_len_bytes);
        crc.update(&TREE_TRAILER_MAGIC);
    }
    let file_crc64_nvme = crc.finalize();

    phase_clock.transition(BuildPhase::FileAssembly);
    // The file's tail after the data area, in file order: footer region, HEF trailer, then the proof appendix.
    let tail = [footer_region, trailer]
        .into_iter()
        .chain(proof_trees)
        .chain((proof_appendix_len != 0).then(|| proof_tree_len_bytes.to_vec()))
        .chain((proof_appendix_len != 0).then(|| TREE_TRAILER_MAGIC.to_vec()));
    let file = match sink.as_deref_mut() {
        // Streamed: the stripes already went out; the tail follows them, and the header — final only now that the
        // footer pointer and file id it carries are known — goes last, to the front of the file.
        Some(sink) => {
            let mut at = data_end;
            for bytes in tail {
                let len = bytes.len() as u64;
                sink(at, bytes);
                at += len;
            }
            sink(0, header_bytes);
            Vec::new()
        }
        // Only the unsinked path concatenates, and by here every segment's length is final, so the room for the whole
        // file is taken in one exact-size step. Each stripe buffer is released as soon as it has been copied.
        None => {
            let mut file = Vec::with_capacity(total_len as usize);
            file.extend_from_slice(&header_bytes);
            for bytes in stripe_bytes {
                file.extend_from_slice(&bytes);
            }
            for bytes in tail {
                file.extend_from_slice(&bytes);
            }
            file
        }
    };

    phase_clock.transition(BuildPhase::RowDestruction);
    // The caller transfers ownership of every payload tree to the build. Releasing a large batch serially was visible
    // on the create/update critical path even after all output bytes were complete; fan the independent tree drops
    // across the same worker pool the encoder already uses. Normalization moved the payload values into their own
    // arena, so both it and the rows that kept the rest are released the same way.
    payloads.release();
    rows.into_par_iter().for_each(drop);
    phase_clock.finish();

    Ok(BuiltHef {
        aliased_block_bytes,
        encoded_blocks,
        reused_source_block_bytes,
        reused_source_blocks,
        bytes: file,
        file_seal,
        file_crc64_nvme,
        file_id,
        footer,
        footer_len,
        header,
        total_len,
        tree_len,
    })
}

/// The kind each relationship column stores, in [`RELATIONSHIP_COLUMN_IDS`] order.
const RELATIONSHIP_KIND_COLUMNS: [RelationshipKind; 4] = [
    RelationshipKind::Parent,
    RelationshipKind::Root,
    RelationshipKind::Link,
    RelationshipKind::Related,
];

/// The relationship column ids in `RELATIONSHIP_COLUMNS` declaration order.
const RELATIONSHIP_COLUMN_IDS: [u32; 4] = [
    column_ids::PARENT_REF,
    column_ids::ROOT_REF,
    column_ids::LINKED_REFS,
    column_ids::RELATED_REFS,
];

/// Encodes one granule's payload arena: the key dictionary over every residual key, then each row's residual value —
/// or external reference — straight into the arena, with an `(offset, len)` entry per row (`(0, 0)` for a row that
/// stores nothing). Returns the dictionary, offsets table and arena bytes.
fn encode_residual(
    residuals: &[Option<Residual<'_>>],
    residual_fields: &[(&str, &VariantValue)],
    external_refs: &[Option<&str>],
    residual_value_bytes: usize,
    payloads: &NormalizedPayloads,
) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>), FormatError> {
    let row_count = residuals.len();
    // Granule variant dictionary: the residual keys, self-contained for the rows it governs. The names are gathered
    // borrowed and owned once for the granule, so a key every row carries is copied once rather than once per row.
    let mut key_names: HashSet<&str> = HashSet::with_capacity(payloads.identities().field_names().len());
    let mut nested_names: Vec<&str> = Vec::new();
    for residual in residuals.iter().flatten() {
        match residual {
            Residual::Fields(range) => {
                for (path, value) in residual_fields.get(range.clone()).unwrap_or_default() {
                    key_names.insert(path);
                    collect_key_names(value, &mut nested_names);
                }
            }
            Residual::Whole(value) => collect_key_names(value, &mut nested_names),
        }
        key_names.extend(nested_names.drain(..));
    }
    let dictionary = KeyDictionary::build(key_names.into_iter().map(str::to_owned));
    let dictionary_bytes = encode_variant_dictionary(&dictionary);

    // Residual block + offsets table (offset, len per row; (0,0) = none). Each row is encoded straight into the arena,
    // so no row pays for a buffer of its own.
    // Room for the whole arena taken once. The row loop measured the values that stayed; what the estimate adds is one
    // object header per row, so the arena is sized close to what it ends up holding instead of being copied to a wider
    // allocation a dozen times while it fills. Saturating: the estimate is bounded by the payloads already in memory,
    // and an over-long one must not wrap into a tiny reservation the writer would then grow out of.
    let residual_capacity = residual_value_bytes.saturating_add(row_count.saturating_mul(RESIDUAL_ROW_FRAMING_BYTES));
    let mut residual = Writer::with_capacity(residual_capacity);
    let mut offsets = Writer::with_capacity(row_count * 8);
    // Shared across every row's residual so the encode recursion's container buffers are cleared and reused instead
    // of allocated fresh per container per row.
    let mut scratch = EncodeScratch::new();
    for (row_residual, external) in residuals.iter().zip(external_refs.iter()) {
        match (row_residual, external) {
            (Some(value), _) => {
                let start = residual.len();
                match value {
                    Residual::Fields(range) => encode_object_fields_into_with_scratch(
                        residual_fields.get(range.clone()).unwrap_or_default(),
                        &dictionary,
                        &mut scratch,
                        &mut residual,
                    )?,
                    Residual::Whole(value) => {
                        encode_value_into_with_scratch(value, &dictionary, &mut scratch, &mut residual)?
                    }
                }
                offsets.put_u32(start as u32);
                offsets.put_u32((residual.len() - start) as u32);
            }
            (None, Some(reference)) => {
                offsets.put_u32(residual.len() as u32);
                offsets.put_u32(reference.len() as u32);
                residual.put_slice(reference.as_bytes());
            }
            (None, None) => {
                offsets.put_u32(0);
                offsets.put_u32(0);
            }
        }
    }
    Ok((dictionary_bytes, offsets.into_bytes(), residual.into_bytes()))
}

/// Builds one granule's typed pieces from its rows. `wanted` narrows the blocks to build to those columns — the
/// rest are still routed, so what moves out of the residual is unchanged, but never stored — and `build_residual`
/// says whether the payload arena, its dictionary and offsets are encoded at all; an update that borrows a granule's
/// arena from the source passes `false`. `None` and `true` build everything.
#[expect(clippy::too_many_arguments, reason = "single internal call site")]
fn build_granule(
    granule_id: u32,
    first_row_ordinal: u64,
    rows: &mut [BuildRow],
    granule_payloads: &[NormalizedPayload],
    dictionaries: &FileDictionaries,
    dictionary_index: &DictionaryIndex<'_>,
    promotion: &PromotionPlan,
    shred_plan: &[ShredEntry],
    shred_kinds: &HashMap<String, ColumnKind>,
    freetext: &[FreetextEntry],
    field_routes: &[FieldRoute],
    shape_routes: &ShapeRoutes,
    payloads: &NormalizedPayloads,
    freetext_row_offset_index: bool,
    carries_provenance: bool,
    carries_relationships: bool,
    wanted: Option<&HashSet<u32>>,
    build_residual: bool,
) -> Result<GranulePieces, FormatError> {
    let row_count = rows.len();
    let wants = |column_id: u32| wanted.is_none_or(|columns| columns.contains(&column_id));
    // The granule's own `(epoch, sequence)` bounds, taken by value up front: from the row loop on, the rows are on
    // loan to the borrowed residual and column values and cannot be looked at again.
    // The lowest and highest rather than the first and last row: the same thing for the primary `(epoch, sequence)`
    // order, and the true sequence bounds for the entity projection, whose granule rows interleave across entities.
    let (first_epoch, first_sequence) =
        rows.iter()
            .map(|row| (row.epoch, row.sequence))
            .min()
            .ok_or(FormatError::Structural {
                rule: "granules are non-empty",
            })?;
    let (last_epoch, last_sequence) =
        rows.iter()
            .map(|row| (row.epoch, row.sequence))
            .max()
            .ok_or(FormatError::Structural {
                rule: "granules are non-empty",
            })?;
    let mut epoch = Vec::with_capacity(row_count);
    let mut sequence = Vec::with_capacity(row_count);
    let mut stream_id = Vec::with_capacity(row_count);
    let mut stream_sequence = Vec::with_capacity(row_count);
    let mut occurred = Vec::with_capacity(row_count);
    let mut ingested = Vec::with_capacity(row_count);
    let mut source_id = Vec::with_capacity(row_count);
    let mut event_type_id = Vec::with_capacity(row_count);
    let mut entity_type_id = Vec::with_capacity(row_count);
    // Exact per-dictionary-id counts for this granule, filled alongside `source_id`/`event_type_id`/`entity_type_id`
    // below so the file-wide totals never re-derive the same dictionary ids in a second pass over every row.
    let mut by_source = vec![0u64; dictionaries.source.len()];
    let mut by_event_type = vec![0u64; dictionaries.event_type.len()];
    let mut by_entity_type = vec![0u64; dictionaries.entity_type.len()];
    let mut entity_hash_low = Vec::with_capacity(row_count);
    let mut entity_hash_high = Vec::with_capacity(row_count);
    let mut payload_ref = Vec::with_capacity(row_count);
    let mut flags = Vec::with_capacity(row_count);
    let mut schema_version = Vec::with_capacity(row_count);
    let mut event_id = Vec::with_capacity(row_count);
    // The identity columns copy their text straight out of the rows, so each one's exact arena size is known before a
    // byte is copied — one pass over lengths the row loop below then walks again warm, in place of three text buffers
    // doubling their way up to a granule's worth of identities.
    let (account_id_bytes, actor_id_bytes, entity_id_bytes) = rows.iter().fold((0, 0, 0), |totals, row| {
        let envelope = &row.envelope;
        (
            totals.0 + envelope.account_id.as_ref().map_or(0, String::len),
            totals.1 + envelope.actor_id.as_ref().map_or(0, String::len),
            totals.2 + envelope.entity_id.as_ref().map_or(0, String::len),
        )
    });
    let mut entity_id = StringColumn::with_capacity(row_count, entity_id_bytes);
    let mut actor_id = StringColumn::with_capacity(row_count, actor_id_bytes);
    let mut account_id = StringColumn::with_capacity(row_count, account_id_bytes);
    let mut actor_hash_low = Vec::with_capacity(row_count);
    let mut account_hash_low = Vec::with_capacity(row_count);
    let mut trace_hash_low = Vec::with_capacity(row_count);
    let mut dedupe_low = Vec::with_capacity(row_count);
    let mut dedupe_high = Vec::with_capacity(row_count);
    let mut payload_flags = Vec::with_capacity(row_count);

    // Promoted (copies), shredded and free-text (moves) columns.
    let mut promoted_values: Vec<(Vec<u8>, TypedColumn)> = promotion
        .columns
        .iter()
        .map(|promoted| (vec![0u8; row_count.div_ceil(8)], TypedColumn::for_kind(promoted.kind)))
        .collect();
    let mut shredded_values: Vec<(Vec<u8>, TypedColumn)> = shred_plan
        .iter()
        .map(|entry| {
            let kind = shred_kinds
                .get(entry.path.as_str())
                .copied()
                .unwrap_or(ColumnKind::String);
            (vec![0u8; row_count.div_ceil(8)], TypedColumn::for_kind(kind))
        })
        .collect();
    // The fixed decimal scale of each shredded column, established by the first decimal moved into it. Non-decimal
    // columns leave their slot `None`.
    let mut shred_column_scales: Vec<Option<u8>> = vec![None; shred_plan.len()];
    let mut freetext_values: Vec<(Vec<u8>, StringColumn)> = freetext
        .iter()
        .map(|_| (vec![0u8; row_count.div_ceil(8)], StringColumn::new()))
        .collect();

    // Signed-event provenance: one nullable slot per row, filled only for rows that carry a signature. Byte-valued
    // fields are stored in the protocol's own lowercase hex, so the canonical serialization rebuilds verbatim.
    // A signed row fills all six columns, so counting those rows up front is the exact length of every one of them —
    // and a file of unsigned events counts nothing, because it declares no provenance family at all.
    let signed_rows = if carries_provenance {
        rows.iter().filter(|row| row.provenance.is_some()).count()
    } else {
        0
    };
    let mut provenance_presence = vec![0u8; row_count.div_ceil(8)];
    let mut author_pubkey = StringColumn::with_capacity(signed_rows, 0);
    let mut signature = StringColumn::with_capacity(signed_rows, 0);
    let mut signature_scheme = StringColumn::with_capacity(signed_rows, 0);
    let mut protocol_event_id = StringColumn::with_capacity(signed_rows, 0);
    let mut protocol_kind: Vec<i64> = Vec::with_capacity(signed_rows);
    let mut claimed_at: Vec<i64> = Vec::with_capacity(signed_rows);

    // Relationship references: one column per kind, each with its own presence bitmap — an event may declare a parent
    // without a root or vice versa. Values are the canonical `<space>:<hex>` text an equality filter compares.
    let mut relationship_columns: [(Vec<u8>, StringColumn); 4] =
        std::array::from_fn(|_| (vec![0u8; row_count.div_ceil(8)], StringColumn::new()));

    // Residual values after moves, borrowed from the rows rather than copied out of them: the fields that stayed go
    // into one flat list for the whole granule and each row records its slice of it, so a row's residual costs no map
    // of its own, no copied key, and no copied value. Encoded against the granule dictionary below.
    //
    // The list is sized before it fills, from how many fields each row leaves behind. Whether a field stays is a
    // property of its shape rather than of the row — it stays unless its route moves it to a shredded or a free-text
    // column — so it is counted once per distinct shape and then summed over the granule's rows. A shredded field
    // whose value the typed column refuses stays too, so this is a close estimate rather than a ceiling, and being
    // short costs a single growth.
    let residual_fields_by_shape: Vec<usize> = payloads
        .identities()
        .shapes
        .iter()
        .map(|shape| {
            shape
                .iter()
                .filter(|&&field_id| {
                    field_routes
                        .get(field_id as usize)
                        .is_none_or(|route| route.shred_index.is_none() && route.freetext_index.is_none())
                })
                .count()
        })
        .collect();
    let residual_field_estimate: usize = granule_payloads
        .iter()
        .map(|&entry| {
            residual_fields_by_shape
                .get(payloads.shape_id(entry) as usize)
                .copied()
                .unwrap_or_default()
        })
        .sum();
    let mut residual_fields: Vec<(&str, &VariantValue)> = Vec::with_capacity(residual_field_estimate);
    // The residual values' own bytes, accumulated as the row loop decides what stays, so the arena below is sized
    // without a second walk of the payloads.
    let mut residual_value_bytes = 0usize;
    let mut residuals: Vec<Option<Residual>> = Vec::with_capacity(row_count);
    let mut external_refs: Vec<Option<&str>> = Vec::with_capacity(row_count);
    // Scratch buffers the row loop below reuses for hex/relationship text, instead of allocating a fresh `String`
    // per signed row's provenance fields or per relationship kind.
    let mut hex_scratch = String::new();
    let mut relationship_scratch = String::new();

    for (row_index, row) in rows.iter_mut().enumerate() {
        let envelope = &mut row.envelope;
        epoch.push(row.epoch);
        sequence.push(row.sequence);
        stream_id.push(envelope.stream_id.0);
        stream_sequence.push(envelope.stream_sequence);
        occurred.push(envelope.occurred_at.physical_nanos());
        ingested.push(envelope.ingested_at.physical_nanos());
        let this_source_id = dictionary_id(&dictionary_index.source, &envelope.source);
        let this_event_type_id = dictionary_id(&dictionary_index.event_type, &envelope.event_type);
        let this_entity_type_id = dictionary_id(&dictionary_index.entity_type, &envelope.entity_type);
        if let Some(count) = by_source.get_mut(this_source_id as usize) {
            *count += 1;
        }
        if let Some(count) = by_event_type.get_mut(this_event_type_id as usize) {
            *count += 1;
        }
        if let Some(count) = by_entity_type.get_mut(this_entity_type_id as usize) {
            *count += 1;
        }
        source_id.push(this_source_id);
        event_type_id.push(this_event_type_id);
        entity_type_id.push(this_entity_type_id);
        entity_hash_low.push(envelope.entity_id_hash_low);
        entity_hash_high.push(envelope.entity_id_hash_high);
        payload_ref.push(first_row_ordinal + row_index as u64);
        flags.push(u64::from(envelope.flags.0));
        schema_version.push(u64::from(envelope.schema_version));
        event_id.push(envelope.event_id.uuid().as_u128());
        // Moved, not cloned: the rows are owned by the build, nothing reads these identity strings after this loop,
        // and the precomputed identity hashes stay behind for the later passes. A block that is not wanted leaves
        // its strings in place, for a build that may yet start over from the rows.
        if wants(column_ids::ENTITY_ID) {
            entity_id.push(envelope.entity_id.take().as_deref());
        }
        if wants(column_ids::ACTOR_ID) {
            actor_id.push(envelope.actor_id.take().as_deref());
        }
        if wants(column_ids::ACCOUNT_ID) {
            account_id.push(envelope.account_id.take().as_deref());
        }
        actor_hash_low.push(envelope.actor_id_hash_low);
        account_hash_low.push(envelope.account_id_hash_low);
        trace_hash_low.push(envelope.trace_id_hash_low);
        dedupe_low.push(envelope.dedupe_hash_low);
        dedupe_high.push(envelope.dedupe_hash_high);

        if let Some(signed) = &row.provenance {
            if let Some(byte) = provenance_presence.get_mut(row_index / 8) {
                *byte |= 1 << (row_index % 8);
            }
            hex_scratch.clear();
            hex_lower_into(&signed.author_pubkey, &mut hex_scratch);
            author_pubkey.push(Some(&hex_scratch));
            hex_scratch.clear();
            hex_lower_into(&signed.signature, &mut hex_scratch);
            signature.push(Some(&hex_scratch));
            signature_scheme.push(Some(signed.scheme.as_str()));
            hex_scratch.clear();
            hex_lower_into(&signed.protocol_event_id, &mut hex_scratch);
            protocol_event_id.push(Some(&hex_scratch));
            protocol_kind.push(i64::from(signed.protocol_kind));
            claimed_at.push(signed.claimed_at.physical_nanos());
        }

        if let Some(relationships) = &row.relationships {
            for (slot, kind) in relationship_columns.iter_mut().zip(RELATIONSHIP_KIND_COLUMNS) {
                if relationships.column_text_into(kind, &mut relationship_scratch) {
                    if let Some(byte) = slot.0.get_mut(row_index / 8) {
                        *byte |= 1 << (row_index % 8);
                    }
                    slot.1.push(Some(&relationship_scratch));
                }
            }
        }

        let entry = granule_payloads
            .get(row_index)
            .copied()
            .unwrap_or(NormalizedPayload::None);
        match entry {
            NormalizedPayload::None => {
                payload_flags.push(0u64);
                residuals.push(None);
                external_refs.push(None);
            }
            NormalizedPayload::ExternalRef => {
                payload_flags.push(u64::from(PAYLOAD_FLAG_EXTERNAL_REF));
                residuals.push(None);
                // Normalization leaves the reference text in the row, which is why it is read from there rather than
                // from the value arena.
                external_refs.push(match &row.payload {
                    BuildPayload::ExternalRef(reference) => Some(reference.as_str()),
                    _ => None,
                });
            }
            NormalizedPayload::Whole { .. } => {
                payload_flags.push(0u64);
                external_refs.push(None);
                let Some(value) = payloads.values(entry).first() else {
                    residuals.push(None);
                    continue;
                };
                residual_value_bytes = residual_value_bytes.saturating_add(variant_value_bytes(value));
                residuals.push(Some(Residual::Whole(value)));
            }
            NormalizedPayload::Object { shape_id, .. } => {
                payload_flags.push(0u64);
                external_refs.push(None);
                // One pass over the fields decides where each lands: a shredded or free-text field moves to its typed
                // column, a promoted field is copied to its column and also kept, and everything else stays in the
                // residual. Nothing is copied — a field that stays is recorded by name and by reference.
                //
                // Where each of this row's fields goes was settled once for its whole shape, before any granule
                // started: no path hash and no string comparison remain here, and a field that stays in the
                // residual carries `FIELD_STAYS_RESIDUAL`, which no route can be found under. The names come from the
                // shape's own field ids, so a residual field is recorded without touching a per-row key string.
                let field_values = payloads.values(entry);
                let shape_field_ids = payloads.identities().shape(shape_id);
                let shape_field_routes = shape_routes.field_routes(shape_id);
                debug_assert_eq!(field_values.len(), shape_field_routes.len());
                let start = residual_fields.len();
                for ((field_value, &name_id), &field_id) in
                    field_values.iter().zip(shape_field_ids).zip(shape_field_routes)
                {
                    let path = payloads.field_name(name_id);
                    let Some(route) = field_routes.get(field_id as usize) else {
                        residual_value_bytes = residual_value_bytes.saturating_add(variant_value_bytes(field_value));
                        residual_fields.push((path, field_value));
                        continue;
                    };
                    // Promoted columns are acceleration copies: the payload stays complete in residual + shredded
                    // blocks. A path may be promoted more than once, under different `since_schema_version`s.
                    for &index in &route.promoted_indices {
                        if wants(column_ids::PROMOTED_BASE + index as u32)
                            && let Some(promoted) = promotion.columns.get(index)
                            && promoted_present(promoted, envelope.schema_version)
                            && let Some((bitmap, values)) = promoted_values.get_mut(index)
                            && values.push_variant(field_value)
                        {
                            if let Some(byte) = bitmap.get_mut(row_index / 8) {
                                *byte |= 1 << (row_index % 8);
                            }
                        }
                    }
                    // Shredded paths move: the value lives in exactly one of the typed column or the residual.
                    if let Some(index) = route.shred_index
                        && let Some(scale) = shred_column_scales.get_mut(index)
                        && let Some((bitmap, values)) = shredded_values.get_mut(index)
                        && values.push_variant_shred(field_value, scale)
                    {
                        if let Some(byte) = bitmap.get_mut(row_index / 8) {
                            *byte |= 1 << (row_index % 8);
                        }
                        continue;
                    }
                    // Free-text fields move by declaration.
                    if let Some(index) = route.freetext_index
                        && let VariantValue::String(text) = field_value
                    {
                        if freetext.get(index).is_some_and(|entry| wants(entry.column_id))
                            && let Some((bitmap, values)) = freetext_values.get_mut(index)
                        {
                            if let Some(byte) = bitmap.get_mut(row_index / 8) {
                                *byte |= 1 << (row_index % 8);
                            }
                            values.push(Some(text));
                        }
                        continue;
                    }
                    residual_value_bytes = residual_value_bytes.saturating_add(variant_value_bytes(field_value));
                    residual_fields.push((path, field_value));
                }
                residuals.push(Some(Residual::Fields(start..residual_fields.len())));
            }
        }
    }

    // Per free-text column, the per-row byte-offset index: the raw row bytes plus a dense `(offset, len)` entry for
    // every row (an absent row gets `(0, 0)`, mirroring the residual arena's own offsets table), so a point read
    // resolves one row without decoding the column's compressed block. Walked out of the values and presence bits the
    // row loop already collected, so no row buffers its text a second time. No column is indexed unless the build
    // opted in — the index restates, uncompressed, values the column block already holds and can itself address per
    // value — so an empty slice here leaves the loop below with nothing to walk.
    let indexed_freetext: &[FreetextEntry] = if freetext_row_offset_index { freetext } else { &[] };
    let mut freetext_row_offsets: Vec<(u32, Vec<u8>, Vec<u8>)> = Vec::with_capacity(indexed_freetext.len());
    for (entry, (presence, values)) in indexed_freetext
        .iter()
        .zip(freetext_values.iter())
        .filter(|(entry, _)| wants(entry.column_id))
    {
        let mut bytes = Writer::with_capacity(values.text_len());
        let mut offsets = Writer::with_capacity(row_count * 8);
        let mut next_value = 0usize;
        for row_index in 0..row_count {
            let present = presence
                .get(row_index / 8)
                .is_some_and(|byte| byte & (1 << (row_index % 8)) != 0);
            let text = if present {
                let text = values.get(next_value).flatten();
                next_value += 1;
                text
            } else {
                None
            };
            let Some(text) = text else {
                offsets.put_u32(0);
                offsets.put_u32(0);
                continue;
            };
            // The per-row offsets are 32-bit; an arena past that range would wrap the cast and point later rows at the
            // wrong bytes. The stripe clamp normally cuts long before this, but the clamp is configurable, so the
            // truncation is refused outright rather than corrupting the index.
            if bytes.len() > u32::MAX as usize || text.len() > u32::MAX as usize {
                return Err(FormatError::Structural {
                    rule: "free-text arena exceeds the u32 per-row offset range",
                });
            }
            // A present empty string has the same zero length as an absent value, so presence is carried in the
            // offset: the field was moved out of the residual payload, and reading it back as absent would lose it for
            // good.
            let offset = if text.is_empty() {
                EMPTY_VALUE_ROW_OFFSET
            } else {
                bytes.len() as u32
            };
            offsets.put_u32(offset);
            offsets.put_u32(text.len() as u32);
            bytes.put_slice(text.as_bytes());
        }
        freetext_row_offsets.push((entry.column_id, bytes.into_bytes(), offsets.into_bytes()));
    }

    let (dictionary_bytes, offsets_bytes, residual_bytes) = if build_residual {
        encode_residual(
            &residuals,
            &residual_fields,
            &external_refs,
            residual_value_bytes,
            payloads,
        )?
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    // Whether this granule alone carries a late event (an inversion between two of its own rows' `occurred_at`,
    // checked in ingest order exactly like `file_has_late_events`, but over the `occurred` values this granule
    // already collected above instead of re-reading the rows). A late event whose inversion instead crosses a
    // granule boundary is caught by the file-wide fold over `min_occurred_at_physical`/running-max in the caller.
    let mut max_occurred_so_far = i64::MIN;
    let mut has_internal_late_event = false;
    for &occurred_at in &occurred {
        if occurred_at < max_occurred_so_far {
            has_internal_late_event = true;
        }
        max_occurred_so_far = max_occurred_so_far.max(occurred_at);
    }

    let entry = GranuleEntry {
        granule_id,
        stripe_id: 0, // patched by the stripe assignment
        first_row_ordinal,
        row_count: row_count as u32,
        first_epoch,
        first_sequence,
        last_epoch,
        last_sequence,
        min_occurred_at_physical: occurred.iter().copied().min().unwrap_or(0),
        max_occurred_at_physical: occurred.iter().copied().max().unwrap_or(0),
        min_ingested_at_physical: ingested.iter().copied().min().unwrap_or(0),
        max_ingested_at_physical: ingested.iter().copied().max().unwrap_or(0),
        compressed_bytes_estimate: residual_bytes.len() as u64,
    };

    // Every block this granule will hold is already known: the required columns below, one per promoted, shredded and
    // free-text column, and the two optional families the file declared. Taking that room once keeps the vector from
    // being copied to a wider allocation part-way through filling it.
    let required_blocks = [
        PendingBlock {
            column_id: column_ids::EPOCH,
            presence: Vec::new(),
            data: ColumnData::U64(epoch),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::SEQUENCE,
            presence: Vec::new(),
            data: ColumnData::U64(sequence),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::STREAM_ID,
            presence: Vec::new(),
            data: ColumnData::U64(stream_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::STREAM_SEQUENCE,
            presence: Vec::new(),
            data: ColumnData::U64(stream_sequence),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::OCCURRED_AT,
            presence: Vec::new(),
            data: ColumnData::I64(occurred),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::INGESTED_AT,
            presence: Vec::new(),
            data: ColumnData::I64(ingested),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::SOURCE_ID,
            presence: Vec::new(),
            data: ColumnData::U64(source_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::EVENT_TYPE_ID,
            presence: Vec::new(),
            data: ColumnData::U64(event_type_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ENTITY_TYPE_ID,
            presence: Vec::new(),
            data: ColumnData::U64(entity_type_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ENTITY_ID_HASH_LOW,
            presence: Vec::new(),
            data: ColumnData::U64(entity_hash_low),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ENTITY_ID_HASH_HIGH,
            presence: Vec::new(),
            data: ColumnData::U64(entity_hash_high),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::PAYLOAD_REF,
            presence: Vec::new(),
            data: ColumnData::U64(payload_ref),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::FLAGS,
            presence: Vec::new(),
            data: ColumnData::U64(flags),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::SCHEMA_VERSION,
            presence: Vec::new(),
            data: ColumnData::U64(schema_version),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::EVENT_ID,
            presence: Vec::new(),
            data: ColumnData::U128(event_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ENTITY_ID,
            presence: Vec::new(),
            data: ColumnData::Strings(entity_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ACTOR_ID,
            presence: Vec::new(),
            data: ColumnData::Strings(actor_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ACCOUNT_ID,
            presence: Vec::new(),
            data: ColumnData::Strings(account_id),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ACTOR_ID_HASH_LOW,
            presence: Vec::new(),
            data: ColumnData::U64(actor_hash_low),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::ACCOUNT_ID_HASH_LOW,
            presence: Vec::new(),
            data: ColumnData::U64(account_hash_low),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::TRACE_ID_HASH_LOW,
            presence: Vec::new(),
            data: ColumnData::U64(trace_hash_low),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::DEDUPE_HASH_LOW,
            presence: Vec::new(),
            data: ColumnData::U64(dedupe_low),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::DEDUPE_HASH_HIGH,
            presence: Vec::new(),
            data: ColumnData::U64(dedupe_high),
            random_access: false,
        },
        PendingBlock {
            column_id: column_ids::PAYLOAD_FLAGS,
            presence: Vec::new(),
            data: ColumnData::U64(payload_flags),
            random_access: false,
        },
    ];
    let mut blocks = Vec::with_capacity(
        required_blocks.len()
            + promoted_values.len()
            + shred_plan.len()
            + freetext.len()
            + if carries_provenance {
                PROVENANCE_COLUMNS.len()
            } else {
                0
            }
            + if carries_relationships {
                relationship_columns.len()
            } else {
                0
            },
    );
    blocks.extend(required_blocks);
    for (index, (bitmap, values)) in promoted_values.into_iter().enumerate() {
        blocks.push(PendingBlock {
            column_id: column_ids::PROMOTED_BASE + index as u32,
            presence: bitmap,
            data: values.into_column_data()?,
            random_access: false,
        });
    }
    for (entry, (bitmap, values)) in shred_plan.iter().zip(shredded_values) {
        blocks.push(PendingBlock {
            column_id: entry.column_id,
            presence: bitmap,
            data: values.into_shredded_column_data()?,
            // Shredded scan-path columns preserve random access.
            random_access: true,
        });
    }
    for (entry, (bitmap, values)) in freetext.iter().zip(freetext_values) {
        blocks.push(PendingBlock {
            column_id: entry.column_id,
            presence: bitmap,
            data: ColumnData::Strings(values),
            // Free text is point-read a row at a time, so its block keeps random access too: whole-block compression
            // would put a whole-granule decompress in front of every single-row read.
            random_access: true,
        });
    }

    if carries_provenance {
        for (column_id, data) in [
            (column_ids::AUTHOR_PUBKEY, ColumnData::Strings(author_pubkey)),
            (column_ids::SIGNATURE, ColumnData::Strings(signature)),
            (column_ids::SIGNATURE_SCHEME, ColumnData::Strings(signature_scheme)),
            (column_ids::PROTOCOL_EVENT_ID, ColumnData::Strings(protocol_event_id)),
            (column_ids::PROTOCOL_KIND, ColumnData::I64(protocol_kind)),
            (column_ids::CLAIMED_AT, ColumnData::I64(claimed_at)),
        ] {
            blocks.push(PendingBlock {
                column_id,
                presence: provenance_presence.clone(),
                data,
                random_access: true,
            });
        }
    }

    if carries_relationships {
        for ((presence, values), column_id) in relationship_columns.into_iter().zip(RELATIONSHIP_COLUMN_IDS) {
            blocks.push(PendingBlock {
                column_id,
                presence,
                data: ColumnData::Strings(values),
                random_access: true,
            });
        }
    }

    if wanted.is_some() {
        blocks.retain(|block| wants(block.column_id));
    }

    Ok(GranulePieces {
        entry,
        blocks,
        by_entity_type,
        by_event_type,
        by_source,
        dictionary_bytes,
        freetext_row_offsets,
        has_internal_late_event,
        offsets_bytes,
        residual_bytes,
    })
}

#[cfg(test)]
#[path = "test/build.rs"]
mod tests;
