//! The self-describing block of metadata at the tail of a stored file — its column list, the directories that map data
//! to disk offsets, the exact counts, and the feature flags — written so older readers can skip what they do not
//! understand.
//!
//! Layout: `[preamble][section directory][section bytes…][footer_len u64]["HEF1"]`. The section directory doubles as
//! the checksum directory: every section carries its BLAKE3, and a reader validates a section's checksum before using
//! the feature it backs. Unknown section ids are ignorable extension blocks; whether the *file* is readable is governed
//! by the required-feature flags in the preamble, which refuse.

use super::required_features;
use crate::compat::EscapeHatch;
use crate::encoding::{ColumnData, PipelineId, decode_block, encode_block};
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer, slice};
use hashbrown::HashMap;
use std::collections::BTreeMap;
use std::ops::Range;

/// Section ids. New ids are forward-compatible extension blocks: readers ignore ids they do not know.
pub mod sections {
    pub const CLUSTERING_METADATA: u32 = 16;
    pub const COLUMNS: u32 = 1;
    pub const DICTIONARIES: u32 = 5;
    /// Optional: per-row byte-offset index for internal embedding/vector columns. Absent in files that do not declare
    /// `TYPED_COLUMN_ROW_OFFSETS`.
    pub const EMBEDDING_ROW_OFFSETS: u32 = 19;
    /// Optional: byte ranges of the per-granule identity-hash membership filters stored in the data area, so a
    /// lookup for one entity id reads only the granules that could hold it and a cold open never fetches the filter
    /// bytes. Absent in files that do not declare `ENTITY_HASH_POINT_FILTERS`.
    pub const ENTITY_HASH_FILTERS: u32 = 27;
    pub const ESCAPE_HATCHES: u32 = 13;
    pub const EXACT_COUNTS: u32 = 7;
    /// Optional: the file's external-id index, sorted `(id hash, row ordinal)` pairs. Absent in files whose rows carry
    /// no external id.
    pub const EXTERNAL_IDS: u32 = 28;
    pub const FREETEXT: u32 = 11;
    /// Optional: per-row byte-offset index for declared free-text columns. Absent in files that do not declare
    /// `TYPED_COLUMN_ROW_OFFSETS`.
    pub const FREETEXT_ROW_OFFSETS: u32 = 18;
    pub const GRANULES: u32 = 3;
    /// Exact commitments for non-stripe byte ranges between the fixed header and footer. These are normally alignment
    /// padding; recording their roots lets a cold reader authenticate the complete segment partition without fetching
    /// padding bytes.
    pub const INTEGRITY_GAPS: u32 = 26;
    pub const IO_ALIGNMENT: u32 = 15;
    pub const MARKS: u32 = 4;
    /// Optional: per-page byte-range directory within each column mark. Absent in files that do not declare
    /// `PER_PAGE_MARKS`.
    pub const PAGE_DIRECTORY: u32 = 14;
    pub const PAGE_MINMAX: u32 = 17;
    pub const PAGE_STATS: u32 = 6;
    pub const PAYLOAD_GRANULES: u32 = 8;
    pub const PRESENCE: u32 = 9;
    /// Optional: per-(relationship column, granule) membership filters over the stored references. Present in every
    /// file that carries relationship columns written since the filters existed; absent otherwise, and a reader then
    /// scans every granule.
    pub const REFERENCE_FILTERS: u32 = 29;
    pub const SHREDDED: u32 = 10;
    /// Optional: one file-scope dictionary alphabet per string column that shares it, under the
    /// `shared_dictionaries` required feature — blocks recording the file dictionary scope resolve their codes here.
    pub const SHARED_DICTIONARIES: u32 = 24;
    /// Optional: the sparse shredded key set — payload paths stored as sparse columns under the
    /// `sparse_shredded_columns` required feature, bounded per file by the writer's pinned key budget.
    pub const SPARSE_KEYS: u32 = 23;
    pub const STRIPE_CHECKSUMS: u32 = 12;
    /// Per-stripe outboard proof-tree geometry. The proof bytes themselves follow the HEF content as non-authoritative
    /// metadata; these authenticated entries bind each byte range to its stripe checksum root.
    pub const STRIPE_PROOFS: u32 = 25;
    /// Optional: per-(promoted column, stripe) distinct-count estimates for the planner. Absent when the file has no
    /// promoted columns; readers that predate the id skip it by its directory-recorded length.
    pub const STRIPE_NDV: u32 = 22;
    pub const STRIPES: u32 = 2;
    /// Optional: per-page token-membership filters for string columns, stored inline in the footer. Emitted only by
    /// writers from before the filters moved to the data area; readers that predate the id ignore it.
    pub const TEXT_TOKEN: u32 = 20;
    /// Optional: byte ranges of per-page token-membership filters stored in the data area. Absent in files that do
    /// not declare `TEXT_TOKEN_FILTER_OFFSETS`.
    pub const TEXT_TOKEN_OFFSETS: u32 = 21;
}

/// Logical column kind codes (stored in the column directory).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnKind {
    Decimal = 4,
    F64 = 3,
    I64 = 1,
    String = 5,
    U128 = 2,
    U64 = 0,
}

impl ColumnKind {
    /// Reads the column kind from its stored byte code, or an error for an unknown value.
    pub fn from_u8(value: u8) -> Result<Self, FormatError> {
        Ok(match value {
            0 => ColumnKind::U64,
            1 => ColumnKind::I64,
            2 => ColumnKind::U128,
            3 => ColumnKind::F64,
            4 => ColumnKind::Decimal,
            5 => ColumnKind::String,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown column kind",
                });
            }
        })
    }
}

/// One column's entry in the file's column directory: its id, name, type, and whether it may ever reach a public
/// caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDescriptor {
    pub column_id: u32,
    /// Internal columns never cross the public boundary.
    pub internal_only: bool,
    pub kind: ColumnKind,
    pub name: String,
    pub nullable: bool,
}

/// One stripe's entry in the stripe directory: where it sits in the file and which rows it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeEntry {
    pub byte_len: u64,
    pub file_offset: u64,
    pub first_row_ordinal: u64,
    pub row_count: u64,
    pub stripe_id: u32,
}

/// One non-stripe range in the data-area integrity partition. Its bytes are normally zero alignment padding, but its
/// digest is authoritative so even a cold footer+header open can reconstruct the exact file seal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegrityGapEntry {
    pub blake3: [u8; 32],
    pub byte_len: u64,
    pub file_offset: u64,
}

/// Where one stripe's Bao-style outboard proof nodes live in the object-level proof appendix. `tree_offset` is relative
/// to the start of that appendix; a zero `tree_len` means the stripe fits in one chunk group and must be fetched whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeProofEntry {
    pub chunk_group_bytes: u32,
    pub stripe_id: u32,
    pub tree_len: u64,
    pub tree_offset: u64,
}

/// One granule's entry in the granule directory: where its rows sit and the sequence and time ranges they span, so
/// pruning can skip it whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GranuleEntry {
    pub compressed_bytes_estimate: u64,
    pub first_epoch: u64,
    pub first_row_ordinal: u64,
    pub first_sequence: u64,
    pub granule_id: u32,
    pub last_epoch: u64,
    pub last_sequence: u64,
    pub max_ingested_at_physical: i64,
    pub max_occurred_at_physical: i64,
    pub min_ingested_at_physical: i64,
    pub min_occurred_at_physical: i64,
    pub row_count: u32,
    pub stripe_id: u32,
}

/// `ColumnMark`: the authoritative random-access mapping for `(column, projection, granule)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnMark {
    pub codec_pipeline_id: PipelineId,
    pub column_id: u32,
    pub compressed_offset: u64,
    pub compressed_size: u64,
    pub first_value_offset: Option<u64>,
    pub granule_id: u32,
    pub page_count: u32,
    pub projection_id: u32,
    pub row_count: u32,
    pub uncompressed_offset: u64,
    pub uncompressed_size: u64,
}

/// One entry in the marks section's two-level directory: locates one `(projection, column, stripe)`'s independently-
/// fetchable columnar marks page. Paired with [`encode_columnar_marks`] and [`decode_columnar_marks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarksPageEntry {
    pub column_id: u32,
    pub page_len: u64,
    pub page_offset: u64,
    pub projection_id: u32,
    pub stripe_id: u32,
}

/// Per-block stats: the page metadata and the source of the granule-granularity minmax SkipIndex. Declared exact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageStats {
    pub column_id: u32,
    pub granule_id: u32,
    pub max_f64: Option<f64>,
    pub max_i128: Option<i128>,
    pub min_f64: Option<f64>,
    pub min_i128: Option<i128>,
    pub null_count: u32,
    pub row_count: u32,
}

/// One page's addressing entry within a granule's column block.
///
/// When a file declares `PER_PAGE_MARKS`, each `(column, projection, granule)` block is subdivided into independently
/// readable pages. A reader can fetch exactly one page's byte range without touching neighbouring pages, then decode it
/// using the parent mark's codec pipeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageDirectoryEntry {
    pub column_id: u32,
    pub compressed_len: u64,
    pub compressed_offset: u64,
    pub first_row_ordinal: u64,
    pub granule_id: u32,
    pub max_f64: Option<f64>,
    pub max_i128: Option<i128>,
    pub max_occurred_at_physical: i64,
    pub max_sequence: u64,
    pub min_f64: Option<f64>,
    pub min_i128: Option<i128>,
    pub min_occurred_at_physical: i64,
    pub min_sequence: u64,
    pub null_count: u32,
    pub page_index: u32,
    pub projection_id: u32,
    pub row_count: u32,
}

/// Where one granule's point-membership filter over the identity-hash column sits in the data area. The bytes are a
/// [`SplitBlockBloomFilter`](crate::indexes::probabilistic::SplitBlockBloomFilter) built from the distinct
/// `entity_id_hash_low` values the granule holds; probe them in place with
/// [`SplitBlockBloomFilter::contains_encoded`](crate::indexes::probabilistic::SplitBlockBloomFilter::contains_encoded).
/// `index_offset` is stripe-relative, like every other data-area offset the footer records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntityHashFilterEntry {
    pub granule_id: u32,
    pub index_len: u64,
    pub index_offset: u64,
}

/// One row's entry in the file's external-id index: a stable hash of the id and the row that carries it. The footer
/// keeps the entries sorted by `(id_hash, row_ordinal)`, so finding an id is a binary search; two ids can share a
/// hash, so a reader confirms every hit against the stored id before answering.
///
/// See: hef-query-metadata-and-indexes/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalIdEntry {
    /// [`stable_hash`](crate::indexes::stable_hash) of the id's bytes.
    pub id_hash: u64,
    pub row_ordinal: u64,
}

/// A small filter that says whether one granule might hold a given reference in one relationship column, so a "who
/// points at X" lookup reads only the granules that could answer. The filter is a
/// [`SplitBlockBloomFilter`](crate::indexes::probabilistic::SplitBlockBloomFilter) over the
/// [`stable_hash`](crate::indexes::stable_hash) of each stored reference's `<space>:<hex>` text. A granule that holds no
/// reference of the column's kind has no entry at all. Entries are sorted by `(column_id, granule_id)`.
///
/// See: hef-query-metadata-and-indexes/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceFilterEntry {
    pub column_id: u32,
    pub filter: Vec<u8>,
    pub granule_id: u32,
}

/// One string page's token-membership filter: the encoded [`TextTokenIndex`](crate::indexes::text_token::TextTokenIndex)
/// built over the page's values, so a scan can prove a queried string absent and skip the page without reading its
/// bytes. `page_index` is 0 for a column block the writer did not split into pages.
///
/// See: hef-query-metadata-and-indexes/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTokenEntry {
    pub column_id: u32,
    pub granule_id: u32,
    /// [`TextTokenIndex::encode`](crate::indexes::text_token::TextTokenIndex::encode) output; decode with
    /// [`TextTokenIndex::decode`](crate::indexes::text_token::TextTokenIndex::decode).
    pub index_bytes: Vec<u8>,
    pub page_index: u32,
}

/// Where one string page's token-membership filter lives in the data area: the byte range of its encoded
/// [`TextTokenIndex`](crate::indexes::text_token::TextTokenIndex), written inside the owning stripe. The footer
/// carries only this geometry, so a cold open never fetches the filter bytes; a reader slices and decodes a filter on
/// first demand. `page_index` is 0 for a column block the writer did not split into pages.
///
/// See: hef-query-metadata-and-indexes/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextTokenOffsetsEntry {
    pub column_id: u32,
    pub granule_id: u32,
    pub index_len: u64,
    /// Stripe-relative under `STRIPE_RELATIVE_MARKS`, like every other data-area offset the footer records.
    pub index_offset: u64,
    pub page_index: u32,
}

/// One promoted column's distinct-count estimate over one stripe, for the planner's join and grouping cardinality
/// estimates. `exact` marks a true count (the stripe's column held few distinct values); otherwise `distinct_count`
/// is a HyperLogLog-class estimate. Counts only — never value bytes, so nothing crosses the per-subject encryption or
/// public-output boundaries.
///
/// See: hef-aggregation-metadata/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeNdvEntry {
    pub column_id: u32,
    pub distinct_count: u64,
    pub exact: bool,
    pub stripe_id: u32,
}

/// Per-page min/max statistics decoded from the `PAGE_MINMAX` optional extension section.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageMinMax {
    pub column_id: u32,
    pub granule_id: u32,
    pub max_i128: Option<i128>,
    pub min_i128: Option<i128>,
    pub null_count: u32,
    pub page_index: u32,
    pub row_count: u32,
}

