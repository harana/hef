use super::*;
use crate::cache::model::BlockKind;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;

fn key(file_id: u128) -> BlockKey {
    BlockKey {
        file_id,
        kind: BlockKind::Footer,
        length: 4,
        offset: 0,
        tenant_id: TenantId::new_test_id(0x5E),
    }
}

#[test]
fn a_put_block_reads_back_byte_identical() {
    let root = tempfile::tempdir().unwrap();
    let tier = DiskTier::open(root.path(), 1024);
    tier.put(&key(1), b"footer");
    assert!(tier.contains(&key(1)));
    assert_eq!(tier.get(&key(1)), Some(b"footer".to_vec()));
    assert_eq!(tier.used_bytes(), 6);
}

#[test]
fn evicts_the_least_recently_used_block_when_full() {
    let root = tempfile::tempdir().unwrap();
    let tier = DiskTier::open(root.path(), 10);
    tier.put(&key(1), b"aaaa");
    tier.put(&key(2), b"bbbb");
    assert_eq!(tier.get(&key(1)), Some(b"aaaa".to_vec()));
    tier.put(&key(3), b"cccc");
    assert!(tier.contains(&key(1)));
    assert!(!tier.contains(&key(2)));
    assert!(tier.contains(&key(3)));
    assert_eq!(tier.used_bytes(), 8);
    assert_eq!(std::fs::read_dir(root.path().join(DISK_TIER_DIR)).unwrap().count(), 2);
}

#[test]
fn a_corrupted_file_is_a_miss_and_is_dropped() {
    let root = tempfile::tempdir().unwrap();
    let tier = DiskTier::open(root.path(), 1024);
    tier.put(&key(1), b"good bytes");
    std::fs::write(tier.path_of(&key(1)), b"bad  bytes").unwrap();
    assert_eq!(tier.get(&key(1)), None);
    assert!(!tier.contains(&key(1)));
    assert_eq!(tier.used_bytes(), 0);
}

#[test]
fn a_missing_file_is_a_miss() {
    let root = tempfile::tempdir().unwrap();
    let tier = DiskTier::open(root.path(), 1024);
    tier.put(&key(1), b"bytes");
    std::fs::remove_file(tier.path_of(&key(1))).unwrap();
    assert_eq!(tier.get(&key(1)), None);
    assert_eq!(tier.entry_count(), 0);
}

#[test]
fn evict_deletes_the_file() {
    let root = tempfile::tempdir().unwrap();
    let tier = DiskTier::open(root.path(), 1024);
    tier.put(&key(1), b"bytes");
    tier.evict(&key(1));
    assert!(!tier.path_of(&key(1)).exists());
    assert_eq!(tier.used_bytes(), 0);
}

#[test]
fn opening_clears_files_a_previous_process_left_but_not_the_rest_of_the_root() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("keep.txt"), b"other").unwrap();
    let stale = DiskTier::open(root.path(), 1024);
    stale.put(&key(1), b"bytes");
    let stale_file = stale.path_of(&key(1));
    drop(stale);

    let fresh = DiskTier::open(root.path(), 1024);
    assert!(!stale_file.exists());
    assert!(!fresh.contains(&key(1)));
    assert!(root.path().join("keep.txt").exists());
}

#[test]
fn no_tenant_or_file_id_appears_in_file_names() {
    let root = tempfile::tempdir().unwrap();
    let tier = DiskTier::open(root.path(), 1024);
    let key = key(0xABCDEF);
    tier.put(&key, b"bytes");
    let name = tier.path_of(&key).file_name().unwrap().to_string_lossy().into_owned();
    assert!(!name.contains("abcdef"));
    assert!(!name.contains(&key.tenant_id.to_string()));
}
