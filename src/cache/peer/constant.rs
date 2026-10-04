//! Fixed values shared by the peer cache.
//!
//! See: hef-hardware-deployment/spec.md

/// Byte length at or above which the cost of one-sided RDMA or NVMe-oF setup is worth paying. Ranges below this always
/// take the two-sided inline path.
pub const ONE_SIDED_MIN_BYTES: u64 = 65_536;

/// Byte length at or above which a holder compresses an inline fill when both sides support it. Below this the DEFLATE
/// overhead and the extra CPU outweigh the bandwidth saved.
pub const COMPRESSED_INLINE_MIN_BYTES: u64 = 4_096;

/// How long [`super::api::PeerCache::fetch`] waits on its preferred peers together before giving up and falling
/// through to the durable tier. Every preferred holder shares this one deadline rather than getting its own full wait,
/// so trying more replicas never multiplies how long a read can be held open by unreachable or silent peers.
pub const PEER_FILL_DEADLINE: std::time::Duration = std::time::Duration::from_millis(250);

/// Hard ceiling on the bound a requester accepts for the decoded length of a compressed inline fill. A declared
/// decoded length past the bound is refused before any decompression starts, so a bad or hostile peer cannot make a
/// requester allocate more than this from a single fill; the fill is skipped and the read falls through exactly as
/// for any other unusable reply.
///
/// It is set to the peer transport's maximum frame length (16 MiB) because a plain inline fill carries one whole
/// object in a single peer frame, so any object that could have been sent uncompressed decodes to at most this.
/// [`MAX_INLINE_DEFLATE_RATIO`] can only pull the accepted length *below* this ceiling, for a declared length that is
/// implausible given the compressed bytes actually sent; it never raises the bound past this hard cap, because the
/// decompressor allocates its destination from the declared length before decompression has verified anything.
///
/// This crate never decodes a compressed fill itself — [`super::model::CacheRangeResponse::bytes`] is already-decoded
/// content by the time it reaches [`super::api::PeerCache`]. Decoding, and enforcing this bound before it, is the
/// responsibility of whichever [`super::api::PeerCacheTransport`] implementer actually receives the compressed wire
/// bytes, which lives in the embedding application.
pub const DECOMPRESSED_FILL_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// How far past the compressed bytes actually received a declared decoded length may go before the fill is refused.
/// DEFLATE cannot expand a stream by more than 1032:1, so an honest reply always sits inside this; a hostile peer that
/// declares a large decoded length for a small compressed frame is caught here, tightening the bound below
/// [`DECOMPRESSED_FILL_MAX_BYTES`] rather than ever raising it past that hard cap.
pub const MAX_INLINE_DEFLATE_RATIO: u64 = 1_032;