/// File-level dictionaries for the source/type/entity id columns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileDictionaries {
    pub entity_type: Vec<String>,
    pub event_type: Vec<String>,
    pub source: Vec<String>,
}

/// Exact file counts (declared exact; never approximate).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExactCounts {
    pub by_entity_type: Vec<(u32, u64)>,
    pub by_event_type: Vec<(u32, u64)>,
    pub by_source: Vec<(u32, u64)>,
    pub row_count: u64,
}

/// Residual-block compression declared in marks/payload index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResidualCompression {
    /// Hot granules: uncompressed for offset-jump point access.
    None = 0,
    /// Cold/rewritten granules: page-level Zstd-3 over the whole arena, declared here. Written by builds before the
    /// seekable form existed; still read, never written.
    Zstd3 = 1,
    /// Cold/rewritten granules: Zstd-3 over independently decompressible frames plus a seek table, so a point read
    /// inflates one frame instead of the whole arena. A reader that does not know this discriminant refuses the file
    /// rather than misreading the frames, which is why the enum is closed.
    ZstdSeekable = 2,
}

/// Payload-arena geometry for one granule: the granule's variant dictionary block, the row offset table, and the
/// residual value block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadGranule {
    pub dictionary_len: u64,
    pub dictionary_offset: u64,
    pub granule_id: u32,
    pub offsets_len: u64,
    pub offsets_offset: u64,
    pub residual_compression: ResidualCompression,
    pub residual_len: u64,
    pub residual_offset: u64,
}

/// Promoted-column presence map entry, keyed by schema version: granules whose rows predate `since_schema_version` fall
/// back to the payload blocks rather than reading the column as NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenceEntry {
    pub column_id: u32,
    pub since_schema_version: u32,
}

/// One shredded payload path lifted into a typed column (`variant_shredded_field_blocks`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShredEntry {
    pub column_id: u32,
    /// Top-level payload path (field name in the governing dictionary).
    pub path: String,
}

/// One column's file-scope dictionary alphabet: the sorted distinct values every shared-scope block of the column
/// resolves its codes against, stored once per file instead of once per block.
///
/// See: hef-encodings-and-compression/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedDictionaryEntry {
    pub column_id: u32,
    /// Sorted ascending and deduplicated — the same order block-local dictionaries use, so codes keep the ascending
    /// sorted-order assignment in every scope.
    pub values: Vec<String>,
}

/// One schema-declared free-text field shredded by declaration into the free-text columnar family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FreetextEntry {
    pub column_id: u32,
    pub declared_field: String,
}

/// One declared free-text column's per-row byte-offset index for one granule: an `(offset, len)` entry per row into a
/// raw, uncompressed byte block holding that row's exact text bytes — the same shape as the residual variant arena's
/// per-row slot (`PayloadGranule.offsets_offset`/`offsets_len` over `residual_offset`/`residual_len`), so a point read
/// costs two indexed reads instead of decoding the whole granule's free-text column block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreetextRowOffsets {
    pub bytes_len: u64,
    pub bytes_offset: u64,
    pub column_id: u32,
    pub granule_id: u32,
    pub offsets_len: u64,
    pub offsets_offset: u64,
}

/// One internal embedding/vector column's per-row byte-offset index for one granule: an `(offset, len)` entry per row
/// into a raw, uncompressed byte block holding that row's exact vector bytes — the same shape as
/// [`FreetextRowOffsets`] and the residual variant arena's own per-row slot, so an exact per-row vector fetch costs two
/// indexed reads instead of decoding the whole granule's vector column block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingRowOffsets {
    pub bytes_len: u64,
    pub bytes_offset: u64,
    pub column_id: u32,
    pub granule_id: u32,
    pub offsets_len: u64,
    pub offsets_offset: u64,
}

/// Sort order of a clustered column projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// The proof that a projection's rows are globally sorted by the named columns in a given direction, recorded so the
/// planner can elide downstream sort operators when it plans a query over that projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortednessProof {
    pub column_names: Vec<String>,
    pub direction: SortDirection,
}

/// One granule's clustering entry: how well its rows cluster in the named projection, and — if they are fully sorted —
/// the proof of that sort order.
#[derive(Debug, Clone, PartialEq)]
pub struct ClusteringEntry {
    /// Fraction of rows that fell into their projected sort bucket; 1.0 means perfectly sorted, 0.0 means random.
    pub clustering_quality: f32,
    pub granule_id: u32,
    pub projection_id: u32,
    pub sortedness_proof: Option<SortednessProof>,
}

/// Everything decoded out of a file's footer: its feature flags, the column list, the on-disk directories, the exact
/// counts, and the payload geometry.
#[derive(Debug, Clone, PartialEq)]
pub struct Footer {
    pub clustering: Vec<ClusteringEntry>,
    pub columns: Vec<ColumnDescriptor>,
    pub dictionaries: FileDictionaries,
    /// Per-row byte-offset index for internal embedding/vector columns. Present when `TYPED_COLUMN_ROW_OFFSETS` is
    /// declared; empty in files that read embedding/vector columns at granule granularity only.
    pub embedding_row_offsets: Vec<EmbeddingRowOffsets>,
    /// Byte ranges of the per-granule identity-hash membership filters, so an entity-id point lookup skips the
    /// granules that cannot hold the id. Present when `ENTITY_HASH_POINT_FILTERS` is declared; empty otherwise, and a
    /// reader then scans every granule exactly as it did before the filters existed.
    pub entity_hash_filters: Vec<EntityHashFilterEntry>,
    /// Forward-compatibility notes: how an older reader may still read an optional block it predates, via a portable
    /// decoder the fleet trusts.
    pub escape_hatches: Vec<EscapeHatch>,
    pub exact_counts: ExactCounts,
    /// The external-id index, sorted by `(id_hash, row_ordinal)`. Empty in files whose rows carry no external id.
    pub external_ids: Vec<ExternalIdEntry>,
    pub format_version: (u16, u16),
    pub freetext: Vec<FreetextEntry>,
    /// Per-row byte-offset index for declared free-text columns. Present when `TYPED_COLUMN_ROW_OFFSETS` is declared;
    /// empty in files that read free-text at granule granularity only.
    pub freetext_row_offsets: Vec<FreetextRowOffsets>,
    pub granules: Vec<GranuleEntry>,
    /// Authenticated commitments for the padding ranges between stripes.
    pub integrity_gaps: Vec<IntegrityGapEntry>,
    /// Byte alignment for column block boundaries, enabling aligned `O_DIRECT` reads of individual pages. Zero means
    /// not declared.
    pub io_alignment_bytes: u32,
    pub marks: Vec<ColumnMark>,
    /// The two-level marks directory (one entry per `(projection, column, stripe)`) when `columnar_marks` is declared;
    /// empty for the row-oriented form, where `marks` above is already the complete decoded directory. Paired with
    /// `marks_pages`: a directory entry's `page_offset`/`page_len` locate its group's page within it.
    pub marks_directory: Vec<MarksPageEntry>,
    /// Directory of per-stripe columnar marks pages co-located in the data area beside each stripe's filter bytes, so
    /// one ranged IO fetches a surviving stripe's marks and filters together. Each entry's `page_offset` is
    /// stripe-relative (like every other data-area offset the footer records), unlike `marks_directory`, whose offsets
    /// index the footer's own `marks_pages` blob. Governed by the `STRIPE_MARKS_PAGES` required feature; empty in
    /// files written before the placement existed, whose pages ride the footer through `marks_directory` instead.
    pub marks_page_offsets: Vec<MarksPageEntry>,
    /// The raw, not-yet-decoded page bytes a `columnar_marks` file's `marks_directory` entries point into. Empty for
    /// the row-oriented form. A reader decodes one stripe's pages from here lazily, only once that stripe survives
    /// pruning — see [`decode_columnar_marks_page`].
    pub marks_pages: Vec<u8>,
    pub optional_feature_flags: u64,
    /// Per-page addressing directory. Present when `PER_PAGE_MARKS` is declared; empty in files that read at granule
    /// granularity only.
    pub page_directory: Vec<PageDirectoryEntry>,
    pub page_minmax: Vec<PageMinMax>,
    pub page_stats: Vec<PageStats>,
    pub payload_granules: Vec<PayloadGranule>,
    pub presence: Vec<PresenceEntry>,
    /// Per-(relationship column, granule) reference filters. `None` when the file predates them or carries no
    /// relationship columns, in which case a lookup scans every granule.
    pub reference_filters: Option<Vec<ReferenceFilterEntry>>,
    pub required_feature_flags: u64,
    pub schema_fingerprint: [u8; 32],
    pub shared_dictionaries: Vec<SharedDictionaryEntry>,
    pub shredded: Vec<ShredEntry>,
    /// Payload paths stored as sparse shredded columns — hot but under the dense promotion threshold — declared here
    /// so the sparse key set is closed per file, exactly as the presence map declares promoted columns. Every entry
    /// also appears in `shredded`, since sparse columns ride the same block machinery.
    pub sparse_keys: Vec<ShredEntry>,
    pub stripe_checksums: Vec<[u8; 32]>,
    /// Authenticated geometry for each stripe's outboard verified-streaming proof tree.
    pub stripe_proofs: Vec<StripeProofEntry>,
    /// Per-(promoted column, stripe) distinct-count estimates the planner reads for join and grouping cardinality —
    /// exact when the stripe's column held few distinct values, a HyperLogLog-class estimate beyond. Counts only;
    /// never value bytes.
    pub stripe_ndv: Vec<StripeNdvEntry>,
    pub stripes: Vec<StripeEntry>,
    /// Per-page token-membership filters for string columns, inline in the footer. Empty in files whose writer stores
    /// filter bytes in the data area (see `text_token_offsets`) and in files written before the index existed.
    pub text_token_indexes: Vec<TextTokenEntry>,
    /// Byte ranges of per-page token-membership filters stored in the data area. Empty unless the file declares
    /// `TEXT_TOKEN_FILTER_OFFSETS`.
    pub text_token_offsets: Vec<TextTokenOffsetsEntry>,
}

impl Footer {
    /// Returns `true` when every granule of the file carries, for `projection_id`, a sortedness proof that covers
    /// `column_names` in `direction` order — the planner may then set `ordering_proven` and elide any downstream sort
    /// over that ordering.
    ///
    /// Returns `false` when the file has no granules, when any physical granule has no clustering entry for the
    /// projection, or when an entry's proof is missing or names a different ordering — the planner then falls back to
    /// sorting. A granule with no entry is the case that matters: proving one granule sorted says nothing about the
    /// rows of another, so sort elision over the projection would reorder unproven data.
    pub fn all_granules_sorted_for(&self, projection_id: u32, column_names: &[&str], direction: SortDirection) -> bool {
        if self.granules.is_empty() {
            return false;
        }
        // Index this projection's clustering entries by granule once, rather than scanning the whole clustering list
        // for every granule — O(clustering + granules) instead of O(granules × clustering).
        let by_granule: HashMap<u32, &ClusteringEntry> = self
            .clustering
            .iter()
            .filter(|entry| entry.projection_id == projection_id)
            .map(|entry| (entry.granule_id, entry))
            .collect();
        self.granules.iter().all(|granule| {
            by_granule
                .get(&granule.granule_id)
                .and_then(|entry| entry.sortedness_proof.as_ref())
                .is_some_and(|proof| {
                    proof.direction == direction
                        && proof.column_names.len() == column_names.len()
                        && proof.column_names.iter().zip(column_names).all(|(a, b)| a == b)
                })
        })
    }
}

fn put_string(out: &mut Writer, value: &str) {
    out.put_u32(value.len() as u32);
    out.put_slice(value.as_bytes());
}

fn read_string(reader: &mut Reader<'_>) -> Result<String, FormatError> {
    let len = reader.u32("string length")? as usize;
    let bytes = reader.take(len, "string")?;
    simdutf8::basic::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| FormatError::InvalidUtf8 { what: "footer string" })
}

fn put_opt_i128(out: &mut Writer, value: Option<i128>) {
    match value {
        Some(value) => {
            out.put_u8(1);
            out.put_u128(value as u128);
        }
        None => out.put_u8(0),
    }
}

fn read_opt_i128(reader: &mut Reader<'_>) -> Result<Option<i128>, FormatError> {
    match reader.u8("option tag")? {
        0 => Ok(None),
        1 => Ok(Some(reader.u128("i128")? as i128)),
        _ => Err(FormatError::Structural {
            rule: "footer option tag must be 0 or 1",
        }),
    }
}

fn put_opt_f64(out: &mut Writer, value: Option<f64>) {
    match value {
        Some(value) => {
            out.put_u8(1);
            out.put_u64(value.to_bits());
        }
        None => out.put_u8(0),
    }
}

fn read_opt_f64(reader: &mut Reader<'_>) -> Result<Option<f64>, FormatError> {
    match reader.u8("option tag")? {
        0 => Ok(None),
        1 => Ok(Some(f64::from_bits(reader.u64("f64")?))),
        _ => Err(FormatError::Structural {
            rule: "footer option tag must be 0 or 1",
        }),
    }
}

fn put_opt_u64(out: &mut Writer, value: Option<u64>) {
    match value {
        Some(value) => {
            out.put_u8(1);
            out.put_u64(value);
        }
        None => out.put_u8(0),
    }
}

fn read_opt_u64(reader: &mut Reader<'_>) -> Result<Option<u64>, FormatError> {
    match reader.u8("option tag")? {
        0 => Ok(None),
        1 => Ok(Some(reader.u64("u64")?)),
        _ => Err(FormatError::Structural {
            rule: "footer option tag must be 0 or 1",
        }),
    }
}

