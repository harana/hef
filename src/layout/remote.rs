//! Reads a stored file straight out of object storage, fetching only the pieces a read needs instead of downloading
//! the whole object first.
//!
//! [`HefFile::open_remote`] issues one request for the file's tail, sized exactly from the manifest entry, and binds
//! the footer it holds to the manifest seal. Every later read of a marks page, column block, or payload slot fetches
//! just the stripe range it touches, widened to the outboard proof leaves that cover it, and proves those bytes
//! against the stripe's authenticated root before serving any of them. A stripe pruning never reaches is never
//! fetched. Fetched ranges are held for the reader's lifetime, so later reads inside them cost no request, and are
//! shared with an optional [`BlockCache`] so other readers on the node reuse them.
//!
//! [`RemoteGraph`] answers graph questions over such files. It gathers every edge block one step of a walk needs,
//! joins nearby ranges into requests of up to a few megabytes, sends them as batches of at most the caller's in-flight
//! cap, and sends a duplicate batch when one runs past the observed 95th-percentile latency, taking whichever answers
//! first. Chain-cover labels ride in the same edge blocks, and resolving protocol event ids fetches the marks pages and
//! external-id blocks it needs the same way. [`RemoteState`] reads the subject's state chunks behind it.
//!
//! See: hef-file-layout/spec.md

use super::api::{AuthChains, EventGraph, EventIds, SubjectState};
use super::cache::{BlockCache, BlockKey, BlockKind};
use super::constant::{
    GRAPH_FETCH_GAP_BYTES, GRAPH_FETCH_WINDOW_BYTES, GRAPH_HEDGE_MIN_SAMPLES, GRAPH_HEDGE_SAMPLES, HELD_RANGE_BUCKETS,
};
use super::footer::{Footer, StripeEntry};
use super::graph::{ChainLabel, ChainLink, EdgeKind, GranuleEdges};
use super::reader::{DECODED_CACHE_BUDGET_BYTES, HeaderCommitment, HefFile, HefFooter, VerifiedStripeRead};
use super::{HefHeader, LayoutClass};
use crate::artifacts::state_chunk::{self, StateChange, StateChunkStore, StateKey, StateRoot};
use crate::clock::Clock;
use crate::columns::column_ids;
use crate::error::{FormatError, StateChunkError};
use crate::events::TenantId;
use crate::file::bytes::slice;
use crate::file::{ByteRange, FileError, RangeSource};
use crate::lifecycle::HefFileEntry;
use crate::security::StateChunkKey;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Range;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::Duration;

/// Where an open file's bytes come from: the whole object already in memory, or a remote object fetched a verified
/// range at a time as reads need it.
#[derive(Debug)]
pub(super) enum FileBytes {
    Local(Arc<Vec<u8>>),
    Remote(Box<RemoteFile>),
}

impl FileBytes {
    /// The same bytes for a new reader: the shared buffer of a local file, or a remote file's source, cache, and
    /// authenticated footer with none of this reader's fetched ranges.
    pub(super) fn fresh(&self) -> Self {
        match self {
            FileBytes::Local(bytes) => FileBytes::Local(Arc::clone(bytes)),
            FileBytes::Remote(remote) => FileBytes::Remote(Box::new(remote.fresh())),
        }
    }
}

/// A file in remote storage, read a verified range at a time.
pub(super) struct RemoteFile {
    cache: Option<Arc<BlockCache>>,
    file_id: u128,
    footer: Arc<HefFooter>,
    held: HeldRanges,
    /// Each stripe's co-located marks pages as one file-absolute `(start, end)` extent. A read inside one fetches all
    /// of it, because the first lookup in a stripe reads every column's page there.
    marks_extents: Arc<Vec<(u64, u64)>>,
    source: Arc<dyn RangeSource>,
    /// The object's trailing bytes from the tail fetch, authenticated by the seal at open.
    tail: Arc<[u8]>,
    /// File offset of `tail`'s first byte.
    tail_start: u64,
    /// Where the stripes and integrity gaps end. Only bytes at or past both this and `tail_start` are served from
    /// `tail`, so a speculative tail that reached back into the data area never serves unproven stripe bytes.
    tail_served_from: u64,
    tenant_id: TenantId,
}

impl std::fmt::Debug for RemoteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFile")
            .field("file_id", &self.file_id)
            .field("tail_start", &self.tail_start)
            .finish_non_exhaustive()
    }
}

