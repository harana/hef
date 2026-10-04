//! One local disk the disk tier writes block copies to, with two interchangeable backends: a portable one over plain
//! file calls, and an accelerated one that preallocates each file's blocks first where the host supports it.
//!
//! See: hef-hardware-deployment/spec.md

use super::constant::WRITE_ZEROES_PROBE_BYTES;
#[cfg(target_os = "linux")]
use super::constant::{FALLOC_FL_KEEP_SIZE, FALLOC_FL_WRITE_ZEROES};
use super::error::PlacementError;
use super::placement::LocalVolume;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
use rustix::fs::{FallocateFlags, fallocate};

/// Distinguishes concurrent staging files for the same blob within one process.
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

/// One configured local disk the disk tier writes block copies to.
///
/// A blob is one copy of a cached block (a whole copy, or one stripe chunk) named by a path relative to the volume's
/// root. Implementations only move bytes; losing a blob only costs a re-fetch.
///
/// See: hef-hardware-deployment/spec.md
pub trait CacheVolume: std::fmt::Debug + Send + Sync {
    /// The volume's stable id, matching the [`LocalVolume::id`] it was configured from.
    fn id(&self) -> &str;

    /// Reads back the blob at `relative_path`. Returns `None` when no blob is there, and an error only when the disk
    /// itself failed, so a caller can tell "not cached" from "the disk is broken".
    fn read_blob(&self, relative_path: &str) -> Result<Option<Vec<u8>>, PlacementError>;

    /// Writes `bytes` as the blob at `relative_path`, replacing any previous copy, and makes it durable before
    /// returning. A reader never sees a half-written blob.
    fn write_blob(&self, relative_path: &str, bytes: &[u8]) -> Result<(), PlacementError>;

    /// Deletes the blob at `relative_path`. A blob that is already gone is not an error.
    fn delete_blob(&self, relative_path: &str);

    /// Deletes every blob under `relative_dir` in one sweep. A restarting cache calls this on the blobs a previous
    /// process left behind, which it has no bookkeeping for.
    fn clear_directory(&self, relative_dir: &str);
}

/// A cache volume over plain file calls: each blob is written to a temporary file, synced, and renamed into place. It
/// works on any host and is the reference the accelerated volume must match.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug)]
pub struct FsCacheVolume {
    id: String,
    root: PathBuf,
}

impl FsCacheVolume {
    /// A portable volume with id `id` rooted at `root`. The root directory is created on first write.
    pub fn new(id: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        Self {
            id: id.into(),
            root: root.into(),
        }
    }
}

impl CacheVolume for FsCacheVolume {
    fn id(&self) -> &str {
        &self.id
    }

    fn read_blob(&self, relative_path: &str) -> Result<Option<Vec<u8>>, PlacementError> {
        read_blob_at(&self.id, &self.root.join(relative_path))
    }

    fn write_blob(&self, relative_path: &str, bytes: &[u8]) -> Result<(), PlacementError> {
        write_blob_at(&self.root.join(relative_path), bytes, false).map_err(|error| io_error(&self.id, &error))
    }

    fn delete_blob(&self, relative_path: &str) {
        let _ = std::fs::remove_file(self.root.join(relative_path));
    }

    fn clear_directory(&self, relative_dir: &str) {
        let _ = std::fs::remove_dir_all(self.root.join(relative_dir));
    }
}

/// A cache volume that preallocates and zeroes each blob's whole extent in one `fallocate` call before writing it, so
/// the write lands in already-materialised blocks.
///
/// It probes the disk when opened; where the host lacks the write-zeroes mode every write takes the portable path.
/// Either way the bytes stored and returned, and the errors raised, are identical to [`FsCacheVolume`]; only latency
/// and CPU differ.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug)]
pub struct AcceleratedCacheVolume {
    efficient_extent_zeroing: bool,
    id: String,
    root: PathBuf,
}

impl AcceleratedCacheVolume {
    /// Opens the volume rooted at `root`, creating the directory if absent and probing whether the filesystem under it
    /// supports the `fallocate` write-zeroes mode.
    pub fn open(id: impl Into<String>, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let _ = std::fs::create_dir_all(&root);
        Self {
            efficient_extent_zeroing: probe_write_zeroes(&root),
            id: id.into(),
            root,
        }
    }