fn put_counts(out: &mut Writer, counts: &[(u32, u64)]) {
    out.put_u32(counts.len() as u32);
    for (id, count) in counts {
        out.put_u32(*id);
        out.put_u64(*count);
    }
}

fn read_counts(reader: &mut Reader<'_>) -> Result<Vec<(u32, u64)>, FormatError> {
    let count = reader.u32("count entries")? as usize;
    // Each entry: u32 id (4) + u64 value (8) = 12 bytes minimum.
    let count = bounded_count(count, 12, reader, "count entries exceeds input")?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let id = reader.u32("count id")?;
        let value = reader.u64("count value")?;
        entries.push((id, value));
    }
    Ok(entries)
}

fn put_dictionary(out: &mut Writer, values: &[String]) {
    out.put_u32(values.len() as u32);
    for value in values {
        put_string(out, value);
    }
}

fn read_dictionary(reader: &mut Reader<'_>) -> Result<Vec<String>, FormatError> {
    let count = reader.u32("dictionary count")? as usize;
    // Each entry is a string, whose minimum encoding is its own u32 length prefix (4 bytes).
    let count = bounded_count(count, 4, reader, "dictionary count exceeds input")?;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(read_string(reader)?);
    }
    Ok(values)
}

/// Every footer section laid end to end in one buffer, with each section's id and byte range recorded alongside.
///
/// The section directory has to be written ahead of the section bytes, so the bytes cannot go straight into the footer
/// blob. They all go into this single buffer instead — one allocation for the whole section area rather than one per
/// section, and one copy into the blob rather than twenty.
struct SectionArea {
    bytes: Writer,
    directory: Vec<(u32, Range<usize>)>,
}

impl SectionArea {
    /// Appends one section, recording everything `write` puts into the buffer as the section with this id.
    fn push(&mut self, id: u32, write: impl FnOnce(&mut Writer)) {
        let start = self.bytes.len();
        write(&mut self.bytes);
        self.directory.push((id, start..self.bytes.len()));
    }
}

fn encode_sections(footer: &Footer) -> SectionArea {
    let mut sections = SectionArea {
        bytes: Writer::new(),
        directory: Vec::new(),
    };

    sections.push(sections::COLUMNS, |out| {
        out.put_u32(footer.columns.len() as u32);
        for column in &footer.columns {
            out.put_u32(column.column_id);
            put_string(out, &column.name);
            out.put_u8(column.kind as u8);
            out.put_u8(u8::from(column.nullable));
            out.put_u8(u8::from(column.internal_only));
        }
    });

    sections.push(sections::STRIPES, |out| {
        out.put_u32(footer.stripes.len() as u32);
        for stripe in &footer.stripes {
            out.put_u32(stripe.stripe_id);
            out.put_u64(stripe.file_offset);
            out.put_u64(stripe.byte_len);
            out.put_u64(stripe.first_row_ordinal);
            out.put_u64(stripe.row_count);
        }
    });

    sections.push(sections::GRANULES, |out| {
        out.put_u32(footer.granules.len() as u32);
        for granule in &footer.granules {
            out.put_u32(granule.granule_id);
            out.put_u32(granule.stripe_id);
            out.put_u64(granule.first_row_ordinal);
            out.put_u32(granule.row_count);
            out.put_u64(granule.first_epoch);
            out.put_u64(granule.first_sequence);
            out.put_u64(granule.last_epoch);
            out.put_u64(granule.last_sequence);
            out.put_i64(granule.min_occurred_at_physical);
            out.put_i64(granule.max_occurred_at_physical);
            out.put_i64(granule.min_ingested_at_physical);
            out.put_i64(granule.max_ingested_at_physical);
            out.put_u64(granule.compressed_bytes_estimate);
        }
    });

    // Governed by the `columnar_marks` required feature: a writer that declares it emits the two-level per-stripe
    // columnar directory here instead of the row-oriented struct array, using the granule directory's own
    // `granule_id -> stripe_id` mapping — folding the per-page directory into the same per-stripe pages so a pruned
    // stripe costs zero per-page directory bytes too (see the `PAGE_DIRECTORY` section below, left empty for a
    // columnar file). The writer declares the flag (see `required_features::ALL`), so this is the deployed form;
    // the row-oriented branch below serves legacy files and hand-built footers.
    sections.push(sections::MARKS, |out| {
        if footer.required_feature_flags & required_features::STRIPE_MARKS_PAGES != 0 {
            // Co-located placement: the pages already sit in the data area beside each stripe's filter bytes (written
            // by the builder), so the section carries only the per-stripe directory — offsets stripe-relative into the
            // data area — and an empty pages area. Same directory wire as the in-footer form below.
            out.put_u32(footer.marks_page_offsets.len() as u32);
            for entry in &footer.marks_page_offsets {
                out.put_u32(entry.projection_id);
                out.put_u32(entry.column_id);
                out.put_u32(entry.stripe_id);
                out.put_u64(entry.page_offset);
                out.put_u64(entry.page_len);
            }
        } else if footer.required_feature_flags & required_features::COLUMNAR_MARKS != 0 {
            let granule_stripe_ids = footer
                .granules
                .iter()
                .map(|granule| (granule.granule_id, granule.stripe_id))
                .collect::<BTreeMap<_, _>>();
            let columnar = encode_columnar_marks(&footer.marks, &footer.page_directory, &granule_stripe_ids)
                .expect("columnar marks encode: every mark's granule must appear in the granule directory");
            out.put_slice(&columnar);
        } else {
            out.put_u32(footer.marks.len() as u32);
            for mark in &footer.marks {
                out.put_u32(mark.column_id);
                out.put_u32(mark.projection_id);
                out.put_u32(mark.granule_id);
                out.put_u64(mark.compressed_offset);
                out.put_u64(mark.compressed_size);
                out.put_u64(mark.uncompressed_offset);
                out.put_u64(mark.uncompressed_size);
                out.put_u32(mark.row_count);
                out.put_u32(mark.page_count);
                out.put_u32(mark.codec_pipeline_id.0);
                put_opt_u64(out, mark.first_value_offset);
            }
        }
    });

    sections.push(sections::DICTIONARIES, |out| {
        put_dictionary(out, &footer.dictionaries.source);
        put_dictionary(out, &footer.dictionaries.event_type);
        put_dictionary(out, &footer.dictionaries.entity_type);
    });

    sections.push(sections::PAGE_STATS, |out| {
        out.put_u32(footer.page_stats.len() as u32);
        for stats in &footer.page_stats {
            out.put_u32(stats.column_id);
            out.put_u32(stats.granule_id);
            out.put_u32(stats.row_count);
            out.put_u32(stats.null_count);
            put_opt_i128(out, stats.min_i128);
            put_opt_i128(out, stats.max_i128);
            put_opt_f64(out, stats.min_f64);
            put_opt_f64(out, stats.max_f64);
        }
    });

    sections.push(sections::EXACT_COUNTS, |out| {
        out.put_u64(footer.exact_counts.row_count);
        put_counts(out, &footer.exact_counts.by_source);
        put_counts(out, &footer.exact_counts.by_event_type);
        put_counts(out, &footer.exact_counts.by_entity_type);
    });

    sections.push(sections::PAYLOAD_GRANULES, |out| {
        out.put_u32(footer.payload_granules.len() as u32);
        for payload in &footer.payload_granules {
            out.put_u32(payload.granule_id);
            out.put_u64(payload.dictionary_offset);
            out.put_u64(payload.dictionary_len);
            out.put_u64(payload.offsets_offset);
            out.put_u64(payload.offsets_len);
            out.put_u64(payload.residual_offset);
            out.put_u64(payload.residual_len);
            out.put_u8(payload.residual_compression as u8);
        }
    });

    sections.push(sections::PRESENCE, |out| {
        out.put_u32(footer.presence.len() as u32);
        for entry in &footer.presence {
            out.put_u32(entry.column_id);
            out.put_u32(entry.since_schema_version);
        }
    });

    sections.push(sections::SHREDDED, |out| {
        out.put_u32(footer.shredded.len() as u32);
        for entry in &footer.shredded {
            out.put_u32(entry.column_id);
            put_string(out, &entry.path);
        }
    });

    sections.push(sections::FREETEXT, |out| {
        out.put_u32(footer.freetext.len() as u32);
        for entry in &footer.freetext {
            out.put_u32(entry.column_id);
            put_string(out, &entry.declared_field);
        }
    });

    sections.push(sections::STRIPE_CHECKSUMS, |out| {
        out.put_u32(footer.stripe_checksums.len() as u32);
        for checksum in &footer.stripe_checksums {
            out.put_slice(checksum);
        }
    });

    sections.push(sections::INTEGRITY_GAPS, |out| {
        out.put_u32(footer.integrity_gaps.len() as u32);
        for gap in &footer.integrity_gaps {
            out.put_u64(gap.file_offset);
            out.put_u64(gap.byte_len);
            out.put_slice(&gap.blake3);
        }
    });

    sections.push(sections::STRIPE_PROOFS, |out| {
        out.put_u32(footer.stripe_proofs.len() as u32);
        for proof in &footer.stripe_proofs {
            out.put_u32(proof.stripe_id);
            out.put_u32(proof.chunk_group_bytes);
            out.put_u64(proof.tree_offset);
            out.put_u64(proof.tree_len);
        }
    });

    // Free-text per-row byte-offset index: optional, emitted only when TYPED_COLUMN_ROW_OFFSETS is declared. Older
    // readers that do not know the section id ignore it.
    if !footer.freetext_row_offsets.is_empty() {
        sections.push(sections::FREETEXT_ROW_OFFSETS, |out| {
            out.put_u32(footer.freetext_row_offsets.len() as u32);
            for entry in &footer.freetext_row_offsets {
                out.put_u32(entry.column_id);
                out.put_u32(entry.granule_id);
                out.put_u64(entry.offsets_offset);
                out.put_u64(entry.offsets_len);
                out.put_u64(entry.bytes_offset);
                out.put_u64(entry.bytes_len);
            }
        });
    }

    // Embedding/vector per-row byte-offset index: optional, emitted only when TYPED_COLUMN_ROW_OFFSETS is declared.
    // Older readers that do not know the section id ignore it.
    if !footer.embedding_row_offsets.is_empty() {
        sections.push(sections::EMBEDDING_ROW_OFFSETS, |out| {
            out.put_u32(footer.embedding_row_offsets.len() as u32);
            for entry in &footer.embedding_row_offsets {
                out.put_u32(entry.column_id);
                out.put_u32(entry.granule_id);
                out.put_u64(entry.offsets_offset);
                out.put_u64(entry.offsets_len);
                out.put_u64(entry.bytes_offset);
                out.put_u64(entry.bytes_len);
            }
        });
    }

    // Clustering metadata is an optional extension block: emitted only when entries are present, so files without
    // clustering data carry no empty section and older readers ignore it.
    if !footer.clustering.is_empty() {
        sections.push(sections::CLUSTERING_METADATA, |out| {
            out.put_u32(footer.clustering.len() as u32);
            for entry in &footer.clustering {
                out.put_u32(entry.projection_id);
                out.put_u32(entry.granule_id);
                out.put_u32(entry.clustering_quality.to_bits());
                match &entry.sortedness_proof {
                    None => out.put_u8(0),
                    Some(proof) => {
                        out.put_u8(1);
                        out.put_u8(match proof.direction {
                            SortDirection::Ascending => 0,
                            SortDirection::Descending => 1,
                        });
                        out.put_u32(proof.column_names.len() as u32);
                        for name in &proof.column_names {
                            put_string(out, name);
                        }
                    }
                }
            }
        });
    }

    // IO alignment is an optional extension block: emitted only when the writer declares an alignment, so files without
    // it carry no empty section and older readers ignore it.
    if footer.io_alignment_bytes > 0 {
        sections.push(sections::IO_ALIGNMENT, |out| {
            out.put_u32(footer.io_alignment_bytes);
        });
    }

    // Forward-compatibility escape hatches are a genuine optional extension block: emitted only when present, so files
    // that do not use them carry no empty section and older readers simply do not see the id. Per the spec, each
    // entry's bytes are ordered min_reader_version, portable_decoder_ref, then the optional feature flag.
    if !footer.escape_hatches.is_empty() {
        sections.push(sections::ESCAPE_HATCHES, |out| {
            out.put_u32(footer.escape_hatches.len() as u32);
            for hatch in &footer.escape_hatches {
                out.put_u32(hatch.min_reader_version);
                put_string(out, &hatch.portable_decoder_ref);
                out.put_u64(hatch.optional_feature_bit);
            }
        });
    }

    // Per-page addressing directory: optional, emitted only when PER_PAGE_MARKS is declared. Older readers that do not
    // know the section id ignore it. A `columnar_marks` file instead folds this into the per-stripe marks pages above,
    // so a pruned stripe costs zero per-page directory bytes; this row-oriented section stays absent for such a file.
    if footer.required_feature_flags & required_features::COLUMNAR_MARKS == 0 && !footer.page_directory.is_empty() {
        sections.push(sections::PAGE_DIRECTORY, |out| {
            out.put_u32(footer.page_directory.len() as u32);
            for entry in &footer.page_directory {
                out.put_u32(entry.column_id);
                out.put_u32(entry.projection_id);
                out.put_u32(entry.granule_id);
                out.put_u32(entry.page_index);
                out.put_u64(entry.compressed_offset);
                out.put_u64(entry.compressed_len);
                out.put_u64(entry.first_row_ordinal);
                out.put_u32(entry.row_count);
                out.put_u32(entry.null_count);
                put_opt_i128(out, entry.min_i128);
                put_opt_i128(out, entry.max_i128);
                put_opt_f64(out, entry.min_f64);
                put_opt_f64(out, entry.max_f64);
                out.put_u64(entry.min_sequence);
                out.put_u64(entry.max_sequence);
                out.put_i64(entry.min_occurred_at_physical);
                out.put_i64(entry.max_occurred_at_physical);
            }
        });
    }

    // Per-page min/max: optional, emitted only for columns whose measured heat justifies page-granularity stats (see
    // `hef/writer/build.rs`). Absent in files with no hot columns; older readers ignore it.
    if !footer.page_minmax.is_empty() {
        sections.push(sections::PAGE_MINMAX, |out| {
            out.put_u32(footer.page_minmax.len() as u32);
            for entry in &footer.page_minmax {
                out.put_u32(entry.column_id);
                out.put_u32(entry.granule_id);
                out.put_u32(entry.page_index);
                out.put_u32(entry.row_count);
                out.put_u32(entry.null_count);
                put_opt_i128(out, entry.min_i128);
                put_opt_i128(out, entry.max_i128);
            }
        });
    }

    // Per-page string token filters: optional, emitted only for files with string columns. Older readers ignore it.
    if !footer.text_token_indexes.is_empty() {
        sections.push(sections::TEXT_TOKEN, |out| {
            out.put_u32(footer.text_token_indexes.len() as u32);
            for entry in &footer.text_token_indexes {
                out.put_u32(entry.column_id);
                out.put_u32(entry.granule_id);
                out.put_u32(entry.page_index);
                out.put_u32(entry.index_bytes.len() as u32);
                out.put_slice(&entry.index_bytes);
            }
        });
    }

    // Byte ranges of string token filters stored in the data area: optional, emitted only when the writer relocated
    // the filter bytes out of the footer. Older readers ignore it and simply never prune by token filter.
    if !footer.text_token_offsets.is_empty() {
        sections.push(sections::TEXT_TOKEN_OFFSETS, |out| {
            out.put_u32(footer.text_token_offsets.len() as u32);
            for entry in &footer.text_token_offsets {
                out.put_u32(entry.column_id);
                out.put_u32(entry.granule_id);
                out.put_u32(entry.page_index);
                out.put_u64(entry.index_offset);
                out.put_u64(entry.index_len);
            }
        });
    }

    // Byte ranges of the per-granule identity-hash membership filters: optional, emitted only when the writer's
    // budget allowed the filters. Older readers ignore the section and scan every granule, which is always correct.
    if !footer.entity_hash_filters.is_empty() {
        sections.push(sections::ENTITY_HASH_FILTERS, |out| {
            out.put_u32(footer.entity_hash_filters.len() as u32);
            for entry in &footer.entity_hash_filters {
                out.put_u32(entry.granule_id);
                out.put_u64(entry.index_offset);
                out.put_u64(entry.index_len);
            }
        });
    }

    // The external-id index: optional, emitted only when some row carries an external id.
    if !footer.external_ids.is_empty() {
        sections.push(sections::EXTERNAL_IDS, |out| {
            out.put_u32(footer.external_ids.len() as u32);
            for entry in &footer.external_ids {
                out.put_u64(entry.id_hash);
                out.put_u64(entry.row_ordinal);
            }
        });
    }

    // Reference filters: emitted, possibly empty, whenever the file carries relationship columns, so a reader can
    // tell "no granule holds this kind" from "this file has no filters".
    if let Some(filters) = &footer.reference_filters {
        sections.push(sections::REFERENCE_FILTERS, |out| {
            out.put_u32(filters.len() as u32);
            for entry in filters {
                out.put_u32(entry.column_id);
                out.put_u32(entry.granule_id);
                out.put_u32(entry.filter.len() as u32);
                out.put_slice(&entry.filter);
            }
        });
    }

    // Per-(promoted column, stripe) distinct counts: optional, emitted only when the writer computed any.
    if !footer.stripe_ndv.is_empty() {
        sections.push(sections::STRIPE_NDV, |out| {
            out.put_u32(footer.stripe_ndv.len() as u32);
            for entry in &footer.stripe_ndv {
                out.put_u32(entry.column_id);
                out.put_u32(entry.stripe_id);
                out.put_u64(entry.distinct_count);
                out.put_u8(u8::from(entry.exact));
            }
        });
    }

    // File-scope dictionary alphabets: optional, emitted only for columns whose blocks recorded the file scope.
    if !footer.shared_dictionaries.is_empty() {
        sections.push(sections::SHARED_DICTIONARIES, |out| {
            out.put_u32(footer.shared_dictionaries.len() as u32);
            for entry in &footer.shared_dictionaries {
                out.put_u32(entry.column_id);
                out.put_u32(entry.values.len() as u32);
                for value in &entry.values {
                    out.put_u16(value.len() as u16);
                    out.put_slice(value.as_bytes());
                }
            }
        });
    }

    // The sparse shredded key set: optional, emitted only when the writer stored sparse columns.
    if !footer.sparse_keys.is_empty() {
        sections.push(sections::SPARSE_KEYS, |out| {
            out.put_u32(footer.sparse_keys.len() as u32);
            for entry in &footer.sparse_keys {
                out.put_u32(entry.column_id);
                out.put_u16(entry.path.len() as u16);
                out.put_slice(entry.path.as_bytes());
            }
        });
    }

    sections
}

