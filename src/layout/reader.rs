//! Opens a stored file, checks it is intact, and reads columns and payload values out of it.
//!
//! Opening validates the file end to end: the header checksums, the versioned footer, refuse required-feature
//! gating, every section's checksum, and — when the caller supplies the catalogue entry's seal — the
//! domain-separated segment seal derived from the authenticated stripe roots and the remaining stored regions.
//! Reads then go through the marks, the authoritative map from `(column, projection, granule)` to a byte range, and a
//! single payload field is fetched by offset navigation rather than scanning its siblings.
//!
//! Every column read follows one of two named modes. **Search-cache init** ([`HefFile::cached_column`], used by
//! [`HefFile::payload`], [`HefFile::payload_path`] and [`HefFile::read_payload_paths`]) decodes a column block once, builds a rank index over its
//! presence bitmap, and caches both — paying that cost once so repeated point lookups into the same granule are
//! O(log runs) instead of a re-decode each time. A per-value-addressable block short-circuits even that: a cold
//! point read decodes just the requested row through its byte-range path and never initializes the cache. **Cold-scan** ([`HefFile::bulk_read_family`], and any caller that
//! walks granules sequentially through [`HefFile::read_column`]/[`HefFile::read_page`] directly) never touches the
//! cache or builds a rank index: a query that reads every row once gets nothing from an index it will only consult a
//! single time. The invariant: a cold scan must never pay the search-cache's initialization cost.

use super::constant::{FOOTER_TAIL_TRAILER_LEN, HEF_MAGIC};
use super::footer::{
    ColumnMark, EmbeddingRowOffsets, Footer, FreetextRowOffsets, GranuleEntry, MarksPageEntry, PageDirectoryEntry,
    PageStats, PayloadGranule, ResidualCompression, StripeEntry, TextTokenOffsetsEntry, decode_footer,
    decode_marks_page, decode_marks_page_directory, decode_marks_page_extents,
};
use super::remote::FileBytes;
use super::{EMPTY_VALUE_ROW_OFFSET, HEADER_BLOCK_LEN, HefHeader, decode_header, optional_features, required_features};
use crate::artifacts::batch::decode_variant_dictionary;
use crate::columns::column_ids;
use crate::compat;
use crate::encoding::decompressor::active_decompressor;
use crate::encoding::{
    ColumnData, Compression, PipelineId, PresenceRank, SharedStringViewDictionary, SideStream, Transform, ValueKind,
    count_set_bits, decode_block_range_shared, decode_block_shared, decode_string_block_views_shared_prepared,
    prepare_shared_string_view_dictionary, present_position, seekable_zstd,
};
use crate::error::FormatError;
use crate::events::provenance::hex_lower;
use crate::events::relationships::{RelationshipKind, RelationshipRef, TargetIdSpace};
use crate::events::variant::{KeyDictionary, PathSegment, VariantRef, VariantValue};
use crate::file::bytes::{Reader, slice};
use crate::file::constant::{TREE_TRAILER_LEN, TREE_TRAILER_MAGIC};
use crate::file::integrity::RangeFault;
use crate::indexes::bitmap::RoaringRangeBitmap;
use crate::indexes::probabilistic::SplitBlockBloomFilter;
use crate::indexes::rank_select::RankSelect;
use crate::indexes::text_token::TextTokenIndex;
use crate::security::AeadScheme;
use arrow_array::builder::StringViewBuilder;
use arrow_array::{Array, StringViewArray};
use dashmap::{DashMap, DashSet};
use hashbrown::{HashMap, HashSet};
use memchr::memmem;
use rayon::prelude::*;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};
use zeekstd::SeekTable;

#[path = "scan.rs"]
pub mod scan;
pub use scan::*;

/// Concatenates two `ColumnData` values of the same variant. Returns an error when the variants differ (a structural
/// violation — multi-page marks within one granule always encode the same column type).
fn concat_column_data(a: ColumnData, b: ColumnData) -> Result<ColumnData, FormatError> {
    match (a, b) {
        (ColumnData::U64(mut av), ColumnData::U64(bv)) => {
            av.extend(bv);
            Ok(ColumnData::U64(av))
        }
        (ColumnData::I64(mut av), ColumnData::I64(bv)) => {
            av.extend(bv);
            Ok(ColumnData::I64(av))
        }
        (ColumnData::U128(mut av), ColumnData::U128(bv)) => {
            av.extend(bv);
            Ok(ColumnData::U128(av))
        }
        (ColumnData::F64(mut av), ColumnData::F64(bv)) => {
            av.extend(bv);
            Ok(ColumnData::F64(av))
        }
        (ColumnData::Strings(mut av), ColumnData::Strings(bv)) => {
            av.append(&bv);
            Ok(ColumnData::Strings(av))
        }
        (
            ColumnData::Decimal { values: mut av, scale },
            ColumnData::Decimal {
                values: bv,
                scale: b_scale,
            },
        ) => {
            if scale != b_scale {
                return Err(FormatError::Structural {
                    rule: "multi-page mark pages must have the same decimal scale",
                });
            }
            av.extend(bv);
            Ok(ColumnData::Decimal { values: av, scale })
        }
        _ => Err(FormatError::Structural {
            rule: "multi-page mark pages must have the same column type",
        }),
    }
}

/// A distinct-count estimate for one column, combined across a file's stripes from footer metadata alone — what the
/// planner reads for cardinality estimation without scanning any column bytes.
///
/// See: hef-aggregation-metadata/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnDistinctEstimate {
    pub distinct_count: u64,
    pub exact: bool,
}

/// A row's late-materialized payload.
#[derive(Debug, Clone, PartialEq)]
pub enum PayloadRead {
    /// Internal immutable payload reference (never public output).
    External(String),
    None,
    /// The reconstructed canonical value: the deterministic merge of shredded typed values and the residual value.
    Value(VariantValue),
}

/// One row whose payload a caller wants from a batched read (see [`HefFile::read_payloads`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PayloadRef {
    pub row_ordinal: u64,
}

/// The results of a batched payload read: one entry per requested reference, in the caller's original order, each
/// identical to what [`HefFile::payload`] returns for the same row.
#[derive(Debug, Clone, PartialEq)]
pub struct PayloadBatch {
    pub payloads: Vec<PayloadRead>,
}

/// One row's residual payload bytes, held where they already are rather than copied out: a span of the open file's own
/// buffer for an uncompressed granule, or a span of the granule's shared inflated arena for a compressed one. Derefs
/// to those bytes, so a caller reads it exactly like a slice.
enum ResidualBytes<'a> {
    InArena {
        arena: Arc<Vec<u8>>,
        end: usize,
        start: usize,
    },
    InFile(&'a [u8]),
}

impl std::ops::Deref for ResidualBytes<'_> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            // Both bounds were checked against this arena when the span was taken, so the range is always in bounds.
            ResidualBytes::InArena { arena, end, start } => arena.get(*start..*end).unwrap_or_default(),
            ResidualBytes::InFile(bytes) => bytes,
        }
    }
}

/// One column block read through its mark: presence bitmap (for shredded/free-text/promoted columns) plus the decoded
/// data.
#[derive(Debug, Clone)]
pub struct ColumnRead {
    pub data: ColumnData,
    pub presence: Vec<u8>,
}

/// One column block (or page) located but not yet decoded: the codec pipeline to decode `body` with, the presence
/// bitmap mapping stored values to rows, and the stored row count the decode must produce. Lets a caller hand the
/// encoded bytes straight to a kernel that builds an Arrow array, skipping the `ColumnData` materialization.
#[derive(Debug, Clone)]
pub struct RawColumnBlock<'a> {
    pub body: &'a [u8],
    pub pipeline: PipelineId,
    /// Borrowed from the block's own bytes when the file stores the bitmap verbatim, so a repeated point read costs
    /// no copy; owned only for the forms that have to be rebuilt (runs, positions, all-present, all-absent).
    pub presence: Cow<'a, [u8]>,
    pub row_count: u32,
}

/// One column block decoded once and paired with a rank index over its presence bitmap, cached per
/// `(column_id, granule_id)` so per-row payload reconstruction decodes each column once instead of re-decoding the whole
/// block on every row.
///
/// For a shredded/free-text column the dense `data` holds only the values for present rows: `rank.rank(row)` gives a
/// row's dense position — the number of present rows before it — and `rank.rank(row + 1) > rank.rank(row)` reports
/// whether the row itself is present, both in O(log runs). Dense columns (e.g. payload flags) carry a value for every
/// row and index `data` directly by row, so they never consult the rank. Caching this is what keeps reconstruction
/// linear in rows rather than quadratic.
#[derive(Debug)]
struct CachedColumn {
    data: ColumnData,
    rank: RankSelect,
}

impl CachedColumn {
    /// One row's value out of this already-decoded block, or `None` when the row carries no value for the column.
    ///
    /// A row carries a value exactly when its presence bit is set, and `before` is then its dense position among the
    /// present rows. Both come from the rank index in one O(log runs) search, so reading every row of a granule is
    /// linear in rows, not quadratic.
    fn value_at(&self, row_in_granule: u64) -> Option<VariantValue> {
        let (before, present) = self.rank.rank_and_contains(row_in_granule);
        if !present {
            return None;
        }
        HefFile::typed_variant_at(&self.data, before as usize)
    }
}

/// One decoded block held in the reader's search cache, with the accounting eviction needs: its stable decoded byte
/// size and the access tick it was last touched at.
#[derive(Debug)]
struct CachedColumnSlot {
    bytes: u64,
    column: Arc<CachedColumn>,
    last_used: u64,
}

/// One granule's column block for the path a batched read is asking for, located once and then answered for every one
/// of that granule's rows in the batch.
#[derive(Debug)]
enum PathBlock<'a> {
    /// The block's encoded bytes and presence bitmap, fetched once, with a rank over that bitmap built alongside it:
    /// each row's own value is decoded straight out of them by its position, so no row after the first re-fetches the
    /// block, re-decodes its presence stream, or re-counts the presence prefix ahead of its row.
    Located {
        rank: PresenceRank,
        raw: RawColumnBlock<'a>,
    },
    /// A block with no per-value form — multi-page, an elided constant, or a pipeline that cannot address one value —
    /// decoded whole once for the granule and answered from its rank index.
    Whole(Arc<CachedColumn>),
}

/// The state a batched single-path payload read carries while it visits one granule: which columns the requested path
/// resolves to (fixed for the whole call), the granule being visited, and that granule's column block — resolved at
/// most once for the granule, however many of its rows the batch asks for.
#[derive(Debug)]
struct PathGroup<'a> {
    block: Option<PathBlock<'a>>,
    freetext_column: Option<u32>,
    granule: &'a GranuleEntry,
    shredded_column: Option<u32>,
}

/// The state a batched full-payload read ([`HefFile::read_payloads`]) carries while it visits one granule: every
/// accelerator a row's reconstruction touches — the payload-flags block, the key dictionary, and each shredded/
/// free-text column's block — resolved at most once for the granule, however many of its rows the batch asks for,
/// instead of once per row (a mutex lock and an atomic tick per column per row, even on a cache hit).
#[derive(Debug)]
struct PayloadGroup<'a> {
    dictionary: Option<Arc<KeyDictionary>>,
    field_blocks: HashMap<u32, PathBlock<'a>>,
    flags: Arc<CachedColumn>,
    granule: &'a GranuleEntry,
    payload: &'a PayloadGranule,
}

/// One inflated span of a granule's residual arena, with the same eviction accounting as [`CachedColumnSlot`].
#[derive(Debug)]
struct InflatedResidualSlot {
    arena: Arc<Vec<u8>>,
    last_used: u64,
}

/// One granule's decoded payload key dictionary, with the same eviction accounting as [`CachedColumnSlot`].
#[derive(Debug)]
struct DictionarySlot {
    bytes: u64,
    dictionary: Arc<KeyDictionary>,
    last_used: u64,
}

/// One decoded block for an extent that more than one mark references (mark aliasing), so a scan touching every
/// aliasing column fetches and decodes the shared bytes once. Same eviction accounting as [`CachedColumnSlot`].
#[derive(Debug)]
struct AliasedExtentSlot {
    bytes: u64,
    last_used: u64,
    read: Arc<ColumnRead>,
}

/// Deterministic byte accounting for a decoded block held in the reader's caches: the value buffers' sizes plus a
/// small fixed per-string overhead — a string column's span and presence flag, on top of its shared text arena.
/// Approximate but stable, so eviction decisions replay identically under deterministic simulation.
fn column_data_bytes(data: &ColumnData) -> u64 {
    match data {
        ColumnData::U64(values) => (values.len() * 8) as u64,
        ColumnData::I64(values) => (values.len() * 8) as u64,
        ColumnData::F64(values) => (values.len() * 8) as u64,
        ColumnData::U128(values) => (values.len() * 16) as u64,
        ColumnData::Decimal { values, .. } => (values.len() * 16) as u64,
        ColumnData::Strings(values) => (values.len() * 9 + values.text_len()) as u64,
    }
}

/// Deterministic byte accounting for a decoded key dictionary held in the reader's caches: each key's text plus the
/// fixed per-`String` overhead. Approximate but stable, like [`column_data_bytes`].
fn dictionary_bytes(dictionary: &KeyDictionary) -> u64 {
    dictionary.keys().map(|key| key.len() as u64 + 24).sum()
}

/// Yields the row index of every set bit in a packed LSB-0 presence bitmap, in ascending order. A test-only ground
/// truth for the rank/bitmap machinery built over presence streams.
#[cfg(test)]
fn presence_set_rows(presence: &[u8]) -> impl Iterator<Item = u64> + '_ {
    presence.iter().enumerate().flat_map(|(byte_index, &byte)| {
        (0..8u32).filter_map(move |bit| (byte & (1 << bit) != 0).then_some((byte_index as u64) * 8 + u64::from(bit)))
    })
}

/// Checks that a decoded block's value count agrees with how many rows its mark (or page directory entry) declares, so a
/// corrupt or truncated block is rejected instead of silently misaligning or dropping rows.
///
/// A *dense* block carries a value for every row and stores no presence bitmap (`presence` is empty): it must decode
/// exactly `declared_rows` values. A *sparse* block (shredded/free-text/promoted columns) stores only the present rows'
/// values behind a presence bitmap: it must decode exactly one value per present row — the number of set bits — and its
/// bitmap must be long enough to cover all `declared_rows` with no present bit landing at or beyond them. Without these
/// checks a block that decodes to fewer values than it declares would read past the true data or leave later rows
/// pointing at the wrong values.
fn validate_block_counts(presence: &[u8], decoded_rows: usize, declared_rows: usize) -> Result<(), FormatError> {
    if presence.is_empty() {
        if decoded_rows != declared_rows {
            return Err(FormatError::Structural {
                rule: "dense column block value count must equal its declared row count",
            });
        }
        return Ok(());
    }
    let present = count_set_bits(presence, presence.len() * 8);
    if present != decoded_rows {
        return Err(FormatError::Structural {
            rule: "sparse column block value count must equal its presence set-bit count",
        });
    }
    if presence.len() * 8 < declared_rows {
        return Err(FormatError::Structural {
            rule: "presence bitmap too short to cover its declared row count",
        });
    }
    // Every bit at or beyond `declared_rows` must be clear. Rather than walking every set bit of the whole bitmap,
    // check just its tail: the boundary byte's high bits (the ones at/after `declared_rows` within that byte) and
    // every whole byte after it — the only bytes any out-of-range bit could live in.
    let boundary_byte = declared_rows / 8;
    let boundary_bit = declared_rows % 8;
    if let Some(&byte) = presence.get(boundary_byte) {
        let high_bits = 0xFFu8 << boundary_bit;
        if byte & high_bits != 0 {
            return Err(FormatError::Structural {
                rule: "presence bit set at or beyond the declared row count",
            });
        }
    }
    if presence
        .get(boundary_byte.saturating_add(1)..)
        .is_some_and(|tail| tail.iter().any(|&byte| byte != 0))
    {
        return Err(FormatError::Structural {
            rule: "presence bit set at or beyond the declared row count",
        });
    }
    Ok(())
}

fn append_presence_bits(dst: &mut Vec<u8>, dst_start_bit: usize, src: &[u8], len: usize) {
    if len == 0 {
        return;
    }
    let needed = (dst_start_bit + len).div_ceil(8);
    if dst.len() < needed {
        dst.resize(needed, 0);
    }
    if dst_start_bit % 8 != 0 {
        // Not byte-aligned (a preceding page's own row count wasn't a multiple of 8): shift-merge one bit at a time.
        for bit in 0..len {
            if src.get(bit / 8).is_some_and(|byte| byte & (1 << (bit % 8)) != 0) {
                let dst_bit = dst_start_bit + bit;
                if let Some(byte) = dst.get_mut(dst_bit / 8) {
                    *byte |= 1 << (dst_bit % 8);
                }
            }
        }
        return;
    }
    // Byte-aligned: every interior byte lines up between src and dst, so it copies whole rather than bit by bit;
    // only the trailing partial byte (when len isn't a multiple of 8) needs a mask.
    let dst_byte_start = dst_start_bit / 8;
    let full_bytes = len / 8;
    for i in 0..full_bytes {
        if let Some(dst_byte) = dst.get_mut(dst_byte_start + i) {
            *dst_byte |= src.get(i).copied().unwrap_or(0);
        }
    }
    let rem = len % 8;
    if rem > 0
        && let Some(dst_byte) = dst.get_mut(dst_byte_start + full_bytes)
    {
        let mask = (1u8 << rem) - 1;
        *dst_byte |= src.get(full_bytes).copied().unwrap_or(0) & mask;
    }
}

/// Marks `len` dense rows present in a combined multi-page presence bitmap. A page-local dense block carries no
/// bitmap at all, but once any sibling page is sparse the column-level bitmap must explicitly mark the dense page's
/// rows or they would be mistaken for gaps.
fn append_all_present_bits(dst: &mut Vec<u8>, dst_start_bit: usize, len: usize) {
    if len == 0 {
        return;
    }
    let needed = (dst_start_bit + len).div_ceil(8);
    if dst.len() < needed {
        dst.resize(needed, 0);
    }
    for bit in dst_start_bit..dst_start_bit + len {
        if let Some(byte) = dst.get_mut(bit / 8) {
            *byte |= 1 << (bit % 8);
        }
    }
}

/// Validates `bytes` as UTF-8 and hands back an owned `String`.
fn owned_utf8(bytes: &[u8], what: &'static str) -> Result<String, FormatError> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| FormatError::InvalidUtf8 { what })
}

/// How many trailing bytes to read when the manifest entry does not record `footer_len`: a fixed speculative fetch that
/// covers the common footer in one request. The exact size is a benchmark question, not a format commitment — the
/// footer-length word inside the fetched bytes gives the precise length for a single exact retry on underflow.
pub const SPECULATIVE_TAIL_BYTES: u64 = 256 * 1024;

/// Cold point reads of one shredded block before the reader decodes the whole granule into the column cache. A point
/// read costs single-digit microseconds and a whole-granule decode plus rank build costs on the order of a hundred
/// times that, so this sits well under the crossover: a scattered probe pattern stays on the cheap point path, while a
/// dense per-row reconstruction loop migrates to the rank-indexed cache instead of paying a point read for every row.
const POINT_READS_BEFORE_WHOLE_GRANULE_DECODE: u32 = 32;

/// Default byte budget for the reader's decoded-block caches (`column_cache` and `inflated_residuals` together, each
/// individually held under it). A long-lived reader over a day-scale file holds a bounded working set instead of every
/// block it ever decoded; eviction is performance-only — an evicted block re-decodes through the normal verified path.
/// Distinct from the page-granular object cache below the decoder, which budgets verified bytes; this budgets decoded
/// blocks above it.
pub const DECODED_CACHE_BUDGET_BYTES: u64 = 256 * 1024 * 1024;

/// Entry cap on the point-probe counters. The counters are a heuristic only, so when a reader has probed this many
/// distinct blocks the map is simply reset — correctness-neutral, and it keeps a reader that touches millions of
/// blocks from growing the map forever.
const POINT_PROBE_ENTRIES_MAX: usize = 1 << 20;

/// Byte length of the AES-256 file DEK (`footer_dek`) used to decrypt a sealed footer.
const FOOTER_DEK_LEN: usize = 32;

/// Byte length of the manifest's domain-separated file seal (`expected_seal`), a BLAKE3-derived commitment over the
/// header, footer, and stripe checksums — see [`crate::integrity::derive_file_seal_from_header_hash`].
const SEAL_LEN: usize = 32;

/// Byte width of one row's stored `(offset: u32, len: u32)` entry in the payload/freetext/embedding per-row
/// byte-offset indexes.
const ROW_OFFSET_ENTRY_LEN: usize = 8;

/// Which trailing bytes of a stored file a reader should fetch to open it: where the tail starts, how many bytes to
/// read, and whether that range is the exact footer geometry or a speculative guess that may need one retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TailRange {
    /// True when `footer_len` was known, so this is the exact last `footer_len + tree_len` bytes; false for a
    /// speculative fetch that `HefFooter::open_speculative` may ask the caller to retry once at the exact length.
    pub exact: bool,
    pub len: u64,
    pub start: u64,
}

/// Sizes the tail GET for a cold open. With `footer_len` (and `tree_len` when the file carries an outboard tree) the
/// range is exact — the last `footer_len + tree_len` bytes, so the open is a single request. Without `footer_len` it is
/// a speculative last-`SPECULATIVE_TAIL_BYTES` read that [`HefFooter::open_speculative`] may ask the caller to retry
/// once at the precise length read from the footer-length word.
pub fn tail_range(file_size: u64, footer_len: Option<u64>, tree_len: Option<u64>) -> TailRange {
    match footer_len {
        Some(footer_len) => {
            let len = footer_len.saturating_add(tree_len.unwrap_or(0)).min(file_size);
            TailRange {
                exact: true,
                len,
                start: file_size - len,
            }
        }
        None => {
            let len = SPECULATIVE_TAIL_BYTES.min(file_size);
            TailRange {
                exact: false,
                len,
                start: file_size - len,
            }
        }
    }
}

