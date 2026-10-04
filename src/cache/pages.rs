//! Keeps the hot pages of a large HEF file in RAM without pulling in the whole file: a page earns residency only after
//! it has been read more than once, so a one-off scan never pushes out pages that are read again and again.
//!
//! See: hef-hardware-deployment/spec.md

use super::constant::{MAX_TRACKED_PAGES, PAGE_ADMIT_AFTER, PAGE_BYTES};
use super::error::PageReadError;
use crate::events::TenantId;
use hashbrown::HashMap;
use std::ops::{Range, RangeInclusive};
use std::sync::{Mutex, MutexGuard, PoisonError};

/// One page of one tenant's file.
type PageKey = (TenantId, u128, u64);

/// One page's admission bookkeeping: how many times it has been read, its bytes once admitted, and the logical tick it
/// was last touched at, so the least-recently-used page — admitted or not — can be found for eviction.
#[derive(Debug, Default)]
struct PageSlot {
    bytes: Option<Vec<u8>>,
    hits: u32,
    last_access: u64,
}

#[derive(Debug, Default)]
struct State {
    admitted_bytes: u64,
    clock: u64,
    slots: HashMap<PageKey, PageSlot>,
}

/// A page cache for range reads that admits a page only once it has been read [`PAGE_ADMIT_AFTER`] times.
///
/// A block read once is never kept alongside a block read repeatedly, and cold pages of a file are not dragged in with
/// a hot one. It tracks at most [`MAX_TRACKED_PAGES`] pages, evicting the least-recently-used *not-yet-admitted* page
/// first, so a one-shot scan across many cold pages cannot grow it without bound or evict an admitted hot page. The
/// bytes held by admitted pages never exceed the configured budget: admitting past it evicts the least-recently-used
/// admitted pages until it fits.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug)]
pub struct PageAdmissionCache {
    admit_after: u32,
    max_admitted_bytes: u64,
    state: Mutex<State>,
}

impl PageAdmissionCache {
    /// A page cache whose admitted pages never hold more than `max_admitted_bytes`.
    pub fn new(max_admitted_bytes: u64) -> Self {
        Self {
            admit_after: PAGE_ADMIT_AFTER,
            max_admitted_bytes,
            state: Mutex::new(State::default()),
        }
    }

    /// How many bytes admitted pages hold right now.
    pub fn admitted_bytes(&self) -> u64 {
        self.state().admitted_bytes
    }

    /// Whether page `page` of the tenant's file is resident right now.
    pub fn contains_page(&self, tenant_id: TenantId, file_id: u128, page: u64) -> bool {
        self.state()
            .slots
            .get(&(tenant_id, file_id, page))
            .is_some_and(|slot| slot.bytes.is_some())
    }

    /// Drops every tracked page of one file, so a deleted file's pages can never be served again.
    pub fn evict_file(&self, tenant_id: TenantId, file_id: u128) {
        let mut state = self.state();
        let mut freed = 0u64;
        state.slots.retain(|(tenant, file, _), slot| {
            if *tenant == tenant_id && *file == file_id {
                freed = freed.saturating_add(slot.bytes.as_ref().map_or(0, |bytes| bytes.len() as u64));
                false
            } else {
                true
            }
        });
        state.admitted_bytes = state.admitted_bytes.saturating_sub(freed);
    }