/// Bytes the footer preamble occupies: format version, the two feature-flag words, the schema fingerprint, and the
/// section count.
const PREAMBLE_LEN: usize = 2 + 2 + 8 + 8 + 32 + 4;

/// Bytes one section directory entry occupies: id, offset, length, and the section's BLAKE3.
const DIRECTORY_ENTRY_LEN: usize = 4 + 8 + 8 + 32;

/// Serializes the footer blob (preamble + section/checksum directory + sections). The caller appends `footer_len` +
/// magic.
pub fn encode_footer(footer: &Footer) -> Vec<u8> {
    let sections = encode_sections(footer);
    let area = sections.bytes.into_bytes();
    let mut out = Writer::with_capacity(PREAMBLE_LEN + sections.directory.len() * DIRECTORY_ENTRY_LEN + area.len());
    out.put_u16(footer.format_version.0);
    out.put_u16(footer.format_version.1);
    out.put_u64(footer.required_feature_flags);
    out.put_u64(footer.optional_feature_flags);
    out.put_slice(&footer.schema_fingerprint);
    out.put_u32(sections.directory.len() as u32);
    // Directory entries: id, offset (relative to section area), len, blake3.
    for (id, range) in &sections.directory {
        let bytes = area.get(range.clone()).unwrap_or_default();
        out.put_u32(*id);
        out.put_u64(range.start as u64);
        out.put_u64(bytes.len() as u64);
        out.put_slice(crate::file::integrity::hash_tree(bytes).as_bytes());
    }
    out.put_slice(&area);
    out.into_bytes()
}

fn decode_section<'a>(
    directory: &[(u32, u64, u64, [u8; 32])],
    section_area: &'a [u8],
    id: u32,
) -> Result<Option<&'a [u8]>, FormatError> {
    let Some((_, offset, len, checksum)) = directory.iter().find(|(sid, ..)| *sid == id) else {
        return Ok(None);
    };
    let bytes = slice(section_area, *offset as usize, *len as usize, "section")?;
    // Known feature blocks are used only after their checksum verifies.
    if crate::file::integrity::hash_tree(bytes).as_bytes() != checksum {
        return Err(FormatError::Blake3Mismatch {
            scope: "footer section",
        });
    }
    Ok(Some(bytes))
}

fn require<'a>(section: Option<&'a [u8]>, rule: &'static str) -> Result<&'a [u8], FormatError> {
    section.ok_or(FormatError::Structural { rule })
}

/// Refusing bound on a decoded element count, checked before any allocation: a count whose minimum encoding cannot
/// fit in the bytes the reader still holds is a structural violation. `Vec::with_capacity` must never see an
/// unvalidated wire count — a hostile footer could demand hundreds of gigabytes through a single u32.
fn bounded_count(
    count: usize,
    min_entry_bytes: usize,
    reader: &Reader<'_>,
    rule: &'static str,
) -> Result<usize, FormatError> {
    if count > reader.remaining() / min_entry_bytes.max(1) {
        return Err(FormatError::Structural { rule });
    }
    Ok(count)
}

