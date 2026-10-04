//! The handful of types a caller passes to the peer cache: which node, which object, and which byte range.
//!
//! See: hef-hardware-deployment/spec.md

use crate::events::TenantId;

/// A byte range within an object: where it starts and how long it is.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectRange {
    pub length: u64,
    pub offset: u64,
}

/// One product node in the cluster, identified by its peer-service `node_id` (a `u16`, globally unique among running
/// nodes).
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerId(pub u16);

/// Which fill paths an endpoint can actually drive, negotiated per request so a holder never offers a transport the
/// requester cannot redeem — and never wastes a grant on one it cannot serve itself. The two-sided inline path needs no
/// capability (it is always available), so the all-`false` default means "inline only" and is always safe.
///
/// A requester advertises its capabilities in [`CacheRangeRequest::accepts`]; a holder intersects them with its own in
/// [`super::selection::negotiate_fill_transport`]. Capabilities come from startup probes — a fabric endpoint that can
/// issue one-sided reads, a working NVMe-over-Fabrics initiator, a usable decompressor — never from configuration
/// guesses, so advertising nothing costs only performance, never correctness.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TransportCapabilities {
    /// Holder side: can produce DEFLATE-compressed inline fills. Requester side: can decode them.
    pub compressed_inline: bool,
    /// Holder side: can export the range for an NVMe-over-Fabrics read. Requester side: has an initiator to redeem one.
    pub nvme_over_fabrics: bool,
    /// Holder side: can register memory for a one-sided RDMA read. Requester side: can issue one.
    pub rdma_read: bool,
}

impl TransportCapabilities {
    /// The floor every node can always take: two-sided inline fills, nothing one-sided, no compression.
    pub const fn inline_only() -> Self {
        Self {
            compressed_inline: false,
            nvme_over_fabrics: false,
            rdma_read: false,
        }
    }

    /// Every path enabled — the posture of the in-memory doubles, and of a live node whose startup probes all passed.
    pub const fn all() -> Self {
        Self {
            compressed_inline: true,
            nvme_over_fabrics: true,
            rdma_read: true,
        }
    }
}

/// How an inline fill's bytes are encoded in the response frame. The holder chooses DEFLATE only when both sides
/// advertised support ([`TransportCapabilities::compressed_inline`]) and the object is big enough to be worth it
/// ([`super::selection::select_inline_encoding`]); the requester decodes before the usual BLAKE3 verification, so the
/// encoding never changes what is served, only how many bytes cross the wire.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InlineEncoding {
    /// DEFLATE-compressed (RFC 1951), produced by the platform's QAT-backed compressor or its software fallback —
    /// byte-identical bytes after decoding either way.
    Deflate,
    /// The bytes exactly as held.
    #[default]
    Identity,
}

/// A tenant-scoped, content-addressed request for one immutable object's bytes from a peer's cache. The object is
/// located by `file_id` and identified — and later verified — by its root `file_blake3`.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRangeRequest {
    /// The fill paths this requester can redeem; a holder never replies with a transport or encoding outside this set.
    /// [`PeerCache`](super::api::PeerCache) stamps it from its own configuration before the request leaves the node.
    pub accepts: TransportCapabilities,
    pub file_blake3: [u8; blake3::OUT_LEN],
    pub file_id: u128,
    pub range: ObjectRange,
    pub tenant_id: TenantId,
}

/// The bytes a peer returned for a [`CacheRangeRequest`], before the caller verifies them against the object's
/// `file_blake3`.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRangeResponse {
    /// Already-decoded content: a compressed wire fill is decoded by the [`super::api::PeerCacheTransport`]
    /// implementer that received it, enforcing [`super::constant::DECOMPRESSED_FILL_MAX_BYTES`], before it ever
    /// reaches this type.
    pub bytes: Vec<u8>,
    pub transport: FillTransport,
    /// Outboard BLAKE3 hash tree for the object, when the peer returned it alongside the bytes for range-scoped
    /// verification. `None` for whole-object fills that use the top-level `file_blake3` directly.
    pub tree: Option<Vec<u8>>,
}

/// How the peer moved the bytes: two-sided inline copy, one-sided RDMA read out of registered RAM, or one-sided NVMe-oF
/// read off the peer's NVMe cache tier.
///
/// The choice is made by the holder ([`super::selection::select_fill_transport`]) based on the range length and how it
/// holds the bytes. A caller that only cares about the bytes can ignore this field; it exists so the caller can record
/// or observe which transport was used.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FillTransport {
    /// Two-sided copy: the holder reads its bytes and sends them over the fabric. Always available; the default for
    /// small ranges.
    #[default]
    Inline,
    /// One-sided NVMe-oF read: the requester pulls the bytes directly off the holder's NVMe namespace without the
    /// holder's CPU in the data path.
    NvmeOverFabrics,
    /// One-sided RDMA read: the requester pulls the bytes directly out of the holder's registered memory region without
    /// the holder's CPU.
    RdmaRead,
}

/// Where the holder keeps the bytes it is about to fill.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Residency {
    /// Bytes are on the holder's local NVMe device.
    Nvme,
    /// Bytes are in the holder's RDMA-registered memory.
    RegisteredRam,
}

#[cfg(test)]
#[path = "test/model.rs"]
mod tests;
