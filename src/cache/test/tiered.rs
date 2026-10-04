use super::*;
use crate::cache::disk::DiskTier;
use crate::cache::memory::MemoryTier;
use crate::cache::model::BlockKind;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;
use std::cell::Cell;

fn key(file_id: u128) -> BlockKey {
    BlockKey {
        file_id,
        kind: BlockKind::ColumnBlock,
        length: 4,
        offset: 0,
        tenant_id: TenantId::new_test_id(0x5E),
    }
}

fn stack(root: &std::path::Path) -> TieredCache<MemoryTier, DiskTier> {
    TieredCache::new(MemoryTier::new(8), DiskTier::open(root, 1024))
}

#[test]
fn a_second_read_of_the_same_block_issues_no_durable_read() {
    let root = tempfile::tempdir().unwrap();
    let cache = stack(root.path());
    let fetches = Cell::new(0);
    let fetch = || {
        fetches.set(fetches.get() + 1);
        Ok::<_, ()>(b"data".to_vec())
    };
    assert_eq!(cache.get_or_fetch(&key(1), fetch), Ok(b"data".to_vec()));
    assert_eq!(cache.get_or_fetch(&key(1), fetch), Ok(b"data".to_vec()));
    assert_eq!(fetches.get(), 1);
    assert_eq!(cache.metrics().durable_reads(), 1);
}

#[test]
fn a_block_evicted_from_memory_is_served_from_disk_and_promoted() {
    let root = tempfile::tempdir().unwrap();
    let cache = stack(root.path());
    cache.put(&key(1), b"aaaa");
    cache.put(&key(2), b"bbbb");
    // Memory holds 8 bytes, so a third incompressible block pushes the first out of RAM but not off the disk.
    cache.put(&key(3), b"cccc");
    assert!(!cache.upper().contains(&key(1)));
    let served = cache.get_or_fetch(&key(1), || Err::<Vec<u8>, _>("durable storage must not be read"));
    assert_eq!(served, Ok(b"aaaa".to_vec()));
    assert_eq!(cache.metrics().lower_hits(), 1);
    assert_eq!(cache.metrics().durable_reads(), 0);
    assert!(cache.upper().contains(&key(1)), "the disk hit was promoted into memory");
}

#[test]
fn a_failed_fetch_caches_nothing() {
    let root = tempfile::tempdir().unwrap();
    let cache = stack(root.path());
    assert_eq!(cache.get_or_fetch(&key(1), || Err::<Vec<u8>, _>("down")), Err("down"));
    assert!(!cache.contains(&key(1)));
}

#[test]
fn a_corrupt_disk_copy_is_refetched_from_durable_storage() {
    let root = tempfile::tempdir().unwrap();
    let cache = TieredCache::new(MemoryTier::new(8), DiskTier::open(root.path(), 1024));
    cache.lower().put(&key(1), b"good");
    std::fs::write(cache.lower().path_of(&key(1)), b"evil").unwrap();
    let served = cache.get_or_fetch(&key(1), || Ok::<_, ()>(b"good".to_vec()));
    assert_eq!(served, Ok(b"good".to_vec()));
    assert_eq!(cache.metrics().durable_reads(), 1);
}

#[test]
fn evict_drops_the_block_from_both_tiers() {
    let root = tempfile::tempdir().unwrap();
    let cache = stack(root.path());
    cache.put(&key(1), b"data");
    cache.evict(&key(1));
    assert!(!cache.upper().contains(&key(1)));
    assert!(!cache.lower().contains(&key(1)));
}
