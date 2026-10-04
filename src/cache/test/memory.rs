use super::*;
use crate::cache::model::BlockKind;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;

fn key(file_id: u128) -> BlockKey {
    page(file_id, 0)
}

fn page(file_id: u128, index: u64) -> BlockKey {
    BlockKey {
        file_id,
        kind: BlockKind::ColumnBlock,
        length: 4096,
        offset: index * 4096,
        tenant_id: TenantId::new_test_id(0x5E),
    }
}

/// Bytes lz4 shrinks far past the keep bar, with enough structure that a corrupted round trip would be caught.
fn compressible(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Bytes lz4 cannot shrink at all: a deterministic xorshift stream with no repeats for lz4 to find.
fn incompressible(len: usize) -> Vec<u8> {
    let mut x = 0x9E37_79B9_u32;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x >> 24) as u8
        })
        .collect()
}

#[test]
fn evicts_the_least_recently_used_block_when_full() {
    let tier = MemoryTier::new(10);
    let (a, b, c) = (key(1), key(2), key(3));
    tier.put(&a, b"aaaa");
    tier.put(&b, b"bbbb");
    // Touch `a` so `b` is now the least-recently-used.
    assert_eq!(tier.get(&a), Some(b"aaaa".to_vec()));
    tier.put(&c, b"cccc");
    assert!(tier.contains(&a));
    assert!(!tier.contains(&b), "the least-recently-used entry was evicted");
    assert!(tier.contains(&c));
    assert_eq!(tier.used_bytes(), 8);
    assert_eq!(tier.entry_count(), 2);
}

#[test]
fn a_block_larger_than_the_whole_tier_is_never_kept() {
    let tier = MemoryTier::new(4);
    tier.put(&key(1), b"too large");
    assert!(!tier.contains(&key(1)));
    assert_eq!(tier.used_bytes(), 0);
}

#[test]
fn re_putting_a_held_block_replaces_its_bytes() {
    let tier = MemoryTier::new(1024);
    tier.put(&key(1), b"first");
    tier.put(&key(1), b"second");
    assert_eq!(tier.get(&key(1)), Some(b"second".to_vec()));
    assert_eq!(tier.entry_count(), 1);
}

#[test]
fn re_putting_tracks_the_size_delta_and_can_evict_to_make_room() {
    let tier = MemoryTier::new(8);
    let (a, b) = (key(1), key(2));
    tier.put(&a, b"aa");
    tier.put(&b, b"bb");
    assert_eq!(tier.used_bytes(), 4);

    // Growing `a` from 2 to 6 bytes raises used_bytes by the delta; 6 + 2 still fits, so `b` survives.
    tier.put(&a, b"aaaaaa");
    assert_eq!(tier.used_bytes(), 8);
    assert!(tier.contains(&b));

    // Growing `a` again past the remaining room evicts the least-recently-used block (`b`) to fit.
    tier.put(&a, b"aaaaaaaa");
    assert_eq!(tier.used_bytes(), 8);
    assert!(
        !tier.contains(&b),
        "the colder block was evicted to make room for the larger re-put"
    );
    assert_eq!(tier.get(&a), Some(b"aaaaaaaa".to_vec()));
}

#[test]
fn an_empty_or_oversized_re_put_drops_the_stale_copy() {
    let tier = MemoryTier::new(4);
    tier.put(&key(1), b"aa");
    tier.put(&key(1), b"");
    assert!(!tier.contains(&key(1)));
    tier.put(&key(2), b"bb");
    tier.put(&key(2), b"too large");
    assert!(!tier.contains(&key(2)));
    assert_eq!(tier.used_bytes(), 0);
    assert_eq!(tier.entry_count(), 0);
}

#[test]
fn evict_drops_one_block_and_reclaims_its_bytes() {
    let tier = MemoryTier::new(1024);
    tier.put(&key(1), b"bytes");
    tier.evict(&key(1));
    assert!(!tier.contains(&key(1)));
    assert_eq!(tier.used_bytes(), 0);
    assert_eq!(tier.get(&key(1)), None);
}