/// The tail of a stored file opened on its own — just the footer directories a reader needs to plan a scan, obtained
/// from the trailing bytes without ever reading the front-of-file header.
///
/// A planning-only cold open is one exact tail GET followed by [`HefFooter::open`]. Before serving data, an untrusted
/// remote reader also fetches the fixed header block and uses [`HefFooter::open_authenticated`] to bind the footer's
/// stripe roots and proof geometry to the manifest seal. Whole-file [`HefFile::open`] remains the eager fallback.
#[derive(Debug, Clone)]
pub struct HefFooter {
    authenticated: bool,
    footer: Footer,
    proof_trees: Vec<u8>,
    usable_optional_features: u64,
}

/// One exact content GET needed for an authenticated stripe-relative range. The request expands to complete proof
/// leaves; [`HefFooter::verify_stripe_range`] trims it back to the originally requested bytes after verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedStripeRead {
    pub file_offset: u64,
    pub length: u64,
    proof_length: u64,
    proof_offset: u64,
    requested_length: u64,
    requested_offset: u64,
    pub stripe_id: u32,
}

/// What a reader needs to know about the fixed header block to authenticate a footer it fetched without the header:
/// the block's BLAKE3, which the manifest seal binds, plus the two header fields the open itself consults. A publisher
/// records it beside the seal (see [`HeaderCommitment::from_header_block`]) so a cold open is the tail request alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderCommitment {
    pub blake3: [u8; 32],
    /// The header's required-feature bits — the footer's own plus `FOOTER_ENCRYPTED` when the footer is sealed.
    pub feature_flags: u64,
    pub file_id: u128,
}

impl HeaderCommitment {
    /// Reads the commitment off a file's fixed header block, checking the block first.
    pub fn from_header_block(header_block: &[u8]) -> Result<Self, FormatError> {
        let header = decode_header(header_block)?;
        Ok(Self::of(header_block, &header))
    }

    fn of(header_block: &[u8], header: &HefHeader) -> Self {
        Self {
            blake3: *crate::file::integrity::hash_tree(header_block).as_bytes(),
            feature_flags: header.feature_flags,
            file_id: header.file_id,
        }
    }
}

/// A byte range inside one stripe, measured from the stripe's first byte: what a cold reader asks the object store
/// for, one column block or marks page at a time, through [`HefFooter::plan_verified_stripe_read`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StripeRange {
    pub length: u64,
    pub stripe_id: u32,
    pub stripe_offset: u64,
}

/// The result of opening from a speculative tail whose exact length was not known before the fetch.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the footer variant is the common outcome and is moved out at once; boxing it would cost an allocation per open"
)]
pub enum SpeculativeTail {
    /// The speculative fetch was shorter than the footer. Re-fetch exactly the last `tail_len` bytes of the object and
    /// call [`HefFooter::open`] on them — the one exact retry.
    NeedsExactTail { tail_len: u64 },
    /// The speculative fetch covered the whole footer; the footer is open.
    Opened(HefFooter),
}

/// Reads the outboard tree trailer's declared length from the end of `tail`, without requiring the tree body itself to
/// be present — only the fixed trailer (the length word plus magic) needs to be in hand. Returns `None` when `tail`
/// does not end in the trailer magic (no outboard tree).
fn outboard_tree_len(tail: &[u8]) -> Result<Option<u64>, FormatError> {
    if tail.len() < 4 || slice(tail, tail.len() - 4, 4, "object trailing magic")? != TREE_TRAILER_MAGIC {
        return Ok(None);
    }
    if tail.len() < TREE_TRAILER_LEN {
        return Err(FormatError::Truncated {
            what: "outboard tree trailer",
        });
    }
    let len_bytes = slice(tail, tail.len() - TREE_TRAILER_LEN, 8, "outboard tree length")?;
    Ok(Some(u64::from_le_bytes(len_bytes.try_into().unwrap_or([0; 8]))))
}

/// Strips an outboard BLAKE3 tree trailer (`[tree][tree_len u64]["HEFT"]`) from the end of `tail`, returning the content
/// tail whose last bytes are the file's own footer. A tail that ends without the trailer magic is already content.
fn strip_outboard_tree(tail: &[u8]) -> Result<&[u8], FormatError> {
    let Some(tree_len) = outboard_tree_len(tail)? else {
        return Ok(tail);
    };
    let content_end = tail
        .len()
        .checked_sub(TREE_TRAILER_LEN)
        .and_then(|n| n.checked_sub(tree_len as usize))
        .ok_or(FormatError::Truncated { what: "outboard tree" })?;
    Ok(slice(tail, 0, content_end, "outboard content tail")?)
}

/// Splits a complete object tail into its HEF content tail and concatenated per-stripe proof-tree bytes.
fn split_outboard_tree(tail: &[u8]) -> Result<(&[u8], &[u8]), FormatError> {
    let Some(tree_len) = outboard_tree_len(tail)? else {
        return Ok((tail, &[]));
    };
    let content_end = tail
        .len()
        .checked_sub(TREE_TRAILER_LEN)
        .and_then(|n| n.checked_sub(tree_len as usize))
        .ok_or(FormatError::Truncated { what: "outboard tree" })?;
    Ok((
        slice(tail, 0, content_end, "outboard content tail")?,
        slice(tail, content_end, tree_len as usize, "stripe proof trees")?,
    ))
}

/// Validates that the authenticated proof directory is one exact partition of the appendix and matches every stripe's
/// expected BLAKE3 tree shape. Proof bytes remain non-authoritative: a corrupt node is rejected when traversed.
fn validate_stripe_proofs(footer: &Footer, proof_trees: &[u8]) -> Result<(), FormatError> {
    if footer.stripe_proofs.len() != footer.stripes.len() {
        return Err(FormatError::Structural {
            rule: "stripe proof count must match the stripe directory",
        });
    }
    let mut cursor = 0u64;
    let mut seen = HashSet::with_capacity(footer.stripe_proofs.len());
    for stripe in &footer.stripes {
        let proof = footer
            .stripe_proofs
            .iter()
            .find(|proof| proof.stripe_id == stripe.stripe_id)
            .ok_or(FormatError::Structural {
                rule: "every stripe must have proof geometry",
            })?;
        if !seen.insert(proof.stripe_id) {
            return Err(FormatError::Structural {
                rule: "stripe proof ids must be unique",
            });
        }
        let group = proof.chunk_group_bytes as usize;
        if group < blake3::CHUNK_LEN || !group.is_power_of_two() || group % blake3::CHUNK_LEN != 0 {
            return Err(FormatError::Structural {
                rule: "stripe proof chunk group must be a power-of-two BLAKE3 chunk multiple",
            });
        }
        let stripe_len = usize::try_from(stripe.byte_len).map_err(|_| FormatError::Structural {
            rule: "stripe proof input length does not fit in memory",
        })?;
        let expected_len = crate::file::integrity::outboard_tree_bytes(stripe_len, group) as u64;
        if proof.tree_offset != cursor || proof.tree_len != expected_len {
            return Err(FormatError::Structural {
                rule: "stripe proof entries must exactly partition the expected outboard trees",
            });
        }
        cursor = cursor.checked_add(proof.tree_len).ok_or(FormatError::Structural {
            rule: "stripe proof range overflows",
        })?;
    }
    // An object whose appendix is missing altogether (no `"HEFT"` trailer) is still the same sealed content: the
    // trees are non-authoritative and rebuildable, so the footer opens and every range read into a stripe whose tree
    // is missing takes the whole-stripe fallback (see `HefFooter::verify_stripe_range_in`). An appendix that is
    // present but disagrees with the authenticated geometry is refused.
    if cursor != proof_trees.len() as u64 && !proof_trees.is_empty() {
        return Err(FormatError::Structural {
            rule: "stripe proof directory must cover the complete proof appendix",
        });
    }
    if seen.len() != footer.stripe_proofs.len() {
        return Err(FormatError::Structural {
            rule: "stripe proof directory must not contain unknown stripe ids",
        });
    }
    Ok(())
}

/// Reads the footer-length word from the end of a content tail (`[…][footer blob][footer_len u64]["HEF1"]`), after
/// validating the trailing `"HEF1"` magic. This is the byte length of the footer blob, so the whole footer region is
/// `footer_len + FOOTER_TAIL_TRAILER_LEN` bytes (blob + the length word + the magic).
fn footer_blob_len(content: &[u8]) -> Result<usize, FormatError> {
    if content.len() < FOOTER_TAIL_TRAILER_LEN {
        return Err(FormatError::Truncated { what: "footer tail" });
    }
    if slice(
        content,
        content.len() - HEF_MAGIC.len(),
        HEF_MAGIC.len(),
        "hef tail magic",
    )? != HEF_MAGIC
    {
        return Err(FormatError::BadMagic { expected: "HEF1" });
    }
    let len_bytes = slice(content, content.len() - FOOTER_TAIL_TRAILER_LEN, 8, "footer length")?;
    Ok(u64::from_le_bytes(len_bytes.try_into().unwrap_or([0; 8])) as usize)
}

/// Decrypts the sealed footer region at the end of a whole-file body tail (`[nonce||ciphertext][enc_len u64]["HEF1"]`)
/// with the file DEK, and re-frames the recovered plaintext blob as a bare content tail
/// (`[footer blob][footer_len u64]["HEF1"]`) that [`HefFooter::open`] decodes exactly like a plaintext file. The file
/// identity is the associated data the seal bound, so a footer lifted onto another file — or opened with the wrong key
/// — fails the authentication tag and is rejected rather than surfacing altered bytes.
fn decrypt_footer_tail(body_tail: &[u8], dek: &[u8; FOOTER_DEK_LEN], file_id: u128) -> Result<Vec<u8>, FormatError> {
    let scheme = AeadScheme::AesGcm256;
    let enc_len = footer_blob_len(body_tail)?;
    let nonce_len = scheme.nonce_len();
    if enc_len < nonce_len {
        return Err(FormatError::Truncated {
            what: "encrypted footer nonce",
        });
    }
    // Subtract in two checked steps: `FOOTER_TAIL_TRAILER_LEN + enc_len` would overflow before a single `checked_sub`
    // could run when a hostile length word is near `usize::MAX`.
    let enc_start = body_tail
        .len()
        .checked_sub(FOOTER_TAIL_TRAILER_LEN)
        .and_then(|n| n.checked_sub(enc_len))
        .ok_or(FormatError::Truncated {
            what: "encrypted footer",
        })?;
    let nonce = slice(body_tail, enc_start, nonce_len, "encrypted footer nonce")?;
    let ciphertext = slice(
        body_tail,
        enc_start + nonce_len,
        enc_len - nonce_len,
        "encrypted footer ciphertext",
    )?;
    let plaintext = scheme
        .open(dek, nonce, &file_id.to_le_bytes(), ciphertext)
        .ok_or(FormatError::Blake3Mismatch {
            scope: "encrypted footer",
        })?;
    let blob_len = plaintext.len() as u64;
    let mut content = plaintext;
    content.extend_from_slice(&blob_len.to_le_bytes());
    content.extend_from_slice(&HEF_MAGIC);
    Ok(content)
}

impl HefFooter {
    /// Opens the footer from the file's trailing bytes alone. `tail_bytes` must end at the end of the object and hold
    /// the whole footer region — and the outboard tree trailer, when present. A shorter tail is a truncation error; use
    /// [`HefFooter::open_speculative`] when the exact length is not yet known.
    pub fn open(tail_bytes: &[u8]) -> Result<Self, FormatError> {
        let (content, proof_trees) = split_outboard_tree(tail_bytes)?;
        let blob_len = footer_blob_len(content)?;
        Self::from_content_tail(content, blob_len, proof_trees, false)
    }

    /// Opens the footer from a speculative tail of unknown exact length. Returns [`SpeculativeTail::NeedsExactTail`]
    /// with the precise byte count to re-fetch when the speculative read underflowed the footer, so the caller retries
    /// exactly once at the exact length rather than growing the fetch blindly.
    pub fn open_speculative(tail_bytes: &[u8]) -> Result<SpeculativeTail, FormatError> {
        match Self::tail_shortfall(tail_bytes)? {
            Some(tail_len) => Ok(SpeculativeTail::NeedsExactTail { tail_len }),
            None => Ok(SpeculativeTail::Opened(Self::open(tail_bytes)?)),
        }
    }

    /// Whether a speculative tail is long enough to open: `None` when `tail_bytes` holds the whole footer region (and
    /// the outboard tree trailer, when present), so [`HefFooter::open`] will open it; otherwise the exact number of
    /// trailing bytes to re-fetch, so the caller retries exactly once at that length rather than growing the fetch
    /// blindly.
    pub fn tail_shortfall(tail_bytes: &[u8]) -> Result<Option<u64>, FormatError> {
        if let Some(tree_len) = outboard_tree_len(tail_bytes)?
            && (tail_bytes.len() as u64) < (TREE_TRAILER_LEN as u64).saturating_add(tree_len)
        {
            // The speculative window doesn't even hold the whole outboard tree yet, so the footer beneath it is
            // unreachable at this length. Ask for the tree plus one more speculative footer window instead of failing
            // closed on the tree's own truncation.
            return Ok(Some(
                tree_len
                    .saturating_add(TREE_TRAILER_LEN as u64)
                    .saturating_add(SPECULATIVE_TAIL_BYTES),
            ));
        }
        let (content, _) = split_outboard_tree(tail_bytes)?;
        let blob_len = footer_blob_len(content)?;
        // The whole footer region is the blob plus its length word and magic (FOOTER_TAIL_TRAILER_LEN bytes).
        // Compared with the subtraction rather than `blob_len + FOOTER_TAIL_TRAILER_LEN`, which a hostile length word
        // near `usize::MAX` would overflow; `footer_blob_len` already guaranteed
        // `content.len() >= FOOTER_TAIL_TRAILER_LEN`.
        if content.len() - FOOTER_TAIL_TRAILER_LEN >= blob_len {
            return Ok(None);
        }
        // Underflow: the exact object tail is the footer region plus whatever tree trailer sat after the magic.
        let tree_trailer = (tail_bytes.len() - content.len()) as u64;
        Ok(Some(
            (blob_len as u64)
                .saturating_add(FOOTER_TAIL_TRAILER_LEN as u64)
                .saturating_add(tree_trailer),
        ))
    }

    /// Opens an exact remote tail and authenticates its stripe roots and proof geometry against the manifest seal using
    /// only the fixed header block. No stripe or alignment-padding bytes are fetched. An encrypted footer is decrypted
    /// with `footer_dek` after its stored ciphertext has been bound into the seal.
    pub fn open_authenticated(
        header_block: &[u8],
        tail_bytes: &[u8],
        file_size: u64,
        expected_seal: &[u8; SEAL_LEN],
        footer_dek: Option<&[u8; FOOTER_DEK_LEN]>,
    ) -> Result<Self, FormatError> {
        let header = decode_header(header_block)?;
        let opened = Self::open_authenticated_tail(
            tail_bytes,
            file_size,
            expected_seal,
            &HeaderCommitment::of(header_block, &header),
            footer_dek,
        )?;
        if header.row_count != opened.footer.exact_counts.row_count {
            return Err(FormatError::Structural {
                rule: "header row_count must match exact counts",
            });
        }
        Ok(opened)
    }

    /// [`open_authenticated`](Self::open_authenticated) without the header block: the manifest's recorded
    /// [`HeaderCommitment`] stands in for it, so a cold open is the single tail request. Every check the header-backed
    /// open makes on the tail is made here too; only the header's own row count, which the sealed footer's exact
    /// counts already carry, goes unchecked.
    pub fn open_authenticated_tail(
        tail_bytes: &[u8],
        file_size: u64,
        expected_seal: &[u8; SEAL_LEN],
        header: &HeaderCommitment,
        footer_dek: Option<&[u8; FOOTER_DEK_LEN]>,
    ) -> Result<Self, FormatError> {
        compat::check_features(header.feature_flags, 0)?;
        let (raw_content_tail, proof_trees) = split_outboard_tree(tail_bytes)?;
        let raw_blob_len = footer_blob_len(raw_content_tail)?;
        let raw_blob_start = raw_content_tail
            .len()
            .checked_sub(FOOTER_TAIL_TRAILER_LEN)
            .and_then(|n| n.checked_sub(raw_blob_len))
            .ok_or(FormatError::Truncated { what: "footer" })?;
        let raw_footer = slice(raw_content_tail, raw_blob_start, raw_blob_len, "footer region")?;
        let raw_trailer = slice(
            raw_content_tail,
            raw_content_tail.len() - FOOTER_TAIL_TRAILER_LEN,
            FOOTER_TAIL_TRAILER_LEN,
            "footer trailer",
        )?;

        let mut opened = if header.feature_flags & required_features::FOOTER_ENCRYPTED != 0 {
            let dek = footer_dek.ok_or(FormatError::Structural {
                rule: "encrypted footer requires a file DEK to open",
            })?;
            let plaintext_tail = decrypt_footer_tail(raw_content_tail, dek, header.file_id)?;
            let plaintext_len = footer_blob_len(&plaintext_tail)?;
            Self::from_content_tail(&plaintext_tail, plaintext_len, proof_trees, true)?
        } else {
            Self::from_content_tail(raw_content_tail, raw_blob_len, proof_trees, true)?
        };

        let appendix_len = (tail_bytes.len() - raw_content_tail.len()) as u64;
        let content_len = file_size.checked_sub(appendix_len).ok_or(FormatError::Truncated {
            what: "HEF content length",
        })?;
        let footer_len = (raw_blob_len as u64).saturating_add(FOOTER_TAIL_TRAILER_LEN as u64);
        let data_end = content_len
            .checked_sub(footer_len)
            .ok_or(FormatError::Truncated { what: "HEF data area" })?;
        let data_end_usize = usize::try_from(data_end).map_err(|_| FormatError::Structural {
            rule: "HEF data length does not fit in memory",
        })?;
        let commitments = crate::integrity::declared_data_commitments(
            data_end_usize,
            &opened.footer.stripes,
            &opened.footer.stripe_checksums,
            &opened.footer.integrity_gaps,
        )?;
        let actual = crate::integrity::derive_file_seal_from_header_hash(
            HEADER_BLOCK_LEN as u64,
            &header.blake3,
            raw_footer,
            raw_trailer,
            content_len,
            &commitments,
        );
        if &actual != expected_seal {
            return Err(FormatError::Blake3Mismatch { scope: "hef file seal" });
        }
        opened.authenticated = true;
        Ok(opened)
    }

    fn from_content_tail(
        content: &[u8],
        blob_len: usize,
        proof_trees: &[u8],
        authenticated: bool,
    ) -> Result<Self, FormatError> {
        // Subtract in two checked steps: `FOOTER_TAIL_TRAILER_LEN + blob_len` would overflow before a single
        // `checked_sub` could run when a hostile length word is near `usize::MAX`.
        let blob_start = content
            .len()
            .checked_sub(FOOTER_TAIL_TRAILER_LEN)
            .and_then(|n| n.checked_sub(blob_len))
            .ok_or(FormatError::Truncated { what: "footer" })?;
        let footer_blob = slice(content, blob_start, blob_len, "footer blob")?;
        let footer = decode_footer(footer_blob)?;
        // Versioned-footer and feature gates: unknown required refuses, unknown optional is ignored.
        compat::check_format_version(footer.format_version.0, footer.format_version.1)?;
        let usable_optional_features =
            compat::check_features(footer.required_feature_flags, footer.optional_feature_flags)?;
        validate_stripe_proofs(&footer, proof_trees)?;
        Ok(Self {
            authenticated,
            footer,
            proof_trees: proof_trees.to_vec(),
            usable_optional_features,
        })
    }

    /// Plans the exact file range needed to prove `length` bytes at `stripe_offset`. The returned GET is expanded only
    /// to the proof leaf boundaries it intersects; a stripe too small to have an outboard tree — or whose tree the
    /// object no longer carries — is fetched whole.
    pub fn plan_verified_stripe_read(
        &self,
        stripe_id: u32,
        stripe_offset: u64,
        length: u64,
    ) -> Result<VerifiedStripeRead, FormatError> {
        self.plan_stripe_read(stripe_id, stripe_offset, length, false)
    }

    /// Plans the whole-stripe fetch that proves `length` bytes at `stripe_offset` without the outboard tree: the
    /// fallback when [`Self::verify_stripe_range_in`] found the tree unusable. The stripe's bytes are hashed whole
    /// against the seal-authenticated stripe checksum and only the requested range is served.
    pub fn plan_whole_stripe_read(
        &self,
        stripe_id: u32,
        stripe_offset: u64,
        length: u64,
    ) -> Result<VerifiedStripeRead, FormatError> {
        self.plan_stripe_read(stripe_id, stripe_offset, length, true)
    }

