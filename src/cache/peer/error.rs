//! Every way a peer-cache call can fail, in one place.
//!
//! See: hef-hardware-deployment/spec.md

use thiserror::Error;

/// What went wrong while fetching object bytes from a peer.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Error)]
pub enum PeerCacheError {
    /// The peer transport failed (connection, timeout, or mTLS). The [`PeerCache`](super::api::PeerCache) treats this
    /// as a miss and falls through to the durable tier rather than failing the read.
    #[error("peer cache transport: {0}")]
    Transport(String),
}
