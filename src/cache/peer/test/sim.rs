use super::{PeerBehaviour, SimulatedPeerTransport};

use crate::cache::peer::api::{PeerCache, PeerCacheTransport};
use crate::cache::peer::model::{CacheRangeRequest, ObjectRange, PeerId};
use crate::events::TenantId;
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::file::integrity::build_outboard_tree;
use crate::typed_id::TypedIdTestExt;

/// The HEF file id every test request names.
const HELD_FILE: u128 = 0xE3B;

fn request(tenant: TenantId, file_blake3: [u8; 32], length: u64) -> CacheRangeRequest {
    CacheRangeRequest {
        accepts: crate::cache::peer::model::TransportCapabilities::inline_only(),
        file_blake3,
        file_id: HELD_FILE,
        range: ObjectRange { length, offset: 0 },
        tenant_id: tenant,
    }
}

#[test]
fn transport_serves_held_bytes() {
    let tenant = TenantId::new_test_id(1);
    let block = b"hello".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, file_blake3, block.clone());

    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant, file_blake3, block.len() as u64))
        .unwrap();
    assert_eq!(got.map(|response| response.bytes), Some(block));
}

#[test]
fn transport_returns_the_outboard_tree_for_a_multi_group_object() {
    let tenant = TenantId::new_test_id(1);
    let object: Vec<u8> = (0..2 * CHUNK_GROUP_BYTES + 512).map(|i| (i % 251) as u8).collect();
    let file_blake3 = *blake3::hash(&object).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, file_blake3, object.clone());

    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant, file_blake3, 4096))
        .unwrap()
        .expect("peer holds the object");
    assert_eq!(got.tree, build_outboard_tree(&object, CHUNK_GROUP_BYTES));
    assert!(got.tree.is_some());
}

#[test]
fn transport_returns_no_tree_for_a_single_group_object() {
    let tenant = TenantId::new_test_id(1);
    let block = b"hello".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, file_blake3, block.clone());

    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant, file_blake3, block.len() as u64))
        .unwrap()
        .expect("peer holds the object");
    assert!(got.tree.is_none());
}

#[test]
fn corrupt_peer_still_returns_the_honest_tree() {
    let tenant = TenantId::new_test_id(1);
    let object: Vec<u8> = (0..2 * CHUNK_GROUP_BYTES + 512).map(|i| (i % 251) as u8).collect();
    let file_blake3 = *blake3::hash(&object).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, file_blake3, object.clone());
    transport.set_behaviour(PeerId(2), PeerBehaviour::Corrupt);

    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant, file_blake3, 4096))
        .unwrap()
        .expect("peer answers, corruptly");
    assert_ne!(got.bytes, object);
    assert_eq!(got.tree, build_outboard_tree(&object, CHUNK_GROUP_BYTES));
}

#[test]
fn transport_misses_when_no_peer_holds_the_object() {
    let tenant = TenantId::new_test_id(1);
    let transport = SimulatedPeerTransport::new();
    let got = transport
        .fetch_range(&[PeerId(2), PeerId(3)], &request(tenant, [3u8; 32], 5))
        .unwrap();
    assert!(got.is_none());
}

#[test]
fn cache_rejects_a_corrupt_peer() {
    let tenant = TenantId::new_test_id(1);
    let block = b"embedding".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, file_blake3, block.clone());
    transport.set_behaviour(PeerId(2), PeerBehaviour::Corrupt);

    let cache = PeerCache::new(transport, 3);
    assert!(
        cache
            .fetch(&[PeerId(2)], &request(tenant, file_blake3, block.len() as u64))
            .is_none()
    );
}

#[test]
fn cache_treats_an_unreachable_peer_as_a_miss() {
    let tenant = TenantId::new_test_id(1);
    let block = b"embedding".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, file_blake3, block.clone());
    transport.set_behaviour(PeerId(2), PeerBehaviour::Unreachable);

    let cache = PeerCache::new(transport, 3);
    assert!(
        cache
            .fetch(&[PeerId(2)], &request(tenant, file_blake3, block.len() as u64))
            .is_none()
    );
}

#[test]
fn one_tenants_request_does_not_read_another_objects_bytes() {
    let tenant = TenantId::new_test_id(1);
    let held = b"held block".to_vec();
    let held_hash = *blake3::hash(&held).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant, HELD_FILE, held_hash, held);

    let other_hash = *blake3::hash(b"a different object").as_bytes();
    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant, other_hash, 8))
        .unwrap();
    assert!(got.is_none());
}

#[test]
fn a_request_never_reads_another_tenants_object_even_when_the_hash_matches() {
    let tenant_a = TenantId::new_test_id(0xA);
    let tenant_b = TenantId::new_test_id(0xB);
    let block = b"shared content".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(2), tenant_a, HELD_FILE, file_blake3, block.clone());

    // Tenant B asks for the same content hash the peer holds only for tenant A: it must miss.
    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant_b, file_blake3, block.len() as u64))
        .unwrap();
    assert!(got.is_none(), "an object stocked for tenant A must not serve tenant B");

    // Tenant A, the one it was stocked for, still hits.
    let got = transport
        .fetch_range(&[PeerId(2)], &request(tenant_a, file_blake3, block.len() as u64))
        .unwrap();
    assert_eq!(got.map(|response| response.bytes), Some(block));
}

/// One faulty top-ranked holder must not suppress the healthy replicas ranked behind it: each preferred peer gets its
/// own turn, so a corrupt or unreachable first holder costs only that turn and the read still hits the cache.
#[test]
fn a_faulty_top_ranked_holder_does_not_hide_a_healthy_replica() {
    let tenant = TenantId::new_test_id(1);
    let block = b"embedding".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();
    let members = vec![PeerId(1), PeerId(2), PeerId(3)];

    for faulty in [PeerBehaviour::Corrupt, PeerBehaviour::Unreachable] {
        let transport = SimulatedPeerTransport::new();
        // Every member holds a copy, so whichever peer ranks first is the one that fails.
        for peer in &members {
            transport.give(*peer, tenant, HELD_FILE, file_blake3, block.clone());
        }
        let top = crate::cache::peer::selection::preferred_peers(&members, &file_blake3, 1)[0];
        transport.set_behaviour(top, faulty);

        let cache = PeerCache::new(transport, 3);
        assert_eq!(
            cache.fetch(&members, &request(tenant, file_blake3, block.len() as u64)),
            Some(block.clone()),
            "a healthy replica ranked behind the faulty {faulty:?} holder served the read"
        );
    }
}
