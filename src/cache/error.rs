//! Every way a cache call can fail, in one place.
//!
//! See: hef-hardware-deployment/spec.md

use thiserror::Error;

/// Why local disks could not be configured, or why one block's copies could not be placed on them.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PlacementError {
    /// Two configured volumes share an id or a root path.
    #[error("duplicate volume id or root: {id}")]
    DuplicateVolume { id: String },
    /// `mirror`/`stripe` configured with fewer than two distinct volumes. Refused at startup, never discovered
    /// mid-write.
    #[error("{layout} layout requires at least two distinct volumes, found {found}")]
    InsufficientVolumes { found: usize, layout: &'static str },
    /// A `mirror` write achieved fewer copies than its required count.
    #[error("mirror achieved {achieved} of {required} write copies")]
    MirrorCopiesNotMet { achieved: usize, required: usize },
    #[error("no local volumes configured")]
    NoVolumes,
    /// A stripe chunk could not be written, so the block is incomplete.
    #[error("stripe chunk {stripe_index} failed to write")]
    StripeWriteFailed { stripe_index: u32 },
    /// A volume read or write failed.
    #[error("volume {volume_id} I/O failed: {detail}")]
    VolumeIo { detail: String, volume_id: String },
}

/// Why a page-assembled range read could not be served.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum PageReadError<E> {
    /// Reading a missing page from durable storage failed; the error is the fetcher's own.
    #[error("durable read failed")]
    Fetch(E),
    /// The requested offset is past the end of the file.
    #[error("offset {offset} is past the end of a {size}-byte file")]
    InvalidRange { offset: u64, size: u64 },
}