#[test]
fn tiny_blocks_cannot_grow_the_tier_past_its_entry_bound() {
    let capacity = 64 * 1024;
    let tier = MemoryTier::new(capacity);
    for i in 0..10_000u128 {
        tier.put(&key(i), b"a");
    }
    let bound = usize::try_from(capacity / ENTRY_OVERHEAD_BYTES).unwrap();
    assert!(tier.entry_count() <= bound, "entries stayed within the derived bound");
    assert!(tier.used_bytes() < capacity);
}

#[test]
fn ranges_of_one_file_are_cached_and_evicted_independently() {
    let tier = MemoryTier::new(8);
    tier.put(&page(1, 0), b"aaaa");
    tier.put(&page(1, 1), b"bbbb");
    // Touch page 0 so page 1 is now the least-recently-used block.
    assert_eq!(tier.get(&page(1, 0)), Some(b"aaaa".to_vec()));
    tier.put(&page(1, 2), b"cccc");
    assert!(tier.contains(&page(1, 0)));
    assert!(!tier.contains(&page(1, 1)));
    assert!(tier.contains(&page(1, 2)));
}

#[test]
fn tenants_never_share_a_cached_copy() {
    let tier = MemoryTier::new(1024);
    let mine = key(1);
    let theirs = BlockKey {
        tenant_id: TenantId::new_test_id(0x77),
        ..mine
    };
    tier.put(&mine, b"mine");
    assert_eq!(tier.get(&theirs), None);
}

#[test]
fn an_evicted_block_is_kept_compressed_and_reads_back_byte_identical() {
    let tier = MemoryTier::new(1024);
    let payload = compressible(700);
    tier.put(&key(1), &payload);
    // The second block does not fit next to a hot first one, so the first is demoted instead of dropped.
    tier.put(&key(2), &compressible(700));
    assert!(tier.contains(&key(1)), "the evicted block stayed resident, compressed");
    assert_eq!(tier.metrics().demoted_original_bytes(), 700);
    assert!(tier.metrics().compression_ratio() > 1.0);

    assert_eq!(tier.get(&key(1)), Some(payload.clone()));
    assert_eq!(tier.metrics().cold_hits(), 1);
    assert_eq!(tier.get(&key(1)), Some(payload));
    assert_eq!(tier.metrics().hot_hits(), 1);
    assert!(tier.used_bytes() <= 1024, "promotion never overflows the budget");
}

#[test]
fn cold_blocks_charge_their_compressed_size_to_the_budget() {
    let tier = MemoryTier::new(1024);
    tier.put(&key(1), &compressible(700));
    tier.put(&key(2), &compressible(700));
    let compressed = tier.metrics().demoted_compressed_bytes();
    assert!(compressed > 0 && compressed <= 700 * 3 / 4, "the keep bar held");
    assert_eq!(tier.used_bytes(), 700 + compressed);
}

#[test]
fn an_incompressible_evicted_block_is_dropped_not_kept() {
    let tier = MemoryTier::new(1024);
    tier.put(&key(1), &incompressible(600));
    tier.put(&key(2), &incompressible(600));
    assert!(!tier.contains(&key(1)), "a block that misses the keep bar is dropped");
    assert!(tier.contains(&key(2)));
    assert_eq!(tier.used_bytes(), 600);
    assert_eq!(tier.metrics().incompressible_drops(), 1);
}

#[test]
fn a_cold_block_picked_by_a_later_eviction_round_is_dropped() {
    let tier = MemoryTier::new(1024);
    tier.put(&key(1), &compressible(700));
    tier.put(&key(2), &compressible(700));
    tier.put(&key(3), &incompressible(600));
    assert!(!tier.contains(&key(1)), "the cold block lost its second eviction round");
    assert!(tier.contains(&key(2)));
    assert!(tier.contains(&key(3)));
    assert!(tier.used_bytes() <= 1024);
}
