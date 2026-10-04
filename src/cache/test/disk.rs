use super::*;
use crate::cache::model::BlockKind;
use crate::cache::volume::FsCacheVolume;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn key(file_id: u128) -> BlockKey {
    BlockKey {
        file_id,
        kind: BlockKind::Footer,
        length: 4,
        offset: 0,
        tenant_id: TenantId::new_test_id(0x5E),
    }
}

/// A configured volume rooted at a fresh temp dir; the returned `TempDir` must outlive the tier.
fn volume(id: &str) -> (LocalVolume, TempDir) {
    let dir = TempDir::new().unwrap();
    let volume = LocalVolume {
        id: id.to_owned(),
        path: dir.path().to_str().unwrap().to_owned(),
        weight: 1,
    };
    (volume, dir)
}

fn fs_tier(capacity: u64, layout: LocalLayout, volumes: &[LocalVolume]) -> DiskTier {
    let policy = PlacementPolicy::new(layout, volumes.to_vec()).unwrap();
    let opened: Vec<Box<dyn CacheVolume>> = volumes
        .iter()
        .map(|volume| Box::new(FsCacheVolume::new(&volume.id, &volume.path)) as Box<dyn CacheVolume>)
        .collect();
    DiskTier::new(capacity, policy, opened)
}

/// Every block file under one volume's cache directory.
fn blob_files(volume: &LocalVolume) -> Vec<PathBuf> {
    match std::fs::read_dir(Path::new(&volume.path).join(DISK_TIER_DIR)) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// A volume whose reads always fail with a disk fault (writes are accepted but discarded).
#[derive(Debug)]
struct FailingReadVolume {
    id: String,
}

impl CacheVolume for FailingReadVolume {
    fn id(&self) -> &str {
        &self.id
    }

    fn read_blob(&self, _relative_path: &str) -> Result<Option<Vec<u8>>, PlacementError> {
        Err(PlacementError::VolumeIo {
            detail: "injected disk read fault".to_owned(),
            volume_id: self.id.clone(),
        })
    }

    fn write_blob(&self, _relative_path: &str, _bytes: &[u8]) -> Result<(), PlacementError> {
        Ok(())
    }

    fn delete_blob(&self, _relative_path: &str) {}

    fn clear_directory(&self, _relative_dir: &str) {}
}

#[test]
fn single_layout_round_trips_bytes() {
    let (vol, _dir) = volume("a");
    let tier = fs_tier(1024, LocalLayout::Single, &[vol]);
    tier.put(&key(1), b"footer");
    assert!(tier.contains(&key(1)));
    assert_eq!(tier.get(&key(1)), Some(b"footer".to_vec()));
    assert_eq!(tier.used_bytes(), 6);
}

#[test]
fn eviction_drops_the_coldest_and_deletes_its_files() {
    let (vol, _dir) = volume("a");
    let tier = fs_tier(8, LocalLayout::Single, std::slice::from_ref(&vol));
    tier.put(&key(1), b"aaaa");
    tier.put(&key(2), b"bbbb");
    assert_eq!(tier.get(&key(1)), Some(b"aaaa".to_vec()));
    tier.put(&key(3), b"cccc");
    assert!(tier.contains(&key(1)));
    assert!(!tier.contains(&key(2)), "the coldest block was evicted");
    assert!(tier.contains(&key(3)));
    assert_eq!(tier.used_bytes(), 8);
    assert_eq!(blob_files(&vol).len(), 2, "the evicted block's file is gone from disk");
}

#[test]
fn a_block_bigger_than_the_tier_is_never_kept() {
    let (vol, _dir) = volume("a");
    let tier = fs_tier(4, LocalLayout::Single, &[vol]);
    tier.put(&key(1), b"too-big-to-fit");
    assert!(!tier.contains(&key(1)));
}

#[test]
fn a_corrupt_local_copy_is_a_miss_not_a_served_bad_byte() {
    let (vol, _dir) = volume("a");
    let tier = fs_tier(1024, LocalLayout::Single, std::slice::from_ref(&vol));
    tier.put(&key(1), b"good bytes");
    std::fs::write(&blob_files(&vol)[0], b"bad  bytes").unwrap();
    assert_eq!(tier.get(&key(1)), None);
    assert!(!tier.contains(&key(1)));
    assert_eq!(tier.used_bytes(), 0);
}

#[test]
fn evict_removes_the_entry_and_its_file() {
    let (vol, _dir) = volume("a");
    let tier = fs_tier(1024, LocalLayout::Single, std::slice::from_ref(&vol));
    tier.put(&key(1), b"bye");
    tier.evict(&key(1));
    assert!(!tier.contains(&key(1)));
    assert!(blob_files(&vol).is_empty());
}

#[test]
fn mirror_writes_a_copy_per_disk_and_survives_a_lost_copy() {
    let (a, _da) = volume("a");
    let (b, _db) = volume("b");
    let tier = fs_tier(1024, LocalLayout::Mirror, &[a.clone(), b.clone()]);
    tier.put(&key(1), b"redundant");
    assert_eq!(blob_files(&a).len(), 1);
    assert_eq!(blob_files(&b).len(), 1);
    std::fs::remove_file(&blob_files(&a)[0]).unwrap();
    assert_eq!(tier.get(&key(1)), Some(b"redundant".to_vec()));
}

#[test]
fn a_mirror_read_fault_falls_through_to_a_healthy_mirror_instead_of_discarding_it() {
    let (healthy, _dir) = volume("healthy");
    let policy = PlacementPolicy::new(
        LocalLayout::Mirror,
        vec![
            LocalVolume {
                id: "broken".to_owned(),
                path: "/broken-disk".to_owned(),
                weight: 1,
            },
            healthy.clone(),
        ],
    )
    .unwrap();
    let volumes: Vec<Box<dyn CacheVolume>> = vec![
        Box::new(FailingReadVolume {
            id: "broken".to_owned(),
        }),
        Box::new(FsCacheVolume::new(&healthy.id, &healthy.path)),
    ];
    let tier = DiskTier::new(1024, policy, volumes);
    tier.put(&key(1), b"redundant");
    assert_eq!(tier.get(&key(1)), Some(b"redundant".to_vec()));
    assert!(tier.contains(&key(1)));
}

#[test]
fn stripe_splits_across_disks_and_reassembles() {
    let (a, _da) = volume("a");
    let (b, _db) = volume("b");
    let tier = fs_tier(1024, LocalLayout::Stripe, &[a.clone(), b.clone()]);
    let bytes = b"the quick brown fox jumps";
    tier.put(&key(1), bytes);
    assert_eq!(blob_files(&a).len(), 1);
    assert_eq!(blob_files(&b).len(), 1);
    assert!(std::fs::read(&blob_files(&a)[0]).unwrap().len() < bytes.len());
    assert_eq!(tier.get(&key(1)), Some(bytes.to_vec()));
}

#[test]
fn a_lost_stripe_chunk_is_a_miss() {
    let (a, _da) = volume("a");
    let (b, _db) = volume("b");
    let tier = fs_tier(1024, LocalLayout::Stripe, &[a, b.clone()]);
    tier.put(&key(1), b"the quick brown fox jumps");
    std::fs::remove_file(&blob_files(&b)[0]).unwrap();
    assert_eq!(tier.get(&key(1)), None);
    assert!(!tier.contains(&key(1)));
}

#[test]
fn a_restart_reclaims_leftover_files_but_not_the_rest_of_the_root() {
    let (vol, dir) = volume("a");
    std::fs::write(dir.path().join("keep.txt"), b"other").unwrap();
    let first = fs_tier(8, LocalLayout::Single, std::slice::from_ref(&vol));
    first.put(&key(1), b"aaaa");
    first.put(&key(2), b"bbbb");
    drop(first);

    let restarted = fs_tier(8, LocalLayout::Single, std::slice::from_ref(&vol));
    assert_eq!(restarted.used_bytes(), 0);
    assert!(blob_files(&vol).is_empty());
    assert!(dir.path().join("keep.txt").exists());
    restarted.put(&key(3), b"cccc");
    restarted.put(&key(4), b"dddd");
    assert_eq!(restarted.used_bytes(), 8);
    assert_eq!(blob_files(&vol).len(), 2);
}

#[test]
fn open_rejects_a_mirror_with_one_disk_and_builds_a_working_tier_otherwise() {
    let (only, _do) = volume("only");
    assert!(matches!(
        DiskTier::open(1024, LocalLayout::Mirror, vec![only.clone()]),
        Err(PlacementError::InsufficientVolumes { .. })
    ));
    let tier = DiskTier::open(1024, LocalLayout::Single, vec![only]).unwrap();
    tier.put(&key(1), b"bytes");
    assert_eq!(tier.get(&key(1)), Some(b"bytes".to_vec()));
}

#[test]
fn no_tenant_or_file_id_appears_in_file_names() {
    let (vol, _dir) = volume("a");
    let tier = fs_tier(1024, LocalLayout::Single, std::slice::from_ref(&vol));
    let key = key(0xABCDEF);
    tier.put(&key, b"bytes");
    let name = blob_files(&vol)[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(!name.contains("abcdef"));
    assert!(!name.contains(&key.tenant_id.to_string()));
}
