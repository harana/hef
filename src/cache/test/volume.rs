use super::*;
use tempfile::TempDir;

#[test]
fn accelerated_volume_is_byte_equivalent_to_the_portable_one() {
    let accel_dir = TempDir::new().unwrap();
    let fs_dir = TempDir::new().unwrap();
    let accel = AcceleratedCacheVolume::open("accel", accel_dir.path());
    let portable = FsCacheVolume::new("portable", fs_dir.path());
    let bytes = b"identical either way";
    accel.write_blob("cache/blob", bytes).unwrap();
    portable.write_blob("cache/blob", bytes).unwrap();
    assert_eq!(
        accel.read_blob("cache/blob").unwrap(),
        portable.read_blob("cache/blob").unwrap()
    );
    assert_eq!(accel.read_blob("cache/blob").unwrap().unwrap(), bytes);

    accel.write_blob("cache/blob", b"replaced").unwrap();
    assert_eq!(accel.read_blob("cache/blob").unwrap().unwrap(), b"replaced");
    accel.delete_blob("cache/blob");
    assert_eq!(accel.read_blob("cache/blob").unwrap(), None);
}

#[test]
fn preallocation_never_changes_the_written_bytes_or_length() {
    let dir = TempDir::new().unwrap();
    let target = dir.path().join("blob");
    let bytes = vec![7u8; 10_000];
    write_blob_at(&target, &bytes, true).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), bytes);
    write_blob_at(&target, b"short", true).unwrap();
    assert_eq!(std::fs::read(&target).unwrap(), b"short");
}

#[test]
fn the_probe_leaves_no_file_behind() {
    let dir = TempDir::new().unwrap();
    let _ = probe_write_zeroes(dir.path());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn no_staging_file_is_left_after_a_write() {
    let dir = TempDir::new().unwrap();
    let volume = FsCacheVolume::new("v", dir.path());
    volume.write_blob("cache/blob", b"bytes").unwrap();
    let names: Vec<_> = std::fs::read_dir(dir.path().join("cache")).unwrap().collect();
    assert_eq!(names.len(), 1);
}

#[test]
fn a_missing_blob_reads_as_none_and_clearing_removes_the_directory() {
    let dir = TempDir::new().unwrap();
    let volume = FsCacheVolume::new("v", dir.path());
    assert_eq!(volume.read_blob("cache/absent").unwrap(), None);
    volume.write_blob("cache/blob", b"bytes").unwrap();
    volume.clear_directory("cache");
    assert!(!dir.path().join("cache").exists());
}

#[test]
fn open_cache_volume_returns_a_working_volume_either_way() {
    let dir = TempDir::new().unwrap();
    let volume = open_cache_volume(&LocalVolume {
        id: "v".to_owned(),
        path: dir.path().to_string_lossy().into_owned(),
        weight: 1,
    });
    assert_eq!(volume.id(), "v");
    volume.write_blob("cache/blob", b"bytes").unwrap();
    assert_eq!(volume.read_blob("cache/blob").unwrap().unwrap(), b"bytes");
}
