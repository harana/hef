use super::*;
use crate::typed_id::TypedIdTestExt;
use std::collections::HashMap as StdHashMap;

#[derive(Default)]
struct DiskDouble {
    entries: Mutex<StdHashMap<BlockKey, Vec<u8>>>,
}

impl CacheStore for DiskDouble {
    fn get(&self, key: &BlockKey) -> Result<Option<Vec<u8>>, FileError> {
        Ok(self.entries.lock().unwrap().get(key).cloned())
    }

    fn put(&self, key: &BlockKey, bytes: &[u8]) -> Result<(), FileError> {
        self.entries.lock().unwrap().insert(*key, bytes.to_vec());
        Ok(())
    }

    fn remove(&self, key: &BlockKey) -> Result<(), FileError> {
        self.entries.lock().unwrap().remove(key);
        Ok(())
    }
}

fn key(tenant: u128, offset: u64, len: u64) -> BlockKey {
    BlockKey {
        file_id: 9,
        kind: BlockKind::Stripe,
        len,
        offset,
        tenant_id: TenantId::new_test_id(tenant),
    }
}

fn bytes(fill: u8, len: usize) -> Arc<[u8]> {
    vec![fill; len].into()
}

#[test]
fn the_memory_tier_drops_the_least_recently_used_piece_past_its_budget() {
    let cache = BlockCache::new(25, None);
    cache.insert(key(1, 0, 10), bytes(1, 10));
    cache.insert(key(1, 10, 10), bytes(2, 10));
    // Touch the first piece so the second is now the least recently used.
    assert_eq!(cache.get(&key(1, 0, 10)).as_deref(), Some(&[1u8; 10][..]));
    cache.insert(key(1, 20, 10), bytes(3, 10));

    assert!(cache.memory_bytes() <= 25);
    assert!(
        cache.get(&key(1, 10, 10)).is_none(),
        "the least recently used piece is evicted"
    );
    assert!(cache.get(&key(1, 0, 10)).is_some());
    assert!(cache.get(&key(1, 20, 10)).is_some());
}

#[test]
fn a_piece_larger_than_the_memory_budget_lives_only_on_disk() {
    let disk = Arc::new(DiskDouble::default());
    let cache = BlockCache::new(4, Some(disk.clone()));
    cache.insert(key(1, 0, 10), bytes(7, 10));
    assert_eq!(cache.memory_bytes(), 0);
    assert_eq!(cache.get(&key(1, 0, 10)).as_deref(), Some(&[7u8; 10][..]));
    assert_eq!(disk.entries.lock().unwrap().len(), 1);
}

#[test]
fn a_disk_hit_is_promoted_into_memory() {
    let disk = Arc::new(DiskDouble::default());
    disk.put(&key(1, 0, 3), b"abc").unwrap();
    let cache = BlockCache::new(1024, Some(disk.clone()));
    assert_eq!(cache.get(&key(1, 0, 3)).as_deref(), Some(&b"abc"[..]));
    assert_eq!(cache.memory_bytes(), 3);

    disk.remove(&key(1, 0, 3)).unwrap();
    assert_eq!(
        cache.get(&key(1, 0, 3)).as_deref(),
        Some(&b"abc"[..]),
        "served from memory now"
    );
}

#[test]
fn one_tenants_piece_is_never_served_to_another_tenant() {
    let disk = Arc::new(DiskDouble::default());
    let cache = BlockCache::new(1024, Some(disk));
    cache.insert(key(1, 0, 4), bytes(5, 4));
    assert!(cache.get(&key(2, 0, 4)).is_none());
    assert!(cache.get(&key(1, 0, 4)).is_some());
}

#[test]
fn remove_drops_a_piece_from_both_tiers() {
    let disk = Arc::new(DiskDouble::default());
    let cache = BlockCache::new(1024, Some(disk.clone()));
    cache.insert(key(1, 0, 4), bytes(5, 4));
    cache.remove(&key(1, 0, 4));
    assert_eq!(cache.memory_bytes(), 0);
    assert!(disk.entries.lock().unwrap().is_empty());
    assert!(cache.get(&key(1, 0, 4)).is_none());
}