    /// Whether the host reported the fast path. When `false` the volume still works, with identical results.
    pub fn is_accelerated(&self) -> bool {
        self.efficient_extent_zeroing
    }
}

impl CacheVolume for AcceleratedCacheVolume {
    fn id(&self) -> &str {
        &self.id
    }

    fn read_blob(&self, relative_path: &str) -> Result<Option<Vec<u8>>, PlacementError> {
        read_blob_at(&self.id, &self.root.join(relative_path))
    }

    fn write_blob(&self, relative_path: &str, bytes: &[u8]) -> Result<(), PlacementError> {
        write_blob_at(&self.root.join(relative_path), bytes, self.efficient_extent_zeroing)
            .map_err(|error| io_error(&self.id, &error))
    }

    fn delete_blob(&self, relative_path: &str) {
        let _ = std::fs::remove_file(self.root.join(relative_path));
    }

    fn clear_directory(&self, relative_dir: &str) {
        let _ = std::fs::remove_dir_all(self.root.join(relative_dir));
    }
}

/// Opens the cache volume for one configured disk: the accelerated volume where the host's probe confirms the fast
/// path, the portable volume otherwise.
pub fn open_cache_volume(volume: &LocalVolume) -> Box<dyn CacheVolume> {
    let accelerated = AcceleratedCacheVolume::open(&volume.id, &volume.path);
    if accelerated.is_accelerated() {
        Box::new(accelerated)
    } else {
        Box::new(FsCacheVolume::new(&volume.id, &volume.path))
    }
}

/// Reads a whole blob file, mapping "the file is absent" to `None` and any real disk fault to an error.
fn read_blob_at(volume_id: &str, path: &Path) -> Result<Option<Vec<u8>>, PlacementError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error(volume_id, &error)),
    }
}

/// Writes `bytes` to a fresh staging file beside `target`, optionally preallocating its extent first, syncs it, and
/// renames it over `target`, so `target` only ever names a complete copy. A failed write removes the staging file.
fn write_blob_at(target: &Path, bytes: &[u8], preallocate: bool) -> std::io::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staging = target.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        STAGING_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&staging)
        .and_then(|mut file| {
            if preallocate {
                preallocate_zeroed(&file, bytes.len() as u64);
            }
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&staging, target));
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

/// Preallocates and zeroes `[0, len)` of `file` without growing its logical size, so the write that follows lands in
/// ready blocks. Best-effort: returns whether the kernel accepted the call, and never changes the file's bytes.
#[cfg(target_os = "linux")]
fn preallocate_zeroed(file: &std::fs::File, len: u64) -> bool {
    if len == 0 {
        return true;
    }
    let mode = FallocateFlags::from_bits_retain(FALLOC_FL_WRITE_ZEROES | FALLOC_FL_KEEP_SIZE);
    fallocate(file, mode, 0, len).is_ok()
}

/// Hosts other than Linux have no write-zeroes mode, so every write keeps allocation-on-write.
#[cfg(not(target_os = "linux"))]
fn preallocate_zeroed(_file: &std::fs::File, _len: u64) -> bool {
    false
}

/// Asks the filesystem under `dir` whether it supports the write-zeroes mode, by trying it once against a throwaway
/// file and removing it. Any failure answers `false`, the conservative result.
fn probe_write_zeroes(dir: &Path) -> bool {
    let path = dir.join(format!(".write-zeroes-probe.{}", std::process::id()));
    let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
    else {
        return false;
    };
    let supported = preallocate_zeroed(&file, WRITE_ZEROES_PROBE_BYTES);
    drop(file);
    let _ = std::fs::remove_file(&path);
    supported
}

fn io_error(volume_id: &str, error: &std::io::Error) -> PlacementError {
    PlacementError::VolumeIo {
        detail: error.to_string(),
        volume_id: volume_id.to_owned(),
    }
}

#[cfg(test)]
#[path = "test/volume.rs"]
mod tests;
