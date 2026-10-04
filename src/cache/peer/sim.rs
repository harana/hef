//! An in-memory stand-in for the peer transport, for tests and deterministic simulation: each peer holds some objects
//! and a behaviour, and there is no network.
//!
//! See: hef-hardware-deployment/spec.md

use super::api::PeerCacheTransport;
use super::error::PeerCacheError;
use super::model::{CacheRangeRequest, CacheRangeResponse, FillTransport, PeerId};
use crate::events::TenantId;
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::file::integrity::build_outboard_tree;
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// How a simulated peer answers a request for an object it holds.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerBehaviour {
    /// Return bytes that will fail verification.
    Corrupt,
    /// Return the held bytes unchanged (the default).
    Serve,
    /// Fail with a transport error.
    Unreachable,
}

#[derive(Debug, Default)]
struct SimPeer {
    behaviour: Option<PeerBehaviour>,
    held: BTreeMap<(TenantId, u128, [u8; blake3::OUT_LEN]), Vec<u8>>,
}

/// A [`PeerCacheTransport`] backed by process memory. Tests stock peers with [`give`](Self::give) and shape failures
/// with [`set_behaviour`](Self::set_behaviour); no network is involved, so a run is reproducible from its seed.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Default)]
pub struct SimulatedPeerTransport {
    peers: Mutex<BTreeMap<PeerId, SimPeer>>,
}

impl SimulatedPeerTransport {
    /// An empty transport with no peers holding anything.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes `peer` hold `bytes` for HEF file `file_id` in `tenant`, whose content hash is `file_blake3`. A
    /// request only reads it back when its tenant and file id match, so the transport enforces the same
    /// tenant-scoped isolation a live peer would.
    pub fn give(
        &self,
        peer: PeerId,
        tenant: TenantId,
        file_id: u128,
        file_blake3: [u8; blake3::OUT_LEN],
        bytes: Vec<u8>,
    ) {
        let mut peers = self.peers();
        peers
            .entry(peer)
            .or_default()
            .held
            .insert((tenant, file_id, file_blake3), bytes);
    }

    /// Sets how `peer` answers when asked for an object it holds.
    pub fn set_behaviour(&self, peer: PeerId, behaviour: PeerBehaviour) {
        let mut peers = self.peers();
        peers.entry(peer).or_default().behaviour = Some(behaviour);
    }

    fn peers(&self) -> MutexGuard<'_, BTreeMap<PeerId, SimPeer>> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl PeerCacheTransport for SimulatedPeerTransport {
    fn fetch_range(
        &self,
        peers: &[PeerId],
        request: &CacheRangeRequest,
    ) -> Result<Option<CacheRangeResponse>, PeerCacheError> {
        let table = self.peers();
        for peer in peers {
            let Some(sim) = table.get(peer) else {
                continue;
            };
            let Some(bytes) = sim.held.get(&(request.tenant_id, request.file_id, request.file_blake3)) else {
                continue;
            };
            // The tree always describes the real object — its root is the object's `file_blake3` — so a corrupt fill
            // still carries the honest tree and still fails the requester's range check. Small objects (one chunk
            // group or less) have no tree, exercising the whole-content fallback.
            let tree = build_outboard_tree(bytes, CHUNK_GROUP_BYTES);
            return match sim.behaviour.unwrap_or(PeerBehaviour::Serve) {
                PeerBehaviour::Serve => Ok(Some(CacheRangeResponse {
                    bytes: bytes.clone(),
                    transport: FillTransport::Inline,
                    tree,
                })),
                PeerBehaviour::Corrupt => {
                    // Flip a byte inside the requested range itself (falling back to the last byte if the range runs
                    // past the object), so verification is checking exactly the chunk group(s) the range touches —
                    // not always the first one — and fails for any requested range, not just those over the start of
                    // the object.
                    let mut corrupted = bytes.clone();
                    let offset = usize::try_from(request.range.offset).unwrap_or(usize::MAX);
                    let index = if offset < corrupted.len() {
                        offset
                    } else {
                        corrupted.len().wrapping_sub(1)
                    };
                    match corrupted.get_mut(index) {
                        Some(target) => *target ^= 0xFF,
                        None => corrupted.push(0xFF),
                    }
                    Ok(Some(CacheRangeResponse {
                        bytes: corrupted,
                        transport: FillTransport::Inline,
                        tree,
                    }))
                }
                PeerBehaviour::Unreachable => Err(PeerCacheError::Transport(format!("peer {} unreachable", peer.0))),
            };
        }
        Ok(None)
    }
}

#[cfg(test)]
#[path = "test/sim.rs"]
mod tests;