impl HefFile {
    /// Opens a file that lives in remote storage without downloading it: one request for the file's tail, sized
    /// exactly from `entry` when it records `footer_len` (otherwise a speculative tail plus at most one exact retry),
    /// then each marks page, column block, and payload slot fetched only when a read needs it.
    ///
    /// `header` is the [`HeaderCommitment`] the publisher recorded beside the seal; with it the footer is bound to
    /// `entry.file_seal` without ever reading the front of the object. `footer_dek` opens a sealed footer, as in
    /// [`Self::open_with_keys`]. Pass a shared `cache` to serve pieces other readers on the node already fetched; a
    /// cached piece is proven again before use, and one that fails is dropped and fetched afresh.
    ///
    /// Every byte served is first proven against the authenticated stripe root, through the outboard proof tree when
    /// the object carries one and by hashing the whole stripe otherwise. The fixed header block is never read, so
    /// [`Self::header`] is rebuilt from the authenticated footer, `entry`, and `header`: `created_at_physical`,
    /// `footer_pointer_hint`, and `generation_id` read as zero, `layout_class` as compact, and `projection_count` as
    /// one, because only the front-of-file block records them.
    pub fn open_remote(
        source: Arc<dyn RangeSource>,
        cache: Option<Arc<BlockCache>>,
        entry: &HefFileEntry,
        header: &HeaderCommitment,
        footer_dek: Option<&[u8; 32]>,
    ) -> Result<Self, FormatError> {
        if header.file_id != entry.file_id {
            return Err(FormatError::Structural {
                rule: "header commitment names a different file than the manifest entry",
            });
        }
        let (tail_start, tail, opened) = open_tail(source.as_ref(), cache.as_deref(), entry, header, footer_dek)?;
        let footer = opened.footer().clone();
        if footer.profile != entry.profile {
            return Err(FormatError::Structural {
                rule: "manifest entry profile must match footer profile",
            });
        }
        let usable_optional_features = opened.usable_optional_features();
        let remote = RemoteFile {
            cache,
            file_id: entry.file_id,
            held: HeldRanges::new(),
            marks_extents: Arc::new(marks_extents(&footer)),
            source,
            tail,
            tail_served_from: data_area_end(&footer).max(tail_start),
            tail_start,
            tenant_id: entry.tenant_id,
            footer: Arc::new(opened),
        };
        Self::assemble(
            FileBytes::Remote(Box::new(remote)),
            remote_header(&footer, entry, header),
            footer,
            usable_optional_features,
            None,
            DECODED_CACHE_BUDGET_BYTES,
        )
    }
}

/// Fetches and authenticates the object's tail, from the cache when it holds a tail that still passes the seal.
/// Returns the tail's file offset, its bytes, and the opened footer.
fn open_tail(
    source: &dyn RangeSource,
    cache: Option<&BlockCache>,
    entry: &HefFileEntry,
    header: &HeaderCommitment,
    footer_dek: Option<&[u8; 32]>,
) -> Result<(u64, Arc<[u8]>, HefFooter), FormatError> {
    let range = entry.tail_range();
    let open =
        |tail: &[u8]| HefFooter::open_authenticated_tail(tail, entry.size_bytes, &entry.file_seal, header, footer_dek);
    let key = |offset: u64, len: u64| BlockKey {
        file_id: entry.file_id,
        kind: BlockKind::Footer,
        len,
        offset,
        tenant_id: entry.tenant_id,
    };
    if let Some(cache) = cache
        && let Some(tail) = cache.get(&key(range.start, range.len))
    {
        match open(&tail) {
            Ok(footer) => return Ok((range.start, tail, footer)),
            Err(_) => cache.remove(&key(range.start, range.len)),
        }
    }
    let mut start = range.start;
    let mut tail = fetch_exact(source, entry.file_id, start, range.len)?;
    if !range.exact
        && let Some(exact_len) = HefFooter::tail_shortfall(&tail)?
    {
        let exact_len = exact_len.min(entry.size_bytes);
        start = entry.size_bytes - exact_len;
        tail = fetch_exact(source, entry.file_id, start, exact_len)?;
    }
    let footer = open(&tail)?;
    let tail: Arc<[u8]> = tail.into();
    if let Some(cache) = cache {
        cache.insert(key(start, tail.len() as u64), Arc::clone(&tail));
    }
    Ok((start, tail, footer))
}

