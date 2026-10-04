//! The front door other components call: a [`PeerCache`] that finds an object's bytes on a peer node and returns the
//! requested byte range only after verifying it against the object's content hash.
//!
//! See: hef-hardware-deployment/spec.md

use super::constant::PEER_FILL_DEADLINE;
use super::error::PeerCacheError;
use super::model::{CacheRangeRequest, CacheRangeResponse, ObjectRange, PeerId, TransportCapabilities};
use super::selection;
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::file::integrity::{RangeFault, verify_range};

/// The interface the distributed cache uses to ask other nodes for object bytes this node does not hold locally.
///
/// HEF defines this interface only; the embedding application supplies the real implementation over its own peer
/// network, and [`SimulatedPeerTransport`](super::sim::SimulatedPeerTransport) stands in for tests, so HEF never
/// depends on a network stack.
///
/// See: hef-hardware-deployment/spec.md
pub trait PeerCacheTransport: Send + Sync {
    /// Asks `peers`, in order, for the object in `request`, returning the first peer's bytes (still unverified) or
    /// `None` if no peer holds it. A transport failure is an `Err`; the [`PeerCache`] treats it as a miss.
    fn fetch_range(
        &self,
        peers: &[PeerId],
        request: &CacheRangeRequest,
    ) -> Result<Option<CacheRangeResponse>, PeerCacheError>;

    /// Like [`fetch_range`](Self::fetch_range) but gives up after `budget`, so a read that has already spent most of
    /// its [`PEER_FILL_DEADLINE`] asking one peer cannot spend the whole deadline again on the next one. The default
    /// ignores the budget — the in-memory doubles answer without waiting; the live transport overrides it to bound its
    /// wait.
    fn fetch_range_within(
        &self,
        peers: &[PeerId],
        request: &CacheRangeRequest,
        budget: std::time::Duration,
    ) -> Result<Option<CacheRangeResponse>, PeerCacheError> {
        let _ = budget;
        self.fetch_range(peers, request)
    }
}

/// Finds an immutable object's bytes on a peer that already holds them, so a node can skip a slow read from the durable
/// tier (object storage).
///
/// It picks the peers most likely to hold the object by rendezvous hashing, asks them through the injected
/// [`PeerCacheTransport`], and returns the requested byte range **only** after it verifies against the object's root
/// content hash — through the peer's outboard BLAKE3 tree when one came back, otherwise by hashing the whole content.
/// Anything else — a miss, an unverified response, or a transport failure — yields `None`, so the caller falls through
/// to the durable tier and never serves unverified bytes.
///
/// See: hef-hardware-deployment/spec.md
pub struct PeerCache<T: PeerCacheTransport> {
    accepts: TransportCapabilities,
    replication: usize,
    transport: T,
}

impl<T: PeerCacheTransport> PeerCache<T> {
    /// Builds a peer cache over `transport`, consulting up to `replication` preferred peers per object before giving up
    /// and falling through. Requests advertise the inline-only floor; use
    /// [`with_accepted_transports`](Self::with_accepted_transports) when this node's probes passed for more.
    pub fn new(transport: T, replication: usize) -> Self {
        Self::with_accepted_transports(transport, replication, TransportCapabilities::inline_only())
    }

    /// Like [`new`](Self::new) but advertises `accepts` — the fill paths this node's startup probes confirmed it can
    /// redeem — on every request, so holders may answer with a one-sided or compressed fill.
    pub fn with_accepted_transports(transport: T, replication: usize, accepts: TransportCapabilities) -> Self {
        Self {
            accepts,
            replication,
            transport,
        }
    }

    /// Returns the requested byte range of the object from a peer that has it, or `None` so the caller reads the
    /// durable tier. The range is BLAKE3-verified against `request.file_blake3` — through the peer's outboard tree when
    /// it returned one, otherwise by hashing the whole content — before it is returned, so no unverified byte ever
    /// leaves here. Each preferred holder is asked on its own, so one faulty peer costs only its own turn and the
    /// replicas ranked behind it are still tried, but all the turns together are bounded by one [`PEER_FILL_DEADLINE`]
    /// — however many replicas there are, the read falls through to the durable tier within it. The request goes out
    /// stamped with the transports this cache was built to accept, whatever the caller set. `members` is the current
    /// cluster membership view (a hint, never a source of visibility or lease truth).
    pub fn fetch(&self, members: &[PeerId], request: &CacheRangeRequest) -> Option<Vec<u8>> {
        let peers = selection::preferred_peers(members, &request.file_blake3, self.replication);
        if peers.is_empty() {
            return None;
        }
        let request = CacheRangeRequest {
            accepts: self.accepts,
            ..request.clone()
        };
        // Each preferred holder is asked on its own, in rank order, so a holder that fails — an unreachable peer, a
        // transport error, or a response the requester cannot prove — only costs its own turn: the healthy copies
        // ranked behind it are still tried before the read falls through to the durable tier. Verification happens
        // here, on the requester, whichever peer delivered the bytes: the requested range is checked against the
        // object's root, so the result never depends on which peer answered — which is also why the transport cannot
        // skip a corrupt holder for us. The turns share one deadline rather than each getting their own, so a higher
        // replication count never multiplies how long an unreachable or silent set of peers can hold the read open.
        let deadline = std::time::Instant::now() + PEER_FILL_DEADLINE;
        for peer in &peers {
            let budget = deadline.saturating_duration_since(std::time::Instant::now());
            if budget.is_zero() {
                break;
            }
            let Ok(Some(response)) = self
                .transport
                .fetch_range_within(std::slice::from_ref(peer), &request, budget)
            else {
                continue;
            };
            if let Some(range) = verified_range(
                &response.bytes,
                response.tree.as_deref(),
                &request.file_blake3,
                request.range,
            ) {
                return Some(range);
            }
        }
        None
    }
}

/// Verifies the requested `range` of an object against its root `file_blake3` and returns just those bytes, or `None`
/// when they cannot be proven.
///
/// When the object carries an outboard BLAKE3 `tree`, only the chunk groups the range touches are checked against the
/// root — the range is proven without re-hashing the unread bytes, the same range-integrity primitive a durable-tier
/// range read uses ([`crate::file::integrity::verify_range`]). Without a usable tree it falls back to hashing the whole
/// `content` against the root. Either way no unverified byte is ever returned: a corrupt, short, or out-of-range
/// request yields `None`, and the caller falls through to the durable tier.
pub(super) fn verified_range(
    content: &[u8],
    tree: Option<&[u8]>,
    file_blake3: &[u8; blake3::OUT_LEN],
    range: ObjectRange,
) -> Option<Vec<u8>> {
    if let Some(tree) = tree {
        match verify_range(
            content,
            tree,
            file_blake3,
            range.offset,
            range.length,
            CHUNK_GROUP_BYTES,
        ) {
            Ok(bytes) => return Some(bytes.to_vec()),
            // The tree proved the bytes do not match the root: reject outright.
            Err(RangeFault::Corrupt) => return None,
            // The tree could not be used; fall back to whole-content verification.
            Err(RangeFault::TreeUnusable) => {}
        }
    }
    if blake3::hash(content).as_bytes() != file_blake3 {
        return None;
    }
    let start = usize::try_from(range.offset).ok()?;
    let end = start.checked_add(usize::try_from(range.length).ok()?)?;
    content.get(start..end).map(<[u8]>::to_vec)
}

#[cfg(test)]
#[path = "test/api.rs"]
mod tests;