    fn plan_stripe_read(
        &self,
        stripe_id: u32,
        stripe_offset: u64,
        length: u64,
        whole: bool,
    ) -> Result<VerifiedStripeRead, FormatError> {
        if !self.authenticated {
            return Err(FormatError::Structural {
                rule: "verified stripe reads require a manifest-authenticated footer",
            });
        }
        let stripe = self
            .footer
            .stripes
            .iter()
            .find(|stripe| stripe.stripe_id == stripe_id)
            .ok_or(FormatError::RefOutOfRange { what: "stripe id" })?;
        let end = stripe_offset
            .checked_add(length)
            .ok_or(FormatError::RefOutOfRange { what: "stripe range" })?;
        if end > stripe.byte_len {
            return Err(FormatError::RefOutOfRange { what: "stripe range" });
        }
        if stripe.byte_len == 0 {
            return Err(FormatError::Structural {
                rule: "verified streaming does not admit empty stripes",
            });
        }
        let proof = self
            .footer
            .stripe_proofs
            .iter()
            .find(|proof| proof.stripe_id == stripe_id)
            .ok_or(FormatError::Structural {
                rule: "stripe proof geometry missing",
            })?;
        // Bao's final-chunk rule: even an empty logical read proves one byte's leaf. At EOF it proves the final leaf.
        let effective_offset = if length == 0 {
            stripe_offset.min(stripe.byte_len - 1)
        } else {
            stripe_offset
        };
        let effective_end = if length == 0 { effective_offset + 1 } else { end };
        let (proof_offset, proof_end) = if whole || proof.tree_len == 0 || self.proof_trees.is_empty() {
            (0, stripe.byte_len)
        } else {
            let group = u64::from(proof.chunk_group_bytes);
            (
                (effective_offset / group) * group,
                effective_end.div_ceil(group).saturating_mul(group).min(stripe.byte_len),
            )
        };
        let proof_length = proof_end.checked_sub(proof_offset).ok_or(FormatError::Structural {
            rule: "stripe proof fetch range is inverted",
        })?;
        Ok(VerifiedStripeRead {
            file_offset: stripe
                .file_offset
                .checked_add(proof_offset)
                .ok_or(FormatError::Structural {
                    rule: "verified stripe file offset overflows",
                })?,
            length: proof_length,
            proof_length: effective_end - effective_offset,
            proof_offset: effective_offset,
            requested_length: length,
            requested_offset: stripe_offset,
            stripe_id,
        })
    }

    /// Verifies bytes returned for `read` against the manifest-authenticated stripe root and returns only the caller's
    /// requested subrange. No byte is exposed before its complete proof path validates.
    pub fn verify_stripe_range<'a>(
        &self,
        read: &VerifiedStripeRead,
        fetched: &'a [u8],
    ) -> Result<&'a [u8], FormatError> {
        if !self.authenticated || fetched.len() as u64 != read.length {
            return Err(FormatError::Structural {
                rule: "verified stripe response does not match its authenticated read plan",
            });
        }
        self.verify_stripe_range_in(read, fetched, read.file_offset)
            .map_err(|_| FormatError::Blake3Mismatch { scope: "stripe proof" })
    }

    /// [`verify_stripe_range`](Self::verify_stripe_range) when the planned bytes arrived inside a larger fetch —
    /// one GET that several planned reads of the same stripe were coalesced into — starting at file offset
    /// `fetched_file_offset`. Returns the caller's requested subrange once its proof path reaches the stripe root,
    /// and otherwise says which way to refuse: [`RangeFault::Corrupt`] means the bytes do not match the
    /// authenticated root and must not be served; [`RangeFault::TreeUnusable`] means the outboard tree is missing or
    /// malformed, so the caller should fetch the stripe whole ([`Self::plan_whole_stripe_read`]) and try again —
    /// a fetch that covers the whole stripe is hashed against the stripe checksum directly and never consults the
    /// tree.
    pub fn verify_stripe_range_in<'a>(
        &self,
        read: &VerifiedStripeRead,
        fetched: &'a [u8],
        fetched_file_offset: u64,
    ) -> Result<&'a [u8], RangeFault> {
        if !self.authenticated {
            return Err(RangeFault::Corrupt);
        }
        let position = self
            .footer
            .stripes
            .iter()
            .position(|stripe| stripe.stripe_id == read.stripe_id)
            .ok_or(RangeFault::Corrupt)?;
        let stripe = self.footer.stripes.get(position).ok_or(RangeFault::Corrupt)?;
        let checksum = self.footer.stripe_checksums.get(position).ok_or(RangeFault::Corrupt)?;
        let fetched_start = fetched_file_offset
            .checked_sub(stripe.file_offset)
            .ok_or(RangeFault::Corrupt)?;
        let fetched_end = fetched_start
            .checked_add(fetched.len() as u64)
            .ok_or(RangeFault::Corrupt)?;
        let planned_end = read
            .proof_offset
            .checked_add(read.proof_length)
            .ok_or(RangeFault::Corrupt)?;
        if fetched_end > stripe.byte_len || read.proof_offset < fetched_start || planned_end > fetched_end {
            return Err(RangeFault::Corrupt);
        }
        if fetched_start == 0 && fetched_end == stripe.byte_len {
            // The whole stripe is in hand: its checksum is the seal-authenticated root itself, so hash it directly.
            let matches = crate::file::integrity::hash_segments(&[fetched])
                .first()
                .is_some_and(|root| root.as_bytes() == checksum);
            if !matches {
                return Err(RangeFault::Corrupt);
            }
        } else {
            let proof = self
                .footer
                .stripe_proofs
                .iter()
                .find(|proof| proof.stripe_id == read.stripe_id)
                .ok_or(RangeFault::TreeUnusable)?;
            let tree = slice(
                &self.proof_trees,
                proof.tree_offset as usize,
                proof.tree_len as usize,
                "stripe proof tree",
            )
            .map_err(|_| RangeFault::TreeUnusable)?;
            crate::file::integrity::verify_range_from_slice(
                stripe.byte_len,
                fetched,
                fetched_start,
                tree,
                checksum,
                read.proof_offset,
                read.proof_length,
                proof.chunk_group_bytes as usize,
            )?;
        }
        let requested_start = read
            .requested_offset
            .checked_sub(fetched_start)
            .ok_or(RangeFault::Corrupt)? as usize;
        slice(
            fetched,
            requested_start,
            read.requested_length as usize,
            "verified stripe range",
        )
        .map_err(|_| RangeFault::Corrupt)
    }

    /// The stripe that holds `granule_id`, from the granule and stripe directories.
    fn stripe_of_granule(&self, granule_id: u32) -> Result<&StripeEntry, FormatError> {
        let granule = self
            .footer
            .granules
            .iter()
            .find(|granule| granule.granule_id == granule_id)
            .ok_or(FormatError::RefOutOfRange { what: "granule id" })?;
        self.footer
            .stripes
            .iter()
            .find(|stripe| stripe.stripe_id == granule.stripe_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule references a stripe absent from the stripe directory",
            })
    }

    /// Where a stripe keeps one column's marks page when the file stores its marks beside each stripe's data: the
    /// range to fetch and verify before that column's blocks in the stripe can be located. `None` when the footer
    /// carries the marks itself (see [`Self::stripe_marks`]) or the stripe stores nothing for the column.
    pub fn marks_page_range(&self, column_id: u32, projection_id: u32, stripe_id: u32) -> Option<StripeRange> {
        if self.footer.required_feature_flags & required_features::STRIPE_MARKS_PAGES == 0 {
            return None;
        }
        self.footer
            .marks_page_offsets
            .iter()
            .find(|entry| {
                entry.stripe_id == stripe_id && entry.column_id == column_id && entry.projection_id == projection_id
            })
            .map(|entry| StripeRange {
                length: entry.page_len,
                stripe_id,
                stripe_offset: entry.page_offset,
            })
    }

    /// One column's marks within one stripe. For a file whose marks pages ride beside the stripe, `page` must be the
    /// verified bytes of [`Self::marks_page_range`]; for every other form the footer already holds the marks and
    /// `page` is ignored.
    pub fn stripe_marks(
        &self,
        column_id: u32,
        projection_id: u32,
        stripe_id: u32,
        page: Option<&[u8]>,
    ) -> Result<Vec<ColumnMark>, FormatError> {
        let flags = self.footer.required_feature_flags;
        if flags & required_features::STRIPE_MARKS_PAGES != 0 {
            return match page {
                Some(page) => decode_marks_page(column_id, projection_id, page),
                None if self.marks_page_range(column_id, projection_id, stripe_id).is_none() => Ok(Vec::new()),
                None => Err(FormatError::Structural {
                    rule: "stripe marks live beside the stripe: fetch its marks page first",
                }),
            };
        }
        if flags & required_features::COLUMNAR_MARKS != 0 {
            let mut marks = Vec::new();
            for entry in self.footer.marks_directory.iter().filter(|entry| {
                entry.stripe_id == stripe_id && entry.column_id == column_id && entry.projection_id == projection_id
            }) {
                let page = slice(
                    &self.footer.marks_pages,
                    entry.page_offset as usize,
                    entry.page_len as usize,
                    "columnar marks page",
                )?;
                marks.extend(decode_marks_page(column_id, projection_id, page)?);
            }
            return Ok(marks);
        }
        let granules: HashSet<u32> = self
            .footer
            .granules
            .iter()
            .filter(|granule| granule.stripe_id == stripe_id)
            .map(|granule| granule.granule_id)
            .collect();
        Ok(self
            .footer
            .marks
            .iter()
            .filter(|mark| {
                mark.column_id == column_id
                    && mark.projection_id == projection_id
                    && granules.contains(&mark.granule_id)
            })
            .copied()
            .collect())
    }

    /// The stripe-relative byte range of a mark's encoded block — what to fetch and verify to read it. An elided
    /// constant block, which stores no bytes, yields a zero-length range.
    pub fn block_range(&self, mark: &ColumnMark) -> Result<StripeRange, FormatError> {
        let stripe = self.stripe_of_granule(mark.granule_id)?;
        let stripe_offset = if self.footer.required_feature_flags & required_features::STRIPE_RELATIVE_MARKS != 0 {
            mark.compressed_offset
        } else {
            mark.compressed_offset
                .checked_sub(stripe.file_offset)
                .ok_or(FormatError::RefOutOfRange {
                    what: "mark lies before its stripe",
                })?
        };
        if stripe_offset
            .checked_add(mark.compressed_size)
            .is_none_or(|end| end > stripe.byte_len)
        {
            return Err(FormatError::RefOutOfRange {
                what: "mark lies outside its stripe",
            });
        }
        Ok(StripeRange {
            length: mark.compressed_size,
            stripe_id: stripe.stripe_id,
            stripe_offset,
        })
    }

    /// Whether this footer's roots were bound to a manifest seal through [`Self::open_authenticated`].
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }

    /// Whether the object carried the outboard trees its footer declares. When it did not, every range read into a
    /// stripe that declares a tree is planned as a whole-stripe fetch and verified against the stripe checksum.
    pub fn proof_trees_present(&self) -> bool {
        !self.proof_trees.is_empty() || self.footer.stripe_proofs.iter().all(|proof| proof.tree_len == 0)
    }

    /// The decoded footer: the column list, on-disk directories, exact counts, and payload geometry a reader plans a
    /// scan from.
    pub fn footer(&self) -> &Footer {
        &self.footer
    }

    /// The optional features this reader may actually use for this file; unknown optional bits are already dropped.
    pub fn usable_optional_features(&self) -> u64 {
        self.usable_optional_features
    }
}

/// The lazily-decoded per-stripe marks state for a file that declares `columnar_marks`: the two-level directory and
/// raw page bytes `decode_footer` parsed out of `Footer::marks_directory`/`Footer::marks_pages`, plus every stripe's
/// marks and per-page directory entries decoded so far — both ride the same per-stripe page, so they decode together.
/// A stripe's marks page decodes once, the first time any of its granules is looked up — never at open, and never for
/// a stripe pruning rejects.
#[derive(Debug)]
struct ColumnarMarks {
    decoded: HashMap<(u32, u32, u32), ColumnMark>,
    /// Which `(column, projection, stripe)` per-page directories have been decoded — tracked apart from the marks
    /// because the two ride one page but are wanted by different callers.
    decoded_page_directories: HashSet<(u32, u32, u32)>,
    /// Which `(column, projection, stripe)` marks pages have been decoded. A column absent from a decoded stripe
    /// stays in here too, so a repeated lookup of a column the stripe stores nothing for costs one set probe.
    decoded_pages: HashSet<(u32, u32, u32)>,
    granule_stripe_id: HashMap<u32, u32>,
    page_marks: BTreeMap<(u32, u32, u32, u32), PageDirectoryEntry>,
    pages: Vec<u8>,
    /// True under `STRIPE_MARKS_PAGES`: the directory's offsets are stripe-relative into the data area — each
    /// stripe's pages sit beside its filter bytes, one contiguous extent per stripe — and `pages` above is empty.
    pages_in_data_area: bool,
    /// Stripes whose shared extents have been found, so the scan runs once per stripe however many of its columns
    /// are later read.
    shared_extents_found: HashSet<u32>,
    /// Each stripe's base file offset, for resolving a co-located page's stripe-relative offset. Unused when the
    /// pages ride the footer.
    stripe_bases: HashMap<u32, u64>,
    /// The marks-page directory grouped by stripe — every column's page for one stripe in one place, which is both
    /// how the shared-extent scan reads a stripe and how a single column's page is found within it.
    stripe_pages: HashMap<u32, Vec<MarksPageEntry>>,
    /// Extents a just-decoded stripe revealed as shared by more than one mark (mark aliasing), as
    /// `(granule_id, offset, size)` with the offset still stripe-relative; [`HefFile::mark`] drains these into the
    /// file-level absolute set as stripes decode.
    pending_aliased: Vec<(u32, u64, u64)>,
}

impl ColumnarMarks {
    fn new(footer: &Footer, pages_in_data_area: bool) -> Self {
        Self {
            decoded: HashMap::new(),
            decoded_page_directories: HashSet::new(),
            decoded_pages: HashSet::new(),
            granule_stripe_id: footer
                .granules
                .iter()
                .map(|granule| (granule.granule_id, granule.stripe_id))
                .collect(),
            page_marks: BTreeMap::new(),
            pages: footer.marks_pages.clone(),
            pages_in_data_area,
            pending_aliased: Vec::new(),
            shared_extents_found: HashSet::new(),
            stripe_bases: footer
                .stripes
                .iter()
                .map(|stripe| (stripe.stripe_id, stripe.file_offset))
                .collect(),
            stripe_pages: {
                let directory = if pages_in_data_area {
                    &footer.marks_page_offsets
                } else {
                    &footer.marks_directory
                };
                let mut by_stripe: HashMap<u32, Vec<MarksPageEntry>> = HashMap::new();
                for entry in directory {
                    by_stripe.entry(entry.stripe_id).or_default().push(*entry);
                }
                by_stripe
            },
        }
    }

    /// The bytes of one marks page. Co-located pages are sliced from the data area at the stripe's base; footer
    /// pages from the section's own blob — identical page bytes either way, so a decode cannot tell.
    fn page_bytes<'a>(&'a self, file: &'a HefFile, entry: &MarksPageEntry) -> Result<&'a [u8], FormatError> {
        if !self.pages_in_data_area {
            return Ok(slice(
                &self.pages,
                entry.page_offset as usize,
                entry.page_len as usize,
                "columnar marks page",
            )?);
        }
        let position = self
            .stripe_bases
            .get(&entry.stripe_id)
            .copied()
            .and_then(|stripe_base| stripe_base.checked_add(entry.page_offset))
            .and_then(|position| usize::try_from(position).ok())
            .ok_or(FormatError::RefOutOfRange {
                what: "marks page references a stripe absent from the stripe directory",
            })?;
        file.data(position, entry.page_len as usize, "stripe marks page")
    }

    /// Finds the extents more than one of a stripe's marks points at (mark aliasing), the first time any of the
    /// stripe's granules is looked up.
    ///
    /// Whether an extent is shared is a fact about the whole stripe, so this has to look at every column's page —
    /// but it reads only the extent fields of each, never whole marks and never the per-page directory, because
    /// those are wanted only for the columns a caller actually reads. All of a stripe's marks share one base, so
    /// relative equality is absolute equality. A pruned stripe is never touched at all.
    fn find_shared_extents(&mut self, file: &HefFile, stripe_id: u32) -> Result<(), FormatError> {
        if self.shared_extents_found.contains(&stripe_id) {
            return Ok(());
        }
        let entries = self.stripe_pages.get(&stripe_id).cloned().unwrap_or_default();
        let mut extent_counts: HashMap<(u64, u64), (u32, u32)> = HashMap::new();
        for entry in &entries {
            let extents = {
                let page_bytes = self.page_bytes(file, entry)?;
                decode_marks_page_extents(page_bytes)?
            };
            for (granule_id, offset, size) in extents {
                let counted = extent_counts.entry((offset, size)).or_insert((0, granule_id));
                counted.0 += 1;
            }
        }
        for ((offset, size), (count, granule)) in extent_counts {
            if count > 1 {
                self.pending_aliased.push((granule, offset, size));
            }
        }
        self.shared_extents_found.insert(stripe_id);
        Ok(())
    }

    /// Decodes one column's marks page for `granule_id`'s stripe the first time that column is asked for in it,
    /// caching every mark and per-page directory entry the page holds, then serves this and every later lookup of
    /// that column in that stripe from the cache.
    ///
    /// A stripe holds one page per column, so a caller reading one column pays for one page rather than for every
    /// column the file has. The stripe's shared extents are found on its first touch either way — that answer needs
    /// every column, and a read that shares an extent has to know before it fetches the bytes.
    fn ensure_column_decoded(
        &mut self,
        file: &HefFile,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
    ) -> Result<(), FormatError> {
        let stripe_id = *self
            .granule_stripe_id
            .get(&granule_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule missing from stripe index",
            })?;
        self.find_shared_extents(file, stripe_id)?;
        if self.decoded_pages.contains(&(column_id, projection_id, stripe_id)) {
            return Ok(());
        }
        let entries: Vec<MarksPageEntry> = self
            .stripe_pages
            .get(&stripe_id)
            .map(|pages| {
                pages
                    .iter()
                    .filter(|entry| entry.column_id == column_id && entry.projection_id == projection_id)
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        for entry in &entries {
            let marks = {
                let page_bytes = self.page_bytes(file, entry)?;
                decode_marks_page(entry.column_id, entry.projection_id, page_bytes)?
            };
            for mark in marks {
                self.decoded
                    .insert((mark.column_id, mark.projection_id, mark.granule_id), mark);
            }
        }
        self.decoded_pages.insert((column_id, projection_id, stripe_id));
        Ok(())
    }

    /// Decodes one column's per-page directory for `granule_id`'s stripe the first time a caller asks for one of its
    /// pages. It rides the same page as the column's marks but is read separately, because addressing a single-page
    /// block needs the mark alone and that is what nearly every read does.
    fn ensure_page_directory_decoded(
        &mut self,
        file: &HefFile,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
    ) -> Result<(), FormatError> {
        let stripe_id = *self
            .granule_stripe_id
            .get(&granule_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule missing from stripe index",
            })?;
        if self
            .decoded_page_directories
            .contains(&(column_id, projection_id, stripe_id))
        {
            return Ok(());
        }
        let entries: Vec<MarksPageEntry> = self
            .stripe_pages
            .get(&stripe_id)
            .map(|pages| {
                pages
                    .iter()
                    .filter(|entry| entry.column_id == column_id && entry.projection_id == projection_id)
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        for entry in &entries {
            let page_directory = {
                let page_bytes = self.page_bytes(file, entry)?;
                decode_marks_page_directory(entry.column_id, entry.projection_id, page_bytes)?
            };
            for page in page_directory {
                // Mirrors the oversize bound `HefFile::open` enforces on the row-oriented form's page directory:
                // a multi-page mark's real per-page size lives here, so bound it directly here too.
                if page.compressed_len > super::MAX_PAGE_BYTES {
                    return Err(FormatError::Structural {
                        rule: "page/chunk exceeds the maximum size",
                    });
                }
                self.page_marks.insert(
                    (page.column_id, page.projection_id, page.granule_id, page.page_index),
                    page,
                );
            }
        }
        self.decoded_page_directories
            .insert((column_id, projection_id, stripe_id));
        Ok(())
    }

    fn mark(
        &mut self,
        file: &HefFile,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
    ) -> Result<Option<ColumnMark>, FormatError> {
        self.ensure_column_decoded(file, column_id, projection_id, granule_id)?;
        Ok(self.decoded.get(&(column_id, projection_id, granule_id)).copied())
    }

    fn page_mark(
        &mut self,
        file: &HefFile,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
        page_index: u32,
    ) -> Result<Option<PageDirectoryEntry>, FormatError> {
        self.ensure_page_directory_decoded(file, column_id, projection_id, granule_id)?;
        Ok(self
            .page_marks
            .get(&(column_id, projection_id, granule_id, page_index))
            .copied())
    }

    /// [`Self::mark`], answered only if `granule_id`'s stripe's column page is already decoded — `None` when it is
    /// not, so the caller knows to fall back to the exclusive, decoding path. Takes `&self`, so it is callable under
    /// a shared lock: the read side of the `RwLock` fast path.
    fn mark_if_decoded(
        &self,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
    ) -> Result<Option<Option<ColumnMark>>, FormatError> {
        let stripe_id = *self
            .granule_stripe_id
            .get(&granule_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule missing from stripe index",
            })?;
        if !self.decoded_pages.contains(&(column_id, projection_id, stripe_id)) {
            return Ok(None);
        }
        Ok(Some(self.decoded.get(&(column_id, projection_id, granule_id)).copied()))
    }

    /// [`Self::page_mark`]'s read-only fast path — see [`Self::mark_if_decoded`].
    fn page_mark_if_decoded(
        &self,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
        page_index: u32,
    ) -> Result<Option<Option<PageDirectoryEntry>>, FormatError> {
        let stripe_id = *self
            .granule_stripe_id
            .get(&granule_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule missing from stripe index",
            })?;
        if !self
            .decoded_page_directories
            .contains(&(column_id, projection_id, stripe_id))
        {
            return Ok(None);
        }
        Ok(Some(
            self.page_marks
                .get(&(column_id, projection_id, granule_id, page_index))
                .copied(),
        ))
    }

    /// [`HefFile::page_directory_for`]'s read-only fast path — see [`Self::mark_if_decoded`].
    fn page_directory_if_decoded(
        &self,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
    ) -> Result<Option<Vec<PageDirectoryEntry>>, FormatError> {
        let stripe_id = *self
            .granule_stripe_id
            .get(&granule_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule missing from stripe index",
            })?;
        if !self
            .decoded_page_directories
            .contains(&(column_id, projection_id, stripe_id))
        {
            return Ok(None);
        }
        let span = (column_id, projection_id, granule_id, 0)..=(column_id, projection_id, granule_id, u32::MAX);
        Ok(Some(self.page_marks.range(span).map(|(_, entry)| *entry).collect()))
    }
}

