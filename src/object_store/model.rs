//! The values that cross the object-store interface, and the form a catalogue generation takes once it is stored.
//!
//! See: hef-manifest-integration/spec.md

use crate::lifecycle::{FooterMirrorObject, HefFileEntry, IndexArtifactRef, ManifestGeneration, Retirement};
use hashbrown::{HashMap, HashSet};
use std::hash::Hash;

/// The version tag a store reports for an object (an S3 ETag). Opaque: only ever compared for equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ETag(pub String);

/// One multipart upload in progress: the key it will create and the store's id for the upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultipartUpload {
    pub key: String,
    pub upload_id: String,
}

/// The size and checksum a store recorded for an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectStat {
    /// Whole-object CRC-64/NVME the store computed over the bytes it holds.
    pub crc64_nvme: u64,
    pub size_bytes: u64,
}

/// What a conditional write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutOutcome {
    /// The condition did not hold - the key was already taken for a create-only write, or the ETag had moved for an
    /// If-Match write - so nothing was written.
    PreconditionFailed,
    /// The object was written and now carries `etag`.
    Written { etag: ETag },
}

/// An object's bytes together with the ETag they were read at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub bytes: Vec<u8>,
    pub etag: ETag,
}

/// One part of a multipart upload the store accepted, as `complete_multipart` needs it back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadedPart {
    pub etag: ETag,
    /// Starts at 1, in file order.
    pub part_number: u32,
}

/// One catalogue generation as written to the object store.
///
/// Every `CHECKPOINT_INTERVAL`th generation is a full checkpoint. Every other generation stores only how it differs
/// from the checkpoint before it, so a generation object's size follows recent churn rather than the total number of
/// files, and a reader rebuilds any generation from one checkpoint plus one delta.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum StoredGeneration {
    Checkpoint(ManifestGeneration),
    Delta(GenerationDelta),
}

/// How one generation differs from the checkpoint it names. Applying it to that checkpoint gives back the generation's
/// full catalogue: the checkpoint's surviving entries in their order, changed ones replaced where they stood, and new
/// ones after them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct GenerationDelta {
    /// Index artifacts not present in the checkpoint.
    pub(crate) added_artifacts: Vec<IndexArtifactRef>,
    pub(crate) checkpoint: u64,
    pub(crate) footer_mirror: Option<FooterMirrorObject>,
    pub(crate) generation: u64,
    pub(crate) removed_artifact_keys: Vec<String>,
    /// Files the checkpoint lists that this generation no longer does (swept away).
    pub(crate) removed_file_ids: Vec<u128>,
    /// Stored whole: it lists only files still awaiting deletion, so it stays small however many files are live.
    pub(crate) retirements: Vec<Retirement>,
    /// Entries that are new since the checkpoint or whose contents (such as their state) changed since it.
    pub(crate) upserted_files: Vec<HefFileEntry>,
}

impl GenerationDelta {
    /// The changes that turn `checkpoint` into `next`.
    pub(crate) fn between(checkpoint: &ManifestGeneration, next: &ManifestGeneration) -> Self {
        let (upserted_files, removed_file_ids) = diff(&checkpoint.files, &next.files, |entry| entry.file_id);
        let (added_artifacts, removed_artifact_keys) =
            diff(&checkpoint.index_artifacts, &next.index_artifacts, |artifact| {
                artifact.object_key.clone()
            });
        Self {
            added_artifacts,
            checkpoint: checkpoint.generation,
            footer_mirror: next.footer_mirror.clone(),
            generation: next.generation,
            removed_artifact_keys,
            removed_file_ids,
            retirements: next.retirements.clone(),
            upserted_files,
        }
    }

    /// The full generation this delta describes, rebuilt on top of its `checkpoint`.
    pub(crate) fn apply(self, checkpoint: ManifestGeneration) -> ManifestGeneration {
        ManifestGeneration {
            files: apply(checkpoint.files, self.upserted_files, &self.removed_file_ids, |entry| {
                entry.file_id
            }),
            footer_mirror: self.footer_mirror,
            generation: self.generation,
            index_artifacts: apply(
                checkpoint.index_artifacts,
                self.added_artifacts,
                &self.removed_artifact_keys,
                |artifact| artifact.object_key.clone(),
            ),
            retirements: self.retirements,
        }
    }
}

/// The items of `next` that are new or changed relative to `base`, and the keys of the `base` items `next` dropped.
fn diff<T: Clone + PartialEq, K: Eq + Hash>(base: &[T], next: &[T], key: impl Fn(&T) -> K) -> (Vec<T>, Vec<K>) {
    let base_by_key: HashMap<K, &T> = base.iter().map(|item| (key(item), item)).collect();
    let next_keys: HashSet<K> = next.iter().map(&key).collect();
    let changed = next
        .iter()
        .filter(|&item| base_by_key.get(&key(item)) != Some(&item))
        .cloned()
        .collect();
    let removed = base
        .iter()
        .map(&key)
        .filter(|base_key| !next_keys.contains(base_key))
        .collect();
    (changed, removed)
}

/// `base` without the `removed` keys, each upserted item replacing the base item with its key in place, and the
/// upserted items with no base counterpart appended in their own order.
fn apply<T, K: Eq + Hash>(base: Vec<T>, upserted: Vec<T>, removed: &[K], key: impl Fn(&T) -> K) -> Vec<T> {
    let removed: HashSet<&K> = removed.iter().collect();
    let position: HashMap<K, usize> = upserted
        .iter()
        .enumerate()
        .map(|(index, item)| (key(item), index))
        .collect();
    let mut upserted: Vec<Option<T>> = upserted.into_iter().map(Some).collect();
    let mut result = Vec::with_capacity(base.len() + upserted.len());
    for item in base {
        let item_key = key(&item);
        if removed.contains(&item_key) {
            continue;
        }
        let replacement = position
            .get(&item_key)
            .and_then(|&index| upserted.get_mut(index))
            .and_then(Option::take);
        result.push(replacement.unwrap_or(item));
    }
    result.extend(upserted.into_iter().flatten());
    result
}

#[cfg(test)]
#[path = "test/model.rs"]
mod tests;
