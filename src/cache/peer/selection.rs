//! Picking which peers should hold an object's cached bytes, by rendezvous (highest-random-weight) hashing — so every
//! node agrees on the same preferred holders without a coordinator.
//!
//! See: hef-hardware-deployment/spec.md

use hashbrown::HashMap;

use super::constant::{COMPRESSED_INLINE_MIN_BYTES, ONE_SIDED_MIN_BYTES};
use super::model::{FillTransport, InlineEncoding, PeerId, Residency, TransportCapabilities};

/// Picks the road a holder should use to serve a range of `range_len` bytes it keeps at `residency`. Small ranges
/// always go two-sided ([`FillTransport::Inline`]) because a one-sided fill's setup cost isn't worth it; a large range
/// takes the one-sided road its residency allows — [`FillTransport::RdmaRead`] out of registered RAM,
/// [`FillTransport::NvmeOverFabrics`] off the NVMe cache tier. The choice never changes which bytes are served, only
/// how fast they arrive, so a caller may ignore it.
pub fn select_fill_transport(range_len: u64, residency: Residency) -> FillTransport {
    if range_len < ONE_SIDED_MIN_BYTES {
        return FillTransport::Inline;
    }
    match residency {
        Residency::Nvme => FillTransport::NvmeOverFabrics,
        Residency::RegisteredRam => FillTransport::RdmaRead,
    }
}

/// Picks the road a fill takes once both endpoints' capabilities are on the table: the size-and-residency choice of
/// [`select_fill_transport`], narrowed to what the `holder` can serve and the `requester` can redeem. A one-sided road
/// needs both sides — a holder that cannot register memory, or a requester with no NVMe-over-Fabrics initiator, drops
/// the fill back to the always-available inline path rather than failing it. A performance choice only: the bytes
/// served are identical whichever road wins.
pub fn negotiate_fill_transport(
    range_len: u64,
    residency: Residency,
    holder: TransportCapabilities,
    requester: TransportCapabilities,
) -> FillTransport {
    match select_fill_transport(range_len, residency) {
        FillTransport::NvmeOverFabrics if holder.nvme_over_fabrics && requester.nvme_over_fabrics => {
            FillTransport::NvmeOverFabrics
        }
        FillTransport::RdmaRead if holder.rdma_read && requester.rdma_read => FillTransport::RdmaRead,
        _ => FillTransport::Inline,
    }
}

/// Picks how an inline fill's bytes are encoded on the wire: DEFLATE when both sides can handle it and the object is
/// big enough to be worth compressing ([`super::constant::COMPRESSED_INLINE_MIN_BYTES`]), identity otherwise. Like the
/// transport choice this never changes the bytes served — the requester decodes before verifying — only how many cross
/// the fabric.
pub fn select_inline_encoding(
    content_len: u64,
    holder: TransportCapabilities,
    requester: TransportCapabilities,
) -> InlineEncoding {
    if content_len >= COMPRESSED_INLINE_MIN_BYTES && holder.compressed_inline && requester.compressed_inline {
        InlineEncoding::Deflate
    } else {
        InlineEncoding::Identity
    }
}

/// Ranks `members` for one object (keyed by its content hash) and returns the top `count` preferred cache holders.
/// Every node computes the same ranking from the same inputs, so reads converge on a small, stable set of holders; when
/// a peer leaves, only the objects it ranked first re-home to another node.
pub fn preferred_peers(members: &[PeerId], key: &[u8; blake3::OUT_LEN], count: usize) -> Vec<PeerId> {
    let mut weighted: Vec<(u64, PeerId)> = members.iter().map(|peer| (weight(*peer, key), *peer)).collect();
    // Highest weight first; ties broken by ascending peer id so the order is total and identical on every node.
    weighted.sort_unstable_by(|left, right| right.0.cmp(&left.0).then(left.1.0.cmp(&right.1.0)));
    weighted.into_iter().take(count).map(|(_, peer)| peer).collect()
}

/// Ranks `members` like [`preferred_peers`] and then drops any peer `capacity` reports has fewer than
/// `min_free_bytes` free cache bytes — a full or pressured node (per the free capacity it last reported) is never
/// picked to hold a fresh copy. A peer missing from `capacity` (no gossip received yet) is treated as available, so
/// selection stays capacity-blind until the peer has actually reported.
///
/// The pressured peer is dropped from the top-`count` holders rather than replaced by the next-ranked node, because a
/// read looks for an object only on its capacity-blind top-`count` holders ([`preferred_peers`]). A replacement holder
/// would be a peer no later read ever asks, so the copy would be paid for and never found; placing on fewer peers
/// instead leaves those reads exactly where they would have been.
pub fn preferred_peers_with_capacity(
    members: &[PeerId],
    key: &[u8; blake3::OUT_LEN],
    count: usize,
    capacity: &HashMap<PeerId, u64>,
    min_free_bytes: u64,
) -> Vec<PeerId> {
    preferred_peers(members, key, count)
        .into_iter()
        .filter(|peer| capacity.get(peer).is_none_or(|&free| free >= min_free_bytes))
        .collect()
}

/// The rendezvous weight of `peer` for one object `key`: BLAKE3 over the peer id and the key, read as a big-endian
/// `u64`. Deterministic across nodes.
fn weight(peer: PeerId, key: &[u8; blake3::OUT_LEN]) -> u64 {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&peer.0.to_be_bytes());
    hasher.update(key);
    let [a, b, c, d, e, f, g, h, ..] = *hasher.finalize().as_bytes();
    u64::from_be_bytes([a, b, c, d, e, f, g, h])
}

#[cfg(test)]
#[path = "test/selection.rs"]
mod tests;