/// Asks `source` for one range and refuses a reply of the wrong length.
fn fetch_exact(source: &dyn RangeSource, object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FormatError> {
    let bytes = source.read_range(object, offset, len).map_err(unavailable)?;
    if bytes.len() as u64 != len {
        return Err(FormatError::Unavailable {
            detail: format!("asked for {len} bytes at offset {offset}, got {}", bytes.len()),
        });
    }
    Ok(bytes)
}

fn unavailable(error: FileError) -> FormatError {
    FormatError::Unavailable {
        detail: error.to_string(),
    }
}

/// Each stripe's co-located marks pages as one file-absolute extent, for a file that stores them beside the stripe.
fn marks_extents(footer: &Footer) -> Vec<(u64, u64)> {
    footer
        .stripes
        .iter()
        .filter_map(|stripe| {
            let pages = footer
                .marks_page_offsets
                .iter()
                .filter(|entry| entry.stripe_id == stripe.stripe_id);
            let start = pages.clone().map(|entry| entry.page_offset).min()?;
            let end = pages
                .map(|entry| entry.page_offset.saturating_add(entry.page_len))
                .max()?;
            Some((
                stripe.file_offset.saturating_add(start),
                stripe.file_offset.saturating_add(end),
            ))
        })
        .collect()
}

/// The first file offset past every stripe and integrity gap.
fn data_area_end(footer: &Footer) -> u64 {
    let stripes = footer
        .stripes
        .iter()
        .map(|stripe| stripe.file_offset.saturating_add(stripe.byte_len));
    let gaps = footer
        .integrity_gaps
        .iter()
        .map(|gap| gap.file_offset.saturating_add(gap.byte_len));
    stripes.chain(gaps).max().unwrap_or(0)
}

/// The header a remote reader can know without the front-of-file block: identity and feature flags from the
/// commitment, the tenant from the manifest entry, and version, row count, and coverage from the authenticated footer.
fn remote_header(footer: &Footer, entry: &HefFileEntry, commitment: &HeaderCommitment) -> HefHeader {
    let granules = &footer.granules;
    HefHeader {
        created_at_physical: 0,
        feature_flags: commitment.feature_flags,
        file_id: commitment.file_id,
        footer_pointer_hint: 0,
        generation_id: 0,
        layout_class: LayoutClass::Compact,
        max_epoch: granules.last().map_or(0, |granule| granule.last_epoch),
        max_ingested_at_physical: granules
            .iter()
            .map(|granule| granule.max_ingested_at_physical)
            .max()
            .unwrap_or(0),
        max_occurred_at_physical: granules
            .iter()
            .map(|granule| granule.max_occurred_at_physical)
            .max()
            .unwrap_or(0),
        max_sequence: granules.last().map_or(0, |granule| granule.last_sequence),
        min_epoch: granules.first().map_or(0, |granule| granule.first_epoch),
        min_ingested_at_physical: granules
            .iter()
            .map(|granule| granule.min_ingested_at_physical)
            .min()
            .unwrap_or(0),
        min_occurred_at_physical: granules
            .iter()
            .map(|granule| granule.min_occurred_at_physical)
            .min()
            .unwrap_or(0),
        min_sequence: granules.first().map_or(0, |granule| granule.first_sequence),
        profile: footer.profile,
        projection_count: 1,
        row_count: footer.exact_counts.row_count,
        tenant_id: entry.tenant_id,
        version_major: footer.format_version.0,
        version_minor: footer.format_version.1,
    }
}

impl RemoteFile {
    fn fresh(&self) -> Self {
        Self {
            cache: self.cache.clone(),
            file_id: self.file_id,
            footer: Arc::clone(&self.footer),
            held: HeldRanges::new(),
            marks_extents: Arc::clone(&self.marks_extents),
            source: Arc::clone(&self.source),
            tail: Arc::clone(&self.tail),
            tail_served_from: self.tail_served_from,
            tail_start: self.tail_start,
            tenant_id: self.tenant_id,
        }
    }

    /// The `len` file bytes at `position`, proven before they are served. Answered from a range this reader already
    /// fetched when one covers it; otherwise fetches the stripe range around it (from the cache or the source).
    pub(super) fn read(&self, position: u64, len: usize, what: &'static str) -> Result<&[u8], FormatError> {
        if len == 0 {
            return Ok(&[]);
        }
        let end = position
            .checked_add(len as u64)
            .ok_or(FormatError::RefOutOfRange { what })?;
        if let Some((start, bytes)) = self.held.find(position, end) {
            return Ok(slice(bytes, (position - start) as usize, len, what)?);
        }
        let Some(stripe) = self.stripe_holding(position, end) else {
            if position >= self.tail_served_from {
                return Ok(slice(&self.tail, (position - self.tail_start) as usize, len, what)?);
            }
            return Err(FormatError::RefOutOfRange {
                what: "remote read outside any one stripe",
            });
        };
        let stripe_end = stripe.file_offset + stripe.byte_len;
        let (want_start, want_end) = self
            .marks_extents
            .iter()
            .find(|(start, extent_end)| {
                *start <= position && end <= *extent_end && stripe.file_offset <= *start && *extent_end <= stripe_end
            })
            .copied()
            .unwrap_or((position, end));
        let (start, bytes) = self.fetch_stripe_range(stripe, want_start, want_end)?;
        let held = self.held.hold(start, bytes)?;
        Ok(slice(held, (position - start) as usize, len, what)?)
    }

    /// The stripe whose bytes contain the whole file range `start..end`.
    fn stripe_holding(&self, start: u64, end: u64) -> Option<&StripeEntry> {
        self.footer.footer().stripes.iter().find(|stripe| {
            stripe.file_offset <= start
                && stripe
                    .file_offset
                    .checked_add(stripe.byte_len)
                    .is_some_and(|stripe_end| end <= stripe_end)
        })
    }

    /// Plans the proof-leaf-aligned pieces that cover the file ranges `ranges` (each `(start, end)`, inside one
    /// stripe) this reader does not already hold, in file order, joining the pieces of one stripe that sit within
    /// [`GRAPH_FETCH_GAP_BYTES`] of each other while the joined piece stays within [`GRAPH_FETCH_WINDOW_BYTES`]. A
    /// piece the shared cache holds and still proves is held at once instead of planned.
    fn plan_prefetch(&self, ranges: &[(u64, u64)]) -> Result<Vec<VerifiedStripeRead>, FormatError> {
        let mut wanted: Vec<(u64, u64)> = ranges
            .iter()
            .filter(|(start, end)| start < end && self.held.find(*start, *end).is_none())
            .copied()
            .collect();
        wanted.sort_unstable();
        let mut pieces: Vec<VerifiedStripeRead> = Vec::new();
        for (start, end) in wanted {
            let stripe = self.stripe_holding(start, end).ok_or(FormatError::RefOutOfRange {
                what: "remote read outside any one stripe",
            })?;
            let plan =
                self.footer
                    .plan_verified_stripe_read(stripe.stripe_id, start - stripe.file_offset, end - start)?;
            let plan_end = plan.file_offset + plan.length;
            if let Some(last) = pieces.last_mut()
                && last.stripe_id == plan.stripe_id
            {
                let last_end = last.file_offset + last.length;
                if plan_end <= last_end {
                    continue;
                }
                if plan.file_offset <= last_end.saturating_add(GRAPH_FETCH_GAP_BYTES)
                    && plan_end - last.file_offset <= GRAPH_FETCH_WINDOW_BYTES
                {
                    *last = self.footer.plan_verified_stripe_read(
                        stripe.stripe_id,
                        last.file_offset - stripe.file_offset,
                        plan_end - last.file_offset,
                    )?;
                    continue;
                }
            }
            pieces.push(plan);
        }
        let mut planned = Vec::with_capacity(pieces.len());
        for piece in pieces {
            if !self.hold_cached(&piece)? {
                planned.push(piece);
            }
        }
        Ok(planned)
    }

    /// Holds `piece` from the shared cache when the cache has it and it still proves; `false` otherwise.
    fn hold_cached(&self, piece: &VerifiedStripeRead) -> Result<bool, FormatError> {
        let Some(cache) = &self.cache else {
            return Ok(false);
        };
        let key = self.graph_key(piece);
        let Some(bytes) = cache.get(&key) else {
            return Ok(false);
        };
        if self
            .footer
            .verify_stripe_range_in(piece, &bytes, piece.file_offset)
            .is_err()
        {
            cache.remove(&key);
            return Ok(false);
        }
        self.held.hold(piece.file_offset, bytes)?;
        Ok(true)
    }

    /// Proves `bytes` fetched for `piece` and holds them, sharing them with the cache. Bytes that do not prove are
    /// dropped: a later read of that range fetches it again the ordinary way, which falls back to the whole stripe and
    /// refuses bytes that still do not prove.
    fn admit(&self, piece: &VerifiedStripeRead, bytes: &[u8]) -> Result<(), FormatError> {
        if self
            .footer
            .verify_stripe_range_in(piece, bytes, piece.file_offset)
            .is_err()
        {
            return Ok(());
        }
        let bytes: Arc<[u8]> = bytes.into();
        if let Some(cache) = &self.cache {
            cache.insert(self.graph_key(piece), Arc::clone(&bytes));
        }
        self.held.hold(piece.file_offset, bytes)?;
        Ok(())
    }

    fn graph_key(&self, piece: &VerifiedStripeRead) -> BlockKey {
        BlockKey {
            file_id: self.file_id,
            kind: BlockKind::GraphIndex,
            len: piece.length,
            offset: piece.file_offset,
            tenant_id: self.tenant_id,
        }
    }

    /// Fetches and proves the proof-leaf-aligned range of `stripe` around the file range `start..end`, falling back to
    /// the whole stripe when the outboard tree cannot prove it. Returns the fetched range's file offset and bytes.
    fn fetch_stripe_range(&self, stripe: &StripeEntry, start: u64, end: u64) -> Result<(u64, Arc<[u8]>), FormatError> {
        let planned =
            self.footer
                .plan_verified_stripe_read(stripe.stripe_id, start - stripe.file_offset, end - start)?;
        // Plan again over the aligned range itself, so the proof covers every fetched byte and the whole fetch can
        // serve later reads, not only the bytes first asked for.
        let read = self.footer.plan_verified_stripe_read(
            stripe.stripe_id,
            planned.file_offset - stripe.file_offset,
            planned.length,
        )?;
        if let Some(bytes) = self.fetch_verified(&read)? {
            return Ok((read.file_offset, bytes));
        }
        // Either the bytes are corrupt or the outboard tree cannot prove them. Hashing the whole stripe against its
        // checksum never consults the tree, so it tells the two apart.
        let whole = self
            .footer
            .plan_whole_stripe_read(stripe.stripe_id, 0, stripe.byte_len)?;
        if whole != read
            && let Some(bytes) = self.fetch_verified(&whole)?
        {
            return Ok((whole.file_offset, bytes));
        }
        Err(FormatError::Blake3Mismatch {
            scope: "remote stripe range",
        })
    }

    /// The bytes `read` plans, proven against the stripe root: from the cache when it holds a copy that still proves,
    /// otherwise from the source, admitted to the cache once proven. `None` when the source's bytes do not prove.
    fn fetch_verified(&self, read: &VerifiedStripeRead) -> Result<Option<Arc<[u8]>>, FormatError> {
        let proves = |bytes: &[u8]| {
            self.footer
                .verify_stripe_range_in(read, bytes, read.file_offset)
                .is_ok()
        };
        let key = BlockKey {
            file_id: self.file_id,
            kind: BlockKind::Stripe,
            len: read.length,
            offset: read.file_offset,
            tenant_id: self.tenant_id,
        };
        if let Some(cache) = &self.cache
            && let Some(bytes) = cache.get(&key)
        {
            if proves(&bytes) {
                return Ok(Some(bytes));
            }
            cache.remove(&key);
        }
        let bytes: Arc<[u8]> = fetch_exact(self.source.as_ref(), self.file_id, read.file_offset, read.length)?.into();
        if !proves(&bytes) {
            return Ok(None);
        }
        if let Some(cache) = &self.cache {
            cache.insert(key, Arc::clone(&bytes));
        }
        Ok(Some(bytes))
    }
}

/// The verified ranges one remote reader has fetched, held until the reader is dropped so a read can borrow from them
/// for as long as it borrows the reader. Indexed by start offset.
struct HeldRanges {
    by_start: RwLock<BTreeMap<u64, (u64, usize)>>,
    slots: HeldSlots,
}

impl HeldRanges {
    fn new() -> Self {
        Self {
            by_start: RwLock::new(BTreeMap::new()),
            slots: HeldSlots::new(),
        }
    }

    /// A held range covering `start..end`, as its own start offset and bytes.
    fn find(&self, start: u64, end: u64) -> Option<(u64, &[u8])> {
        let (held_start, slot) = {
            let index = self.by_start.read().unwrap_or_else(PoisonError::into_inner);
            index
                .range(..=start)
                .rev()
                .find(|(_, (held_end, _))| *held_end >= end)
                .map(|(held_start, (_, slot))| (*held_start, *slot))?
        };
        self.slots.get(slot).map(|bytes| (held_start, bytes))
    }

    /// Holds `bytes`, fetched from file offset `start`, for the rest of the reader's life.
    fn hold(&self, start: u64, bytes: Arc<[u8]>) -> Result<&[u8], FormatError> {
        let end = start + bytes.len() as u64;
        let (slot, held) = self.slots.push(bytes)?;
        let mut index = self.by_start.write().unwrap_or_else(PoisonError::into_inner);
        if index.get(&start).is_none_or(|(held_end, _)| *held_end < end) {
            index.insert(start, (end, slot));
        }
        Ok(held)
    }
}

/// One bucket of [`HeldSlots`]: a fixed run of slots, each set at most once.
type HeldBucket = Box<[OnceLock<Arc<[u8]>>]>;

/// An append-only list of byte buffers that hands out borrows living as long as the list: slots are never moved or
/// emptied once set. Slot `i` sits in bucket `log2(i + 1)`, and bucket `b` is allocated on first use with `2^b` slots.
struct HeldSlots {
    buckets: [OnceLock<HeldBucket>; HELD_RANGE_BUCKETS],
    next: AtomicUsize,
}

impl HeldSlots {
    fn new() -> Self {
        Self {
            buckets: std::array::from_fn(|_| OnceLock::new()),
            next: AtomicUsize::new(0),
        }
    }

    fn position(slot: usize) -> (usize, usize) {
        let n = slot + 1;
        let bucket = (usize::BITS - 1 - n.leading_zeros()) as usize;
        (bucket, n - (1 << bucket))
    }

    fn push(&self, bytes: Arc<[u8]>) -> Result<(usize, &[u8]), FormatError> {
        let slot = self.next.fetch_add(1, Ordering::Relaxed);
        let (bucket, index) = Self::position(slot);
        let full = || FormatError::Structural {
            rule: "remote reader holds more fetched ranges than it can index",
        };
        let cells = self
            .buckets
            .get(bucket)
            .ok_or_else(full)?
            .get_or_init(|| (0..1usize << bucket).map(|_| OnceLock::new()).collect());
        let cell = cells.get(index).ok_or_else(full)?;
        // Every slot number is handed out once, so the cell is always empty here.
        let held = cell.get_or_init(|| bytes);
        Ok((slot, &**held))
    }

    fn get(&self, slot: usize) -> Option<&[u8]> {
        let (bucket, index) = Self::position(slot);
        self.buckets.get(bucket)?.get()?.get(index)?.get().map(|bytes| &**bytes)
    }
}

/// Groups `pieces`, in file order, into requests: consecutive pieces join one request while the gap between them is
/// at most [`GRAPH_FETCH_GAP_BYTES`] and the request stays within [`GRAPH_FETCH_WINDOW_BYTES`]. Returns each request
/// with the positions of the pieces it covers.
fn coalesce(pieces: &[VerifiedStripeRead]) -> Vec<(ByteRange, Range<usize>)> {
    let mut requests: Vec<(ByteRange, Range<usize>)> = Vec::new();
    for (index, piece) in pieces.iter().enumerate() {
        let piece_end = piece.file_offset + piece.length;
        if let Some((request, covered)) = requests.last_mut() {
            let request_end = request.offset + request.len;
            let joined_end = piece_end.max(request_end);
            if piece.file_offset <= request_end.saturating_add(GRAPH_FETCH_GAP_BYTES)
                && joined_end - request.offset <= GRAPH_FETCH_WINDOW_BYTES
            {
                request.len = joined_end - request.offset;
                covered.end = index + 1;
                continue;
            }
        }
        let request = ByteRange {
            len: piece.length,
            offset: piece.file_offset,
        };
        requests.push((request, index..index + 1));
    }
    requests
}

/// The answer to one batch of range requests, as a request thread hands it back.
type BatchAnswer = Result<Vec<Vec<u8>>, FileError>;

/// Sends one batch of range requests on a thread of its own, which hands the answer to `sender`.
fn spawn_batch(
    source: &Arc<dyn RangeSource>,
    object: u128,
    asked: &Arc<Vec<ByteRange>>,
    sender: mpsc::Sender<BatchAnswer>,
) -> Result<(), FormatError> {
    let source = Arc::clone(source);
    let asked = Arc::clone(asked);
    std::thread::Builder::new()
        .name("hef-graph-get".to_owned())
        .spawn(move || {
            // Once the other copy of a hedged batch has answered nobody is listening, and this answer is dropped.
            sender.send(source.read_ranges(object, &asked)).ok();
        })
        .map(drop)
        .map_err(|error| FormatError::Unavailable {
            detail: error.to_string(),
        })
}

/// How a [`RemoteGraph`] spends requests on one subject's reads.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteGraphOptions {
    /// How long, in nanoseconds, a batch of requests may run before a duplicate batch is sent: the observed 95th
    /// percentile of recent batches, but never less than this. Until enough batches have been timed it is exactly
    /// this.
    pub hedge_floor_nanos: u64,
    /// The most range requests one read keeps outstanding at once, a duplicate batch included; at least one. A step
    /// that needs more ranges fetches them in successive batches, and a batch is duplicated only when both copies fit.
    pub max_in_flight: usize,
}