/// A file's `(column, projection, granule)` mark lookup: eager for the row-oriented form (already fully decoded by
/// `decode_footer`, so a lookup is a plain map read), lazy for a `columnar_marks` file — per column within a stripe,
/// so a read pays for the columns it asks for, and per stripe, so a stripe pruning rejects is never fetched or
/// decoded at all. See [`HefFile::mark`].
///
/// `Columnar`'s state rides an `RwLock`, not a `Mutex`: decoding a stripe's page needs exclusive access, but reading
/// an already-decoded mark or page directory entry does not, and that read is the first thing every column block
/// read does — so once a stripe is warm, concurrent readers stop serializing on it.
#[derive(Debug)]
enum Marks {
    Columnar(RwLock<ColumnarMarks>),
    Eager(HashMap<(u32, u32, u32), ColumnMark>),
}

/// Which stripes of a lazily opened file have been hashed against their footer checksums so far — one slot per
/// stripe, in stripe-directory order, holding whether the stripe matched. Readers made by [`HefFile::fresh_reader`]
/// share it, so a stripe is hashed once per file rather than once per reader.
#[derive(Debug)]
pub(super) struct StripeVerification {
    verified: Vec<OnceLock<bool>>,
}

impl StripeVerification {
    fn new(stripes: usize) -> Self {
        Self {
            verified: (0..stripes).map(|_| OnceLock::new()).collect(),
        }
    }

    /// Hashes the stripe at `index` the first time it is asked about; refuses it, now and on every later ask, when
    /// its bytes did not match `checksum`.
    fn ensure_verified(
        &self,
        index: usize,
        stripe: &StripeEntry,
        checksum: &[u8; 32],
        bytes: &[u8],
    ) -> Result<(), FormatError> {
        let slot = self.verified.get(index).ok_or(FormatError::Structural {
            rule: "stripe missing from the lazy verification state",
        })?;
        let matches = match slot.get() {
            Some(matches) => *matches,
            None => {
                let stripe_bytes = slice(bytes, stripe.file_offset as usize, stripe.byte_len as usize, "stripe")?;
                *slot.get_or_init(|| {
                    crate::file::integrity::hash_segments(&[stripe_bytes])
                        .first()
                        .is_some_and(|root| root.as_bytes() == checksum)
                })
            }
        };
        if matches {
            Ok(())
        } else {
            Err(FormatError::Blake3Mismatch { scope: "stripe" })
        }
    }
}

/// An opened, fully validated stored file, ready to read columns and payload values from.
#[derive(Debug)]
pub struct HefFile {
    /// Blocks decoded once per shared extent for the extents in `aliased_extents`, keyed by
    /// `(absolute offset, size, pipeline id)`. An accelerator only, sharded like `column_cache`, and held under
    /// the same byte budget by least-recently-used eviction.
    aliased_extent_cache: DashMap<(u64, u64, u32), AliasedExtentSlot, ahash::RandomState>,
    /// Running total of `aliased_extent_cache`'s slot bytes, kept current on every insert and eviction so budget
    /// checks and [`Self::decoded_cache_bytes`] read one atomic instead of summing the map on every call.
    aliased_extent_cache_bytes: AtomicU64,
    /// Extents referenced by more than one single-page mark (mark aliasing), as `(absolute offset, size)`. Counted at
    /// open from the eager marks of a legacy file; for a `columnar_marks` file each stripe contributes its aliases as
    /// its marks page decodes — so the read path knows which extents can share a decode without rescanning the marks.
    aliased_extents: DashSet<(u64, u64), ahash::RandomState>,
    /// Encoded column-block and page fetches since open. Never feeds a read result; it exists so tests can observe
    /// that a scan answered from metadata (a constant block filled from its exact stats) skipped the block's bytes.
    block_reads: AtomicU64,
    /// The whole file in memory, shared with every reader [`Self::fresh_reader`] made from this one so a benchmark's
    /// per-iteration readers need no copy of it; or, for [`Self::open_remote`], the remote object read a verified
    /// range at a time.
    bytes: FileBytes,
    /// Monotonic access tick shared by the decoded-block caches; each hit or insert stamps the slot it touched, so
    /// eviction can drop the least-recently-used slot deterministically.
    cache_access_counter: AtomicU64,
    /// Column blocks decoded once per `(column_id, granule_id)` on the per-row reconstruction path, each paired with a
    /// rank index over its presence bitmap so per-row payload reconstruction stays linear in rows. An accelerator only —
    /// the file's bytes remain the source of truth — sharded so concurrent readers rarely contend. Byte-budgeted:
    /// held under `decoded_cache_budget` by least-recently-used eviction.
    column_cache: DashMap<(u32, u32), CachedColumnSlot, ahash::RandomState>,
    /// Running total of `column_cache`'s slot bytes, kept current on every insert and eviction — see
    /// `aliased_extent_cache_bytes`.
    column_cache_bytes: AtomicU64,
    /// Byte budget for the decoded-block caches, [`DECODED_CACHE_BUDGET_BYTES`] unless the caller sized it via
    /// [`Self::with_decoded_cache_budget`].
    decoded_cache_budget: u64,
    /// Granule-dictionary decodes since open. Never feeds a read result; it exists so tests can observe that a
    /// payload read decodes each granule's dictionary once rather than once per row.
    dictionary_decodes: AtomicU64,
    /// Per internal embedding/vector column's per-row byte-offset index, keyed by `(column_id, granule_id)`. Empty when
    /// the file does not declare `TYPED_COLUMN_ROW_OFFSETS`.
    embedding_row_offsets: HashMap<(u32, u32), EmbeddingRowOffsets>,
    /// `footer.entity_hash_filters` indexed by granule id, so a point lookup finds a granule's identity-hash filter's
    /// byte range by map read instead of a scan over every entry. Empty when the file does not declare
    /// `ENTITY_HASH_POINT_FILTERS`.
    entity_hash_filters_by_granule: HashMap<u32, usize>,
    footer: Footer,
    /// Per declared free-text column's per-row byte-offset index, keyed by `(column_id, granule_id)`. Empty when the
    /// file does not declare `TYPED_COLUMN_ROW_OFFSETS`.
    freetext_row_offsets: HashMap<(u32, u32), FreetextRowOffsets>,
    /// Payload key dictionaries decoded once per granule and reused by every point read into it, so a run of point
    /// reads pays one decode per granule rather than one per row. An accelerator only, like `column_cache`, and
    /// byte-budgeted the same way.
    granule_dictionaries: DashMap<u32, DictionarySlot, ahash::RandomState>,
    /// Running total of `granule_dictionaries`'s slot bytes, kept current on every insert and eviction — see
    /// `aliased_extent_cache_bytes`.
    granule_dictionaries_bytes: AtomicU64,
    /// The file-absolute base offset of each granule's stripe (`StripeEntry.file_offset`), used to turn a mark's
    /// stripe-relative offset back into a file position. Only consulted when `stripe_relative` is set.
    granule_stripe_base: HashMap<u32, u64>,
    header: HefHeader,
    /// Inflated spans of compressed residual arenas, keyed by `(granule_id, frame)` — one whole arena per `Zstd3`
    /// granule under frame 0, one frame per seekable granule — held for repeated point reads. An accelerator only,
    /// like `column_cache`, and sharded for the same reason. Byte-budgeted like `column_cache`.
    inflated_residuals: DashMap<(u32, u32), InflatedResidualSlot, ahash::RandomState>,
    /// Running total of `inflated_residuals`'s arena bytes, kept current on every insert and eviction — see
    /// `aliased_extent_cache_bytes`.
    inflated_residuals_bytes: AtomicU64,
    /// Which stripes still await their checksum hash: `Some` for a file opened with [`Self::open_lazy`], whose
    /// stripes verify on first read through [`Self::data`]; `None` for an eager open, which verified them all.
    lazy_stripes: Option<Arc<StripeVerification>>,
    marks: Marks,
    /// Per-page byte-range index keyed by (column_id, projection_id, granule_id, page_index), for the row-oriented
    /// form only — already fully decoded by `decode_footer`, so a lookup is a plain map read. Empty when
    /// `PER_PAGE_MARKS` is absent, and always empty for a `columnar_marks` file, whose per-page directory instead
    /// rides the same per-stripe pages as `Marks::Columnar` and decodes lazily through it. See
    /// [`HefFile::page_mark`].
    page_marks: BTreeMap<(u32, u32, u32, u32), PageDirectoryEntry>,
    /// `footer.page_stats` keyed by `(column_id, granule_id)` — the writer emits exactly one entry per block, so this
    /// is a lossless index of what [`Self::materialize_elided_constant`] used to find by a linear scan per read.
    page_stats_index: HashMap<(u32, u32), PageStats>,
    /// Each granule's payload-arena geometry, keyed by `granule_id` — what [`Self::payload_granule`] used to find by a
    /// linear scan over `footer.payload_granules` on every payload read.
    payload_granule_index: HashMap<u32, PayloadGranule>,
    /// One file-scope dictionary alphabet per column that shares one, from the footer — what a shared-scope
    /// dictionary block resolves its codes against.
    shared_alphabets: HashMap<u32, Arc<Vec<String>>>,
    /// Arrow backing buffers and entry views prepared lazily from `shared_alphabets`. Opening a file or projecting an
    /// unrelated column does not build them; the first shared-scope view decode does, and later granules reuse the
    /// immutable arena.
    shared_string_views: DashMap<u32, SharedStringViewDictionary>,
    /// Cold point reads served so far per `(column_id, granule_id)` block, compared against
    /// [`POINT_READS_BEFORE_WHOLE_GRANULE_DECODE`] to decide when probing has become dense enough that the
    /// whole-granule decode pays for itself.
    point_probe_counts: DashMap<(u32, u32), u32, ahash::RandomState>,
    /// Parsed seek tables of seekable residual arenas, keyed by `granule_id`. An accelerator only, like
    /// `column_cache` — the file's stored bytes remain the source of truth — but never evicted: a table is a few
    /// bytes per frame, so a whole file's worth costs far less than the decoded blocks `column_cache` budgets, and is
    /// bounded by the file's own granule count.
    residual_seek_tables: DashMap<u32, Arc<SeekTable>, ahash::RandomState>,
    /// True when the file declares `STRIPE_RELATIVE_MARKS`: mark, page, and payload-arena offsets are measured from
    /// their stripe's base rather than the start of the file.
    stripe_relative: bool,
    /// Decoded text-token filters, keyed by `(column_id, granule_id, page_index)`. An accelerator only, like
    /// `column_cache` — the file's stored bytes remain the source of truth — but never evicted: a filter is a small
    /// Bloom filter (bytes on the order of its granule's distinct token count), so a whole file's worth costs far less
    /// than the decoded column blocks `column_cache` already budgets, and is bounded by the file's own granule/page
    /// count either way.
    text_token_cache: DashMap<(u32, u32, u32), Arc<TextTokenIndex>, ahash::RandomState>,
    /// `footer.text_token_indexes` indexed by `(column_id, granule_id, page_index)` — what a legacy file (one storing
    /// filter bytes inline in the footer instead of `TEXT_TOKEN_FILTER_OFFSETS`) used to find by a linear scan on
    /// every probe. Holds the entry's position in `footer.text_token_indexes` rather than a copy, since each entry's
    /// `index_bytes` can be sizeable.
    text_token_indexes_by_key: HashMap<(u32, u32, u32), usize>,
    /// Byte ranges of per-page text-token filters stored in the data area, keyed by
    /// `(column_id, granule_id, page_index)`. Empty when the file does not declare `TEXT_TOKEN_FILTER_OFFSETS`.
    text_token_offsets: HashMap<(u32, u32, u32), TextTokenOffsetsEntry>,
    /// Optional features this reader may use (unknown optional bits are already dropped by the compatibility gate).
    usable_optional_features: u64,
}

impl HefFile {
    /// Opens and validates a file. `expected_seal` is the manifest entry's authoritative segment seal when the caller
    /// has it. A file whose footer is sealed (declares `FOOTER_ENCRYPTED`) refuses here — use
    /// [`open_with_keys`](Self::open_with_keys) with the file DEK to read it.
    pub fn open(bytes: Vec<u8>, expected_seal: Option<&[u8; SEAL_LEN]>) -> Result<Self, FormatError> {
        Self::open_with_keys(bytes, expected_seal, None)
    }

    /// Opens and validates a file, decrypting a sealed footer with `footer_dek` when the file declares
    /// `FOOTER_ENCRYPTED`. Behaves exactly like [`open`](Self::open) for a plaintext footer (the key is ignored). An
    /// encrypted-footer file opened without the matching key refuses rather than reading ciphertext as a footer.
    pub fn open_with_keys(
        bytes: Vec<u8>,
        expected_seal: Option<&[u8; SEAL_LEN]>,
        footer_dek: Option<&[u8; FOOTER_DEK_LEN]>,
    ) -> Result<Self, FormatError> {
        Self::open_inner(bytes, expected_seal, footer_dek, false)
    }

    /// Opens and validates a file like [`open_with_keys`](Self::open_with_keys), but without hashing the stripes up
    /// front: each stripe is checked against its footer checksum the first time any of its bytes are read, so a
    /// reader that touches a few granules of a large file never pays for the rest. The header, footer, seal, and
    /// alignment gaps are still verified here, and no byte of a stripe is served before that stripe has verified.
    pub fn open_lazy(
        bytes: Vec<u8>,
        expected_seal: Option<&[u8; SEAL_LEN]>,
        footer_dek: Option<&[u8; FOOTER_DEK_LEN]>,
    ) -> Result<Self, FormatError> {
        Self::open_inner(bytes, expected_seal, footer_dek, true)
    }

    fn open_inner(
        bytes: Vec<u8>,
        expected_seal: Option<&[u8; SEAL_LEN]>,
        footer_dek: Option<&[u8; FOOTER_DEK_LEN]>,
        lazy: bool,
    ) -> Result<Self, FormatError> {
        // An outboard range tree is non-authoritative metadata and is not part of the HEF seal. The authoritative
        // content ends at the file's own trailing `HEF1` magic.
        let content_len = match outboard_tree_len(&bytes)? {
            Some(tree_len) => bytes
                .len()
                .checked_sub(TREE_TRAILER_LEN)
                .and_then(|n| n.checked_sub(tree_len as usize))
                .ok_or(FormatError::Truncated { what: "outboard tree" })?,
            None => bytes.len(),
        };
        let content = slice(&bytes, 0, content_len, "hef content")?;
        let (_, proof_trees) = split_outboard_tree(&bytes)?;
        let encoded_footer_len = footer_blob_len(content)?;
        let footer_start = content_len
            .checked_sub(FOOTER_TAIL_TRAILER_LEN)
            .and_then(|n| n.checked_sub(encoded_footer_len))
            .ok_or(FormatError::Truncated { what: "footer" })?;
        let header_block = slice(&bytes, 0, HEADER_BLOCK_LEN, "hef header block")?;
        let header = decode_header(header_block)?;

        // Gate the header's required features refuse BEFORE the footer is decoded. The footer-encryption signal
        // rides here because the footer bytes are ciphertext when it is set, so a reader that does not understand a
        // header-declared required feature must reject the file rather than misread it.
        compat::check_features(header.feature_flags, 0)?;

        // The footer opens the same way for a whole-file open as for a range-native cold open: footer-first over the
        // tail, here the bytes after the header. A footer that would overlap the header underflows this tail and fails
        // closed, exactly as a footer larger than the file would. Whole-file open then verifies every stripe below,
        // where a range-native open would instead verify each stripe lazily as it is read.
        let body_tail = slice(
            &bytes,
            HEADER_BLOCK_LEN,
            content_len - HEADER_BLOCK_LEN,
            "hef body tail",
        )?;
        // A sealed footer is decrypted and re-framed as a plaintext content tail before decoding; a plaintext footer
        // opens directly. Both then go through the identical footer validation and feature gating.
        let opened_footer = if header.feature_flags & required_features::FOOTER_ENCRYPTED != 0 {
            let dek = footer_dek.ok_or(FormatError::Structural {
                rule: "encrypted footer requires a file DEK to open",
            })?;
            let stripped_tail = strip_outboard_tree(body_tail)?;
            let plaintext = decrypt_footer_tail(stripped_tail, dek, header.file_id)?;
            let plaintext_len = footer_blob_len(&plaintext)?;
            HefFooter::from_content_tail(&plaintext, plaintext_len, proof_trees, false)?
        } else {
            let plaintext_len = footer_blob_len(body_tail)?;
            HefFooter::from_content_tail(body_tail, plaintext_len, proof_trees, false)?
        };
        let HefFooter {
            footer,
            usable_optional_features,
            ..
        } = opened_footer;

        // The manifest seal authenticates the header, exact segment geometry, stripe checksum leaves, gaps, and footer.
        // It is deliberately verified before reading the large stripes, then each stripe is hashed exactly once against
        // the now-authenticated leaf. There is no second whole-file hash pass.
        if footer.stripe_checksums.len() != footer.stripes.len() {
            return Err(FormatError::Structural {
                rule: "stripe checksum count must match the stripe directory",
            });
        }
        let commitments = crate::integrity::verify_declared_data_commitments(
            content,
            footer_start,
            &footer.stripes,
            &footer.stripe_checksums,
            &footer.integrity_gaps,
        )?;
        if let Some(expected) = expected_seal {
            let footer_blob = slice(content, footer_start, encoded_footer_len, "footer region")?;
            let footer_trailer = slice(
                content,
                content_len - FOOTER_TAIL_TRAILER_LEN,
                FOOTER_TAIL_TRAILER_LEN,
                "footer trailer",
            )?;
            let actual = crate::integrity::derive_file_seal(
                header_block,
                footer_blob,
                footer_trailer,
                content_len as u64,
                &commitments,
            );
            if &actual != expected {
                return Err(FormatError::Blake3Mismatch { scope: "hef file seal" });
            }
        }
        // Every stripe verifies against its own bytes alone. An eager open hashes them all in one flat parallel pass
        // over 1 MiB pieces — one fan-out for the whole file however many stripes it has, instead of a parallel hash
        // nested inside a parallel loop over stripes; a lazy open records that none has been checked yet.
        let lazy_stripes = if lazy {
            Some(Arc::new(StripeVerification::new(footer.stripes.len())))
        } else {
            let stripe_bytes = footer
                .stripes
                .iter()
                .map(|stripe| slice(&bytes, stripe.file_offset as usize, stripe.byte_len as usize, "stripe"))
                .collect::<Result<Vec<_>, _>>()?;
            let roots = crate::file::integrity::hash_segments(&stripe_bytes);
            if roots
                .iter()
                .zip(&footer.stripe_checksums)
                .any(|(root, checksum)| root.as_bytes() != checksum)
            {
                return Err(FormatError::Blake3Mismatch { scope: "stripe" });
            }
            None
        };
        Self::assemble(
            FileBytes::Local(Arc::new(bytes)),
            header,
            footer,
            usable_optional_features,
            lazy_stripes,
            DECODED_CACHE_BUDGET_BYTES,
        )
    }

    /// A new reader over this file's already-verified bytes with nothing decoded or cached — what a fresh open of
    /// the same bytes returns, minus the stripe hashing that open already paid for. A benchmark opens one reader per
    /// iteration this way. A lazily opened file hands on which stripes it has checked so far, and the new reader
    /// keeps checking the rest as it reads them.
    pub fn fresh_reader(&self) -> Result<Self, FormatError> {
        Self::assemble(
            self.bytes.fresh(),
            self.header.clone(),
            self.footer.clone(),
            self.usable_optional_features,
            self.lazy_stripes.clone(),
            self.decoded_cache_budget,
        )
    }

