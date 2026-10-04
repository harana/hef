//! The in-memory object store tests drive: real conditional-write and multipart semantics, plus failures scripted on
//! command, all through the same [`ObjectStore`] interface production code uses.

use super::api::ObjectStore;
use super::model::{ETag, MultipartUpload, ObjectStat, PutOutcome, StoredObject, UploadedPart};
use crate::error::StorageError;
use crate::file::integrity::crc64_nvme;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// A scripted object-store failure, consumed in injection order by the next operation it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectFault {
    /// The next `complete_multipart` fails without creating the object.
    FailCompleteMultipart,
    /// The next `put_if_absent` fails without writing.
    FailPutIfAbsent,
    /// The next `upload_part` fails without storing the part.
    FailUploadPart,
}

/// In-memory object store with S3's conditional-write rules: create-only and If-Match writes, a fresh ETag on every
/// write, and create-only multipart completion.
#[derive(Debug, Default)]
pub struct SimObjectStore {
    state: Mutex<SimState>,
}

#[derive(Debug, Default)]
struct SimState {
    faults: Vec<ObjectFault>,
    next_tag: u64,
    objects: BTreeMap<String, StoredObject>,
    /// Open multipart uploads by upload id: the key each will create and the parts received so far.
    uploads: BTreeMap<String, (String, BTreeMap<u32, Vec<u8>>)>,
}

impl SimState {
    fn fresh_tag(&mut self) -> ETag {
        self.next_tag += 1;
        ETag(format!("etag-{}", self.next_tag))
    }

    /// Consumes the first scripted `fault`, reporting whether there was one.
    fn take_fault(&mut self, fault: ObjectFault) -> bool {
        match self.faults.iter().position(|scripted| *scripted == fault) {
            Some(index) => {
                self.faults.remove(index);
                true
            }
            None => false,
        }
    }

    fn write(&mut self, key: &str, bytes: Vec<u8>) -> PutOutcome {
        let etag = self.fresh_tag();
        self.objects.insert(
            key.to_owned(),
            StoredObject {
                bytes,
                etag: etag.clone(),
            },
        );
        PutOutcome::Written { etag }
    }
}

impl SimObjectStore {
    /// An empty store with no failures scripted.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripts the next failure. Failures are consumed in order by the matching operation.
    pub fn inject(&self, fault: ObjectFault) {
        self.state().faults.push(fault);
    }

    /// The bytes stored at `key`, if any.
    pub fn object(&self, key: &str) -> Option<Vec<u8>> {
        self.state().objects.get(key).map(|object| object.bytes.clone())
    }

    /// Every key that holds an object, in sorted order.
    pub fn keys(&self) -> Vec<String> {
        self.state().objects.keys().cloned().collect()
    }

    /// How many multipart uploads were begun but neither completed nor aborted.
    pub fn open_uploads(&self) -> usize {
        self.state().uploads.len()
    }

    fn state(&self) -> MutexGuard<'_, SimState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn injected(kind: &'static str) -> StorageError {
    StorageError::InjectedFault { kind }
}

impl ObjectStore for SimObjectStore {
    fn get(&self, key: &str) -> Result<Option<StoredObject>, StorageError> {
        Ok(self.state().objects.get(key).cloned())
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, StorageError> {
        let mut state = self.state();
        if state.take_fault(ObjectFault::FailPutIfAbsent) {
            return Err(injected("put_if_absent"));
        }
        if state.objects.contains_key(key) {
            return Ok(PutOutcome::PreconditionFailed);
        }
        Ok(state.write(key, bytes.to_vec()))
    }

    fn put_if_match(&self, key: &str, bytes: &[u8], etag: &ETag) -> Result<PutOutcome, StorageError> {
        let mut state = self.state();
        if state.objects.get(key).map(|object| &object.etag) != Some(etag) {
            return Ok(PutOutcome::PreconditionFailed);
        }
        Ok(state.write(key, bytes.to_vec()))
    }

    fn stat(&self, key: &str) -> Result<Option<ObjectStat>, StorageError> {
        Ok(self.state().objects.get(key).map(|object| ObjectStat {
            crc64_nvme: crc64_nvme(&object.bytes),
            size_bytes: object.bytes.len() as u64,
        }))
    }

    fn begin_multipart(&self, key: &str) -> Result<MultipartUpload, StorageError> {
        let mut state = self.state();
        let ETag(upload_id) = state.fresh_tag();
        state
            .uploads
            .insert(upload_id.clone(), (key.to_owned(), BTreeMap::new()));
        Ok(MultipartUpload {
            key: key.to_owned(),
            upload_id,
        })
    }

    fn upload_part(&self, upload: &MultipartUpload, part_number: u32, bytes: &[u8]) -> Result<ETag, StorageError> {
        let mut state = self.state();
        if state.take_fault(ObjectFault::FailUploadPart) {
            return Err(injected("upload_part"));
        }
        let etag = state.fresh_tag();
        let (_, parts) = state.uploads.get_mut(&upload.upload_id).ok_or(StorageError::Io {
            detail: "unknown multipart upload".to_owned(),
            op: "upload_part",
        })?;
        parts.insert(part_number, bytes.to_vec());
        Ok(etag)
    }

    fn complete_multipart(&self, upload: &MultipartUpload, parts: &[UploadedPart]) -> Result<PutOutcome, StorageError> {
        let mut state = self.state();
        if state.take_fault(ObjectFault::FailCompleteMultipart) {
            return Err(injected("complete_multipart"));
        }
        let unknown = || StorageError::Io {
            detail: "unknown multipart upload or part".to_owned(),
            op: "complete_multipart",
        };
        let (key, received) = state.uploads.get(&upload.upload_id).ok_or_else(unknown)?;
        if state.objects.contains_key(key) {
            return Ok(PutOutcome::PreconditionFailed);
        }
        let mut bytes = Vec::new();
        for part in parts {
            bytes.extend_from_slice(received.get(&part.part_number).ok_or_else(unknown)?);
        }
        let key = key.clone();
        state.uploads.remove(&upload.upload_id);
        Ok(state.write(&key, bytes))
    }

    fn abort_multipart(&self, upload: &MultipartUpload) -> Result<(), StorageError> {
        self.state().uploads.remove(&upload.upload_id);
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.state().objects.remove(key);
        Ok(())
    }
}