/// Answers graph questions ([`EventGraph`]) from graph-profile files, normally ones opened from object storage with
/// [`HefFile::open_remote`].
///
/// Each step of a walk gathers the edge blocks of every granule it touches across the subject's files, joins nearby
/// ranges into requests of a few megabytes, and sends them per file in batches of at most
/// [`RemoteGraphOptions::max_in_flight`]. A batch still running past the 95th-percentile latency of recent batches is
/// sent again and the first answer wins. Fetched blocks are proven against the file's authenticated checksums and held
/// by the file's reader, decoded once, so a later step that needs the same granule sends and decodes nothing.
///
/// See: hef-file-layout/spec.md
pub struct RemoteGraph {
    clock: Arc<dyn Clock>,
    files: Vec<HefFile>,
    /// Recent batch latencies in nanoseconds, newest last, at most [`GRAPH_HEDGE_SAMPLES`] of them.
    latencies: Mutex<VecDeque<u64>>,
    options: RemoteGraphOptions,
}

impl RemoteGraph {
    /// A graph over `files`, every one a graph-profile file that carries edges; latencies are measured with `clock`.
    /// Refuses a file without edges, two files of one subject that hold the same event, or an in-flight cap of zero.
    pub fn new(files: Vec<HefFile>, clock: Arc<dyn Clock>, options: RemoteGraphOptions) -> Result<Self, FormatError> {
        if options.max_in_flight == 0 {
            return Err(FormatError::Structural {
                rule: "a remote graph keeps at least one request in flight",
            });
        }
        let mut spans: Vec<(&[u8], u64, u64)> = Vec::with_capacity(files.len());
        for file in &files {
            let footer = file.footer();
            let graph = footer.graph.as_ref().ok_or(FormatError::Structural {
                rule: "a remote graph reads only graph-profile files that carry edges",
            })?;
            let end = graph.first_sn.saturating_add(footer.exact_counts.row_count);
            spans.push((&graph.subject, graph.first_sn, end));
        }
        spans.sort_unstable();
        if spans
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.0 == right.0 && right.1 < left.2))
        {
            return Err(FormatError::Structural {
                rule: "two graph files of one subject hold the same event",
            });
        }
        Ok(Self {
            clock,
            files,
            latencies: Mutex::new(VecDeque::with_capacity(GRAPH_HEDGE_SAMPLES)),
            options,
        })
    }

    /// Reads the subject state behind this graph's state events, fetching chunks from `chunks` (in production
    /// [`ObjectStoreStateChunks`](crate::artifacts::state_chunk::ObjectStoreStateChunks)) and opening them with the
    /// tenant's `key`.
    pub fn state<'a>(&'a self, chunks: &'a dyn StateChunkStore, key: &'a StateChunkKey) -> RemoteState<'a> {
        RemoteState {
            chunks,
            graph: self,
            key,
        }
    }

    /// The files of `subject`, each with its position.
    fn subject_files<'a>(&'a self, subject: &'a [u8]) -> impl Iterator<Item = (usize, &'a HefFile)> + 'a {
        self.files.iter().enumerate().filter(move |(_, file)| {
            file.footer()
                .graph
                .as_ref()
                .is_some_and(|graph| graph.subject == subject)
        })
    }

    /// The position of the file holding event `sn` of `subject`, and the granule that holds it.
    fn locate(&self, subject: &[u8], sn: u64) -> Result<(usize, u32), FormatError> {
        self.subject_files(subject)
            .find_map(|(index, file)| Some((index, file.graph_granule_of(sn)?)))
            .ok_or(FormatError::RefOutOfRange {
                what: "no graph file holds the event",
            })
    }

    /// One answer per event in `sns`, read out of the event's decoded granule by `read`. Every granule the events touch
    /// is fetched first, all together, then decoded once.
    fn edges_of<T: Default>(
        &self,
        subject: &[u8],
        sns: &[u64],
        read: impl Fn(&GranuleEdges, u64) -> T,
    ) -> Result<Vec<T>, FormatError> {
        let located: Vec<(usize, u32)> = sns
            .iter()
            .map(|&sn| self.locate(subject, sn))
            .collect::<Result<_, _>>()?;
        let granules: BTreeSet<(usize, u32)> = located.iter().copied().collect();
        let mut ranges: BTreeMap<usize, Vec<(u64, u64)>> = BTreeMap::new();
        for &(index, granule_id) in &granules {
            let Some(file) = self.files.get(index) else {
                continue;
            };
            let (position, len) = file.graph_block_range(granule_id)?;
            let end = position.checked_add(len).ok_or(FormatError::RefOutOfRange {
                what: "graph edge block range",
            })?;
            ranges.entry(index).or_default().push((position, end));
        }
        for (index, ranges) in &ranges {
            if let Some(remote) = self.files.get(*index).and_then(HefFile::remote) {
                self.prefetch(remote, ranges)?;
            }
        }
        let mut decoded: BTreeMap<(usize, u32), Arc<GranuleEdges>> = BTreeMap::new();
        for &(index, granule_id) in &granules {
            if let Some(file) = self.files.get(index) {
                decoded.insert((index, granule_id), file.cached_granule_edges(granule_id)?);
            }
        }
        Ok(sns
            .iter()
            .zip(&located)
            .map(|(&sn, key)| decoded.get(key).map(|edges| read(edges, sn)).unwrap_or_default())
            .collect())
    }

    /// Makes `remote`, the bytes of `file`, hold what confirming the protocol ids of events `sns` reads: first the
    /// marks pages of the stripes the events lie in, then the external-id column block of each granule they lie in.
    /// A block split into pages is left to the ordinary per-read fetch.
    fn prefetch_external_ids(&self, file: &HefFile, remote: &RemoteFile, sns: &[u64]) -> Result<(), FormatError> {
        let footer = file.footer();
        let granules: BTreeSet<u32> = sns.iter().filter_map(|&sn| file.graph_granule_of(sn)).collect();
        let stripes: BTreeSet<u32> = footer
            .granules
            .iter()
            .filter(|granule| granules.contains(&granule.granule_id))
            .map(|granule| granule.stripe_id)
            .collect();
        let marks: Vec<(u64, u64)> = footer
            .stripes
            .iter()
            .filter(|stripe| stripes.contains(&stripe.stripe_id))
            .flat_map(|stripe| {
                let stripe_end = stripe.file_offset.saturating_add(stripe.byte_len);
                remote
                    .marks_extents
                    .iter()
                    .copied()
                    .filter(move |(start, end)| stripe.file_offset <= *start && *end <= stripe_end)
            })
            .collect();
        self.prefetch(remote, &marks)?;
        let mut blocks = Vec::with_capacity(granules.len());
        for &granule_id in &granules {
            let Some(mark) = file.mark(column_ids::EXTERNAL_ID, 0, granule_id)? else {
                continue;
            };
            if mark.page_count > 1 || mark.compressed_size == 0 {
                continue;
            }
            let start = file.file_position(granule_id, mark.compressed_offset)? as u64;
            blocks.push((start, start.saturating_add(mark.compressed_size)));
        }
        self.prefetch(remote, &blocks)
    }

    /// Makes `remote` hold `ranges`: plans the pieces it lacks, joins them into requests, and fetches those in
    /// batches of at most the in-flight cap.
    fn prefetch(&self, remote: &RemoteFile, ranges: &[(u64, u64)]) -> Result<(), FormatError> {
        let pieces = remote.plan_prefetch(ranges)?;
        let requests = coalesce(&pieces);
        for batch in requests.chunks(self.options.max_in_flight) {
            let asked: Vec<ByteRange> = batch.iter().map(|(request, _)| *request).collect();
            let replies = self.fetch(&remote.source, remote.file_id, asked)?;
            if replies.len() != batch.len() {
                return Err(FormatError::Unavailable {
                    detail: format!("asked for {} ranges, got {}", batch.len(), replies.len()),
                });
            }
            for ((request, covered), reply) in batch.iter().zip(&replies) {
                if reply.len() as u64 != request.len {
                    return Err(FormatError::Unavailable {
                        detail: format!(
                            "asked for {} bytes at offset {}, got {}",
                            request.len,
                            request.offset,
                            reply.len()
                        ),
                    });
                }
                for piece in pieces.get(covered.clone()).unwrap_or_default() {
                    let start = (piece.file_offset - request.offset) as usize;
                    let bytes = slice(reply, start, piece.length as usize, "coalesced graph request")?;
                    remote.admit(piece, bytes)?;
                }
            }
        }
        Ok(())
    }

    /// Sends one batch of requests and returns its answer, timing it for the hedge delay. When a duplicate batch fits
    /// under the in-flight cap the batch runs on a thread of its own and, if it has not answered within the hedge
    /// delay, a duplicate is sent; the first successful answer wins and the other is dropped when it arrives.
    fn fetch(
        &self,
        source: &Arc<dyn RangeSource>,
        object: u128,
        asked: Vec<ByteRange>,
    ) -> Result<Vec<Vec<u8>>, FormatError> {
        let started = self.clock.now_nanos();
        let answer = if asked.len().saturating_mul(2) > self.options.max_in_flight {
            source.read_ranges(object, &asked).map_err(unavailable)
        } else {
            self.fetch_hedged(source, object, &Arc::new(asked))
        };
        self.record(self.clock.now_nanos().saturating_sub(started));
        answer
    }

    fn fetch_hedged(
        &self,
        source: &Arc<dyn RangeSource>,
        object: u128,
        asked: &Arc<Vec<ByteRange>>,
    ) -> Result<Vec<Vec<u8>>, FormatError> {
        let no_answer = || FormatError::Unavailable {
            detail: "graph request thread ended without an answer".to_owned(),
        };
        let (sender, receiver) = mpsc::channel();
        spawn_batch(source, object, asked, sender.clone())?;
        let first = match receiver.recv_timeout(Duration::from_nanos(self.hedge_delay())) {
            Ok(answer) => return answer.map_err(unavailable),
            Err(RecvTimeoutError::Disconnected) => return Err(no_answer()),
            Err(RecvTimeoutError::Timeout) => {
                spawn_batch(source, object, asked, sender)?;
                receiver.recv().map_err(|_| no_answer())?
            }
        };
        match first {
            Ok(replies) => Ok(replies),
            // The copy that answered first failed; the other may still succeed.
            Err(_) => receiver.recv().map_err(|_| no_answer())?.map_err(unavailable),
        }
    }

    /// How long a batch may run before it is duplicated, in nanoseconds: the 95th percentile of recent batches, never
    /// below the caller's floor, and the floor alone until enough batches have been timed.
    fn hedge_delay(&self) -> u64 {
        let floor = self.options.hedge_floor_nanos;
        let mut buffer = [0u64; GRAPH_HEDGE_SAMPLES];
        let count = {
            let latencies = self.latencies.lock().unwrap_or_else(PoisonError::into_inner);
            if latencies.len() < GRAPH_HEDGE_MIN_SAMPLES {
                return floor;
            }
            for (slot, sample) in buffer.iter_mut().zip(latencies.iter()) {
                *slot = *sample;
            }
            latencies.len().min(GRAPH_HEDGE_SAMPLES)
        };
        let samples = buffer.get_mut(..count).unwrap_or_default();
        let rank = (count * 95).div_ceil(100).saturating_sub(1);
        (*samples.select_nth_unstable(rank).1).max(floor)
    }

    fn record(&self, nanos: i64) {
        let mut latencies = self.latencies.lock().unwrap_or_else(PoisonError::into_inner);
        if latencies.len() == GRAPH_HEDGE_SAMPLES {
            latencies.pop_front();
        }
        latencies.push_back(u64::try_from(nanos).unwrap_or(0));
    }
}