    /// Checks the structure the footer declares and builds the reader's lookup indexes over it. Everything after the
    /// stripe hashing of an open, so a reader made from an already-verified template starts here.
    pub(super) fn assemble(
        bytes: FileBytes,
        header: HefHeader,
        footer: Footer,
        usable_optional_features: u64,
        lazy_stripes: Option<Arc<StripeVerification>>,
        decoded_cache_budget: u64,
    ) -> Result<Self, FormatError> {
        if header.row_count != footer.exact_counts.row_count {
            return Err(FormatError::Structural {
                rule: "header row_count must match exact counts",
            });
        }

        // Oversized page/chunk with no recognized declared feature lifting the bound: reject the file. A single-page
        // mark carries its block's true size, so bound it directly; a multi-page mark's `compressed_size` is the
        // aggregate of all its pages, whose real per-page sizes live in the page directory, so bound those entries
        // instead — otherwise a file whose pages are each valid but sum past the bound is wrongly rejected.
        let mark_oversize = footer
            .marks
            .iter()
            .any(|mark| mark.page_count <= 1 && mark.compressed_size > super::MAX_PAGE_BYTES);
        let page_oversize = footer
            .page_directory
            .iter()
            .any(|entry| entry.compressed_len > super::MAX_PAGE_BYTES);
        if mark_oversize || page_oversize {
            return Err(FormatError::Structural {
                rule: "page/chunk exceeds the maximum size",
            });
        }

        // Row-oriented marks are already fully materialized by `decode_footer`, so an eager map is just a plain
        // lookup; a `columnar_marks` file instead carries only the two-level directory and raw page bytes here, and a
        // stripe's marks decode lazily the first time one of its granules is looked up (see `ColumnarMarks::mark`).
        let marks = if footer.required_feature_flags & required_features::COLUMNAR_MARKS != 0 {
            Marks::Columnar(RwLock::new(ColumnarMarks::new(
                &footer,
                footer.required_feature_flags & required_features::STRIPE_MARKS_PAGES != 0,
            )))
        } else {
            Marks::Eager(
                footer
                    .marks
                    .iter()
                    .map(|mark| ((mark.column_id, mark.projection_id, mark.granule_id), *mark))
                    .collect(),
            )
        };
        let page_marks = footer
            .page_directory
            .iter()
            .map(|e| ((e.column_id, e.projection_id, e.granule_id, e.page_index), *e))
            .collect();
        let freetext_row_offsets = footer
            .freetext_row_offsets
            .iter()
            .map(|entry| ((entry.column_id, entry.granule_id), *entry))
            .collect();
        let embedding_row_offsets = footer
            .embedding_row_offsets
            .iter()
            .map(|entry| ((entry.column_id, entry.granule_id), *entry))
            .collect();
        let text_token_offsets = footer
            .text_token_offsets
            .iter()
            .map(|entry| ((entry.column_id, entry.granule_id, entry.page_index), *entry))
            .collect();
        let text_token_indexes_by_key = footer
            .text_token_indexes
            .iter()
            .enumerate()
            .map(|(index, entry)| ((entry.column_id, entry.granule_id, entry.page_index), index))
            .collect();
        let entity_hash_filters_by_granule = footer
            .entity_hash_filters
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.granule_id, index))
            .collect();
        let payload_granule_index = footer
            .payload_granules
            .iter()
            .map(|payload| (payload.granule_id, *payload))
            .collect();
        // The writer emits exactly one page_stats entry per (column, granule) block, so this index is lossless.
        let page_stats_index = footer
            .page_stats
            .iter()
            .map(|stats| ((stats.column_id, stats.granule_id), *stats))
            .collect();
        // `granule_of_row` binary-searches this directory by `first_row_ordinal`, so a file whose granules are not
        // strictly ascending (and therefore non-overlapping, since each spans exactly `row_count` rows) would make the
        // search silently miss rows or return the wrong granule. Reject such a file instead of misreading it.
        if footer.granules.windows(2).any(|pair| {
            pair[0]
                .first_row_ordinal
                .checked_add(u64::from(pair[0].row_count))
                .is_none_or(|end| end > pair[1].first_row_ordinal)
        }) {
            return Err(FormatError::Structural {
                rule: "granule directory must be strictly ascending by first_row_ordinal with no overlap",
            });
        }
        // Resolve each granule's stripe base offset: granule -> stripe_id -> StripeEntry.file_offset. A mark carries no
        // stripe id of its own; it reaches its stripe through the granule directory the file already stores.
        let stripe_base_by_id: HashMap<u32, u64> = footer
            .stripes
            .iter()
            .map(|stripe| (stripe.stripe_id, stripe.file_offset))
            .collect();
        let stripe_relative = footer.required_feature_flags & required_features::STRIPE_RELATIVE_MARKS != 0;
        // Under stripe-relative marks, a granule's offsets are measured from its stripe's base. A granule whose stripe
        // id is missing from the directory has no base to add back, so its reads would silently fall back to file
        // offset 0 and slice unrelated bytes near the file start. Reject such a file rather than reading garbage.
        if stripe_relative
            && footer
                .granules
                .iter()
                .any(|granule| !stripe_base_by_id.contains_key(&granule.stripe_id))
        {
            return Err(FormatError::Structural {
                rule: "granule references a stripe absent from the stripe directory",
            });
        }
        let granule_stripe_base: HashMap<u32, u64> = footer
            .granules
            .iter()
            .filter_map(|granule| {
                stripe_base_by_id
                    .get(&granule.stripe_id)
                    .map(|base| (granule.granule_id, *base))
            })
            .collect();
        let mut extent_marks: HashMap<(u64, u64), u32> = HashMap::new();
        for mark in &footer.marks {
            // Elided blocks store no bytes; their zero-length extents coincide without sharing anything.
            if mark.page_count <= 1 && mark.compressed_size > 0 {
                let base = if stripe_relative {
                    granule_stripe_base.get(&mark.granule_id).copied().unwrap_or(0)
                } else {
                    0
                };
                if let Some(offset) = base.checked_add(mark.compressed_offset) {
                    *extent_marks.entry((offset, mark.compressed_size)).or_default() += 1;
                }
            }
        }
        let aliased_extents: DashSet<(u64, u64), ahash::RandomState> = extent_marks
            .into_iter()
            .filter_map(|(extent, count)| (count > 1).then_some(extent))
            .collect();
        let shared_alphabets: HashMap<u32, Arc<Vec<String>>> = footer
            .shared_dictionaries
            .iter()
            .map(|entry| (entry.column_id, Arc::new(entry.values.clone())))
            .collect();
        Ok(Self {
            aliased_extent_cache: DashMap::default(),
            aliased_extent_cache_bytes: AtomicU64::new(0),
            aliased_extents,
            block_reads: AtomicU64::new(0),
            bytes,
            cache_access_counter: AtomicU64::new(0),
            column_cache: DashMap::default(),
            column_cache_bytes: AtomicU64::new(0),
            decoded_cache_budget,
            dictionary_decodes: AtomicU64::new(0),
            embedding_row_offsets,
            entity_hash_filters_by_granule,
            footer,
            freetext_row_offsets,
            granule_dictionaries: DashMap::default(),
            granule_dictionaries_bytes: AtomicU64::new(0),
            granule_stripe_base,
            header,
            inflated_residuals: DashMap::default(),
            inflated_residuals_bytes: AtomicU64::new(0),
            lazy_stripes,
            marks,
            page_marks,
            page_stats_index,
            payload_granule_index,
            point_probe_counts: DashMap::default(),
            residual_seek_tables: DashMap::default(),
            shared_alphabets,
            shared_string_views: DashMap::default(),
            stripe_relative,
            text_token_cache: DashMap::default(),
            text_token_indexes_by_key,
            text_token_offsets,
            usable_optional_features,
        })
    }

    /// How many granule dictionaries this reader has decoded since open. Never feeds a read result; it lets tests
    /// observe that a payload read decodes each granule's dictionary once rather than once per row.
    pub fn granule_dictionary_decodes(&self) -> u64 {
        self.dictionary_decodes.load(Ordering::Relaxed)
    }

    /// Bytes of inflated residual arena this reader is holding right now. Never feeds a read result; it lets tests
    /// observe that a point read into a seekable arena inflates the frame it needs rather than the whole arena.
    pub fn residual_cache_bytes(&self) -> u64 {
        self.inflated_residuals_bytes.load(Ordering::Relaxed)
    }

    /// How many encoded column blocks or pages this reader has fetched since open. Never feeds a read result; it lets
    /// tests observe that a scan answered from metadata (for example a constant block filled from its exact stats)
    /// never fetched the block's bytes.
    pub fn column_block_reads(&self) -> u64 {
        self.block_reads.load(Ordering::Relaxed)
    }

    /// The `len` file bytes at `position`, never before they are verified: an eager open checked every stripe
    /// already, and for a lazy open this hashes each stripe the range touches against its footer checksum, the first
    /// time any of its bytes are read. Bytes outside every stripe — the header, the footer, and the alignment gaps —
    /// were verified at open.
    fn data(&self, position: usize, len: usize, what: &'static str) -> Result<&[u8], FormatError> {
        let file = match &self.bytes {
            FileBytes::Local(file) => file,
            FileBytes::Remote(remote) => return remote.read(position as u64, len, what),
        };
        let bytes = slice(file, position, len, what)?;
        if let Some(verification) = &self.lazy_stripes {
            let end = position + len;
            for (index, (stripe, checksum)) in self
                .footer
                .stripes
                .iter()
                .zip(&self.footer.stripe_checksums)
                .enumerate()
            {
                let stripe_end = stripe.file_offset.saturating_add(stripe.byte_len) as usize;
                if (stripe.file_offset as usize) < end && position < stripe_end {
                    verification.ensure_verified(index, stripe, checksum, file)?;
                }
            }
        }
        Ok(bytes)
    }

    /// Whether this file frames block presence as tagged side-stream forms. Files written before the
    /// `compressed_presence` feature frame it as `u32 length | bitmap` instead, and stay readable through
    /// [`super::decode_presence_frame`]'s legacy arm.
    fn compressed_presence(&self) -> bool {
        self.footer.required_feature_flags & required_features::COMPRESSED_PRESENCE != 0
    }

    /// The distinct-count estimate recorded for `column` across this file's stripes, straight from footer metadata —
    /// no column bytes are read. A single exact stripe stays exact; counts combined across several stripes are summed,
    /// which upper-bounds the union, and are marked approximate. Returns `None` when the build recorded no estimate
    /// for the column. For planner cardinality estimation (join ordering, grouping sizes) only — never a query answer.
    pub fn column_distinct_estimate(&self, column: u32) -> Option<ColumnDistinctEstimate> {
        let mut combined: Option<ColumnDistinctEstimate> = None;
        for entry in self.footer.stripe_ndv.iter().filter(|entry| entry.column_id == column) {
            combined = Some(match combined {
                None => ColumnDistinctEstimate {
                    distinct_count: entry.distinct_count,
                    exact: entry.exact,
                },
                Some(previous) => ColumnDistinctEstimate {
                    distinct_count: previous.distinct_count.saturating_add(entry.distinct_count),
                    exact: false,
                },
            });
        }
        combined
    }

    /// The file position a stripe-relative offset for `granule_id` is measured from: the granule's stripe base under
    /// `STRIPE_RELATIVE_MARKS`, or zero for a legacy file whose offsets are already file-absolute. Applies to the
    /// granule's marks, page-directory entries, and its payload arena (dictionary / offsets / residual) alike.
    fn stripe_base_for(&self, granule_id: u32) -> u64 {
        if self.stripe_relative {
            self.granule_stripe_base.get(&granule_id).copied().unwrap_or(0)
        } else {
            0
        }
    }

    /// Turns a footer-declared offset for `granule_id` into an absolute file position by adding the granule's stripe
    /// base. Offsets come from the untrusted footer, so a sum that overflows refuses as a format error instead of
    /// aborting on the arithmetic; `slice` then bounds the resulting position against the file's actual length.
    fn file_position(&self, granule_id: u32, offset: u64) -> Result<usize, FormatError> {
        self.stripe_base_for(granule_id)
            .checked_add(offset)
            .and_then(|position| usize::try_from(position).ok())
            .ok_or(FormatError::RefOutOfRange {
                what: "offset beyond file range",
            })
    }

    /// Resolves the authoritative mark for `(column, projection, granule)` — where the block lives and how it was
    /// encoded — or `None` when the file stores no such block. A plain map read for the row-oriented form; for a
    /// `columnar_marks` file this decodes the granule's stripe on first touch and caches it, so a stripe pruning
    /// never visits is never fetched or decoded.
    pub fn mark(&self, column_id: u32, projection_id: u32, granule_id: u32) -> Result<Option<ColumnMark>, FormatError> {
        match &self.marks {
            Marks::Eager(map) => Ok(map.get(&(column_id, projection_id, granule_id)).copied()),
            Marks::Columnar(state) => {
                // Fast path: a shared read lock, so a reader hitting an already-warm stripe never waits on another
                // reader — only a first-touch decode needs exclusive access.
                if let Some(mark) = state.read().unwrap_or_else(PoisonError::into_inner).mark_if_decoded(
                    column_id,
                    projection_id,
                    granule_id,
                )? {
                    return Ok(mark);
                }
                let mut guard = state.write().unwrap_or_else(PoisonError::into_inner);
                let mark = guard.mark(self, column_id, projection_id, granule_id)?;
                let pending = std::mem::take(&mut guard.pending_aliased);
                drop(guard);
                self.publish_pending_aliases(pending);
                Ok(mark)
            }
        }
    }

    /// Publishes extents a freshly decoded stripe revealed as aliased, as absolute positions, so `read_column` can
    /// share their decode.
    fn publish_pending_aliases(&self, pending: Vec<(u32, u64, u64)>) {
        if pending.is_empty() {
            return;
        }
        for (granule, offset, size) in pending {
            if let Some(absolute) = self.stripe_base_for(granule).checked_add(offset) {
                self.aliased_extents.insert((absolute, size));
            }
        }
    }

    /// The per-page directory entries of one `(column, projection, granule)` block, in page order — from the eager
    /// directory of a legacy file, or decoded from the granule's stripe marks page for a `columnar_marks` file (the
    /// same first touch its marks decode on, so a pruned stripe still costs zero directory bytes). Empty when the
    /// block has no per-page directory.
    pub fn page_directory_for(
        &self,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
    ) -> Result<Vec<PageDirectoryEntry>, FormatError> {
        let span = (column_id, projection_id, granule_id, 0)..=(column_id, projection_id, granule_id, u32::MAX);
        match &self.marks {
            Marks::Eager(_) => Ok(self.page_marks.range(span).map(|(_, entry)| *entry).collect()),
            Marks::Columnar(state) => {
                // Fast path: see `HefFile::mark`.
                if let Some(entries) = state
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .page_directory_if_decoded(column_id, projection_id, granule_id)?
                {
                    return Ok(entries);
                }
                let mut guard = state.write().unwrap_or_else(PoisonError::into_inner);
                guard.ensure_page_directory_decoded(self, column_id, projection_id, granule_id)?;
                let entries = guard.page_marks.range(span).map(|(_, entry)| *entry).collect();
                let pending = std::mem::take(&mut guard.pending_aliased);
                drop(guard);
                self.publish_pending_aliases(pending);
                Ok(entries)
            }
        }
    }

    /// Resolves the per-page directory entry for `(column, projection, granule, page_index)`. A plain map read for
    /// the row-oriented form; for a `columnar_marks` file this rides the same per-stripe page as the granule's marks,
    /// so it decodes (and is bounds-checked) on the same first touch — a pruned stripe's page directory is never
    /// fetched or decoded either.
    fn page_mark(
        &self,
        column_id: u32,
        projection_id: u32,
        granule_id: u32,
        page_index: u32,
    ) -> Result<Option<PageDirectoryEntry>, FormatError> {
        match &self.marks {
            Marks::Eager(_) => Ok(self
                .page_marks
                .get(&(column_id, projection_id, granule_id, page_index))
                .copied()),
            Marks::Columnar(state) => {
                // Fast path: see `HefFile::mark`.
                if let Some(entry) = state
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .page_mark_if_decoded(column_id, projection_id, granule_id, page_index)?
                {
                    return Ok(entry);
                }
                let mut guard = state.write().unwrap_or_else(PoisonError::into_inner);
                let entry = guard.page_mark(self, column_id, projection_id, granule_id, page_index)?;
                let pending = std::mem::take(&mut guard.pending_aliased);
                drop(guard);
                self.publish_pending_aliases(pending);
                Ok(entry)
            }
        }
    }

    /// The file's fixed header (coverage ranges, row count, feature flags).
    pub fn header(&self) -> &HefHeader {
        &self.header
    }

    /// The file's decoded footer (column list, directories, exact counts).
    pub fn footer(&self) -> &Footer {
        &self.footer
    }

    /// The optional features this reader may actually use for this file; unknown optional bits have already been
    /// dropped.
    pub fn usable_optional_features(&self) -> u64 {
        self.usable_optional_features
    }

    /// What this reader should do with an optional feature the file declares: read it natively, read it through a
    /// fleet-resolved portable decoder, or skip it and answer from a scan instead.
    ///
    /// The decision uses the file's own forward-compatibility escape hatch — if it carries one for `feature_bit` —
    /// together with `reader_version` and the decoders `fleet` resolves and trusts. An unknown optional feature with no
    /// usable escape hatch is skipped, never read as raw bytes.
    pub fn optional_feature_plan(
        &self,
        feature_bit: u64,
        reader_version: u32,
        fleet: &dyn compat::PortableDecoderFleet,
    ) -> compat::OptionalBlockPlan {
        let hatch = self
            .footer
            .escape_hatches
            .iter()
            .find(|hatch| hatch.optional_feature_bit == feature_bit);
        compat::plan_optional_block(feature_bit, reader_version, hatch, fleet)
    }

    /// Granule pruning by internal sequence range (the sequence SkipIndex / granule directory).
    pub fn granules_for_sequence(&self, epoch: u64, first: u64, last: u64) -> Vec<&GranuleEntry> {
        self.footer
            .granules
            .iter()
            .filter(|granule| {
                granule.first_epoch <= epoch
                    && epoch <= granule.last_epoch
                    && granule.first_sequence <= last
                    && first <= granule.last_sequence
            })
            .collect()
    }

    /// Granule pruning by occurred-at range (the time SkipIndex).
    pub fn granules_for_occurred(&self, min_nanos: i64, max_nanos: i64) -> Vec<&GranuleEntry> {
        self.footer
            .granules
            .iter()
            .filter(|granule| {
                granule.min_occurred_at_physical <= max_nanos && min_nanos <= granule.max_occurred_at_physical
            })
            .collect()
    }

    /// Granules that could hold rows for one entity id, given the 64-bit hash the file stores beside every entity id
    /// (the `entity_id_hash_low` column).
    ///
    /// This is the entity point lookup's first step: instead of reading every granule's hash block to find the one
    /// row, read only the granules whose membership filter admits the hash. The answer is a superset — two entity ids
    /// can share a hash, and the filter itself admits a small share of hashes it never saw — so the caller must still
    /// compare the hash column and then confirm the candidate row against the real entity id. It is never a subset: a
    /// granule that holds the id is always returned.
    ///
    /// Files written before the filters existed, and files whose filters exceeded the writer's footer budget, carry
    /// none — every granule comes back, which is exactly the scan this replaced.
    pub fn granules_for_entity_hash(&self, entity_id_hash_low: u64) -> Vec<&GranuleEntry> {
        if self.usable_optional_features & optional_features::ENTITY_HASH_POINT_FILTERS == 0 {
            return self.footer.granules.iter().collect();
        }
        self.footer
            .granules
            .iter()
            .filter(|granule| {
                // No filter for this granule, a byte range that does not resolve, or bytes that do not parse as one:
                // keep the granule. A filter is acceleration, and the only unsafe direction is dropping a granule
                // that holds the id.
                self.entity_hash_filters_by_granule
                    .get(&granule.granule_id)
                    .and_then(|&index| self.footer.entity_hash_filters.get(index))
                    .and_then(|entry| {
                        let position = self.file_position(entry.granule_id, entry.index_offset).ok()?;
                        let bytes = self
                            .data(position, entry.index_len as usize, "entity hash filter")
                            .ok()?;
                        SplitBlockBloomFilter::contains_encoded(bytes, entity_id_hash_low).ok()
                    })
                    .unwrap_or(true)
            })
            .collect()
    }

    /// Reads one `(column, granule)` block's raw encoded bytes without decoding them: the codec pipeline, the encoded
    /// body, and the presence bitmap. This is the entry point for callers that decode straight into an Arrow array
    /// (see `encoding::decode_string_block_views`) instead of through `ColumnData`. Only a single-page block has one
    /// raw body; a multi-page block must be read page by page via [`Self::read_page_raw`].
    pub fn read_column_raw(&self, column_id: u32, granule_id: u32) -> Result<RawColumnBlock<'_>, FormatError> {
        let mark = self.mark(column_id, 0, granule_id)?.ok_or(FormatError::RefOutOfRange {
            what: "no mark for (column, projection, granule)",
        })?;
        self.read_column_raw_with_mark(granule_id, &mark)
    }

    /// [`Self::read_column_raw`], given a mark the caller already resolved — every other caller in this file that has
    /// already paid for the mark lookup (a `columnar_marks` file's `RwLock<ColumnarMarks>` lock included) reaches this
    /// instead of resolving it again.
    fn read_column_raw_with_mark(&self, granule_id: u32, mark: &ColumnMark) -> Result<RawColumnBlock<'_>, FormatError> {
        if mark.page_count > 1 {
            return Err(FormatError::RefOutOfRange {
                what: "multi-page column block has no single raw block",
            });
        }
        if mark.compressed_size == 0 {
            return Err(FormatError::Structural {
                rule: "elided constant block has no raw bytes",
            });
        }
        // A pipeline outside its availability window for this file's declared version could not legally have been
        // written by that version: refuse it as corruption before any byte is decoded.
        compat::validate_pipeline_window_pinned(mark.codec_pipeline_id.0, self.footer.format_version)?;
        self.block_reads.fetch_add(1, Ordering::Relaxed);
        let block = self.data(
            self.file_position(granule_id, mark.compressed_offset)?,
            mark.compressed_size as usize,
            "column block",
        )?;
        let mut reader = Reader::new(block);
        let presence = super::decode_presence_frame(&mut reader, mark.row_count, self.compressed_presence())?;
        let body = reader.take(reader.remaining(), "block body")?;
        Ok(RawColumnBlock {
            body,
            pipeline: mark.codec_pipeline_id,
            presence,
            row_count: mark.row_count,
        })
    }

    /// Reads one page's raw encoded bytes without decoding them — the per-page counterpart of
    /// [`Self::read_column_raw`]. When per-page marks are available only the page's own byte range is touched; without
    /// them, falls back to the whole single-page block exactly as [`Self::read_page`] does.
    pub fn read_page_raw(
        &self,
        column_id: u32,
        granule_id: u32,
        page_index: u32,
    ) -> Result<RawColumnBlock<'_>, FormatError> {
        let mark = self.mark(column_id, 0, granule_id)?.ok_or(FormatError::RefOutOfRange {
            what: "no mark for (column, projection, granule)",
        })?;
        self.read_page_raw_with_mark(column_id, granule_id, page_index, &mark)
    }

    /// [`Self::read_page_raw`], given a mark the caller already resolved — see [`Self::read_column_raw_with_mark`].
    fn read_page_raw_with_mark(
        &self,
        column_id: u32,
        granule_id: u32,
        page_index: u32,
        mark: &ColumnMark,
    ) -> Result<RawColumnBlock<'_>, FormatError> {
        if self.usable_optional_features & optional_features::PER_PAGE_MARKS != 0
            && let Some(entry) = self.page_mark(column_id, 0, granule_id, page_index)?
        {
            if entry.compressed_len == 0 {
                return Err(FormatError::Structural {
                    rule: "elided constant block has no raw bytes",
                });
            }
            self.block_reads.fetch_add(1, Ordering::Relaxed);
            let block = self.data(
                self.file_position(granule_id, entry.compressed_offset)?,
                entry.compressed_len as usize,
                "page block",
            )?;
            let mut reader = Reader::new(block);
            let presence = super::decode_presence_frame(&mut reader, entry.row_count, self.compressed_presence())?;
            let pipeline = if mark.page_count > 1 {
                PipelineId(reader.u32("page pipeline id")?)
            } else {
                mark.codec_pipeline_id
            };
            compat::validate_pipeline_window_pinned(pipeline.0, self.footer.format_version)?;
            let body = reader.take(reader.remaining(), "block body")?;
            return Ok(RawColumnBlock {
                body,
                pipeline,
                presence,
                row_count: entry.row_count,
            });
        }
        if mark.page_count > 1 {
            return Err(FormatError::RefOutOfRange {
                what: "no page directory entry for multi-page mark",
            });
        }
        // A single-page column has only page 0. Without this guard the fallback would return that one page for any
        // index, so an out-of-range request would silently read as page 0 instead of being rejected.
        if page_index != 0 {
            return Err(FormatError::RefOutOfRange {
                what: "page index beyond single-page column",
            });
        }
        self.read_column_raw_with_mark(granule_id, mark)
    }

    /// The file-scope dictionary alphabet for `column_id`, when the footer declares one — what a shared-scope
    /// dictionary block resolves its codes against. Pass it to the `_shared` decode and predicate entries when
    /// working from this file's raw block bytes.
    pub fn shared_alphabet(&self, column_id: u32) -> Option<&[String]> {
        self.shared_alphabets.get(&column_id).map(|values| values.as_slice())
    }

    /// Decodes raw string block bytes into Arrow views using this file's prepared shared dictionary, when the pipeline
    /// is directly view-decodable. This is the raw-block counterpart of [`Self::read_column_string_views`]: a scan
    /// that already fetched the block can avoid resolving its mark and presence frame a second time.
    pub fn decode_string_block_views(
        &self,
        column_id: u32,
        pipeline: PipelineId,
        body: &[u8],
    ) -> Result<Option<StringViewArray>, FormatError> {
        let shared = self.shared_alphabet(column_id);
        let uses_shared_dictionary = pipeline.value_kind()? == ValueKind::String
            && pipeline.transform()? == Transform::DictionaryString
            && pipeline.side_stream()? == SideStream::FileScopeDictionary;
        if !uses_shared_dictionary {
            return decode_string_block_views_shared_prepared(pipeline, body, shared, None);
        }

        if let Some(prepared) = self.shared_string_views.get(&column_id) {
            return decode_string_block_views_shared_prepared(pipeline, body, shared, Some(&prepared));
        }
        let alphabet = shared.ok_or(FormatError::Structural {
            rule: "shared-scope dictionary block without its file alphabet",
        })?;
        let prepared = prepare_shared_string_view_dictionary(alphabet)?;
        let prepared = self.shared_string_views.entry(column_id).or_insert(prepared);
        decode_string_block_views_shared_prepared(pipeline, body, shared, Some(&prepared))
    }

    /// Number of file-scope dictionaries prepared for Arrow view reads. This lets scan metrics and tests verify that
    /// opening a file or projecting unrelated columns did not eagerly retain view arenas.
    pub fn prepared_string_view_dictionary_count(&self) -> usize {
        self.shared_string_views.len()
    }

    /// Rebuilds an elided constant block — a zero-length mark — from the block's exact stats: no stored null, equal
    /// integer bounds, a stored value for every mark row. Anything less refuses: an elided block whose stats cannot
    /// prove its one value is corruption, never something to guess.
    fn materialize_elided_constant(
        &self,
        column_id: u32,
        granule_id: u32,
        mark: &ColumnMark,
    ) -> Result<ColumnRead, FormatError> {
        let stats = self
            .page_stats_index
            .get(&(column_id, granule_id))
            .ok_or(FormatError::Structural {
                rule: "elided block without page stats",
            })?;
        let constant = match (stats.null_count, stats.min_i128, stats.max_i128) {
            (0, Some(min), Some(max)) if min == max && stats.row_count == mark.row_count => min,
            _ => {
                return Err(FormatError::Structural {
                    rule: "elided block's stats do not prove a constant",
                });
            }
        };
        if mark.row_count as usize > super::MAX_ELIDED_BLOCK_ROWS {
            return Err(FormatError::Structural {
                rule: "elided block row count beyond the block bound",
            });
        }
        let rows = mark.row_count as usize;
        let data = match mark.codec_pipeline_id.value_kind()? {
            ValueKind::I64 => ColumnData::I64(vec![
                i64::try_from(constant).map_err(|_| FormatError::RefOutOfRange {
                    what: "elided i64 constant",
                })?;
                rows
            ]),
            ValueKind::U64 => ColumnData::U64(vec![
                u64::try_from(constant).map_err(|_| FormatError::RefOutOfRange {
                    what: "elided u64 constant",
                })?;
                rows
            ]),
            _ => {
                return Err(FormatError::Structural {
                    rule: "elided blocks carry only integer kinds",
                });
            }
        };
        Ok(ColumnRead {
            data,
            presence: Vec::new(),
        })
    }

    /// Reads one `(column, granule)` block through its mark: a constant-time range read at the mark's compressed
    /// offset. When the mark has multiple pages (page_count > 1), reads all pages sequentially and concatenates their
    /// values, returning the complete set of rows for this granule.
    pub fn read_column(&self, column_id: u32, granule_id: u32) -> Result<ColumnRead, FormatError> {
        let mark = self.mark(column_id, 0, granule_id)?.ok_or(FormatError::RefOutOfRange {
            what: "no mark for (column, projection, granule)",
        })?;
        self.read_column_with_mark(column_id, granule_id, &mark)
    }

    /// [`Self::read_column`], given a mark the caller already resolved — see [`Self::read_column_raw_with_mark`].
    fn read_column_with_mark(
        &self,
        column_id: u32,
        granule_id: u32,
        mark: &ColumnMark,
    ) -> Result<ColumnRead, FormatError> {
        if mark.page_count <= 1 {
            // An elided block stores no bytes: its zero-length mark plus the block's exact stats rebuild it.
            if mark.compressed_size == 0 {
                return self.materialize_elided_constant(column_id, granule_id, mark);
            }
            // An extent more than one mark references (mark aliasing) is fetched and decoded once per scan: the first
            // aliasing column pays the read, every other one shares its result.
            let extent = (
                self.file_position(granule_id, mark.compressed_offset)? as u64,
                mark.compressed_size,
            );
            // A file-scope dictionary block stores only codes; decode injects the reading column's own footer
            // alphabet, so byte-identical code streams under two columns still decode to different values. Those
            // decodes are never shared — each aliasing column decodes its own copy of the shared bytes.
            let column_free_decode = mark.codec_pipeline_id.side_stream() != Ok(SideStream::FileScopeDictionary);
            let shared_key = (column_free_decode && self.aliased_extents.contains(&extent))
                .then(|| (extent.0, extent.1, mark.codec_pipeline_id.0));
            if let Some(key) = shared_key {
                let tick = self.cache_access_counter.fetch_add(1, Ordering::Relaxed);
                let hit = self.aliased_extent_cache.get_mut(&key).map(|mut slot| {
                    slot.last_used = tick;
                    Arc::clone(&slot.read)
                });
                if let Some(shared) = hit {
                    validate_block_counts(&shared.presence, shared.data.row_count(), mark.row_count as usize)?;
                    return Ok((*shared).clone());
                }
            }
            let raw = self.read_column_raw_with_mark(granule_id, mark)?;
            let data = decode_block_shared(raw.pipeline, raw.body, self.shared_alphabet(column_id))?;
            validate_block_counts(&raw.presence, data.row_count(), raw.row_count as usize)?;
            let read = ColumnRead {
                presence: raw.presence.into_owned(),
                data,
            };
            if let Some(key) = shared_key {
                let tick = self.cache_access_counter.fetch_add(1, Ordering::Relaxed);
                let bytes = column_data_bytes(&read.data);
                let shared = Arc::new(read.clone());
                // This cache shares the decoded-byte budget with the column and residual caches and always yields
                // first (see `shed_aliased_extents`): worst case an aliased extent decodes once per aliasing column
                // again, never a wrong result.
                let slot = self.aliased_extent_cache.entry(key).or_insert(AliasedExtentSlot {
                    bytes,
                    last_used: tick,
                    read: Arc::clone(&shared),
                });
                if Arc::ptr_eq(&slot.read, &shared) {
                    self.aliased_extent_cache_bytes.fetch_add(bytes, Ordering::Relaxed);
                }
                drop(slot);
                self.shed_aliased_extents();
            }
            return Ok(read);
        }

        // Multi-page: iterate all pages in order and concatenate.
        let mut combined_presence: Vec<u8> = Vec::new();
        let mut combined_data: Option<ColumnData> = None;
        let mut row_offset = 0usize;
        for page_idx in 0..mark.page_count {
            let page_rows = self
                .page_mark(column_id, 0, granule_id, page_idx)?
                .map(|entry| entry.row_count as usize)
                .ok_or(FormatError::RefOutOfRange {
                    what: "no page directory entry for multi-page mark",
                })?;
            let page = self.read_page_with_mark(column_id, granule_id, page_idx, mark)?;
            if page.presence.is_empty() {
                if !combined_presence.is_empty() {
                    append_all_present_bits(&mut combined_presence, row_offset, page_rows);
                }
            } else {
                if combined_presence.is_empty() && row_offset != 0 {
                    append_all_present_bits(&mut combined_presence, 0, row_offset);
                }
                append_presence_bits(&mut combined_presence, row_offset, &page.presence, page_rows);
            }
            row_offset += page_rows;
            combined_data = Some(match combined_data {
                None => page.data,
                Some(existing) => concat_column_data(existing, page.data)?,
            });
        }
        let data = combined_data.ok_or(FormatError::RefOutOfRange {
            what: "no pages for multi-page mark",
        })?;
        if row_offset != mark.row_count as usize {
            return Err(FormatError::Structural {
                rule: "sum of page row counts must equal the mark row count",
            });
        }
        validate_block_counts(&combined_presence, data.row_count(), mark.row_count as usize)?;
        Ok(ColumnRead {
            presence: combined_presence,
            data,
        })
    }

    /// Reads one string column block as a zero-copy Arrow string-view array, for whole-column scans that would
    /// otherwise pay one `String` allocation per row materializing through [`ColumnRead`].
    ///
    /// Returns the block's presence bitmap paired with the views; the views hold one entry per stored row (nulls
    /// included), exactly the rows [`Self::read_column`] would decode, backed by the block's shared buffers instead of
    /// per-value allocations. Returns `None` — read through [`Self::read_column`] instead, with identical values —
    /// when the block is not a string column. Multi-page columns copy only their 16-byte views while retaining each
    /// page's immutable arena as an Arrow backing buffer.
    pub fn read_column_string_views(
        &self,
        column_id: u32,
        granule_id: u32,
    ) -> Result<Option<(Vec<u8>, StringViewArray)>, FormatError> {
        let mark = self.mark(column_id, 0, granule_id)?.ok_or(FormatError::RefOutOfRange {
            what: "no mark for (column, projection, granule)",
        })?;
        if mark.page_count > 1 {
            let mut combined_presence = Vec::new();
            let mut builder = StringViewBuilder::with_capacity(mark.row_count as usize);
            let mut row_offset = 0usize;
            for page_index in 0..mark.page_count {
                let raw = self.read_page_raw_with_mark(column_id, granule_id, page_index, &mark)?;
                let page_rows = raw.row_count as usize;
                let Some(views) = self.decode_string_block_views(column_id, raw.pipeline, raw.body)? else {
                    return Ok(None);
                };
                validate_block_counts(&raw.presence, views.len(), page_rows)?;
                if raw.presence.is_empty() {
                    if !combined_presence.is_empty() {
                        append_all_present_bits(&mut combined_presence, row_offset, page_rows);
                    }
                } else {
                    if combined_presence.is_empty() && row_offset != 0 {
                        append_all_present_bits(&mut combined_presence, 0, row_offset);
                    }
                    append_presence_bits(&mut combined_presence, row_offset, &raw.presence, page_rows);
                }
                row_offset += page_rows;
                builder.append_array(&views);
            }
            if row_offset != mark.row_count as usize {
                return Err(FormatError::Structural {
                    rule: "sum of page row counts must equal the mark row count",
                });
            }
            let views = builder.finish();
            validate_block_counts(&combined_presence, views.len(), mark.row_count as usize)?;
            return Ok(Some((combined_presence, views)));
        }
        let raw = self.read_column_raw_with_mark(granule_id, &mark)?;
        let Some(views) = self.decode_string_block_views(column_id, raw.pipeline, raw.body)? else {
            return Ok(None);
        };
        validate_block_counts(&raw.presence, views.len(), raw.row_count as usize)?;
        Ok(Some((raw.presence.into_owned(), views)))
    }

    /// The token-membership filter for one string column page, or `None` when the page carries none.
    ///
    /// Files that declare `TEXT_TOKEN_FILTER_OFFSETS` store the filter bytes in the data area with only their byte
    /// ranges in the footer, so a cold open never fetches them; the filter is sliced and decoded here on first
    /// demand. Files from before the relocation carry the bytes inline in the footer and decode from there, with
    /// identical pruning behaviour.
    ///
    /// Decoded once per `(column, granule, page)` and served from `text_token_cache` afterwards, so a scan probing
    /// the same page across many granules or predicate columns pays one decode, not one per probe.
    pub fn text_token_filter(
        &self,
        column_id: u32,
        granule_id: u32,
        page_index: u32,
    ) -> Result<Option<Arc<TextTokenIndex>>, FormatError> {
        let key = (column_id, granule_id, page_index);
        if let Some(cached) = self.text_token_cache.get(&key) {
            return Ok(Some(Arc::clone(&cached)));
        }
        let Some(decoded) = self.decode_text_token_filter(column_id, granule_id, page_index)? else {
            return Ok(None);
        };
        // Decoded outside the cache; a race just decodes the same filter twice, and the first insertion wins.
        let decoded = Arc::new(decoded);
        let entry = Arc::clone(&self.text_token_cache.entry(key).or_insert(decoded));
        Ok(Some(entry))
    }

    /// Locates and decodes one page's text-token filter straight from the file's bytes, with no cache involved. Only
    /// [`Self::text_token_filter`] calls this; every other caller goes through the cache.
    fn decode_text_token_filter(
        &self,
        column_id: u32,
        granule_id: u32,
        page_index: u32,
    ) -> Result<Option<TextTokenIndex>, FormatError> {
        if self.usable_optional_features & optional_features::TEXT_TOKEN_FILTER_OFFSETS != 0
            && let Some(entry) = self.text_token_offsets.get(&(column_id, granule_id, page_index))
        {
            let bytes = self.data(
                self.file_position(granule_id, entry.index_offset)?,
                entry.index_len as usize,
                "text token filter",
            )?;
            return TextTokenIndex::decode(bytes).map(Some);
        }
        self.text_token_indexes_by_key
            .get(&(column_id, granule_id, page_index))
            .and_then(|&index| self.footer.text_token_indexes.get(index))
            .map(|entry| TextTokenIndex::decode(&entry.index_bytes))
            .transpose()
    }

    /// Streams one declared column family across every granule that holds it, in ascending granule order, returning
    /// each granule's id paired with its decoded block.
    ///
    /// This is the bulk-egress path that release-migration re-extraction and per-subject erasure jobs use: it touches
    /// only the family's column blocks and never the residual variant arena, so the whole family is read in one
    /// sequential pass instead of per-row point lookups. This is HEF's cold-scan mode (see the module docs): it reads
    /// through [`Self::read_column`] directly and never initializes [`Self::cached_column`]'s search cache.
    pub fn bulk_read_family(&self, column_id: u32) -> Result<Vec<(u32, ColumnRead)>, FormatError> {
        let mut granule_ids = Vec::new();
        for granule in &self.footer.granules {
            if self.mark(column_id, 0, granule.granule_id)?.is_some() {
                granule_ids.push(granule.granule_id);
            }
        }
        granule_ids.sort_unstable();
        granule_ids.dedup();
        let mut reads = Vec::with_capacity(granule_ids.len());
        for granule_id in granule_ids {
            reads.push((granule_id, self.read_column(column_id, granule_id)?));
        }
        Ok(reads)
    }

    /// Reads one page from a `(column, granule)` block by its page index.
    ///
    /// When per-page marks are available, only the page's own byte range is read — no neighbouring pages are touched.
    /// Falls back to the full granule-level read when the feature is absent, returning identical results so callers are
    /// insulated from whether the file carries the page directory.
    pub fn read_page(&self, column_id: u32, granule_id: u32, page_index: u32) -> Result<ColumnRead, FormatError> {
        let mark = self.mark(column_id, 0, granule_id)?.ok_or(FormatError::RefOutOfRange {
            what: "no mark for (column, projection, granule)",
        })?;
        self.read_page_with_mark(column_id, granule_id, page_index, &mark)
    }

    /// [`Self::read_page`], given a mark the caller already resolved — see [`Self::read_column_raw_with_mark`].
    fn read_page_with_mark(
        &self,
        column_id: u32,
        granule_id: u32,
        page_index: u32,
        mark: &ColumnMark,
    ) -> Result<ColumnRead, FormatError> {
        // An elided block is single-page, so page 0 is the whole block, rebuilt from its stats.
        if page_index == 0 && mark.page_count <= 1 && mark.compressed_size == 0 {
            return self.materialize_elided_constant(column_id, granule_id, mark);
        }
        let raw = self.read_page_raw_with_mark(column_id, granule_id, page_index, mark)?;
        let data = decode_block_shared(raw.pipeline, raw.body, self.shared_alphabet(column_id))?;
        validate_block_counts(&raw.presence, data.row_count(), raw.row_count as usize)?;
        Ok(ColumnRead {
            data,
            presence: raw.presence.into_owned(),
        })
    }

    /// Finds the granule covering `row_ordinal` by binary search over `footer.granules`, which `open` validated as
    /// ascending by `first_row_ordinal`: `partition_point` finds the last granule starting at or before the row in
    /// O(log granules) rather than scanning every granule ahead of it.
    fn granule_of_row(&self, row_ordinal: u64) -> Result<&GranuleEntry, FormatError> {
        let granules = &self.footer.granules;
        let index = granules.partition_point(|granule| granule.first_row_ordinal <= row_ordinal);
        index
            .checked_sub(1)
            .and_then(|index| granules.get(index))
            .filter(|granule| {
                granule
                    .first_row_ordinal
                    .checked_add(u64::from(granule.row_count))
                    .is_some_and(|end| row_ordinal < end)
            })
            .ok_or(FormatError::RefOutOfRange {
                what: "row ordinal beyond granule directory",
            })
    }

    fn payload_granule(&self, granule_id: u32) -> Result<&PayloadGranule, FormatError> {
        self.payload_granule_index
            .get(&granule_id)
            .ok_or(FormatError::RefOutOfRange {
                what: "granule missing from payload index",
            })
    }

    fn residual_slot(&self, payload: &PayloadGranule, row_in_granule: u64) -> Result<Option<(u64, u64)>, FormatError> {
        let offsets = self.data(
            self.file_position(payload.granule_id, payload.offsets_offset)?,
            payload.offsets_len as usize,
            "payload offsets",
        )?;
        let entry = slice(
            offsets,
            row_in_granule as usize * ROW_OFFSET_ENTRY_LEN,
            ROW_OFFSET_ENTRY_LEN,
            "payload offset entry",
        )?;
        let mut reader = Reader::new(entry);
        let offset = reader.u32("payload offset")?;
        let len = reader.u32("payload len")?;
        if len == 0 {
            return Ok(None);
        }
        Ok(Some((u64::from(offset), u64::from(len))))
    }

    /// One row's residual bytes, never copied out of wherever they already live: read straight from the file's own
    /// buffer for an uncompressed (hot) granule — the common case per [`ResidualCompression`] — and read in place from
    /// the inflated bytes of a compressed (cold) one, whose `Arc` the result keeps alive for as long as the caller
    /// holds the bytes. A seekable arena inflates only the frame the value falls in; a whole-arena `Zstd3` one, the
    /// form builds before the seekable frames wrote, inflates all of it.
    fn residual_bytes(
        &self,
        payload: &PayloadGranule,
        offset: u64,
        len: u64,
    ) -> Result<ResidualBytes<'_>, FormatError> {
        match payload.residual_compression {
            ResidualCompression::None => {
                // Only the row's own slot is read, so a remote reader fetches that slot rather than the whole arena.
                if offset.checked_add(len).is_none_or(|end| end > payload.residual_len) {
                    return Err(FormatError::Truncated { what: "residual value" });
                }
                let position = self
                    .file_position(payload.granule_id, payload.residual_offset)?
                    .checked_add(offset as usize)
                    .ok_or(FormatError::RefOutOfRange {
                        what: "offset beyond file range",
                    })?;
                Ok(ResidualBytes::InFile(self.data(
                    position,
                    len as usize,
                    "residual value",
                )?))
            }
            ResidualCompression::Zstd3 => {
                // The per-row offsets index the inflated bytes, so the whole arena is inflated once and cached per
                // granule for the reads that follow.
                let stored = self.residual_block(payload)?;
                let arena = self.inflated_residual(payload.granule_id, 0, || {
                    active_decompressor().decompress(Compression::Zstd3, stored)
                })?;
                // Bounds-check the span against the arena here, so reading the bytes back out is a plain in-range
                // slice with nothing left to fail on.
                let start = offset as usize;
                let end = start + slice(&arena, start, len as usize, "residual value")?.len();
                Ok(ResidualBytes::InArena { arena, end, start })
            }
            ResidualCompression::ZstdSeekable => {
                let stored = self.residual_block(payload)?;
                let table = self.residual_seek_table(payload, stored)?;
                let end = offset.checked_add(len).ok_or(FormatError::RefOutOfRange {
                    what: "residual value span",
                })?;
                let frame = table.frame_index_decomp(offset);
                let bound = |result: Result<u64, zeekstd::Error>| {
                    result.map_err(|_| FormatError::RefOutOfRange {
                        what: "residual frame index",
                    })
                };
                let frame_start = bound(table.frame_start_decomp(frame))?;
                let frame_end = bound(table.frame_end_decomp(frame))?;
                if end > frame_end {
                    // A value straddling frames is decompressed in one pass over exactly the frames it covers.
                    // Uncached: it is rarer than a value inside one frame, and it is not what the next read wants.
                    let value = seekable_zstd::decompress_range(stored, &table, offset, end)?;
                    return Ok(ResidualBytes::InArena {
                        end: value.len(),
                        arena: Arc::new(value),
                        start: 0,
                    });
                }
                // Inflating the whole frame rather than the value alone is what makes a per-row loop over the granule
                // pay one decompression per frame instead of one per row.
                let arena = self.inflated_residual(payload.granule_id, frame, || {
                    seekable_zstd::decompress_range(stored, &table, frame_start, frame_end)
                })?;
                let start = (offset - frame_start) as usize;
                let end = start + slice(&arena, start, len as usize, "residual value")?.len();
                Ok(ResidualBytes::InArena { arena, end, start })
            }
        }
    }

    /// A granule's residual block exactly as stored — compressed if the granule declares a compression.
    fn residual_block(&self, payload: &PayloadGranule) -> Result<&[u8], FormatError> {
        self.data(
            self.file_position(payload.granule_id, payload.residual_offset)?,
            payload.residual_len as usize,
            "residual block",
        )
    }

    /// The seek table of a granule's seekable residual arena, parsed on first touch and held for the file's lifetime.
    /// Every read of the arena needs it to resolve an offset to a frame, and one table is a handful of bytes per
    /// frame, so it is kept rather than evicted — bounded, like the file's own granule count.
    fn residual_seek_table(&self, payload: &PayloadGranule, stored: &[u8]) -> Result<Arc<SeekTable>, FormatError> {
        if let Some(table) = self.residual_seek_tables.get(&payload.granule_id) {
            return Ok(Arc::clone(&table));
        }
        let table = Arc::new(seekable_zstd::seek_table(stored)?);
        Ok(Arc::clone(
            &self.residual_seek_tables.entry(payload.granule_id).or_insert(table),
        ))
    }

    /// One inflated span of a cold granule's residual arena — the whole arena for a `Zstd3` granule, one frame for a
    /// seekable one — inflated by `inflate` on first touch and cached under `(granule_id, frame)` so a per-row
    /// reconstruction loop pays the inflation once, not once per row. An accelerator only, like `column_cache`; the
    /// file's stored bytes remain the source of truth.
    fn inflated_residual(
        &self,
        granule_id: u32,
        frame: u32,
        inflate: impl FnOnce() -> Result<Vec<u8>, FormatError>,
    ) -> Result<Arc<Vec<u8>>, FormatError> {
        let key = (granule_id, frame);
        let tick = self.cache_access_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(mut slot) = self.inflated_residuals.get_mut(&key) {
            slot.last_used = tick;
            return Ok(Arc::clone(&slot.arena));
        }
        let inflated = Arc::new(inflate()?);
        let bytes = inflated.len() as u64;
        let arena = {
            let slot = self.inflated_residuals.entry(key).or_insert(InflatedResidualSlot {
                arena: Arc::clone(&inflated),
                last_used: tick,
            });
            if Arc::ptr_eq(&slot.arena, &inflated) {
                self.inflated_residuals_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Arc::clone(&slot.arena)
        };
        // The same budget-and-LRU discipline as the column cache: the span just touched always survives.
        while self.inflated_residuals_bytes.load(Ordering::Relaxed) > self.decoded_cache_budget
            && self.inflated_residuals.len() > 1
        {
            let Some(evict) = self
                .inflated_residuals
                .iter()
                .filter(|slot| *slot.key() != key)
                .min_by_key(|slot| slot.last_used)
                .map(|slot| *slot.key())
            else {
                break;
            };
            if let Some((_, evicted)) = self.inflated_residuals.remove(&evict) {
                self.inflated_residuals_bytes
                    .fetch_sub(evicted.arena.len() as u64, Ordering::Relaxed);
            }
        }
        self.shed_aliased_extents();
        Ok(arena)
    }

    /// Sheds least-recently-used aliased-extent slots until they fit in the headroom the column and residual caches
    /// leave under the shared budget. The alias cache always yields first — it only saves duplicate decodes of
    /// byte-identical blocks, the cheapest work to redo — so the combined resident bytes stay bounded.
    fn shed_aliased_extents(&self) {
        // The common case, for a file with no mark aliasing at all: nothing has ever gone into this cache, so there
        // is nothing to shed. Check that first, cheaply, before summing the other three caches on what is otherwise
        // called after every decode-cache insert.
        if self.aliased_extent_cache.is_empty() {
            return;
        }
        let others = self.column_cache_bytes.load(Ordering::Relaxed)
            + self.inflated_residuals_bytes.load(Ordering::Relaxed)
            + self.granule_dictionaries_bytes.load(Ordering::Relaxed);
        let headroom = self.decoded_cache_budget.saturating_sub(others);
        while self.aliased_extent_cache_bytes.load(Ordering::Relaxed) > headroom
            && !self.aliased_extent_cache.is_empty()
        {
            let Some(evict) = self
                .aliased_extent_cache
                .iter()
                .min_by_key(|slot| slot.last_used)
                .map(|slot| *slot.key())
            else {
                break;
            };
            if let Some((_, evicted)) = self.aliased_extent_cache.remove(&evict) {
                self.aliased_extent_cache_bytes
                    .fetch_sub(evicted.bytes, Ordering::Relaxed);
            }
        }
    }

    /// Resolves one row's `(offset, len)` entry from a free-text column's per-row byte-offset index — the same shape as
    /// [`Self::residual_slot`] but scoped to one column's own raw-bytes block instead of the granule-wide residual
    /// block. `None` means the row carries no value for this column.
    fn freetext_row_slot(
        &self,
        entry: &FreetextRowOffsets,
        row_in_granule: u64,
    ) -> Result<Option<(u64, u64)>, FormatError> {
        let offsets = self.data(
            self.file_position(entry.granule_id, entry.offsets_offset)?,
            entry.offsets_len as usize,
            "freetext row offsets",
        )?;
        let slot = slice(
            offsets,
            row_in_granule as usize * ROW_OFFSET_ENTRY_LEN,
            ROW_OFFSET_ENTRY_LEN,
            "freetext row offset entry",
        )?;
        let mut reader = Reader::new(slot);
        let offset = reader.u32("freetext row offset")?;
        let len = reader.u32("freetext row len")?;
        if len == 0 {
            // A zero length is an absent value, unless the offset marks the row as carrying a present empty string —
            // which reads back from the arena's start as the empty slice it was written as.
            return Ok((offset == EMPTY_VALUE_ROW_OFFSET).then_some((0, 0)));
        }
        Ok(Some((u64::from(offset), u64::from(len))))
    }

    /// Reads one row's exact bytes out of a free-text column's raw-bytes block, given the `(offset, len)`
    /// [`Self::freetext_row_slot`] resolved.
    fn freetext_row_bytes(&self, entry: &FreetextRowOffsets, offset: u64, len: u64) -> Result<&[u8], FormatError> {
        let bytes = self.data(
            self.file_position(entry.granule_id, entry.bytes_offset)?,
            entry.bytes_len as usize,
            "freetext row bytes",
        )?;
        Ok(slice(bytes, offset as usize, len as usize, "freetext row value")?)
    }

    /// Resolves one row's `(offset, len)` entry from an internal embedding/vector column's per-row byte-offset index —
    /// the same shape as [`Self::residual_slot`] and [`Self::freetext_row_slot`], scoped to one column's own raw-bytes
    /// block. Returns `None` when the file carries no index for this column/granule (the caller falls back to the
    /// whole-granule decode) or when the row carries no value.
    pub fn embedding_row_slot(
        &self,
        column_id: u32,
        granule_id: u32,
        row_in_granule: u64,
    ) -> Result<Option<(u64, u64)>, FormatError> {
        let Some(entry) = self.embedding_row_offsets.get(&(column_id, granule_id)) else {
            return Ok(None);
        };
        let offsets = self.data(
            self.file_position(entry.granule_id, entry.offsets_offset)?,
            entry.offsets_len as usize,
            "embedding row offsets",
        )?;
        let slot = slice(
            offsets,
            row_in_granule as usize * ROW_OFFSET_ENTRY_LEN,
            ROW_OFFSET_ENTRY_LEN,
            "embedding row offset entry",
        )?;
        let mut reader = Reader::new(slot);
        let offset = reader.u32("embedding row offset")?;
        let len = reader.u32("embedding row len")?;
        if len == 0 {
            return Ok(None);
        }
        Ok(Some((u64::from(offset), u64::from(len))))
    }

    /// Reads one row's exact bytes out of an internal embedding/vector column's raw-bytes block, given the
    /// `(offset, len)` [`Self::embedding_row_slot`] resolved for the same `column_id`/`granule_id`.
    pub fn embedding_row_bytes(
        &self,
        column_id: u32,
        granule_id: u32,
        offset: u64,
        len: u64,
    ) -> Result<Vec<u8>, FormatError> {
        let entry = self
            .embedding_row_offsets
            .get(&(column_id, granule_id))
            .ok_or(FormatError::RefOutOfRange {
                what: "embedding row offsets missing for column/granule",
            })?;
        let bytes = self.data(
            self.file_position(entry.granule_id, entry.bytes_offset)?,
            entry.bytes_len as usize,
            "embedding row bytes",
        )?;
        Ok(slice(bytes, offset as usize, len as usize, "embedding row value")?.to_vec())
    }

    /// Reads one declared free-text field for one row. When the file carries the per-row byte-offset index
    /// (`typed_column_row_offsets`) for this column and granule, resolves the row's `(offset, len)` from the index and
    /// reads only that row's bytes — no whole-granule free-text block decode. Falls back to the whole-granule decode
    /// path ([`Self::shredded_value_for_row`]) when the index is absent, returning byte-identical values either way.
    fn freetext_value_for_row(
        &self,
        column_id: u32,
        granule: &GranuleEntry,
        row_in_granule: u64,
    ) -> Result<Option<VariantValue>, FormatError> {
        if self.usable_optional_features & optional_features::TYPED_COLUMN_ROW_OFFSETS != 0
            && let Some(entry) = self.freetext_row_offsets.get(&(column_id, granule.granule_id))
        {
            let Some((offset, len)) = self.freetext_row_slot(entry, row_in_granule)? else {
                return Ok(None);
            };
            let bytes = self.freetext_row_bytes(entry, offset, len)?;
            let text = std::str::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
                what: "freetext row value",
            })?;
            return Ok(Some(VariantValue::String(text.to_owned())));
        }
        self.shredded_value_for_row(column_id, granule, row_in_granule)
    }

    /// Reads one row's exact stored embedding/vector by row ordinal — the exact-fetch path beside the approximate ANN
    /// candidate-finding path, which this never touches. When the file carries the per-row byte-offset index
    /// (`typed_column_row_offsets`) for this column and granule, resolves the row's `(offset, len)` from the index and
    /// reads only that row's bytes — no whole vector-block decode. Falls back to decoding the whole granule block
    /// ([`Self::read_column`]) when the index is absent, returning byte-identical vectors either way.
    pub fn embedding_value_for_row(&self, column_id: u32, row_ordinal: u64) -> Result<Option<Vec<u8>>, FormatError> {
        let granule = self.granule_of_row(row_ordinal)?;
        let row_in_granule = row_ordinal - granule.first_row_ordinal;
        if self.usable_optional_features & optional_features::TYPED_COLUMN_ROW_OFFSETS != 0
            && self
                .embedding_row_offsets
                .contains_key(&(column_id, granule.granule_id))
        {
            let Some((offset, len)) = self.embedding_row_slot(column_id, granule.granule_id, row_in_granule)? else {
                return Ok(None);
            };
            return Ok(Some(self.embedding_row_bytes(
                column_id,
                granule.granule_id,
                offset,
                len,
            )?));
        }
        let read = self.read_column(column_id, granule.granule_id)?;
        let ColumnData::Strings(values) = &read.data else {
            return Ok(None);
        };
        Ok(values
            .get(row_in_granule as usize)
            .flatten()
            .map(|value| value.as_bytes().to_vec()))
    }

    /// The decoded payload key dictionary of one granule, decoded on first touch and cached per granule so a run of
    /// point reads into the granule pays the decode once, not once per row. An accelerator only, like `column_cache`;
    /// the file's stored bytes remain the source of truth.
    fn granule_dictionary(&self, payload: &PayloadGranule) -> Result<Arc<KeyDictionary>, FormatError> {
        let tick = self.cache_access_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(mut slot) = self.granule_dictionaries.get_mut(&payload.granule_id) {
            slot.last_used = tick;
            return Ok(Arc::clone(&slot.dictionary));
        }
        self.dictionary_decodes.fetch_add(1, Ordering::Relaxed);
        let bytes = self.data(
            self.file_position(payload.granule_id, payload.dictionary_offset)?,
            payload.dictionary_len as usize,
            "granule dictionary",
        )?;
        let decoded = Arc::new(decode_variant_dictionary(bytes)?);
        let bytes = dictionary_bytes(&decoded);
        let dictionary = {
            let slot = self
                .granule_dictionaries
                .entry(payload.granule_id)
                .or_insert(DictionarySlot {
                    bytes,
                    dictionary: Arc::clone(&decoded),
                    last_used: tick,
                });
            if Arc::ptr_eq(&slot.dictionary, &decoded) {
                self.granule_dictionaries_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Arc::clone(&slot.dictionary)
        };
        // The same budget-and-LRU discipline as the column cache: the dictionary just touched always survives.
        while self.granule_dictionaries_bytes.load(Ordering::Relaxed) > self.decoded_cache_budget
            && self.granule_dictionaries.len() > 1
        {
            let Some(evict) = self
                .granule_dictionaries
                .iter()
                .filter(|slot| *slot.key() != payload.granule_id)
                .min_by_key(|slot| slot.last_used)
                .map(|slot| *slot.key())
            else {
                break;
            };
            if let Some((_, evicted)) = self.granule_dictionaries.remove(&evict) {
                self.granule_dictionaries_bytes
                    .fetch_sub(evicted.bytes, Ordering::Relaxed);
            }
        }
        self.shed_aliased_extents();
        Ok(dictionary)
    }

    fn row_payload_flags(&self, granule: &GranuleEntry, row_in_granule: u64) -> Result<u64, FormatError> {
        // Payload flags are dense (one value per row), so the block is decoded once per granule through the cache and
        // indexed directly by row — never re-decoded per row.
        let flags = self.cached_column(crate::columns::column_ids::PAYLOAD_FLAGS, granule.granule_id)?;
        Self::payload_flags_at(&flags, row_in_granule)
    }

    /// One row's payload flags out of an already-decoded `payload_flags` block, indexed directly by row since the
    /// column is dense. Shared by [`Self::row_payload_flags`] (which resolves the block itself, once per row) and
    /// [`Self::payload_in_group`] (which resolves it once for a whole batch of rows sharing a granule).
    fn payload_flags_at(flags: &CachedColumn, row_in_granule: u64) -> Result<u64, FormatError> {
        match &flags.data {
            ColumnData::U64(values) => values
                .get(row_in_granule as usize)
                .copied()
                .ok_or(FormatError::RefOutOfRange {
                    what: "row beyond payload_flags block",
                }),
            _ => Err(FormatError::Structural {
                rule: "payload_flags must be a u64 column",
            }),
        }
    }

    /// One present value of a decoded block as the `VariantValue` a payload merge re-inserts, or `None` when the
    /// column kind has no variant form. `position` is the value's dense position among the block's present rows.
    fn typed_variant_at(data: &ColumnData, position: usize) -> Option<VariantValue> {
        match data {
            ColumnData::I64(values) => values.get(position).map(|v| VariantValue::Int(*v)),
            ColumnData::F64(values) => values.get(position).map(|v| VariantValue::Double(*v)),
            ColumnData::Decimal { values, scale } => values.get(position).map(|v| VariantValue::Decimal {
                unscaled: *v,
                scale: *scale,
            }),
            ColumnData::Strings(values) => values
                .get(position)
                .flatten()
                .map(|v| VariantValue::String(v.to_owned())),
            _ => None,
        }
    }

    fn shredded_value_for_row(
        &self,
        column_id: u32,
        granule: &GranuleEntry,
        row_in_granule: u64,
    ) -> Result<Option<VariantValue>, FormatError> {
        // A granule already decoded for this column answers from the cache: the rank index gives presence and the
        // dense position in O(log runs).
        let cache_hit = {
            let tick = self.cache_access_counter.fetch_add(1, Ordering::Relaxed);
            self.column_cache
                .get_mut(&(column_id, granule.granule_id))
                .map(|mut slot| {
                    slot.last_used = tick;
                    Arc::clone(&slot.column)
                })
        };
        if let Some(column) = cache_hit {
            return Ok(column.value_at(row_in_granule));
        }
        // Cold single-row read of a per-value-addressable block: decode only this row's value instead of paying a
        // whole-granule decode plus a rank-index build to answer one row. Multi-page blocks, pipelines without
        // byte-range extraction, and a block already probed often enough that the whole-granule decode pays for
        // itself all fall through to the whole-granule path below.
        if let Some(mark) = self.mark(column_id, 0, granule.granule_id)?
            && mark.page_count <= 1
            && mark.compressed_size > 0
            && self.count_point_probe(column_id, granule.granule_id) <= POINT_READS_BEFORE_WHOLE_GRANULE_DECODE
        {
            let raw = self.read_column_raw_with_mark(granule.granule_id, &mark)?;
            if raw.pipeline.supports_byte_range_extraction()? {
                let row = row_in_granule as usize;
                let position = if raw.presence.is_empty() {
                    row
                } else {
                    match present_position(&raw.presence, row) {
                        Some(position) => position,
                        None => return Ok(None),
                    }
                };
                let data = decode_block_range_shared(
                    raw.pipeline,
                    raw.body,
                    self.shared_alphabet(column_id),
                    position,
                    position + 1,
                )?;
                return Ok(Self::typed_variant_at(&data, 0));
            }
        }
        Ok(self
            .cached_column(column_id, granule.granule_id)?
            .value_at(row_in_granule))
    }

    /// Returns the decoded column block for `(column_id, granule_id)`, decoding it and building its rank index on first
    /// use and serving a cached copy thereafter. Sharing the decode and the rank index across every row of a granule is
    /// what removes the former O(n²) reconstruction cost. This is HEF's search-cache init mode (see the module docs):
    /// callers making repeated point lookups reach it through [`Self::payload`]/[`Self::payload_path`]/
    /// [`Self::read_payload_paths`], never a cold scan.
    fn cached_column(&self, column_id: u32, granule_id: u32) -> Result<Arc<CachedColumn>, FormatError> {
        let key = (column_id, granule_id);
        let tick = self.cache_access_counter.fetch_add(1, Ordering::Relaxed);
        if let Some(mut slot) = self.column_cache.get_mut(&key) {
            slot.last_used = tick;
            return Ok(Arc::clone(&slot.column));
        }
        // Decode outside the cache; a race just rebuilds the same block, and the first insertion wins. The rank index
        // is built from the presence bitmap `read_column` already decoded — never by re-fetching and re-parsing the
        // block's presence side stream a second time — by a byte-run scan (equivalence against a naive per-bit scan
        // is pinned in the rank_select tests), never by collecting one row id per set bit.
        let read = self.read_column(column_id, granule_id)?;
        let bytes = column_data_bytes(&read.data);
        let rank = RankSelect::from_bitmap(&RoaringRangeBitmap::from_packed_bits(&read.presence));
        let decoded = Arc::new(CachedColumn { data: read.data, rank });
        let column = {
            let slot = self.column_cache.entry(key).or_insert(CachedColumnSlot {
                bytes,
                column: Arc::clone(&decoded),
                last_used: tick,
            });
            if Arc::ptr_eq(&slot.column, &decoded) {
                self.column_cache_bytes.fetch_add(bytes, Ordering::Relaxed);
            }
            Arc::clone(&slot.column)
        };
        // Hold the cache under its byte budget by dropping least-recently-used slots — never the one just touched, so
        // a single block larger than the whole budget still serves its reads.
        while self.column_cache_bytes.load(Ordering::Relaxed) > self.decoded_cache_budget && self.column_cache.len() > 1
        {
            let Some(evict) = self
                .column_cache
                .iter()
                .filter(|slot| *slot.key() != key)
                .min_by_key(|slot| slot.last_used)
                .map(|slot| *slot.key())
            else {
                break;
            };
            if let Some((_, evicted)) = self.column_cache.remove(&evict) {
                self.column_cache_bytes.fetch_sub(evicted.bytes, Ordering::Relaxed);
            }
        }
        self.shed_aliased_extents();
        Ok(column)
    }

    /// Sizes the decoded-block cache budget, replacing [`DECODED_CACHE_BUDGET_BYTES`]. A caller-supplied working-set
    /// bound, not an operator key; eviction only ever costs a re-decode, never a result.
    pub fn with_decoded_cache_budget(mut self, bytes: u64) -> Self {
        self.decoded_cache_budget = bytes;
        self
    }

    /// Decoded bytes currently held by the block, residual, and dictionary caches together. Never feeds a read
    /// result; it lets tests observe that a long-lived reader's decoded working set stays within its budget.
    pub fn decoded_cache_bytes(&self) -> u64 {
        self.aliased_extent_cache_bytes.load(Ordering::Relaxed)
            + self.column_cache_bytes.load(Ordering::Relaxed)
            + self.granule_dictionaries_bytes.load(Ordering::Relaxed)
            + self.inflated_residuals_bytes.load(Ordering::Relaxed)
    }

    /// Bumps and returns the cold point-read count for one `(column, granule)` block, so
    /// [`Self::shredded_value_for_row`] can tell a scattered probe pattern from a dense per-row loop.
    fn count_point_probe(&self, column_id: u32, granule_id: u32) -> u32 {
        // The counters are a heuristic only, so at the entry cap the map resets rather than growing forever; a reset
        // merely re-runs the cheap point path before the next whole-granule migration.
        if self.point_probe_counts.len() >= POINT_PROBE_ENTRIES_MAX
            && !self.point_probe_counts.contains_key(&(column_id, granule_id))
        {
            self.point_probe_counts.clear();
        }
        let mut count = self.point_probe_counts.entry((column_id, granule_id)).or_insert(0);
        *count = count.saturating_add(1);
        *count
    }

    /// Late-materialized payload reconstruction for one final matching row: the deterministic merge of shredded typed
    /// values (and declared free-text fields) into the residual value. A shredded path's value exists in exactly one of
    /// the two.
    pub fn payload(&self, row_ordinal: u64) -> Result<PayloadRead, FormatError> {
        let granule = self.granule_of_row(row_ordinal)?;
        let payload = self.payload_granule(granule.granule_id)?;
        self.payload_in_granule(granule, payload, row_ordinal - granule.first_row_ordinal)
    }

    /// Reads many rows' payloads in one call, amortizing the per-granule work the per-row read repeats: the granule
    /// lookup and payload-directory lookup happen at most once per granule touched, the dictionary decode at most
    /// once per granule for the reader's lifetime, and each granule's rows are visited in ascending order so the
    /// resulting byte ranges can coalesce on a remote file. Returns one entry per reference in the caller's original
    /// order, each identical to what [`Self::payload`] returns for the same row; a row that stores no payload reports
    /// [`PayloadRead::None`] in its position without failing the batch.
    pub fn read_payloads(&self, refs: &[PayloadRef]) -> Result<PayloadBatch, FormatError> {
        let mut order: Vec<usize> = (0..refs.len()).collect();
        order.sort_by_key(|&index| refs[index].row_ordinal);
        let mut payloads = vec![PayloadRead::None; refs.len()];
        // The current group's accelerators — resolved once per run of ascending rows that share a granule, and
        // served to every one of them (see `PayloadGroup`).
        let mut group: Option<PayloadGroup<'_>> = None;
        for index in order {
            let row_ordinal = refs[index].row_ordinal;
            let in_group = group.as_ref().is_some_and(|group| {
                group.granule.first_row_ordinal <= row_ordinal
                    && group
                        .granule
                        .first_row_ordinal
                        .checked_add(u64::from(group.granule.row_count))
                        .is_some_and(|end| row_ordinal < end)
            });
            if !in_group {
                let granule = self.granule_of_row(row_ordinal)?;
                let payload = self.payload_granule(granule.granule_id)?;
                group = Some(self.payload_group(granule, payload)?);
            }
            if let Some(group) = group.as_mut() {
                let row_in_granule = row_ordinal - group.granule.first_row_ordinal;
                payloads[index] = self.payload_in_group(group, row_in_granule)?;
            }
        }
        Ok(PayloadBatch { payloads })
    }

    /// One row's payload reconstruction inside an already-resolved granule — the shared body of [`Self::payload`] and
    /// [`Self::read_payloads`], so the batched path cannot drift from the per-row oracle.
    fn payload_in_granule(
        &self,
        granule: &GranuleEntry,
        payload: &PayloadGranule,
        row_in_granule: u64,
    ) -> Result<PayloadRead, FormatError> {
        let Some((offset, len)) = self.residual_slot(payload, row_in_granule)? else {
            return Ok(PayloadRead::None);
        };
        let bytes = self.residual_bytes(payload, offset, len)?;
        let flags = self.row_payload_flags(granule, row_in_granule)?;
        if flags & u64::from(crate::artifacts::batch::PAYLOAD_FLAG_EXTERNAL_REF) != 0 {
            return Ok(PayloadRead::External(owned_utf8(&bytes, "external payload reference")?));
        }
        let dictionary = self.granule_dictionary(payload)?;
        let mut value = VariantRef::new(&bytes).decode(&dictionary)?;
        // Merge moves back: shredded paths, then declared free-text fields.
        if let VariantValue::Object(fields) = &mut value {
            for entry in &self.footer.shredded {
                if let Some(shredded) = self.shredded_value_for_row(entry.column_id, granule, row_in_granule)? {
                    fields.insert(entry.path.clone(), shredded);
                }
            }
            for entry in &self.footer.freetext {
                if let Some(text) = self.freetext_value_for_row(entry.column_id, granule, row_in_granule)? {
                    fields.insert(entry.declared_field.clone(), text);
                }
            }
        }
        Ok(PayloadRead::Value(value))
    }

    /// Opens a [`PayloadGroup`] for one granule: resolves the payload-flags block up front, since every row in the
    /// granule needs it, and leaves the dictionary and shredded/free-text blocks to resolve lazily on each one's
    /// first use within the group.
    fn payload_group<'a>(
        &'a self,
        granule: &'a GranuleEntry,
        payload: &'a PayloadGranule,
    ) -> Result<PayloadGroup<'a>, FormatError> {
        Ok(PayloadGroup {
            dictionary: None,
            field_blocks: HashMap::new(),
            flags: self.cached_column(crate::columns::column_ids::PAYLOAD_FLAGS, granule.granule_id)?,
            granule,
            payload,
        })
    }

    /// One row's payload reconstruction inside an already-resolved [`PayloadGroup`] — the batched counterpart of
    /// [`Self::payload_in_granule`] that [`Self::read_payloads`] runs per row, reusing every accelerator the group
    /// already resolved instead of re-locking the shared caches for a row whose granule was just visited.
    fn payload_in_group<'a>(
        &'a self,
        group: &mut PayloadGroup<'a>,
        row_in_granule: u64,
    ) -> Result<PayloadRead, FormatError> {
        let Some((offset, len)) = self.residual_slot(group.payload, row_in_granule)? else {
            return Ok(PayloadRead::None);
        };
        let bytes = self.residual_bytes(group.payload, offset, len)?;
        let flags = Self::payload_flags_at(&group.flags, row_in_granule)?;
        if flags & u64::from(crate::artifacts::batch::PAYLOAD_FLAG_EXTERNAL_REF) != 0 {
            return Ok(PayloadRead::External(owned_utf8(&bytes, "external payload reference")?));
        }
        if group.dictionary.is_none() {
            group.dictionary = Some(self.granule_dictionary(group.payload)?);
        }
        let dictionary = group.dictionary.as_ref().expect("resolved above");
        let mut value = VariantRef::new(&bytes).decode(dictionary)?;
        // Merge moves back: shredded paths, then declared free-text fields.
        if let VariantValue::Object(fields) = &mut value {
            for entry in &self.footer.shredded {
                if let Some(shredded) = self.field_value_in_group(group, entry.column_id, row_in_granule)? {
                    fields.insert(entry.path.clone(), shredded);
                }
            }
            for entry in &self.footer.freetext {
                if let Some(text) = self.freetext_value_in_group(group, entry.column_id, row_in_granule)? {
                    fields.insert(entry.declared_field.clone(), text);
                }
            }
        }
        Ok(PayloadRead::Value(value))
    }

    /// One row's value for `column_id`'s block inside an already-resolved [`PayloadGroup`], resolving the block on
    /// the column's first use in the group and reusing it for every row after — the batched counterpart of
    /// [`Self::shredded_value_for_row`] shared by shredded paths and the free-text fallback below.
    fn field_value_in_group<'a>(
        &'a self,
        group: &mut PayloadGroup<'a>,
        column_id: u32,
        row_in_granule: u64,
    ) -> Result<Option<VariantValue>, FormatError> {
        if !group.field_blocks.contains_key(&column_id) {
            let block = self.path_block(column_id, group.granule.granule_id)?;
            group.field_blocks.insert(column_id, block);
        }
        let block = group.field_blocks.get(&column_id).expect("resolved above");
        self.value_in_block(block, column_id, row_in_granule)
    }

    /// One row's declared free-text value for `column_id` inside an already-resolved [`PayloadGroup`] — identical to
    /// [`Self::freetext_value_for_row`], but the fallback decode reuses the group's memoized block instead of the
    /// shared column cache.
    fn freetext_value_in_group<'a>(
        &'a self,
        group: &mut PayloadGroup<'a>,
        column_id: u32,
        row_in_granule: u64,
    ) -> Result<Option<VariantValue>, FormatError> {
        if self.usable_optional_features & optional_features::TYPED_COLUMN_ROW_OFFSETS != 0
            && let Some(entry) = self.freetext_row_offsets.get(&(column_id, group.granule.granule_id))
        {
            let Some((offset, len)) = self.freetext_row_slot(entry, row_in_granule)? else {
                return Ok(None);
            };
            let bytes = self.freetext_row_bytes(entry, offset, len)?;
            let text = std::str::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
                what: "freetext row value",
            })?;
            return Ok(Some(VariantValue::String(text.to_owned())));
        }
        self.field_value_in_group(group, column_id, row_in_granule)
    }

    /// Extracts a single payload path for one row without decoding sibling fields: shredded columns answer directly;
    /// residual paths use offset-based navigation against the granule dictionary.
    pub fn payload_path(&self, row_ordinal: u64, path: &str) -> Result<Option<VariantValue>, FormatError> {
        let granule = self.granule_of_row(row_ordinal)?;
        let row_in_granule = row_ordinal - granule.first_row_ordinal;
        if let Some(entry) = self.footer.shredded.iter().find(|entry| entry.path == path) {
            if let Some(value) = self.shredded_value_for_row(entry.column_id, granule, row_in_granule)? {
                return Ok(Some(value));
            }
            // A shredded path may still carry this row in the residual: a value the typed column could not reproduce
            // exactly (a differently-scaled decimal, a timestamp on an integer column) stays in the residual instead of
            // being moved. So a missing column value means "look in the residual", not "absent".
        }
        if let Some(entry) = self.footer.freetext.iter().find(|entry| entry.declared_field == path) {
            return self.freetext_value_for_row(entry.column_id, granule, row_in_granule);
        }
        // A row whose payload lives outside the file stores the reference itself in the residual slot, not a Variant.
        // It carries no inline paths at all, so report the path as absent rather than reading reference bytes as a
        // Variant — the same check `payload` makes before handing the slot to the decoder.
        if self.row_payload_flags(granule, row_in_granule)?
            & u64::from(crate::artifacts::batch::PAYLOAD_FLAG_EXTERNAL_REF)
            != 0
        {
            return Ok(None);
        }
        let payload = self.payload_granule(granule.granule_id)?;
        let Some((offset, len)) = self.residual_slot(payload, row_in_granule)? else {
            return Ok(None);
        };
        let bytes = self.residual_bytes(payload, offset, len)?;
        let dictionary = self.granule_dictionary(payload)?;
        let target = VariantRef::new(&bytes).get_path(&dictionary, &[PathSegment::Field(path)])?;
        target.map(|reference| reference.decode(&dictionary)).transpose()
    }

    /// Reads one payload path for many rows in one call — the batched form of [`Self::payload_path`], for a query that
    /// has already narrowed to its matching rows and wants a single field of each.
    ///
    /// Returns one entry per reference in the caller's original order, each identical to what [`Self::payload_path`]
    /// returns for the same row and path; the per-row read remains available and is the oracle this is checked
    /// against. Rows are visited in ascending order so each granule the batch reaches into is resolved once and its
    /// block for the path fetched once — every one of that granule's rows is then answered out of the bytes already
    /// in hand — rather than each row re-resolving the granule and fetching the block again. A row that carries no
    /// value for the path reports `None` in its own position without failing the batch.
    pub fn read_payload_paths(
        &self,
        refs: &[PayloadRef],
        path: &str,
    ) -> Result<Vec<Option<VariantValue>>, FormatError> {
        // Which columns the path resolves to is a property of the file, not of a row: resolve it once for the call
        // instead of scanning the footer's shredded and free-text entries per row as the per-row read does.
        let shredded_column = self
            .footer
            .shredded
            .iter()
            .find(|entry| entry.path == path)
            .map(|entry| entry.column_id);
        let freetext_column = self
            .footer
            .freetext
            .iter()
            .find(|entry| entry.declared_field == path)
            .map(|entry| entry.column_id);
        let mut order: Vec<usize> = (0..refs.len()).collect();
        order.sort_by_key(|&index| refs[index].row_ordinal);
        let mut values = vec![None; refs.len()];
        let mut group: Option<PathGroup<'_>> = None;
        for index in order {
            let row_ordinal = refs[index].row_ordinal;
            let in_group = group.as_ref().is_some_and(|group| {
                group.granule.first_row_ordinal <= row_ordinal
                    && group
                        .granule
                        .first_row_ordinal
                        .checked_add(u64::from(group.granule.row_count))
                        .is_some_and(|end| row_ordinal < end)
            });
            if !in_group {
                group = Some(PathGroup {
                    block: None,
                    freetext_column,
                    granule: self.granule_of_row(row_ordinal)?,
                    shredded_column,
                });
            }
            if let Some(group) = group.as_mut() {
                let row_in_granule = row_ordinal - group.granule.first_row_ordinal;
                values[index] = self.payload_path_in_granule(group, row_in_granule, path)?;
            }
        }
        Ok(values)
    }

    /// One row's value for a single payload path inside an already-resolved granule — the body
    /// [`Self::read_payload_paths`] runs per row, in the same order [`Self::payload_path`] tries the three places a
    /// path's value can live, so the two cannot disagree. What it resolves it hands back to `group`, so the granule's
    /// rows that follow reuse it.
    fn payload_path_in_granule<'a>(
        &'a self,
        group: &mut PathGroup<'a>,
        row_in_granule: u64,
        path: &str,
    ) -> Result<Option<VariantValue>, FormatError> {
        if let Some(column_id) = group.shredded_column {
            let granule_id = group.granule.granule_id;
            let block = match &mut group.block {
                Some(block) => block,
                slot @ None => slot.insert(self.path_block(column_id, granule_id)?),
            };
            if let Some(value) = self.value_in_block(block, column_id, row_in_granule)? {
                return Ok(Some(value));
            }
            // A shredded path may still carry this row in the residual — see [`Self::payload_path`].
        }
        if let Some(column_id) = group.freetext_column {
            return self.freetext_value_for_row(column_id, group.granule, row_in_granule);
        }
        if self.row_payload_flags(group.granule, row_in_granule)?
            & u64::from(crate::artifacts::batch::PAYLOAD_FLAG_EXTERNAL_REF)
            != 0
        {
            return Ok(None);
        }
        let payload = self.payload_granule(group.granule.granule_id)?;
        let Some((offset, len)) = self.residual_slot(payload, row_in_granule)? else {
            return Ok(None);
        };
        let bytes = self.residual_bytes(payload, offset, len)?;
        let dictionary = self.granule_dictionary(payload)?;
        let target = VariantRef::new(&bytes).get_path(&dictionary, &[PathSegment::Field(path)])?;
        target.map(|reference| reference.decode(&dictionary)).transpose()
    }

    /// Locates one granule's block for a batched read's path: the block's bytes when it is per-value addressable —
    /// the shape the per-row read's point path accepts, a single-page block that stores bytes under a pipeline that
    /// can decode one value out of them — and the whole block decoded through the column cache otherwise.
    fn path_block(&self, column_id: u32, granule_id: u32) -> Result<PathBlock<'_>, FormatError> {
        let mark = self.mark(column_id, 0, granule_id)?;
        let addressable = match &mark {
            Some(mark) => {
                mark.page_count <= 1
                    && mark.compressed_size > 0
                    && mark.codec_pipeline_id.supports_byte_range_extraction()?
            }
            None => false,
        };
        if addressable {
            let raw = self.read_column_raw_with_mark(granule_id, mark.as_ref().expect("addressable implies Some"))?;
            return Ok(PathBlock::Located {
                rank: PresenceRank::new(&raw.presence),
                raw,
            });
        }
        Ok(PathBlock::Whole(self.cached_column(column_id, granule_id)?))
    }

    /// One row's value out of a granule's already-located block, or `None` when the row carries no value for the
    /// column. The same value the per-row read produces for that row, by the same two routes.
    fn value_in_block(
        &self,
        block: &PathBlock<'_>,
        column_id: u32,
        row_in_granule: u64,
    ) -> Result<Option<VariantValue>, FormatError> {
        let (rank, raw) = match block {
            PathBlock::Located { rank, raw } => (rank, raw),
            PathBlock::Whole(column) => return Ok(column.value_at(row_in_granule)),
        };
        let row = row_in_granule as usize;
        // A dense block carries a value for every row, so the row is its own position; a sparse one stores only the
        // present rows' values, and the rank built once for the granule gives both presence and position in constant
        // time per row.
        let position = if raw.presence.is_empty() {
            row
        } else {
            match rank.position(&raw.presence, row) {
                Some(position) => position,
                None => return Ok(None),
            }
        };
        let data = decode_block_range_shared(
            raw.pipeline,
            raw.body,
            self.shared_alphabet(column_id),
            position,
            position + 1,
        )?;
        Ok(Self::typed_variant_at(&data, 0))
    }

    /// Row ordinals of the events whose `kind` reference names the target — with `kind = parent` this is "the
    /// children of X", with `kind = link`/`related` it is "everything that points at X".
    ///
    /// A plain equality lookup over the relationship column, no graph traversal: rows come back in file order, which
    /// is the envelope's `(epoch, sequence)` order. A file that carries no relationship columns returns no rows — a
    /// stream that declares no relationships has no referencing events, not an error.
    pub fn rows_referencing(
        &self,
        kind: RelationshipKind,
        space: TargetIdSpace,
        target_ref: &[u8],
    ) -> Result<Vec<u64>, FormatError> {
        let column_id = kind.column_id();
        if !self.footer.columns.iter().any(|column| column.column_id == column_id) {
            return Ok(Vec::new());
        }
        let needle = format!("{}:{}", space.as_str(), hex_lower(target_ref));
        // One SIMD substring scan per cell, built once for the whole file: it skips over the reference bytes that
        // cannot start a match instead of splitting every cell into tokens and comparing each one.
        let finder = memmem::Finder::new(needle.as_bytes());
        let mut rows = Vec::new();
        for granule in &self.footer.granules {
            // The granule's reference filter rules it out without reading its block.
            if !super::source_form::reference_may_be_in(&self.footer, column_id, granule.granule_id, &needle)? {
                continue;
            }
            let read = self.read_column(column_id, granule.granule_id)?;
            for_each_present_string(&read, granule.row_count as usize, |row, text| {
                // `link`/`related` store several space-separated references; `parent`/`root` store exactly one, so a
                // hit counts only when the delimiters around it make it a whole reference — the same equality
                // splitting on spaces performed.
                let haystack = text.as_bytes();
                let whole = finder.find_iter(haystack).any(|at| {
                    let starts = at == 0 || haystack.get(at - 1) == Some(&b' ');
                    starts && haystack.get(at + needle.len()).is_none_or(|byte| *byte == b' ')
                });
                if whole {
                    rows.push(granule.first_row_ordinal + row as u64);
                }
            });
        }
        Ok(rows)
    }

    /// Row ordinals of the whole thread rooted at the target: one non-recursive equality lookup on the
    /// root-reference column, in the envelope's `(epoch, sequence)` order. The root event itself carries no root
    /// reference, so it is not in the result; fetch it by its own identity.
    pub fn thread_rows(&self, space: TargetIdSpace, root_target: &[u8]) -> Result<Vec<u64>, FormatError> {
        self.rows_referencing(RelationshipKind::Root, space, root_target)
    }

    /// Row ordinals of the event a reference names, found by equality in the reference's own identifier space.
    ///
    /// Empty when the target is not in this file — not yet ingested, expired, erased, or never existing. A dangling
    /// reference resolves to nothing; it is never an error.
    pub fn resolve_reference(&self, reference: &RelationshipRef) -> Result<Vec<u64>, FormatError> {
        match reference.space {
            TargetIdSpace::EventId => {
                let target: [u8; 16] = match reference.target_ref.as_slice().try_into() {
                    Ok(target) => target,
                    Err(_) => return Ok(Vec::new()),
                };
                let target = u128::from_be_bytes(target);
                let mut rows = Vec::new();
                for granule in &self.footer.granules {
                    let read = self.read_column(column_ids::EVENT_ID, granule.granule_id)?;
                    if let ColumnData::U128(values) = &read.data {
                        for (row, value) in values.iter().enumerate() {
                            if *value == target {
                                rows.push(granule.first_row_ordinal + row as u64);
                            }
                        }
                    }
                }
                Ok(rows)
            }
            TargetIdSpace::ExternalId => self.rows_with_external_id(&reference.target_ref),
            TargetIdSpace::ProtocolEventId => {
                // The protocol id is a stored provenance column; a file without signed events has no such column and
                // so cannot hold the target.
                if !self
                    .footer
                    .columns
                    .iter()
                    .any(|column| column.column_id == column_ids::PROTOCOL_EVENT_ID)
                {
                    return Ok(Vec::new());
                }
                let needle = hex_lower(&reference.target_ref);
                let mut rows = Vec::new();
                for granule in &self.footer.granules {
                    let read = self.read_column(column_ids::PROTOCOL_EVENT_ID, granule.granule_id)?;
                    for_each_present_string(&read, granule.row_count as usize, |row, text| {
                        if text == needle {
                            rows.push(granule.first_row_ordinal + row as u64);
                        }
                    });
                }
                Ok(rows)
            }
        }
    }
}

/// Calls `visit(row_in_granule, value)` for every present value of a string block, expanding a sparse block through
/// its presence bitmap so the caller always sees granule row indexes.
fn for_each_present_string(read: &ColumnRead, row_count: usize, mut visit: impl FnMut(usize, &str)) {
    let ColumnData::Strings(values) = &read.data else {
        return;
    };
    if values.len() == row_count {
        for (row, value) in values.iter().enumerate() {
            if let Some(text) = value {
                visit(row, text);
            }
        }
        return;
    }
    let mut dense = values.iter();
    for row in 0..row_count {
        let present = read
            .presence
            .get(row / 8)
            .is_some_and(|byte| byte & (1 << (row % 8)) != 0);
        if present && let Some(Some(text)) = dense.next() {
            visit(row, text);
        }
    }
}

#[cfg(test)]
#[path = "test/reader.rs"]
mod tests;