    /// Reads `len` bytes at `offset` of a `size`-byte file, page by page: resident pages are served from RAM, and only
    /// missing pages are read through `fetch(start, len)` — always as whole pages, so an admitted page can serve any
    /// later range touching it. A length past the end is clamped to the file, like an object-store range read; an
    /// offset past the end is [`PageReadError::InvalidRange`]. A `fetch` failure is returned unchanged and admits
    /// nothing.
    pub fn read_range<E>(
        &self,
        tenant_id: TenantId,
        file_id: u128,
        size: u64,
        offset: u64,
        len: u64,
        mut fetch: impl FnMut(u64, u64) -> Result<Vec<u8>, E>,
    ) -> Result<Vec<u8>, PageReadError<E>> {
        if offset > size {
            return Err(PageReadError::InvalidRange { offset, size });
        }
        let len = len.min(size - offset);
        let mut assembled = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        if len == 0 {
            return Ok(assembled);
        }
        for page in covering_pages(offset, len) {
            let bounds = page_range(page, size);
            let bytes = match self.get_page(tenant_id, file_id, page) {
                Some(bytes) => bytes,
                None => {
                    let bytes = fetch(bounds.start, bounds.end - bounds.start).map_err(PageReadError::Fetch)?;
                    self.record_read(tenant_id, file_id, page, &bytes);
                    bytes
                }
            };
            let from = usize::try_from(offset.max(bounds.start) - bounds.start).unwrap_or(usize::MAX);
            let to = usize::try_from((offset + len).min(bounds.end) - bounds.start).unwrap_or(usize::MAX);
            match bytes.get(from..to) {
                Some(slice) => assembled.extend_from_slice(slice),
                // A fetch that returned fewer bytes than the page holds is a short read from durable storage; serve
                // what arrived rather than index past it.
                None => assembled.extend_from_slice(bytes.get(from..).unwrap_or_default()),
            }
        }
        Ok(assembled)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // Every mutation leaves `State` consistent before it can panic, so a poisoned lock is still safe to use.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The page's resident bytes, if admitted. Counts as a use for eviction.
    fn get_page(&self, tenant_id: TenantId, file_id: u128, page: u64) -> Option<Vec<u8>> {
        let mut state = self.state();
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        let slot = state.slots.get_mut(&(tenant_id, file_id, page))?;
        slot.last_access = tick;
        slot.bytes.clone()
    }

    /// Records one read of a page's complete bytes, admitting the page once its read count reaches `admit_after`.
    fn record_read(&self, tenant_id: TenantId, file_id: u128, page: u64, bytes: &[u8]) {
        let mut guard = self.state();
        let state = &mut *guard;
        state.clock = state.clock.wrapping_add(1);
        let tick = state.clock;
        let key = (tenant_id, file_id, page);
        if !state.slots.contains_key(&key) {
            while state.slots.len() >= MAX_TRACKED_PAGES {
                let victim = state
                    .slots
                    .iter()
                    .filter(|(_, slot)| slot.bytes.is_none())
                    .min_by_key(|(_, slot)| slot.last_access)
                    .or_else(|| state.slots.iter().min_by_key(|(_, slot)| slot.last_access))
                    .map(|(victim, _)| *victim);
                let Some(victim) = victim else { break };
                if let Some(slot) = state.slots.remove(&victim)
                    && let Some(bytes) = slot.bytes
                {
                    state.admitted_bytes = state.admitted_bytes.saturating_sub(bytes.len() as u64);
                }
            }
        }
        let slot = state.slots.entry(key).or_default();
        slot.hits = slot.hits.saturating_add(1);
        slot.last_access = tick;
        if slot.hits >= self.admit_after && slot.bytes.is_none() {
            slot.bytes = Some(bytes.to_vec());
            state.admitted_bytes = state.admitted_bytes.saturating_add(bytes.len() as u64);
        }
        // The page just admitted is the most recently used, so it is only chosen once nothing else is resident, which
        // holds a budget smaller than one page to its configured limit.
        while state.admitted_bytes > self.max_admitted_bytes {
            let victim = state
                .slots
                .iter()
                .filter(|(_, slot)| slot.bytes.is_some())
                .min_by_key(|(_, slot)| slot.last_access)
                .map(|(victim, _)| *victim);
            let Some(victim) = victim else { break };
            let freed = state
                .slots
                .get_mut(&victim)
                .and_then(|slot| slot.bytes.take())
                .map_or(0, |bytes| bytes.len() as u64);
            state.admitted_bytes = state.admitted_bytes.saturating_sub(freed);
        }
    }
}

/// The page index covering byte `offset`.
pub fn page_of(offset: u64) -> u64 {
    offset / PAGE_BYTES
}

/// `page`'s byte range within a file of `total_len` bytes, clipped so the last page ends exactly at the file's end.
pub fn page_range(page: u64, total_len: u64) -> Range<u64> {
    let start = page.saturating_mul(PAGE_BYTES).min(total_len);
    let end = start.saturating_add(PAGE_BYTES).min(total_len);
    start..end
}

/// The pages a `[offset, offset + len)` range spans, in order.
fn covering_pages(offset: u64, len: u64) -> RangeInclusive<u64> {
    let last = offset.saturating_add(len.saturating_sub(1));
    page_of(offset)..=page_of(last)
}

#[cfg(test)]
#[path = "test/pages.rs"]
mod tests;
