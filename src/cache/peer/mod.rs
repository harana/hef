//! Reads a HEF file's bytes from another node that already holds them before falling through to object storage, so
//! every node's local cache acts as one shared cache across the cluster.
//!
//! It picks the peers likely to hold a file by rendezvous hashing ([`selection`]), asks them through an injected
//! [`PeerCacheTransport`] that the embedding application implements over its own network (and
//! [`SimulatedPeerTransport`] doubles in tests), and returns bytes only after they verify against the file's content
//! hash. A miss or a bad answer falls through to durable storage and never serves unverified bytes.
//!
//! See: hef-hardware-deployment/spec.md

pub mod api;
pub mod constant;
pub mod error;
pub mod model;
pub mod selection;
pub mod sim;

pub use api::{PeerCache, PeerCacheTransport};
pub use error::*;
pub use model::{
    CacheRangeRequest, CacheRangeResponse, FillTransport, InlineEncoding, ObjectRange, PeerId, Residency,
    TransportCapabilities,
};
pub use selection::{negotiate_fill_transport, select_fill_transport, select_inline_encoding};
pub use sim::{PeerBehaviour, SimulatedPeerTransport};
