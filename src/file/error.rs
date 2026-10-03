//! The two kinds of failure the shared file layer can return, kept apart so a caller can always tell corrupt bytes from
//! a storage failure.
//!
//! A decoder handed truncated or hostile bytes returns a [`CodecError`] (refuse, never a panic); a backend that
//! could not read, write, or make bytes durable returns a [`FileError`]. The HEF store and the object store convert
//! these into their own error taxonomies at their boundaries, so "the bytes are wrong" and "the bytes never arrived"
//! stay distinct everywhere.

use thiserror::Error;

/// Something was wrong with the bytes themselves while reading one of the on-disk formats. The variants name the rule
/// that failed, not offsets into caller memory, so they are safe to log and assert on.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CodecError {
    /// Magic bytes did not match the expected structure tag.
    #[error("bad magic, expected {expected}")]
    BadMagic { expected: &'static str },
    /// The authoritative BLAKE3 check failed.
    #[error("blake3 mismatch over {scope}")]
    Blake3Mismatch { scope: &'static str },
    /// The header CRC-64/NVME precheck failed.
    #[error("header crc64 mismatch")]
    HeaderCrcMismatch,
    /// A length, offset, count, or alignment rule was violated.
    #[error("structural rule violated: {rule}")]
    Structural { rule: &'static str },
    /// The input ended before the structure did.
    #[error("truncated input while reading {what}")]
    Truncated { what: &'static str },
}

/// A storage failure from a file backend: the bytes could not be read, written, or made durable. Distinct from
/// [`CodecError`]: bytes that arrived intact but violate a format are codec errors; bytes that never arrived (or a
/// checksum that proves the wrong bytes arrived) are file errors.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum FileError {
    /// A create-only write lost to existing, differing content at the target.
    #[error("{what} already exists with differing content")]
    AlreadyExists { what: &'static str },
    /// The bytes did not match the authoritative BLAKE3 checksum.
    #[error("bytes did not match the expected blake3 checksum")]
    ChecksumMismatch,
    /// An injected fault consumed this operation (simulation only).
    #[error("injected fault: {kind}")]
    InjectedFault { kind: &'static str },
    /// A byte range started past the end of the file.
    #[error("range offset {offset} past end {size}")]
    InvalidRange { offset: u64, size: u64 },
    /// The backend reported a failure for this operation.
    #[error("file i/o failure during {op}: {detail}")]
    Io { detail: String, op: &'static str },
    /// Nothing exists at the requested target.
    #[error("{what} not found")]
    NotFound { what: &'static str },
    /// A block read escaped the target's written extent.
    #[error("read beyond written extent")]
    OutOfBounds,
    /// A deployment that requires the NVMe fast path could not confirm the device or the permission to query it at
    /// startup. The `detail` names the missing device node so startup refuses instead of silently using the
    /// buffered fallback.
    #[error("required NVMe device unavailable: {detail}")]
    RequiredDeviceMissing { detail: String },
    /// A block frame violated the 4096-byte alignment contract for appends.
    #[error("append not 4096-byte aligned")]
    Unaligned,
    /// The append target is unknown to this backend.
    #[error("unknown append target")]
    UnknownTarget,
}

impl From<CodecError> for FileError {
    /// A codec fault surfaced from inside a file operation: the bytes that arrived were malformed, so the operation
    /// could not complete.
    fn from(error: CodecError) -> Self {
        match error {
            CodecError::Blake3Mismatch { .. } | CodecError::HeaderCrcMismatch => FileError::ChecksumMismatch,
            CodecError::BadMagic { expected } => FileError::Io {
                op: "decode",
                detail: format!("bad magic, expected {expected}"),
            },
            CodecError::Structural { rule } => FileError::Io {
                op: "decode",
                detail: rule.to_owned(),
            },
            CodecError::Truncated { what } => FileError::Io {
                op: "decode",
                detail: format!("truncated input while reading {what}"),
            },
        }
    }
}
