//! Puts a built HEF file into the application's object store and proves it landed intact, before any catalogue
//! generation may name it.
//!
//! A file small enough for one request goes up in a single create-only PUT; a larger one goes up as a multipart upload
//! whose parts are cut only where a stripe begins, so no stripe straddles two parts. Any failure aborts the multipart
//! upload. Whichever way it went up, the store's own size and CRC-64/NVME for the object must match the build before
//! the caller may publish it.
//!
//! State chunks go up here too, each in one create-only PUT under its address. A chunk already there holds the same
//! bytes, since its address is its content, so finding it there is success: that is how two states share a chunk.
//!
//! See: hef-write-path/spec.md

use super::build::BuiltHef;
use super::publish::{PublishFailure, hef_upload_segments};
use crate::artifacts::state_chunk::SealedStateChunk;
use crate::error::StateChunkError;
use crate::events::TenantId;
use crate::object_store::{MultipartUpload, ObjectStat, ObjectStore, PutOutcome, UploadedPart, state_chunk_object_key};
use std::ops::Range;

/// Uploads `built` to `key` and checks that the stored object is exactly the file that was built.
///
/// The key is content-addressed, so an object already there is an earlier attempt's upload of the same file: it is
/// checked, not uploaded again. Parts are at least `min_part_bytes` (the last may be smaller); a file that fits in one
/// part is a single create-only PUT. On any failure nothing references the object yet, so a partial multipart upload is
/// aborted and the error returned.
pub(crate) fn upload_hef(
    objects: &dyn ObjectStore,
    key: &str,
    built: &BuiltHef,
    min_part_bytes: u64,
) -> Result<(), PublishFailure> {
    if let Some(stored) = objects.stat(key).map_err(PublishFailure::Storage)? {
        return matches_build(stored, built);
    }
    let parts = upload_parts(&hef_upload_segments(built), min_part_bytes);
    if parts.len() > 1 {
        upload_multipart(objects, key, &built.bytes, &parts)?;
    } else {
        // A racing attempt that created the key first stored the same content-addressed bytes; the check below holds
        // either way.
        objects
            .put_if_absent(key, &built.bytes)
            .map_err(PublishFailure::Storage)?;
    }
    match objects.stat(key).map_err(PublishFailure::Storage)? {
        Some(stored) => matches_build(stored, built),
        None => Err(PublishFailure::Verification(
            "uploaded object is missing from the store",
        )),
    }
}

/// Stores `chunks`, a state tree's, in the order given (children before parents, as
/// [`StateTree`](crate::artifacts::state_chunk::StateTree) lists them), each with a create-only put under its
/// address. Pass the whole tree, or only the chunks its predecessor lacked
/// ([`StateTree::new_chunks`](crate::artifacts::state_chunk::StateTree::new_chunks)) when that predecessor is already
/// stored. A chunk already in the store is not written again. Returns how many chunks this call wrote.
pub fn upload_state_chunks(
    objects: &dyn ObjectStore,
    tenant_id: TenantId,
    chunks: &[SealedStateChunk],
) -> Result<usize, StateChunkError> {
    let mut written = 0;
    for chunk in chunks {
        let key = state_chunk_object_key(tenant_id, &chunk.address);
        if let PutOutcome::Written { .. } = objects.put_if_absent(&key, &chunk.bytes)? {
            written += 1;
        }
    }
    Ok(written)
}

/// Groups stripe-aligned upload segments (see [`hef_upload_segments`]) into multipart parts of at least
/// `min_part_bytes`, cutting only where a segment ends - that is, where a stripe begins. The last part may be smaller,
/// as object stores allow. The parts tile the whole file in order.
pub(crate) fn upload_parts(segments: &[u64], min_part_bytes: u64) -> Vec<Range<u64>> {
    let mut parts = Vec::new();
    let mut start = 0u64;
    let mut end = 0u64;
    for &len in segments {
        end += len;
        if end - start >= min_part_bytes {
            parts.push(start..end);
            start = end;
        }
    }
    if end > start {
        parts.push(start..end);
    }
    parts
}

fn matches_build(stored: ObjectStat, built: &BuiltHef) -> Result<(), PublishFailure> {
    if stored.size_bytes == built.bytes.len() as u64 && stored.crc64_nvme == built.file_crc64_nvme {
        Ok(())
    } else {
        Err(PublishFailure::Verification(
            "stored object does not match the built file",
        ))
    }
}

fn upload_multipart(
    objects: &dyn ObjectStore,
    key: &str,
    bytes: &[u8],
    parts: &[Range<u64>],
) -> Result<(), PublishFailure> {
    let upload = objects.begin_multipart(key).map_err(PublishFailure::Storage)?;
    let sent = send_parts(objects, &upload, bytes, parts);
    if sent.is_err() {
        // The original failure is what the caller needs; an upload this abort cannot reach is reclaimed by the store's
        // incomplete-multipart expiry.
        let _ = objects.abort_multipart(&upload);
    }
    sent
}

fn send_parts(
    objects: &dyn ObjectStore,
    upload: &MultipartUpload,
    bytes: &[u8],
    parts: &[Range<u64>],
) -> Result<(), PublishFailure> {
    let mut uploaded = Vec::with_capacity(parts.len());
    for (range, part_number) in parts.iter().zip(1u32..) {
        let part = usize::try_from(range.start)
            .ok()
            .zip(usize::try_from(range.end).ok())
            .and_then(|(start, end)| bytes.get(start..end))
            .ok_or(PublishFailure::Verification("upload part lies outside the built file"))?;
        let etag = objects
            .upload_part(upload, part_number, part)
            .map_err(PublishFailure::Storage)?;
        uploaded.push(UploadedPart { etag, part_number });
    }
    match objects
        .complete_multipart(upload, &uploaded)
        .map_err(PublishFailure::Storage)?
    {
        PutOutcome::Written { .. } => {}
        // Another attempt created the key first; drop this copy and let the caller's check judge the stored one.
        PutOutcome::PreconditionFailed => {
            let _ = objects.abort_multipart(upload);
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/upload.rs"]
mod tests;
