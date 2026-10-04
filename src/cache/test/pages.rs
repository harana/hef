use super::*;
use crate::typed_id::TypedIdTestExt;
use std::cell::Cell;

const FILE: u128 = 0xF11E;

fn tenant() -> TenantId {
    TenantId::new_test_id(0x5E)
}

/// A file spanning three pages, with varied bytes so a wrong slice never equals the right one.
fn file_bytes() -> Vec<u8> {
    (0..(2 * PAGE_BYTES + 100)).map(|i| (i % 251) as u8).collect()
}

fn fetcher<'a>(bytes: &'a [u8], calls: &'a Cell<u32>) -> impl FnMut(u64, u64) -> Result<Vec<u8>, ()> + 'a {
    move |start, len| {
        calls.set(calls.get() + 1);
        Ok(bytes[start as usize..(start + len) as usize].to_vec())
    }
}

#[test]
fn a_range_reads_back_exactly_across_page_boundaries() {
    let bytes = file_bytes();
    let calls = Cell::new(0);
    let cache = PageAdmissionCache::new(64 * PAGE_BYTES);
    let offset = PAGE_BYTES - 10;
    let read = cache
        .read_range(tenant(), FILE, bytes.len() as u64, offset, 30, fetcher(&bytes, &calls))
        .unwrap();
    assert_eq!(read, bytes[offset as usize..offset as usize + 30]);
    assert_eq!(calls.get(), 2, "the range touches two pages, each fetched whole");
}

#[test]
fn a_page_is_admitted_on_its_second_read_and_then_served_without_a_fetch() {
    let bytes = file_bytes();
    let calls = Cell::new(0);
    let cache = PageAdmissionCache::new(64 * PAGE_BYTES);
    let size = bytes.len() as u64;
    cache
        .read_range(tenant(), FILE, size, 0, 8, fetcher(&bytes, &calls))
        .unwrap();
    assert!(
        !cache.contains_page(tenant(), FILE, 0),
        "one read does not earn residency"
    );
    cache
        .read_range(tenant(), FILE, size, 0, 8, fetcher(&bytes, &calls))
        .unwrap();
    assert!(cache.contains_page(tenant(), FILE, 0));
    assert_eq!(calls.get(), 2);
    let read = cache
        .read_range(tenant(), FILE, size, 4, 8, fetcher(&bytes, &calls))
        .unwrap();
    assert_eq!(read, bytes[4..12]);
    assert_eq!(calls.get(), 2, "the admitted page served the third read");
    assert!(
        !cache.contains_page(tenant(), FILE, 1),
        "cold pages of the same file are not dragged in"
    );
}

#[test]
fn admitted_bytes_never_exceed_the_budget() {
    let bytes = file_bytes();
    let calls = Cell::new(0);
    let cache = PageAdmissionCache::new(PAGE_BYTES);
    let size = bytes.len() as u64;
    for _ in 0..2 {
        cache
            .read_range(tenant(), FILE, size, 0, 1, fetcher(&bytes, &calls))
            .unwrap();
        cache
            .read_range(tenant(), FILE, size, PAGE_BYTES, 1, fetcher(&bytes, &calls))
            .unwrap();
    }
    assert!(cache.admitted_bytes() <= PAGE_BYTES);
    assert!(
        cache.contains_page(tenant(), FILE, 1),
        "the most recently admitted page stays"
    );
    assert!(
        !cache.contains_page(tenant(), FILE, 0),
        "the older admitted page made room"
    );
}

#[test]
fn a_one_shot_scan_does_not_evict_an_admitted_page() {
    let size = (MAX_TRACKED_PAGES as u64 + 10) * PAGE_BYTES;
    let page = vec![1u8; PAGE_BYTES as usize];
    let cache = PageAdmissionCache::new(u64::MAX);
    let fetch = |_: u64, len: u64| Ok::<_, ()>(page[..len as usize].to_vec());
    cache.read_range(tenant(), FILE, size, 0, 1, fetch).unwrap();
    cache.read_range(tenant(), FILE, size, 0, 1, fetch).unwrap();
    for index in 1..=MAX_TRACKED_PAGES as u64 + 5 {
        cache
            .read_range(tenant(), FILE, size, index * PAGE_BYTES, 1, fetch)
            .unwrap();
    }
    assert!(cache.contains_page(tenant(), FILE, 0));
}

#[test]
fn evict_file_drops_every_page_of_that_file_only() {
    let bytes = file_bytes();
    let calls = Cell::new(0);
    let cache = PageAdmissionCache::new(64 * PAGE_BYTES);
    let size = bytes.len() as u64;
    for file in [FILE, FILE + 1] {
        for _ in 0..2 {
            cache
                .read_range(tenant(), file, size, 0, 1, fetcher(&bytes, &calls))
                .unwrap();
        }
    }
    cache.evict_file(tenant(), FILE);
    assert!(!cache.contains_page(tenant(), FILE, 0));
    assert!(cache.contains_page(tenant(), FILE + 1, 0));
    assert_eq!(cache.admitted_bytes(), PAGE_BYTES);
}

#[test]
fn tenants_never_share_admitted_pages() {
    let bytes = file_bytes();
    let calls = Cell::new(0);
    let cache = PageAdmissionCache::new(64 * PAGE_BYTES);
    let size = bytes.len() as u64;
    for _ in 0..2 {
        cache
            .read_range(tenant(), FILE, size, 0, 1, fetcher(&bytes, &calls))
            .unwrap();
    }
    assert!(!cache.contains_page(TenantId::new_test_id(0x77), FILE, 0));
}

#[test]
fn an_offset_past_the_end_is_refused_and_a_long_length_is_clamped() {
    let bytes = file_bytes();
    let calls = Cell::new(0);
    let cache = PageAdmissionCache::new(64 * PAGE_BYTES);
    let size = bytes.len() as u64;
    assert_eq!(
        cache.read_range(tenant(), FILE, size, size + 1, 1, fetcher(&bytes, &calls)),
        Err(PageReadError::InvalidRange { offset: size + 1, size })
    );
    let tail = cache
        .read_range(tenant(), FILE, size, size - 5, 100, fetcher(&bytes, &calls))
        .unwrap();
    assert_eq!(tail, bytes[bytes.len() - 5..]);
}

#[test]
fn a_failed_fetch_is_returned_and_admits_nothing() {
    let cache = PageAdmissionCache::new(64 * PAGE_BYTES);
    for _ in 0..2 {
        assert_eq!(
            cache.read_range(tenant(), FILE, 100, 0, 10, |_, _| Err::<Vec<u8>, _>("down")),
            Err(PageReadError::Fetch("down"))
        );
    }
    assert!(!cache.contains_page(tenant(), FILE, 0));
}