/// Decodes and validates the footer blob. Unknown section ids are ignored (forward-compatible extension blocks);
/// required-feature gating happens in the reader before any section is trusted.
pub fn decode_footer(blob: &[u8]) -> Result<Footer, FormatError> {
    let mut reader = Reader::new(blob);
    let format_version = (reader.u16("footer version major")?, reader.u16("footer version minor")?);
    let required_feature_flags = reader.u64("required feature flags")?;
    let optional_feature_flags = reader.u64("optional feature flags")?;
    let fingerprint_bytes = reader.take(32, "schema fingerprint")?;
    let mut schema_fingerprint = [0u8; 32];
    schema_fingerprint.copy_from_slice(fingerprint_bytes);
    let section_count = reader.u32("section count")? as usize;
    let section_count = bounded_count(
        section_count,
        DIRECTORY_ENTRY_LEN,
        &reader,
        "section count exceeds input",
    )?;
    let mut directory = Vec::with_capacity(section_count);
    for _ in 0..section_count {
        let id = reader.u32("section id")?;
        let offset = reader.u64("section offset")?;
        let len = reader.u64("section len")?;
        let checksum_bytes = reader.take(32, "section checksum")?;
        let mut checksum = [0u8; 32];
        checksum.copy_from_slice(checksum_bytes);
        directory.push((id, offset, len, checksum));
    }
    let section_area = reader.take(reader.remaining(), "section area")?;

    let columns_bytes = require(
        decode_section(&directory, section_area, sections::COLUMNS)?,
        "footer missing column directory",
    )?;
    let mut r = Reader::new(columns_bytes);
    let count = r.u32("column count")? as usize;
    // Each entry: u32 id (4) + name length prefix (4) + kind (1) + nullable (1) + internal_only (1) = 11 bytes
    // minimum.
    let count = bounded_count(count, 11, &r, "column count exceeds input")?;
    let mut columns = Vec::with_capacity(count);
    for _ in 0..count {
        let column_id = r.u32("column id")?;
        let name = read_string(&mut r)?;
        let kind = ColumnKind::from_u8(r.u8("column kind")?)?;
        let nullable = r.u8("column nullable")? != 0;
        let internal_only = r.u8("column internal")? != 0;
        columns.push(ColumnDescriptor {
            column_id,
            name,
            kind,
            nullable,
            internal_only,
        });
    }

    let stripes_bytes = require(
        decode_section(&directory, section_area, sections::STRIPES)?,
        "footer missing stripe directory",
    )?;
    let mut r = Reader::new(stripes_bytes);
    let count = r.u32("stripe count")? as usize;
    // Each entry: u32 id (4) + u64 offset/len/first_row/rows (8 × 4) = 36 bytes minimum.
    let count = bounded_count(count, 36, &r, "stripe count exceeds input")?;
    let mut stripes = Vec::with_capacity(count);
    for _ in 0..count {
        stripes.push(StripeEntry {
            stripe_id: r.u32("stripe id")?,
            file_offset: r.u64("stripe offset")?,
            byte_len: r.u64("stripe len")?,
            first_row_ordinal: r.u64("stripe first row")?,
            row_count: r.u64("stripe rows")?,
        });
    }

    let granules_bytes = require(
        decode_section(&directory, section_area, sections::GRANULES)?,
        "footer missing granule directory",
    )?;
    let mut r = Reader::new(granules_bytes);
    let count = r.u32("granule count")? as usize;
    // Each entry: u32 ids/rows (4 × 3) + u64 first_row/epochs/sequences/estimate (8 × 6) + i64 occurred/ingested
    // bounds (8 × 4) = 12 + 48 + 32 = 92 bytes minimum.
    let count = bounded_count(count, 92, &r, "granule count exceeds input")?;
    let mut granules = Vec::with_capacity(count);
    for _ in 0..count {
        granules.push(GranuleEntry {
            granule_id: r.u32("granule id")?,
            stripe_id: r.u32("granule stripe")?,
            first_row_ordinal: r.u64("granule first row")?,
            row_count: r.u32("granule rows")?,
            first_epoch: r.u64("granule first epoch")?,
            first_sequence: r.u64("granule first sequence")?,
            last_epoch: r.u64("granule last epoch")?,
            last_sequence: r.u64("granule last sequence")?,
            min_occurred_at_physical: r.i64("granule min occurred")?,
            max_occurred_at_physical: r.i64("granule max occurred")?,
            min_ingested_at_physical: r.i64("granule min ingested")?,
            max_ingested_at_physical: r.i64("granule max ingested")?,
            compressed_bytes_estimate: r.u64("granule bytes estimate")?,
        });
    }

    let marks_bytes = require(
        decode_section(&directory, section_area, sections::MARKS)?,
        "footer missing marks",
    )?;
    // Governed by the `columnar_marks` required feature (pinned spec: a section's interior encoding is feature-
    // governed, never a new ad-hoc layout). Columnar files carry a two-level per-stripe directory here instead of the
    // row-oriented struct array: only the (small) directory is parsed now, so a pruned stripe's marks page is never
    // decoded — see `decode_columnar_marks_page`.
    let (marks, marks_directory, marks_page_offsets, marks_pages) =
        if required_feature_flags & required_features::STRIPE_MARKS_PAGES != 0 {
            // Co-located placement: the directory's offsets are stripe-relative into the data area, and the pages
            // area behind it is empty — the reader slices each stripe's pages from the file itself.
            let (marks_page_offsets, _) = decode_columnar_marks_directory(marks_bytes)?;
            (Vec::new(), Vec::new(), marks_page_offsets, Vec::new())
        } else if required_feature_flags & required_features::COLUMNAR_MARKS != 0 {
            let (marks_directory, pages_area) = decode_columnar_marks_directory(marks_bytes)?;
            (Vec::new(), marks_directory, Vec::new(), pages_area.to_vec())
        } else {
            let mut r = Reader::new(marks_bytes);
            let count = r.u32("mark count")? as usize;
            // Each entry: u32 ids/rows/pages/pipeline (4 × 6) + u64 offsets/sizes (8 × 4) + option tag (1) = 24 +
            // 32 + 1 = 57 bytes minimum.
            let count = bounded_count(count, 57, &r, "mark count exceeds input")?;
            let mut marks = Vec::with_capacity(count);
            for _ in 0..count {
                marks.push(ColumnMark {
                    column_id: r.u32("mark column")?,
                    projection_id: r.u32("mark projection")?,
                    granule_id: r.u32("mark granule")?,
                    compressed_offset: r.u64("mark offset")?,
                    compressed_size: r.u64("mark size")?,
                    uncompressed_offset: r.u64("mark uoffset")?,
                    uncompressed_size: r.u64("mark usize")?,
                    row_count: r.u32("mark rows")?,
                    page_count: r.u32("mark pages")?,
                    codec_pipeline_id: PipelineId(r.u32("mark pipeline")?),
                    first_value_offset: read_opt_u64(&mut r)?,
                });
            }
            (marks, Vec::new(), Vec::new(), Vec::new())
        };

    let dictionaries_bytes = require(
        decode_section(&directory, section_area, sections::DICTIONARIES)?,
        "footer missing dictionaries",
    )?;
    let mut r = Reader::new(dictionaries_bytes);
    let dictionaries = FileDictionaries {
        source: read_dictionary(&mut r)?,
        event_type: read_dictionary(&mut r)?,
        entity_type: read_dictionary(&mut r)?,
    };

    let stats_bytes = require(
        decode_section(&directory, section_area, sections::PAGE_STATS)?,
        "footer missing page stats",
    )?;
    let mut r = Reader::new(stats_bytes);
    let count = r.u32("stats count")? as usize;
    // Each entry: u32 ids/counts (4 × 4) + 4 option tags (1 × 4) = 20 bytes minimum.
    let count = bounded_count(count, 20, &r, "stats count exceeds input")?;
    let mut page_stats = Vec::with_capacity(count);
    for _ in 0..count {
        page_stats.push(PageStats {
            column_id: r.u32("stats column")?,
            granule_id: r.u32("stats granule")?,
            row_count: r.u32("stats rows")?,
            null_count: r.u32("stats nulls")?,
            min_i128: read_opt_i128(&mut r)?,
            max_i128: read_opt_i128(&mut r)?,
            min_f64: read_opt_f64(&mut r)?,
            max_f64: read_opt_f64(&mut r)?,
        });
    }

    let counts_bytes = require(
        decode_section(&directory, section_area, sections::EXACT_COUNTS)?,
        "footer missing exact counts",
    )?;
    let mut r = Reader::new(counts_bytes);
    let exact_counts = ExactCounts {
        row_count: r.u64("exact rows")?,
        by_source: read_counts(&mut r)?,
        by_event_type: read_counts(&mut r)?,
        by_entity_type: read_counts(&mut r)?,
    };

    let payload_bytes = require(
        decode_section(&directory, section_area, sections::PAYLOAD_GRANULES)?,
        "footer missing payload granules",
    )?;
    let mut r = Reader::new(payload_bytes);
    let count = r.u32("payload granule count")? as usize;
    // Each entry: u32 id (4) + u64 dict/offsets/residual offset-len pairs (8 × 6) + compression tag (1) = 53 bytes
    // minimum.
    let count = bounded_count(count, 53, &r, "payload granule count exceeds input")?;
    let mut payload_granules = Vec::with_capacity(count);
    for _ in 0..count {
        payload_granules.push(PayloadGranule {
            granule_id: r.u32("payload granule id")?,
            dictionary_offset: r.u64("payload dict offset")?,
            dictionary_len: r.u64("payload dict len")?,
            offsets_offset: r.u64("payload offsets offset")?,
            offsets_len: r.u64("payload offsets len")?,
            residual_offset: r.u64("payload residual offset")?,
            residual_len: r.u64("payload residual len")?,
            residual_compression: match r.u8("payload residual compression")? {
                0 => ResidualCompression::None,
                1 => ResidualCompression::Zstd3,
                2 => ResidualCompression::ZstdSeekable,
                _ => {
                    return Err(FormatError::Structural {
                        rule: "unknown residual compression",
                    });
                }
            },
        });
    }

    let presence_bytes = require(
        decode_section(&directory, section_area, sections::PRESENCE)?,
        "footer missing presence map",
    )?;
    let mut r = Reader::new(presence_bytes);
    let count = r.u32("presence count")? as usize;
    // Each entry: u32 column id (4) + u32 since_schema_version (4) = 8 bytes minimum.
    let count = bounded_count(count, 8, &r, "presence count exceeds input")?;
    let mut presence = Vec::with_capacity(count);
    for _ in 0..count {
        presence.push(PresenceEntry {
            column_id: r.u32("presence column")?,
            since_schema_version: r.u32("presence since")?,
        });
    }

    let shredded_bytes = require(
        decode_section(&directory, section_area, sections::SHREDDED)?,
        "footer missing shred plan",
    )?;
    let mut r = Reader::new(shredded_bytes);
    let count = r.u32("shred count")? as usize;
    // Each entry: u32 column id (4) + path length prefix (4) = 8 bytes minimum.
    let count = bounded_count(count, 8, &r, "shred count exceeds input")?;
    let mut shredded = Vec::with_capacity(count);
    for _ in 0..count {
        shredded.push(ShredEntry {
            column_id: r.u32("shred column")?,
            path: read_string(&mut r)?,
        });
    }

    let freetext_bytes = require(
        decode_section(&directory, section_area, sections::FREETEXT)?,
        "footer missing freetext declaration",
    )?;
    let mut r = Reader::new(freetext_bytes);
    let count = r.u32("freetext count")? as usize;
    // Each entry: u32 column id (4) + declared_field length prefix (4) = 8 bytes minimum.
    let count = bounded_count(count, 8, &r, "freetext count exceeds input")?;
    let mut freetext = Vec::with_capacity(count);
    for _ in 0..count {
        freetext.push(FreetextEntry {
            column_id: r.u32("freetext column")?,
            declared_field: read_string(&mut r)?,
        });
    }

    let checksum_bytes = require(
        decode_section(&directory, section_area, sections::STRIPE_CHECKSUMS)?,
        "footer missing stripe checksums",
    )?;
    let mut r = Reader::new(checksum_bytes);
    let count = r.u32("stripe checksum count")? as usize;
    // Each entry is one BLAKE3 checksum (32 bytes).
    let count = bounded_count(count, 32, &r, "stripe checksum count exceeds input")?;
    let mut stripe_checksums = Vec::with_capacity(count);
    for _ in 0..count {
        let bytes = r.take(32, "stripe checksum")?;
        let mut checksum = [0u8; 32];
        checksum.copy_from_slice(bytes);
        stripe_checksums.push(checksum);
    }

    let gap_bytes = require(
        decode_section(&directory, section_area, sections::INTEGRITY_GAPS)?,
        "footer missing integrity gap commitments",
    )?;
    let mut r = Reader::new(gap_bytes);
    let count = r.u32("integrity gap count")? as usize;
    // Each entry: u64 offset (8) + u64 length (8) + BLAKE3 checksum (32) = 48 bytes minimum.
    let count = bounded_count(count, 48, &r, "integrity gap count exceeds input")?;
    let mut integrity_gaps = Vec::with_capacity(count);
    for _ in 0..count {
        let file_offset = r.u64("integrity gap offset")?;
        let byte_len = r.u64("integrity gap length")?;
        let mut blake3 = [0u8; 32];
        blake3.copy_from_slice(r.take(32, "integrity gap checksum")?);
        integrity_gaps.push(IntegrityGapEntry {
            blake3,
            byte_len,
            file_offset,
        });
    }

    let proof_bytes = require(
        decode_section(&directory, section_area, sections::STRIPE_PROOFS)?,
        "footer missing stripe proof geometry",
    )?;
    let mut r = Reader::new(proof_bytes);
    let count = r.u32("stripe proof count")? as usize;
    // Each entry: u32 id/chunk group (4 × 2) + u64 tree offset/length (8 × 2) = 24 bytes minimum.
    let count = bounded_count(count, 24, &r, "stripe proof count exceeds input")?;
    let mut stripe_proofs = Vec::with_capacity(count);
    for _ in 0..count {
        stripe_proofs.push(StripeProofEntry {
            stripe_id: r.u32("stripe proof id")?,
            chunk_group_bytes: r.u32("stripe proof chunk group")?,
            tree_offset: r.u64("stripe proof tree offset")?,
            tree_len: r.u64("stripe proof tree length")?,
        });
    }

    // Optional extension block: absent in files that do not declare an IO alignment, ignored by readers that predate
    // the feature.
    let io_alignment_bytes =
        if let Some(align_bytes) = decode_section(&directory, section_area, sections::IO_ALIGNMENT)? {
            let mut r = Reader::new(align_bytes);
            r.u32("io alignment bytes")?
        } else {
            0
        };

    // Optional extension block: absent in files that predate clustering metadata. Readers that know the id decode it;
    // others ignore it.
    let mut clustering = Vec::new();
    if let Some(clustering_bytes) = decode_section(&directory, section_area, sections::CLUSTERING_METADATA)? {
        let mut r = Reader::new(clustering_bytes);
        let count = r.u32("clustering entry count")? as usize;
        // Each entry: u32 ids/quality bits (4 × 3) + proof tag (1) = 13 bytes minimum.
        let count = bounded_count(count, 13, &r, "clustering entry count exceeds input")?;
        clustering = Vec::with_capacity(count);
        for _ in 0..count {
            let projection_id = r.u32("clustering projection id")?;
            let granule_id = r.u32("clustering granule id")?;
            let quality_bits = r.u32("clustering quality bits")?;
            let clustering_quality = f32::from_bits(quality_bits);
            let sortedness_proof = match r.u8("clustering proof tag")? {
                0 => None,
                1 => {
                    let direction = match r.u8("clustering sort direction")? {
                        0 => SortDirection::Ascending,
                        1 => SortDirection::Descending,
                        _ => {
                            return Err(FormatError::Structural {
                                rule: "unknown sort direction",
                            });
                        }
                    };
                    let name_count = r.u32("clustering column name count")? as usize;
                    // Each entry is a string, whose minimum encoding is its own u32 length prefix (4 bytes).
                    let name_count = bounded_count(name_count, 4, &r, "column name count exceeds input")?;
                    let mut column_names = Vec::with_capacity(name_count);
                    for _ in 0..name_count {
                        column_names.push(read_string(&mut r)?);
                    }
                    Some(SortednessProof {
                        column_names,
                        direction,
                    })
                }
                _ => {
                    return Err(FormatError::Structural {
                        rule: "unknown clustering proof tag",
                    });
                }
            };
            clustering.push(ClusteringEntry {
                clustering_quality,
                granule_id,
                projection_id,
                sortedness_proof,
            });
        }
    }

    // Optional extension block: absent in files that do not use it. A reader that knows the id reads it (its checksum
    // is validated like any section); one that does not simply ignores it.
    let mut page_minmax = Vec::new();
    if let Some(minmax_bytes) = decode_section(&directory, section_area, sections::PAGE_MINMAX)? {
        let mut r = Reader::new(minmax_bytes);
        let count = r.u32("page minmax count")? as usize;
        // Each entry: 5× u32 + 2× opt_i128 (min 5 bytes each), so 20 bytes minimum.
        let count = bounded_count(count, 20, &r, "page minmax count exceeds input")?;
        page_minmax = Vec::with_capacity(count);
        for _ in 0..count {
            page_minmax.push(PageMinMax {
                column_id: r.u32("page minmax column")?,
                granule_id: r.u32("page minmax granule")?,
                page_index: r.u32("page minmax page")?,
                row_count: r.u32("page minmax rows")?,
                null_count: r.u32("page minmax nulls")?,
                min_i128: read_opt_i128(&mut r)?,
                max_i128: read_opt_i128(&mut r)?,
            });
        }
    }

    let mut escape_hatches = Vec::new();
    if let Some(hatches_bytes) = decode_section(&directory, section_area, sections::ESCAPE_HATCHES)? {
        let mut r = Reader::new(hatches_bytes);
        let count = r.u32("escape hatch count")? as usize;
        let count = bounded_count(count, 16, &r, "escape hatch count exceeds input")?;
        escape_hatches = Vec::with_capacity(count);
        for _ in 0..count {
            let min_reader_version = r.u32("escape hatch min reader version")?;
            let portable_decoder_ref = read_string(&mut r)?;
            let optional_feature_bit = r.u64("escape hatch optional feature bit")?;
            escape_hatches.push(EscapeHatch {
                min_reader_version,
                optional_feature_bit,
                portable_decoder_ref,
            });
        }
    }

    // Free-text per-row byte-offset index: optional, absent in older files or files that do not declare
    // TYPED_COLUMN_ROW_OFFSETS. Min entry size: 4+4 (ids) + 8+8+8+8 (offsets/lens) = 40 bytes.
    let mut freetext_row_offsets = Vec::new();
    if let Some(entries_bytes) = decode_section(&directory, section_area, sections::FREETEXT_ROW_OFFSETS)? {
        let mut r = Reader::new(entries_bytes);
        let count = r.u32("freetext row offsets count")? as usize;
        let count = bounded_count(count, 40, &r, "freetext row offsets count exceeds input")?;
        freetext_row_offsets = Vec::with_capacity(count);
        for _ in 0..count {
            freetext_row_offsets.push(FreetextRowOffsets {
                column_id: r.u32("freetext row offsets column")?,
                granule_id: r.u32("freetext row offsets granule")?,
                offsets_offset: r.u64("freetext row offsets offset")?,
                offsets_len: r.u64("freetext row offsets len")?,
                bytes_offset: r.u64("freetext row bytes offset")?,
                bytes_len: r.u64("freetext row bytes len")?,
            });
        }
    }

    // Embedding/vector per-row byte-offset index: optional, absent in older files or files that do not declare
    // TYPED_COLUMN_ROW_OFFSETS. Min entry size: 4+4 (ids) + 8+8+8+8 (offsets/lens) = 40 bytes.
    let mut embedding_row_offsets = Vec::new();
    if let Some(entries_bytes) = decode_section(&directory, section_area, sections::EMBEDDING_ROW_OFFSETS)? {
        let mut r = Reader::new(entries_bytes);
        let count = r.u32("embedding row offsets count")? as usize;
        let count = bounded_count(count, 40, &r, "embedding row offsets count exceeds input")?;
        embedding_row_offsets = Vec::with_capacity(count);
        for _ in 0..count {
            embedding_row_offsets.push(EmbeddingRowOffsets {
                column_id: r.u32("embedding row offsets column")?,
                granule_id: r.u32("embedding row offsets granule")?,
                offsets_offset: r.u64("embedding row offsets offset")?,
                offsets_len: r.u64("embedding row offsets len")?,
                bytes_offset: r.u64("embedding row bytes offset")?,
                bytes_len: r.u64("embedding row bytes len")?,
            });
        }
    }

    // Per-page addressing directory: optional, absent in older files. Min entry size: 4+4+4+4 (ids/index) + 8+8+8
    // (offsets/len/row) + 4+4 (counts) + 1+1+1+1 (option tags) + 8+8+8+8 (seq/time) = 84 bytes.
    let mut page_directory = Vec::new();
    if let Some(dir_bytes) = decode_section(&directory, section_area, sections::PAGE_DIRECTORY)? {
        let mut r = Reader::new(dir_bytes);
        let count = r.u32("page directory count")? as usize;
        let count = bounded_count(count, 84, &r, "page directory count exceeds input")?;
        page_directory = Vec::with_capacity(count);
        for _ in 0..count {
            let entry = PageDirectoryEntry {
                column_id: r.u32("page dir column")?,
                projection_id: r.u32("page dir projection")?,
                granule_id: r.u32("page dir granule")?,
                page_index: r.u32("page dir index")?,
                compressed_offset: r.u64("page dir compressed offset")?,
                compressed_len: r.u64("page dir compressed len")?,
                first_row_ordinal: r.u64("page dir first row")?,
                row_count: r.u32("page dir rows")?,
                null_count: r.u32("page dir nulls")?,
                min_i128: read_opt_i128(&mut r)?,
                max_i128: read_opt_i128(&mut r)?,
                min_f64: read_opt_f64(&mut r)?,
                max_f64: read_opt_f64(&mut r)?,
                min_sequence: r.u64("page dir min seq")?,
                max_sequence: r.u64("page dir max seq")?,
                min_occurred_at_physical: r.i64("page dir min occurred")?,
                max_occurred_at_physical: r.i64("page dir max occurred")?,
            };
            validate_page_directory_entry(&entry)?;
            page_directory.push(entry);
        }
    }

    // Per-page string token filters: optional, absent in files written before the index existed. Min entry size:
    // 4+4+4 (ids/index) + 4 (byte length) = 16 bytes.
    let mut text_token_indexes = Vec::new();
    if let Some(token_bytes) = decode_section(&directory, section_area, sections::TEXT_TOKEN)? {
        let mut r = Reader::new(token_bytes);
        let count = r.u32("text token entry count")? as usize;
        let count = bounded_count(count, 16, &r, "text token entry count exceeds input")?;
        text_token_indexes = Vec::with_capacity(count);
        for _ in 0..count {
            let column_id = r.u32("text token column")?;
            let granule_id = r.u32("text token granule")?;
            let page_index = r.u32("text token page")?;
            let len = r.u32("text token index length")? as usize;
            let index_bytes = r.take(len, "text token index bytes")?.to_vec();
            text_token_indexes.push(TextTokenEntry {
                column_id,
                granule_id,
                index_bytes,
                page_index,
            });
        }
    }

    // Byte ranges of token filters stored in the data area: optional, absent unless the writer relocated the filter
    // bytes out of the footer. Fixed entry size: 4+4+4 (ids) + 8+8 (offset/len) = 28 bytes.
    let mut text_token_offsets = Vec::new();
    if let Some(offset_bytes) = decode_section(&directory, section_area, sections::TEXT_TOKEN_OFFSETS)? {
        let mut r = Reader::new(offset_bytes);
        let count = r.u32("text token offsets entry count")? as usize;
        let count = bounded_count(count, 28, &r, "text token offsets entry count exceeds input")?;
        text_token_offsets = Vec::with_capacity(count);
        for _ in 0..count {
            let column_id = r.u32("text token offsets column")?;
            let granule_id = r.u32("text token offsets granule")?;
            let page_index = r.u32("text token offsets page")?;
            let index_offset = r.u64("text token offsets offset")?;
            let index_len = r.u64("text token offsets length")?;
            text_token_offsets.push(TextTokenOffsetsEntry {
                column_id,
                granule_id,
                index_len,
                index_offset,
                page_index,
            });
        }
    }

    // Byte ranges of the per-granule identity-hash membership filters: optional, absent in files whose writer emitted
    // none. Fixed entry size: 4 (granule) + 8+8 (offset/len) = 20 bytes.
    let mut entity_hash_filters = Vec::new();
    if let Some(filter_bytes) = decode_section(&directory, section_area, sections::ENTITY_HASH_FILTERS)? {
        let mut r = Reader::new(filter_bytes);
        let count = r.u32("entity hash filter count")? as usize;
        let count = bounded_count(count, 20, &r, "entity hash filter count exceeds input")?;
        entity_hash_filters = Vec::with_capacity(count);
        for _ in 0..count {
            let granule_id = r.u32("entity hash filter granule")?;
            let index_offset = r.u64("entity hash filter offset")?;
            let index_len = r.u64("entity hash filter length")?;
            entity_hash_filters.push(EntityHashFilterEntry {
                granule_id,
                index_len,
                index_offset,
            });
        }
    }

    // The external-id index: optional. Fixed entry size: 8 (id hash) + 8 (row ordinal) = 16 bytes.
    let mut external_ids = Vec::new();
    if let Some(index_bytes) = decode_section(&directory, section_area, sections::EXTERNAL_IDS)? {
        let mut r = Reader::new(index_bytes);
        let count = r.u32("external id count")? as usize;
        let count = bounded_count(count, 16, &r, "external id count exceeds input")?;
        external_ids = Vec::with_capacity(count);
        for _ in 0..count {
            let id_hash = r.u64("external id hash")?;
            let row_ordinal = r.u64("external id row")?;
            external_ids.push(ExternalIdEntry { id_hash, row_ordinal });
        }
    }

    // Reference filters: optional. Minimum entry size: 4+4 (ids) + 4 (filter length) = 12 bytes.
    let mut reference_filters = None;
    if let Some(filter_bytes) = decode_section(&directory, section_area, sections::REFERENCE_FILTERS)? {
        let mut r = Reader::new(filter_bytes);
        let count = r.u32("reference filter count")? as usize;
        let count = bounded_count(count, 12, &r, "reference filter count exceeds input")?;
        let mut filters = Vec::with_capacity(count);
        for _ in 0..count {
            let column_id = r.u32("reference filter column")?;
            let granule_id = r.u32("reference filter granule")?;
            let len = r.u32("reference filter length")? as usize;
            let filter = r.take(len, "reference filter bytes")?.to_vec();
            filters.push(ReferenceFilterEntry {
                column_id,
                filter,
                granule_id,
            });
        }
        reference_filters = Some(filters);
    }

    // Per-(promoted column, stripe) distinct counts: optional planner statistics. Fixed entry size: 4+4 (ids) + 8
    // (count) + 1 (exact flag) = 17 bytes.
    let mut stripe_ndv = Vec::new();
    if let Some(ndv_bytes) = decode_section(&directory, section_area, sections::STRIPE_NDV)? {
        let mut r = Reader::new(ndv_bytes);
        let count = r.u32("stripe ndv entry count")? as usize;
        let count = bounded_count(count, 17, &r, "stripe ndv entry count exceeds input")?;
        stripe_ndv = Vec::with_capacity(count);
        for _ in 0..count {
            let column_id = r.u32("stripe ndv column")?;
            let stripe_id = r.u32("stripe ndv stripe")?;
            let distinct_count = r.u64("stripe ndv distinct count")?;
            let exact = r.u8("stripe ndv exact flag")? != 0;
            stripe_ndv.push(StripeNdvEntry {
                column_id,
                distinct_count,
                exact,
                stripe_id,
            });
        }
    }

    // File-scope dictionary alphabets: optional, bounded like every counted section.
    let mut shared_dictionaries = Vec::new();
    if let Some(shared_bytes) = decode_section(&directory, section_area, sections::SHARED_DICTIONARIES)? {
        let mut r = Reader::new(shared_bytes);
        let count = r.u32("shared dictionary count")? as usize;
        // Each entry: u32 column id (4) + u32 value count (4) = 8 bytes minimum.
        let count = bounded_count(count, 8, &r, "shared dictionary count exceeds input")?;
        shared_dictionaries = Vec::with_capacity(count);
        for _ in 0..count {
            let column_id = r.u32("shared dictionary column")?;
            let value_count = r.u32("shared dictionary value count")? as usize;
            // Each value's minimum encoding is its own u16 length prefix (2 bytes).
            let value_count = bounded_count(value_count, 2, &r, "shared dictionary value count exceeds input")?;
            let mut values = Vec::with_capacity(value_count);
            for _ in 0..value_count {
                let len = r.u16("shared dictionary value length")? as usize;
                values.push(
                    simdutf8::basic::from_utf8(r.take(len, "shared dictionary value")?)
                        .map_err(|_| FormatError::InvalidUtf8 {
                            what: "shared dictionary value",
                        })?
                        .to_owned(),
                );
            }
            // Sorted-code assignment is the contract every shared-scope evaluator binary-searches against; an
            // unsorted alphabet would make predicate pushdown silently diverge from decode-then-filter.
            if !values.windows(2).all(|pair| pair[0] < pair[1]) {
                return Err(FormatError::Structural {
                    rule: "shared dictionary alphabet must be strictly ascending",
                });
            }
            shared_dictionaries.push(SharedDictionaryEntry { column_id, values });
        }
    }

    // The sparse shredded key set: optional, bounded like every counted section.
    let mut sparse_keys = Vec::new();
    if let Some(sparse_bytes) = decode_section(&directory, section_area, sections::SPARSE_KEYS)? {
        let mut r = Reader::new(sparse_bytes);
        let count = r.u32("sparse key count")? as usize;
        // Each entry: u32 column id (4) + u16 path length prefix (2) = 6 bytes minimum.
        let count = bounded_count(count, 6, &r, "sparse key count exceeds input")?;
        sparse_keys = Vec::with_capacity(count);
        for _ in 0..count {
            let column_id = r.u32("sparse key column")?;
            let len = r.u16("sparse key path length")? as usize;
            let path = simdutf8::basic::from_utf8(r.take(len, "sparse key path")?)
                .map_err(|_| FormatError::InvalidUtf8 {
                    what: "sparse key path",
                })?
                .to_owned();
            sparse_keys.push(ShredEntry { column_id, path });
        }
    }

    Ok(Footer {
        clustering,
        columns,
        dictionaries,
        embedding_row_offsets,
        entity_hash_filters,
        escape_hatches,
        exact_counts,
        external_ids,
        format_version,
        freetext,
        freetext_row_offsets,
        granules,
        integrity_gaps,
        io_alignment_bytes,
        marks,
        marks_directory,
        marks_page_offsets,
        marks_pages,
        optional_feature_flags,
        page_directory,
        page_minmax,
        page_stats,
        payload_granules,
        presence,
        reference_filters,
        required_feature_flags,
        schema_fingerprint,
        shared_dictionaries,
        shredded,
        sparse_keys,
        stripe_checksums,
        stripe_proofs,
        stripe_ndv,
        stripes,
        text_token_indexes,
        text_token_offsets,
    })
}

