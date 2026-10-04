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
//! See: hef-file-layout/spec.md

use super::cache::{BlockCache, BlockKey, BlockKind};
use super::constant::HELD_RANGE_BUCKETS;
use super::footer::{Footer, StripeEntry};
use super::reader::{DECODED_CACHE_BUDGET_BYTES, HeaderCommitment, HefFile, HefFooter, VerifiedStripeRead};
use super::{HefHeader, LayoutClass};
use crate::error::FormatError;
use crate::events::TenantId;
use crate::file::bytes::slice;
use crate::file::{FileError, RangeSource};
use crate::lifecycle::HefFileEntry;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

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
        let stripe = self.footer.footer().stripes.iter().find(|stripe| {
            stripe.file_offset <= position
                && stripe
                    .file_offset
                    .checked_add(stripe.byte_len)
                    .is_some_and(|stripe_end| end <= stripe_end)
        });
        let Some(stripe) = stripe else {
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

#[cfg(test)]
#[path = "test/remote.rs"]
mod tests;