impl EventGraph for RemoteGraph {
    fn parents_of(&self, subject: &[u8], sns: &[u64], kind: EdgeKind) -> Result<Vec<Vec<u64>>, FormatError> {
        self.edges_of(subject, sns, |edges, sn| edges.parents(sn, kind).to_vec())
    }

    fn children_of(&self, subject: &[u8], sns: &[u64], kind: EdgeKind) -> Result<Vec<Vec<u64>>, FormatError> {
        self.edges_of(subject, sns, |edges, sn| edges.children(sn, kind))
    }
}

impl EventIds for RemoteGraph {
    /// Looks each id up in the event-id index of every file of `subject`: an id whose hash a file's index lacks costs
    /// that file nothing, and the candidates a file does hold are confirmed against their stored ids after one fetch of
    /// the marks pages and one of the external-id blocks they need.
    fn resolve_ids(&self, subject: &[u8], ids: &[&[u8]]) -> Result<Vec<Option<u64>>, FormatError> {
        let mut found: Vec<Option<u64>> = vec![None; ids.len()];
        for (_, file) in self.subject_files(subject) {
            let Some(index) = file.footer().graph.as_ref().and_then(|graph| graph.event_ids.as_ref()) else {
                continue;
            };
            let candidates: Vec<(usize, u64)> = ids
                .iter()
                .enumerate()
                .filter(|(position, _)| found.get(*position).is_some_and(Option::is_none))
                .flat_map(|(position, id)| index.candidates(id).iter().map(move |&sn| (position, sn)))
                .collect();
            if candidates.is_empty() {
                continue;
            }
            if let Some(remote) = file.remote() {
                let sns: Vec<u64> = candidates.iter().map(|(_, sn)| *sn).collect();
                self.prefetch_external_ids(file, remote, &sns)?;
            }
            for (position, sn) in candidates {
                let (Some(slot), Some(id)) = (found.get_mut(position), ids.get(position)) else {
                    continue;
                };
                if slot.is_some() {
                    continue;
                }
                if let Some(row) = file.graph_row_of(sn)
                    && file.external_id(row)?.as_deref() == Some(*id)
                {
                    *slot = Some(sn);
                }
            }
        }
        Ok(found)
    }
}