/// Writes one `u64` field array with the format's own integer encodings — the same encoder a data column uses —
/// prefixed by its pipeline id and encoded byte length. Takes the field values as an iterator so a caller projecting
/// one field out of a struct array collects straight into the block the encoder reads, rather than into a temporary
/// the encoder then copies.
fn encode_u64_column(out: &mut Writer, values: impl IntoIterator<Item = u64>) {
    let encoded = encode_block(&ColumnData::U64(values.into_iter().collect()), false);
    out.put_u32(encoded.pipeline.0);
    out.put_u32(encoded.bytes.len() as u32);
    out.put_slice(&encoded.bytes);
}

/// Steps past one `u64` field array without decoding it, for a reader that wants a later field on the page.
fn skip_u64_column(reader: &mut Reader<'_>, what: &'static str) -> Result<(), FormatError> {
    let _pipeline = reader.u32("columnar marks column pipeline id")?;
    let len = reader.u32("columnar marks column byte length")? as usize;
    reader.take(len, what)?;
    Ok(())
}

/// Reads one `u64` field array written by [`encode_u64_column`].
fn decode_u64_column(reader: &mut Reader<'_>, what: &'static str) -> Result<Vec<u64>, FormatError> {
    let pipeline = PipelineId(reader.u32("columnar marks column pipeline id")?);
    let len = reader.u32("columnar marks column byte length")? as usize;
    let bytes = reader.take(len, what)?;
    match decode_block(pipeline, bytes)? {
        ColumnData::U64(values) => Ok(values),
        _ => Err(FormatError::Structural {
            rule: "columnar marks field must decode to a u64 column",
        }),
    }
}

