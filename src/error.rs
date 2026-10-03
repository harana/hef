//! The error types the storage engine returns, kept separate so a caller can always tell corrupt data apart from
//! infrastructure failure.
//!
//! Decoders of untrusted or on-disk bytes never panic: every malformed input maps to a `FormatError` (refusal).
//! Faults from the storage backend and from publishing map to their own types instead, so "the bytes are wrong" and
//! "the bytes never arrived" are never confused.

use crate::file::error::{CodecError, FileError};
use thiserror::Error;

/// Something was wrong with the bytes themselves while reading one of the on-disk formats. The variants name the rule
/// that failed, not offsets into caller memory, so they are safe to log and assert on.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum FormatError {
    #[error("bad magic, expected {expected}")]
    BadMagic { expected: &'static str },
    #[error("blake3 mismatch over {scope}")]
    Blake3Mismatch { scope: &'static str },
    #[error("header crc64 mismatch")]
    HeaderCrcMismatch,
    #[error("invalid utf-8 in {what}")]
    InvalidUtf8 { what: &'static str },
    #[error("reference out of range: {what}")]
    RefOutOfRange { what: &'static str },
    #[error("reserved field not zero: {field}")]
    ReservedNotZero { field: &'static str },
    #[error("structural rule violated: {rule}")]
    Structural { rule: &'static str },
    #[error("truncated input while reading {what}")]
    Truncated { what: &'static str },
    #[error("unknown required feature flags {bits:#x}: refusing")]
    UnknownRequiredFeature { bits: u64 },
    #[error("unsupported version {found} in {field}")]
    UnsupportedVersion { field: &'static str, found: u32 },
}

impl From<CodecError> for FormatError {
    /// The shared byte codec reports the same refusing faults this engine already names, so a codec error maps
    /// straight onto its format error.
    fn from(error: CodecError) -> Self {
        match error {
            CodecError::BadMagic { expected } => FormatError::BadMagic { expected },
            CodecError::Blake3Mismatch { scope } => FormatError::Blake3Mismatch { scope },
            CodecError::HeaderCrcMismatch => FormatError::HeaderCrcMismatch,
            CodecError::Structural { rule } => FormatError::Structural { rule },
            CodecError::Truncated { what } => FormatError::Truncated { what },
        }
    }
}

/// Why an introspection query was refused.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum IntrospectionError {
    #[error("unauthorized")]
    Unauthorized,
}

/// Why a signed event failed to re-verify from its stored form. Every variant means the same thing to a caller: the
/// event's authorship cannot be proven, so it must not be written and must not be presented as authentic.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum ProvenanceError {
    #[error("recomputed protocol event id does not match the stored one")]
    EventIdMismatch,
    #[error("hex field is not lowercase hex of the expected length")]
    MalformedHex,
    #[error("author public key is not a valid signing key")]
    MalformedKey,
    #[error("signature bytes are not a valid signature")]
    MalformedSignature,
    #[error("payload does not carry `{field}` in the shape the protocol serializes")]
    PayloadShape { field: &'static str },
    #[error("signature does not verify against the author public key")]
    SignatureRejected,
}

/// Why a declared event relationship was refused before storage. Only the declaration's own shape is checked — its
/// kind and space tags, its multiplicity, and its target width. Whether the target exists is never checked anywhere:
/// a dangling reference is data, not an error.
#[derive(Debug, Clone, Copy, Error, PartialEq, Eq)]
pub enum RelationshipError {
    #[error("stored relationship column value is not `<space>:<lowercase hex>` of the space's width")]
    MalformedColumnValue,
    #[error("an event may declare at most one parent reference")]
    MoreThanOneParent,
    #[error("an event may declare at most one root reference")]
    MoreThanOneRoot,
    #[error("target identifier space tag is not in the registry")]
    UnknownSpace,
    #[error("target length does not match its identifier space")]
    WrongTargetLength,
}

/// A storage-interface failure (I/O error, injected fault, crash point). Distinct from `FormatError`: bytes that
/// arrived intact but violate the format are format errors; bytes that never arrived are storage errors.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum StorageError {
    #[error("injected fault: {kind}")]
    InjectedFault { kind: &'static str },
    #[error("storage i/o failure during {op}: {detail}")]
    Io { op: &'static str, detail: String },
    #[error("read beyond written extent")]
    OutOfBounds,
    #[error("append not 4096-byte aligned")]
    Unaligned,
    #[error("unknown journal shard")]
    UnknownShard,
}

impl From<FileError> for StorageError {
    /// Maps a shared file-layer failure onto the journal's storage-error taxonomy. Block storage only ever surfaces the
    /// alignment, bounds, unknown-target, injected-fault, and I/O cases; the remaining whole-file variants cannot arise
    /// on an append target and collapse to an I/O error.
    fn from(error: FileError) -> Self {
        match error {
            FileError::InjectedFault { kind } => StorageError::InjectedFault { kind },
            FileError::Io { detail, op } => StorageError::Io { op, detail },
            FileError::OutOfBounds | FileError::InvalidRange { .. } => StorageError::OutOfBounds,
            FileError::Unaligned => StorageError::Unaligned,
            FileError::UnknownTarget => StorageError::UnknownShard,
            other => StorageError::Io {
                op: "journal",
                detail: other.to_string(),
            },
        }
    }
}

/// Something went wrong while publishing a file into the shared set of files queries can see. Most variants describe a
/// lost race for the published head, which the publisher recovers from by rebasing and retrying.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PublishError {
    /// If-Match CAS lost: the head moved. Carries the current head so the loser can rebase and retry; it must never
    /// overwrite.
    #[error("cas lost; current head generation {current_generation}")]
    CasLost { current_generation: u64 },
    #[error("generation object already exists")]
    GenerationExists,
    #[error("publish backend failure: {detail}")]
    Io { detail: String },
    #[error("expected generation unknown")]
    UnknownGeneration,
    #[error("publish verification failed: {rule}")]
    VerificationFailed { rule: &'static str },
}
