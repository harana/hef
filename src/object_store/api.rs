//! The object-store interface the application implements for HEF, and where HEF files live in it.
//!
//! See: hef-manifest-integration/spec.md

use super::model::{ETag, MultipartUpload, ObjectStat, PutOutcome, StoredObject, UploadedPart};
use crate::error::StorageError;
use crate::events::TenantId;

/// The application's object store (S3 or anything with the same conditional writes), as HEF needs it: read with an
/// ETag, write create-only or only-if-unchanged, upload large objects in parts, and delete.
///
/// HEF publishes its catalogue and uploads its files only through this trait and carries no store client of its own.
/// Like the engine's other interfaces it is synchronous; an implementation over an async client owns that edge
/// internally. A transport or service failure is a [`StorageError`]; a condition that did not hold is never an error,
/// it is [`PutOutcome::PreconditionFailed`].
///
/// A store without conditional writes cannot implement `put_if_absent` and `put_if_match` honestly. Such a deployment
/// needs an external commit lock serialising catalogue publication — the documented degraded mode — and must not
/// pretend the conditions hold.
///
/// See: hef-manifest-integration/spec.md
pub trait ObjectStore {
    /// Reads a whole object together with the ETag it was read at, or `None` when the key holds nothing.
    fn get(&self, key: &str) -> Result<Option<StoredObject>, StorageError>;

    /// Writes `bytes` to `key` only if nothing is there yet (`If-None-Match: *`). An existing object is left untouched
    /// and reported as `PreconditionFailed`.
    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<PutOutcome, StorageError>;

    /// Replaces the object at `key` only if its current ETag is still `etag` (`If-Match`). An object that changed, or
    /// vanished, since that ETag was read is left untouched and reported as `PreconditionFailed`.
    fn put_if_match(&self, key: &str, bytes: &[u8], etag: &ETag) -> Result<PutOutcome, StorageError>;

    /// The size and whole-object CRC-64/NVME checksum the store recorded for `key`, without downloading it, or `None`
    /// when the key holds nothing. HEF compares both against the file it built before any catalogue entry names the
    /// object, so the store must report the checksum it computed itself over the bytes it holds (S3's full-object
    /// `CRC64NVME`).
    fn stat(&self, key: &str) -> Result<Option<ObjectStat>, StorageError>;

    /// Starts a multipart upload that will create `key`.
    fn begin_multipart(&self, key: &str) -> Result<MultipartUpload, StorageError>;

    /// Uploads one part. `part_number` starts at 1 and follows file order. Returns the part's ETag for
    /// `complete_multipart`.
    fn upload_part(&self, upload: &MultipartUpload, part_number: u32, bytes: &[u8]) -> Result<ETag, StorageError>;

    /// Joins the uploaded parts, in the order given, into the object. Create-only like `put_if_absent`: when the key
    /// already holds an object, nothing is replaced and the outcome is `PreconditionFailed`.
    fn complete_multipart(&self, upload: &MultipartUpload, parts: &[UploadedPart]) -> Result<PutOutcome, StorageError>;

    /// Abandons a multipart upload and discards its parts. HEF calls it on every failed upload.
    fn abort_multipart(&self, upload: &MultipartUpload) -> Result<(), StorageError>;

    /// Removes the object at `key`. Removing a key that holds nothing succeeds, so a repeated sweep is harmless.
    fn delete(&self, key: &str) -> Result<(), StorageError>;
}

/// The object key a tenant's HEF file is stored under. Content-addressed through `file_id`, so republishing the same
/// journal range targets the same key, and the publisher, readers, and the sweeper all agree on where a catalogue
/// entry's bytes live. The application maps keys onto its own bucket and prefix.
pub fn hef_object_key(tenant_id: TenantId, file_id: u128) -> String {
    format!("hef/{tenant_id}/{file_id:032x}.hef")
}