fn u32_from_u64(value: u64, rule: &'static str) -> Result<u32, FormatError> {
    u32::try_from(value).map_err(|_| FormatError::Structural { rule })
}

/// Rejects a decoded page directory entry whose bounds cannot describe a real page.
///
/// The query planner forwards these bounds as exact page statistics and drops a page when a searched value falls
/// outside them, so an inverted or half-present pair fails *open*: every matching row in the page would be skipped. A
/// checksum only proves the bytes are the ones that were written, not that they are ordered, so the invariants the
/// builder upholds are re-checked here rather than trusted.
fn validate_page_directory_entry(entry: &PageDirectoryEntry) -> Result<(), FormatError> {
    if entry.null_count > entry.row_count {
        return Err(FormatError::Structural {
            rule: "page directory null count exceeds row count",
        });
    }
    match (entry.min_i128, entry.max_i128) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(FormatError::Structural {
                rule: "page directory integer bounds must be both present or both absent",
            });
        }
        (Some(min), Some(max)) if min > max => {
            return Err(FormatError::Structural {
                rule: "page directory integer min exceeds max",
            });
        }
        _ => {}
    }
    match (entry.min_f64, entry.max_f64) {
        (Some(_), None) | (None, Some(_)) => {
            return Err(FormatError::Structural {
                rule: "page directory float bounds must be both present or both absent",
            });
        }
        // `!(min <= max)` rather than `min > max` so a NaN bound — which no comparison can order, and which would
        // leave the pruning decision undefined — is rejected too.
        (Some(min), Some(max)) if !(min <= max) => {
            return Err(FormatError::Structural {
                rule: "page directory float min exceeds max",
            });
        }
        _ => {}
    }
    if entry.min_sequence > entry.max_sequence {
        return Err(FormatError::Structural {
            rule: "page directory min sequence exceeds max sequence",
        });
    }
    if entry.min_occurred_at_physical > entry.max_occurred_at_physical {
        return Err(FormatError::Structural {
            rule: "page directory min occurred_at exceeds max occurred_at",
        });
    }
    Ok(())
}

/// Encodes one `(projection, column, stripe)` group's marks — already sharing that key — as parallel FastLanes/DELTA-
/// encoded arrays of the mark fields, ordered by ascending `granule_id`. Paired with [`decode_marks_page`].
fn encode_marks_page(marks: &[ColumnMark], pages: &[PageDirectoryEntry]) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u32(marks.len() as u32);
    encode_u64_column(&mut out, marks.iter().map(|mark| u64::from(mark.granule_id)));
    encode_u64_column(&mut out, marks.iter().map(|mark| mark.compressed_offset));
    encode_u64_column(&mut out, marks.iter().map(|mark| mark.compressed_size));
    encode_u64_column(&mut out, marks.iter().map(|mark| mark.uncompressed_offset));
    encode_u64_column(&mut out, marks.iter().map(|mark| mark.uncompressed_size));
    encode_u64_column(&mut out, marks.iter().map(|mark| u64::from(mark.row_count)));
    encode_u64_column(&mut out, marks.iter().map(|mark| u64::from(mark.page_count)));
    encode_u64_column(&mut out, marks.iter().map(|mark| u64::from(mark.codec_pipeline_id.0)));
    for mark in marks {
        put_opt_u64(&mut out, mark.first_value_offset);
    }
    encode_page_directory_columns(&mut out, pages);
    out.into_bytes()
}

/// Encodes `pages`' fixed-width fields as parallel FastLanes/DELTA-encoded arrays — the per-page directory's columnar
/// form, riding the same per-stripe marks page as the group's marks. Paired with [`decode_page_directory_columns`].
fn encode_page_directory_columns(out: &mut Writer, pages: &[PageDirectoryEntry]) {
    out.put_u32(pages.len() as u32);
    encode_u64_column(out, pages.iter().map(|page| u64::from(page.granule_id)));
    encode_u64_column(out, pages.iter().map(|page| u64::from(page.page_index)));
    encode_u64_column(out, pages.iter().map(|page| page.compressed_offset));
    encode_u64_column(out, pages.iter().map(|page| page.compressed_len));
    encode_u64_column(out, pages.iter().map(|page| page.first_row_ordinal));
    encode_u64_column(out, pages.iter().map(|page| u64::from(page.row_count)));
    encode_u64_column(out, pages.iter().map(|page| u64::from(page.null_count)));
    encode_u64_column(out, pages.iter().map(|page| page.min_sequence));
    encode_u64_column(out, pages.iter().map(|page| page.max_sequence));
    encode_u64_column(out, pages.iter().map(|page| page.min_occurred_at_physical as u64));
    encode_u64_column(out, pages.iter().map(|page| page.max_occurred_at_physical as u64));
    for page in pages {
        put_opt_i128(out, page.min_i128);
        put_opt_i128(out, page.max_i128);
        put_opt_f64(out, page.min_f64);
        put_opt_f64(out, page.max_f64);
    }
}

/// Encodes one stripe's columnar marks pages for co-location in the data area beside the stripe's filter bytes: one
/// independently decodable page per `(projection, column)` group among `marks`, each carrying that group's marks and
/// per-page directory entries in the same columnar form as [`encode_columnar_marks`]'s footer pages. Returns the
/// directory entries — `page_offset` relative to the start of the returned blob; the writer rebases them to its
/// stripe-relative data offset — paired with the concatenated page bytes. Every mark and page entry passed in must
/// belong to `stripe_id`.
pub fn encode_stripe_marks_pages(
    stripe_id: u32,
    marks: &[ColumnMark],
    page_directory: &[PageDirectoryEntry],
) -> (Vec<MarksPageEntry>, Vec<u8>) {
    let mut groups: BTreeMap<(u32, u32), Vec<ColumnMark>> = BTreeMap::new();
    for mark in marks {
        groups
            .entry((mark.projection_id, mark.column_id))
            .or_default()
            .push(*mark);
    }
    let mut page_groups: BTreeMap<(u32, u32), Vec<PageDirectoryEntry>> = BTreeMap::new();
    for page in page_directory {
        page_groups
            .entry((page.projection_id, page.column_id))
            .or_default()
            .push(*page);
    }
    let mut pages = Vec::new();
    let mut directory = Vec::with_capacity(groups.len());
    for ((projection_id, column_id), mut group_marks) in groups {
        group_marks.sort_by_key(|mark| mark.granule_id);
        let mut group_pages = page_groups.remove(&(projection_id, column_id)).unwrap_or_default();
        group_pages.sort_by_key(|page| (page.granule_id, page.page_index));
        let page_offset = pages.len() as u64;
        let page_bytes = encode_marks_page(&group_marks, &group_pages);
        directory.push(MarksPageEntry {
            column_id,
            page_len: page_bytes.len() as u64,
            page_offset,
            projection_id,
            stripe_id,
        });
        pages.extend_from_slice(&page_bytes);
    }
    (directory, pages)
}

/// Decodes one `(projection, column, stripe)` group's page written by [`encode_marks_page`] back into that group's
/// marks, restoring `column_id` and `projection_id` from the directory entry that pointed at the page. The per-page
/// directory riding the same page is left for [`decode_marks_page_directory`], which most reads never call.
pub(crate) fn decode_marks_page(
    column_id: u32,
    projection_id: u32,
    bytes: &[u8],
) -> Result<Vec<ColumnMark>, FormatError> {
    let mut r = Reader::new(bytes);
    let count = r.u32("columnar marks page count")? as usize;
    // One byte per mark is the true minimum here: each mark's `first_value_offset` option tag is the only per-entry
    // byte read directly, and the eight field arrays are FastLanes/RLE-compressed, so a legitimate page can encode a
    // mark's column values in well under a byte. The `lens` check below re-validates `count` against the actually
    // decoded arrays before the marks vector is built.
    let count = bounded_count(count, 1, &r, "columnar marks page count exceeds input")?;

    let granule_ids = decode_u64_column(&mut r, "columnar marks granule ids")?;
    let compressed_offsets = decode_u64_column(&mut r, "columnar marks compressed offsets")?;
    let compressed_sizes = decode_u64_column(&mut r, "columnar marks compressed sizes")?;
    let uncompressed_offsets = decode_u64_column(&mut r, "columnar marks uncompressed offsets")?;
    let uncompressed_sizes = decode_u64_column(&mut r, "columnar marks uncompressed sizes")?;
    let row_counts = decode_u64_column(&mut r, "columnar marks row counts")?;
    let page_counts = decode_u64_column(&mut r, "columnar marks page counts")?;
    let codec_pipeline_ids = decode_u64_column(&mut r, "columnar marks codec pipeline ids")?;

    let lens = [
        granule_ids.len(),
        compressed_offsets.len(),
        compressed_sizes.len(),
        uncompressed_offsets.len(),
        uncompressed_sizes.len(),
        row_counts.len(),
        page_counts.len(),
        codec_pipeline_ids.len(),
    ];
    if lens.iter().any(|len| *len != count) {
        return Err(FormatError::Structural {
            rule: "columnar marks column length mismatch",
        });
    }

    let mut granule_ids = granule_ids.into_iter();
    let mut compressed_offsets = compressed_offsets.into_iter();
    let mut compressed_sizes = compressed_sizes.into_iter();
    let mut uncompressed_offsets = uncompressed_offsets.into_iter();
    let mut uncompressed_sizes = uncompressed_sizes.into_iter();
    let mut row_counts = row_counts.into_iter();
    let mut page_counts = page_counts.into_iter();
    let mut codec_pipeline_ids = codec_pipeline_ids.into_iter();

    let exhausted = || FormatError::Structural {
        rule: "columnar marks column exhausted before mark count",
    };
    let mut marks = Vec::with_capacity(count);
    for _ in 0..count {
        let first_value_offset = read_opt_u64(&mut r)?;
        marks.push(ColumnMark {
            codec_pipeline_id: PipelineId(u32_from_u64(
                codec_pipeline_ids.next().ok_or_else(exhausted)?,
                "columnar marks codec pipeline id exceeds u32",
            )?),
            column_id,
            compressed_offset: compressed_offsets.next().ok_or_else(exhausted)?,
            compressed_size: compressed_sizes.next().ok_or_else(exhausted)?,
            first_value_offset,
            granule_id: u32_from_u64(
                granule_ids.next().ok_or_else(exhausted)?,
                "columnar marks granule id exceeds u32",
            )?,
            page_count: u32_from_u64(
                page_counts.next().ok_or_else(exhausted)?,
                "columnar marks page count exceeds u32",
            )?,
            projection_id,
            row_count: u32_from_u64(
                row_counts.next().ok_or_else(exhausted)?,
                "columnar marks row count exceeds u32",
            )?,
            uncompressed_offset: uncompressed_offsets.next().ok_or_else(exhausted)?,
            uncompressed_size: uncompressed_sizes.next().ok_or_else(exhausted)?,
        });
    }
    Ok(marks)
}

/// The per-page directory entries riding one marks page, decoded on their own.
///
/// They sit behind the page's marks, and most reads never want them — a single-page block is addressed by its mark
/// alone — so they are skipped past on the marks decode and read here only for a caller that asks for a page.
pub(crate) fn decode_marks_page_directory(
    column_id: u32,
    projection_id: u32,
    bytes: &[u8],
) -> Result<Vec<PageDirectoryEntry>, FormatError> {
    let mut r = Reader::new(bytes);
    let count = r.u32("columnar marks page count")? as usize;
    let count = bounded_count(count, 1, &r, "columnar marks page count exceeds input")?;
    for what in [
        "columnar marks granule ids",
        "columnar marks compressed offsets",
        "columnar marks compressed sizes",
        "columnar marks uncompressed offsets",
        "columnar marks uncompressed sizes",
        "columnar marks row counts",
        "columnar marks page counts",
        "columnar marks codec pipeline ids",
    ] {
        skip_u64_column(&mut r, what)?;
    }
    // One option tag per mark sits between the field arrays and the directory.
    for _ in 0..count {
        read_opt_u64(&mut r)?;
    }
    decode_page_directory_columns(&mut r, column_id, projection_id)
}

