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

/// The obvious timestamp-and-scan cache the linked recency list replaces, kept as the reference it must agree with.
#[derive(Default)]
struct ReferenceTier {
    budget_bytes: u64,
    entries: StdHashMap<BlockKey, (Arc<[u8]>, u64)>,
    held_bytes: u64,
    tick: u64,
}

impl ReferenceTier {
    fn get(&mut self, key: &BlockKey) -> Option<Arc<[u8]>> {
        self.tick += 1;
        let entry = self.entries.get_mut(key)?;
        entry.1 = self.tick;
        Some(Arc::clone(&entry.0))
    }

    fn insert(&mut self, key: BlockKey, bytes: Arc<[u8]>) {
        if bytes.len() as u64 > self.budget_bytes {
            return;
        }
        self.tick += 1;
        self.held_bytes += bytes.len() as u64;
        if let Some((replaced, _)) = self.entries.insert(key, (bytes, self.tick)) {
            self.held_bytes -= replaced.len() as u64;
        }
        while self.held_bytes > self.budget_bytes {
            let oldest = *self
                .entries
                .iter()
                .filter(|(candidate, _)| **candidate != key)
                .min_by_key(|(_, (_, used))| *used)
                .unwrap()
                .0;
            let (evicted, _) = self.entries.remove(&oldest).unwrap();
            self.held_bytes -= evicted.len() as u64;
        }
    }

    fn remove(&mut self, key: &BlockKey) {
        if let Some((removed, _)) = self.entries.remove(key) {
            self.held_bytes -= removed.len() as u64;
        }
    }
}

#[test]
fn the_memory_tier_evicts_exactly_as_a_full_recency_scan_would() {
    for budget in [0u64, 7, 40, 200] {
        let cache = BlockCache::new(budget, None);
        let mut reference = ReferenceTier {
            budget_bytes: budget,
            ..ReferenceTier::default()
        };
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for step in 0..5_000u32 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let piece = key(1, state % 16, 0);
            match (state >> 8) % 4 {
                0 | 1 => assert_eq!(cache.get(&piece), reference.get(&piece), "step {step}"),
                2 => {
                    let value = bytes(step as u8, ((state >> 16) % 30) as usize);
                    cache.insert(piece, Arc::clone(&value));
                    reference.insert(piece, value);
                }
                _ => {
                    cache.remove(&piece);
                    reference.remove(&piece);
                }
            }
            assert_eq!(cache.memory_bytes(), reference.held_bytes, "step {step}");
        }
        let memory = cache.memory.lock().unwrap();
        assert!(memory.slots.len() <= 16, "freed slots are reused");
        assert_eq!(memory.entries.len(), reference.entries.len());
    }
}