impl AuthChains for RemoteGraph {
    /// Fetches the edge blocks of every granule the events lie in, as one step of a walk does, since the labels are
    /// stored inside them.
    fn chain_labels(&self, subject: &[u8], sns: &[u64]) -> Result<Vec<Option<ChainLabel>>, FormatError> {
        self.edges_of(subject, sns, |edges, sn| edges.label(sn))
    }

    /// Reads only the files' footers; no request is sent.
    fn chain_links(&self, subject: &[u8], chains: &[u32]) -> Result<Vec<ChainLink>, FormatError> {
        let mut links: Vec<ChainLink> = self
            .subject_files(subject)
            .flat_map(|(_, file)| file.graph_chain_links(chains))
            .collect();
        links.sort_unstable();
        links.dedup();
        Ok(links)
    }
}

/// Reads a subject's state (a chat room's membership, say) as it stood after any state event of a [`RemoteGraph`]:
/// the state root the event's file records, then one key of that state or the changes between two states, from the
/// tenant's state chunks. Made by [`RemoteGraph::state`].
///
/// A lookup reads one chunk per level of the state tree; a diff never reads a subtree the two states share.
///
/// See: hef-apis/spec.md
pub struct RemoteState<'a> {
    chunks: &'a dyn StateChunkStore,
    graph: &'a RemoteGraph,
    key: &'a StateChunkKey,
}

impl SubjectState for RemoteState<'_> {
    /// Reads only the footer of the file that holds `sn`; no request is sent.
    fn state_root(&self, subject: &[u8], sn: u64) -> Result<Option<StateRoot>, FormatError> {
        let (index, _) = self.graph.locate(subject, sn)?;
        Ok(self.graph.files.get(index).and_then(|file| file.state_root(sn)))
    }

    fn state_get(&self, root: &StateRoot, key: StateKey) -> Result<Option<u64>, StateChunkError> {
        state_chunk::state_get(self.chunks, self.key, root, key)
    }

    fn state_diff(&self, before: &StateRoot, after: &StateRoot) -> Result<Vec<StateChange>, StateChunkError> {
        state_chunk::state_diff(self.chunks, self.key, before, after)
    }
}

#[cfg(test)]
#[path = "test/remote.rs"]
mod tests;