/// The block extents one marks page's marks point at, as `(granule id, offset, size)` — the answer to "does another
/// mark share these bytes", without decoding the marks.
///
/// A reader needs this for every column of a stripe (an extent is shared or not only relative to all the others) but
/// needs whole marks only for the columns it actually reads. So this reads the three fields an extent is made of and
/// the page count that says whether the mark owns one, steps past the rest, and stops before the per-page directory
/// entirely. Marks that own no single extent — multi-page ones, and elided constants that store no bytes — are left
/// out: their extents either do not exist or coincide at zero length without sharing anything.
pub(crate) fn decode_marks_page_extents(bytes: &[u8]) -> Result<Vec<(u32, u64, u64)>, FormatError> {
    let mut r = Reader::new(bytes);
    let count = r.u32("columnar marks page count")? as usize;
    let count = bounded_count(count, 1, &r, "columnar marks page count exceeds input")?;

    let granule_ids = decode_u64_column(&mut r, "columnar marks granule ids")?;
    let compressed_offsets = decode_u64_column(&mut r, "columnar marks compressed offsets")?;
    let compressed_sizes = decode_u64_column(&mut r, "columnar marks compressed sizes")?;
    skip_u64_column(&mut r, "columnar marks uncompressed offsets")?;
    skip_u64_column(&mut r, "columnar marks uncompressed sizes")?;
    skip_u64_column(&mut r, "columnar marks row counts")?;
    let page_counts = decode_u64_column(&mut r, "columnar marks page counts")?;

    let lens = [
        granule_ids.len(),
        compressed_offsets.len(),
        compressed_sizes.len(),
        page_counts.len(),
    ];
    if lens.iter().any(|len| *len != count) {
        return Err(FormatError::Structural {
            rule: "columnar marks column length mismatch",
        });
    }

    let mut extents = Vec::with_capacity(count);
    for index in 0..count {
        let (Some(granule_id), Some(offset), Some(size), Some(page_count)) = (
            granule_ids.get(index),
            compressed_offsets.get(index),
            compressed_sizes.get(index),
            page_counts.get(index),
        ) else {
            return Err(FormatError::Structural {
                rule: "columnar marks column exhausted before mark count",
            });
        };
        if *page_count > 1 || *size == 0 {
            continue;
        }
        // The same oversize bound the whole-mark decode enforces, applied here so a stripe's forged mark is still
        // refused on the stripe's first touch rather than only once its own column is read.
        if *size > super::MAX_PAGE_BYTES {
            return Err(FormatError::Structural {
                rule: "page/chunk exceeds the maximum size",
            });
        }
        extents.push((
            u32_from_u64(*granule_id, "columnar marks granule id exceeds u32")?,
            *offset,
            *size,
        ));
    }
    Ok(extents)
}

/// Decodes the per-page directory columns [`encode_page_directory_columns`] wrote, restoring `column_id` and
/// `projection_id` from the directory entry that pointed at the enclosing marks page — the same key every entry in
/// the group shares.
fn decode_page_directory_columns(
    r: &mut Reader<'_>,
    column_id: u32,
    projection_id: u32,
) -> Result<Vec<PageDirectoryEntry>, FormatError> {
    let count = r.u32("columnar page directory count")? as usize;
    // Every entry reads four option tags (min/max i128, min/max f64) straight from the reader, so four bytes per
    // entry is the guaranteed minimum a valid page must still hold — a forged count beyond that cannot decode and
    // must not size any allocation.
    let count = bounded_count(count, 4, r, "columnar page directory count exceeds input")?;

    let granule_ids = decode_u64_column(r, "columnar page directory granule ids")?;
    let page_indices = decode_u64_column(r, "columnar page directory page indices")?;
    let compressed_offsets = decode_u64_column(r, "columnar page directory compressed offsets")?;
    let compressed_lens = decode_u64_column(r, "columnar page directory compressed lens")?;
    let first_row_ordinals = decode_u64_column(r, "columnar page directory first row ordinals")?;
    let row_counts = decode_u64_column(r, "columnar page directory row counts")?;
    let null_counts = decode_u64_column(r, "columnar page directory null counts")?;
    let min_sequences = decode_u64_column(r, "columnar page directory min sequences")?;
    let max_sequences = decode_u64_column(r, "columnar page directory max sequences")?;
    let min_occurred = decode_u64_column(r, "columnar page directory min occurred")?;
    let max_occurred = decode_u64_column(r, "columnar page directory max occurred")?;

    let lens = [
        granule_ids.len(),
        page_indices.len(),
        compressed_offsets.len(),
        compressed_lens.len(),
        first_row_ordinals.len(),
        row_counts.len(),
        null_counts.len(),
        min_sequences.len(),
        max_sequences.len(),
        min_occurred.len(),
        max_occurred.len(),
    ];
    if lens.iter().any(|len| *len != count) {
        return Err(FormatError::Structural {
            rule: "columnar page directory column length mismatch",
        });
    }

    let mut granule_ids = granule_ids.into_iter();
    let mut page_indices = page_indices.into_iter();
    let mut compressed_offsets = compressed_offsets.into_iter();
    let mut compressed_lens = compressed_lens.into_iter();
    let mut first_row_ordinals = first_row_ordinals.into_iter();
    let mut row_counts = row_counts.into_iter();
    let mut null_counts = null_counts.into_iter();
    let mut min_sequences = min_sequences.into_iter();
    let mut max_sequences = max_sequences.into_iter();
    let mut min_occurred = min_occurred.into_iter();
    let mut max_occurred = max_occurred.into_iter();

    let exhausted = || FormatError::Structural {
        rule: "columnar page directory column exhausted before entry count",
    };
    let mut page_directory = Vec::with_capacity(count);
    for _ in 0..count {
        let min_i128 = read_opt_i128(r)?;
        let max_i128 = read_opt_i128(r)?;
        let min_f64 = read_opt_f64(r)?;
        let max_f64 = read_opt_f64(r)?;
        let entry = PageDirectoryEntry {
            column_id,
            compressed_len: compressed_lens.next().ok_or_else(exhausted)?,
            compressed_offset: compressed_offsets.next().ok_or_else(exhausted)?,
            first_row_ordinal: first_row_ordinals.next().ok_or_else(exhausted)?,
            granule_id: u32_from_u64(
                granule_ids.next().ok_or_else(exhausted)?,
                "columnar page directory granule id exceeds u32",
            )?,
            max_f64,
            max_i128,
            max_occurred_at_physical: max_occurred.next().ok_or_else(exhausted)? as i64,
            max_sequence: max_sequences.next().ok_or_else(exhausted)?,
            min_f64,
            min_i128,
            min_occurred_at_physical: min_occurred.next().ok_or_else(exhausted)? as i64,
            min_sequence: min_sequences.next().ok_or_else(exhausted)?,
            null_count: u32_from_u64(
                null_counts.next().ok_or_else(exhausted)?,
                "columnar page directory null count exceeds u32",
            )?,
            page_index: u32_from_u64(
                page_indices.next().ok_or_else(exhausted)?,
                "columnar page directory page index exceeds u32",
            )?,
            projection_id,
            row_count: u32_from_u64(
                row_counts.next().ok_or_else(exhausted)?,
                "columnar page directory row count exceeds u32",
            )?,
        };
        validate_page_directory_entry(&entry)?;
        page_directory.push(entry);
    }
    Ok(page_directory)
}

/// Groups `marks` and `page_directory` by `(projection, column, stripe)` — using `granule_stripe_ids` to place each
/// mark's and each page entry's granule in its stripe — and encodes the two-level columnar marks block: one
/// [`MarksPageEntry`] per group, followed by the group pages the directory points into. Each page holds that group's
/// mark fields (`compressed_offset`, `compressed_size`, `row_count`, and the other fixed-width fields) as parallel
/// FastLanes/DELTA-encoded arrays, exactly as a data column is encoded, and — riding the same page — that group's
/// per-page directory entries (offsets, lengths, row ranges, and per-page stats) columnar in the same way. Governed by
/// the `columnar_marks` required feature; the row-oriented `MARKS` and `PAGE_DIRECTORY` section content keeps decoding
/// independently of this block, so a writer may emit both during migration.
pub fn encode_columnar_marks(
    marks: &[ColumnMark],
    page_directory: &[PageDirectoryEntry],
    granule_stripe_ids: &BTreeMap<u32, u32>,
) -> Result<Vec<u8>, FormatError> {
    let mut groups: BTreeMap<(u32, u32, u32), Vec<ColumnMark>> = BTreeMap::new();
    for mark in marks {
        let stripe_id = *granule_stripe_ids
            .get(&mark.granule_id)
            .ok_or(FormatError::Structural {
                rule: "columnar marks encode missing granule stripe mapping",
            })?;
        groups
            .entry((mark.projection_id, mark.column_id, stripe_id))
            .or_default()
            .push(*mark);
    }

    let mut page_groups: BTreeMap<(u32, u32, u32), Vec<PageDirectoryEntry>> = BTreeMap::new();
    for page in page_directory {
        let stripe_id = *granule_stripe_ids
            .get(&page.granule_id)
            .ok_or(FormatError::Structural {
                rule: "columnar marks encode missing granule stripe mapping",
            })?;
        page_groups
            .entry((page.projection_id, page.column_id, stripe_id))
            .or_default()
            .push(*page);
    }

    let mut pages = Vec::new();
    let mut directory = Vec::with_capacity(groups.len());
    for ((projection_id, column_id, stripe_id), mut group_marks) in groups {
        group_marks.sort_by_key(|mark| mark.granule_id);
        let mut group_pages = page_groups
            .remove(&(projection_id, column_id, stripe_id))
            .unwrap_or_default();
        group_pages.sort_by_key(|page| (page.granule_id, page.page_index));
        let page_offset = pages.len() as u64;
        let page_bytes = encode_marks_page(&group_marks, &group_pages);
        let page_len = page_bytes.len() as u64;
        pages.extend_from_slice(&page_bytes);
        directory.push(MarksPageEntry {
            column_id,
            page_len,
            page_offset,
            projection_id,
            stripe_id,
        });
    }

    let mut out = Writer::new();
    out.put_u32(directory.len() as u32);
    for entry in &directory {
        out.put_u32(entry.projection_id);
        out.put_u32(entry.column_id);
        out.put_u32(entry.stripe_id);
        out.put_u64(entry.page_offset);
        out.put_u64(entry.page_len);
    }
    out.put_slice(&pages);
    Ok(out.into_bytes())
}

/// Decodes just the two-level directory of a columnar marks block written by [`encode_columnar_marks`], leaving every
/// group's page bytes undecoded. Paired with [`decode_columnar_marks_page`]: a reader parses this once (cheap — one
/// entry per `(projection, column, stripe)`, not per granule) and then decodes only the pages for stripes that survive
/// pruning, so a pruned stripe never pays for a mark it will not read. Returns the directory and the remaining pages
/// area the directory's `page_offset`/`page_len` are relative to.
pub fn decode_columnar_marks_directory(bytes: &[u8]) -> Result<(Vec<MarksPageEntry>, &[u8]), FormatError> {
    let mut r = Reader::new(bytes);
    let group_count = r.u32("columnar marks directory count")? as usize;
    // Each entry: u32 ids (4 × 3) + u64 page offset/len (8 × 2) = 28 bytes minimum.
    let group_count = bounded_count(group_count, 28, &r, "columnar marks directory count exceeds input")?;
    let mut directory = Vec::with_capacity(group_count);
    for _ in 0..group_count {
        let projection_id = r.u32("columnar marks directory projection")?;
        let column_id = r.u32("columnar marks directory column")?;
        let stripe_id = r.u32("columnar marks directory stripe")?;
        let page_offset = r.u64("columnar marks directory page offset")?;
        let page_len = r.u64("columnar marks directory page len")?;
        directory.push(MarksPageEntry {
            column_id,
            page_len,
            page_offset,
            projection_id,
            stripe_id,
        });
    }
    let pages_area = r.take(r.remaining(), "columnar marks pages")?;
    Ok((directory, pages_area))
}

/// Decodes one directory entry's marks and per-page directory entries out of the pages area
/// [`decode_columnar_marks_directory`] returned — the lazy, per-group counterpart that only ever touches the bytes
/// for that one `(projection, column, stripe)`.
pub fn decode_columnar_marks_page(
    pages_area: &[u8],
    entry: &MarksPageEntry,
) -> Result<(Vec<ColumnMark>, Vec<PageDirectoryEntry>), FormatError> {
    let page_bytes = slice(
        pages_area,
        entry.page_offset as usize,
        entry.page_len as usize,
        "columnar marks page",
    )?;
    Ok((
        decode_marks_page(entry.column_id, entry.projection_id, page_bytes)?,
        decode_marks_page_directory(entry.column_id, entry.projection_id, page_bytes)?,
    ))
}

/// Decodes a two-level columnar marks block written by [`encode_columnar_marks`] back into the same [`ColumnMark`]
/// and [`PageDirectoryEntry`] entries the row-oriented forms hold — the columnar and row-oriented forms decode to a
/// byte-identical logical directory. Decodes every group eagerly; a reader that wants to skip pruned stripes should
/// use [`decode_columnar_marks_directory`] and [`decode_columnar_marks_page`] instead.
pub fn decode_columnar_marks(bytes: &[u8]) -> Result<(Vec<ColumnMark>, Vec<PageDirectoryEntry>), FormatError> {
    let (directory, pages_area) = decode_columnar_marks_directory(bytes)?;
    let mut marks = Vec::new();
    let mut page_directory = Vec::new();
    for entry in &directory {
        let (group_marks, group_pages) = decode_columnar_marks_page(pages_area, entry)?;
        marks.extend(group_marks);
        page_directory.extend(group_pages);
    }
    Ok((marks, page_directory))
}

#[cfg(test)]
#[path = "test/footer.rs"]
mod tests;
