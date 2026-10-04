use super::{PeerCache, verified_range};

use crate::cache::peer::model::{CacheRangeRequest, ObjectRange, PeerId, TransportCapabilities};
use crate::cache::peer::sim::{PeerBehaviour, SimulatedPeerTransport};
use crate::events::TenantId;
use crate::file::constant::CHUNK_GROUP_BYTES;
use crate::file::integrity::build_outboard_tree;
use crate::typed_id::TypedIdTestExt;

/// The HEF file id every test request names.
const HELD_FILE: u128 = 0xE3B;

fn request(tenant: TenantId, file_blake3: [u8; 32], range: ObjectRange) -> CacheRangeRequest {
    CacheRangeRequest {
        accepts: TransportCapabilities::inline_only(),
        file_blake3,
        file_id: HELD_FILE,
        range,
        tenant_id: tenant,
    }
}

fn whole(length: u64) -> ObjectRange {
    ObjectRange { length, offset: 0 }
}

/// An object big enough to carry an outboard tree (more than one chunk group), with varied bytes so a wrong slice can
/// never accidentally equal the right one.
fn multi_group_object() -> Vec<u8> {
    (0..2 * CHUNK_GROUP_BYTES + 512).map(|i| (i % 251) as u8).collect()
}

#[test]
fn verified_peer_hit_returns_the_bytes() {
    let tenant = TenantId::new_test_id(1);
    let block = b"an embedding block".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, block.clone());
    let cache = PeerCache::new(transport, 3);

    let members = vec![PeerId(1), PeerId(7), PeerId(9)];
    assert_eq!(
        cache.fetch(&members, &request(tenant, file_blake3, whole(block.len() as u64))),
        Some(block)
    );
}

#[test]
fn range_fetch_returns_exactly_the_requested_slice_through_the_tree() {
    let tenant = TenantId::new_test_id(1);
    let object = multi_group_object();
    let file_blake3 = *blake3::hash(&object).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, object.clone());
    let cache = PeerCache::new(transport, 3);

    let range = ObjectRange {
        length: 4096,
        offset: (CHUNK_GROUP_BYTES + 100) as u64,
    };
    let expected = object
        .get(CHUNK_GROUP_BYTES + 100..CHUNK_GROUP_BYTES + 100 + 4096)
        .map(<[u8]>::to_vec);
    assert_eq!(
        cache.fetch(&[PeerId(7)], &request(tenant, file_blake3, range)),
        expected
    );
}

#[test]
fn range_fetch_without_a_tree_falls_back_to_whole_content_verification() {
    let tenant = TenantId::new_test_id(1);
    let block = b"an embedding block".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, block.clone());
    let cache = PeerCache::new(transport, 3);

    let range = ObjectRange { length: 4, offset: 3 };
    assert_eq!(
        cache.fetch(&[PeerId(7)], &request(tenant, file_blake3, range)),
        Some(b"embe".to_vec())
    );
}

#[test]
fn corrupt_peer_bytes_are_rejected_whether_or_not_a_tree_came_back() {
    let tenant = TenantId::new_test_id(1);
    for object in [b"an embedding block".to_vec(), multi_group_object()] {
        let file_blake3 = *blake3::hash(&object).as_bytes();

        let transport = SimulatedPeerTransport::new();
        transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, object.clone());
        transport.set_behaviour(PeerId(7), PeerBehaviour::Corrupt);
        let cache = PeerCache::new(transport, 3);

        let range = ObjectRange { length: 8, offset: 0 };
        assert!(
            cache
                .fetch(&[PeerId(7)], &request(tenant, file_blake3, range))
                .is_none()
        );
    }
}

#[test]
fn corrupt_peer_fails_verification_for_a_range_entirely_in_a_later_chunk_group() {
    // Regression test for #9040: the fault used to corrupt only byte 0, so a range that never touches the first
    // chunk group verified successfully against the peer's honest tree.
    let tenant = TenantId::new_test_id(1);
    let object = multi_group_object();
    let file_blake3 = *blake3::hash(&object).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, object.clone());
    transport.set_behaviour(PeerId(7), PeerBehaviour::Corrupt);
    let cache = PeerCache::new(transport, 3);

    let range = ObjectRange {
        length: 4096,
        offset: (CHUNK_GROUP_BYTES + 100) as u64,
    };
    assert!(
        cache
            .fetch(&[PeerId(7)], &request(tenant, file_blake3, range))
            .is_none()
    );
}

#[test]
fn out_of_range_request_is_a_miss() {
    let tenant = TenantId::new_test_id(1);
    let block = b"an embedding block".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, block.clone());
    let cache = PeerCache::new(transport, 3);

    let range = ObjectRange {
        length: 8,
        offset: block.len() as u64,
    };
    assert!(
        cache
            .fetch(&[PeerId(7)], &request(tenant, file_blake3, range))
            .is_none()
    );
}

#[test]
fn unreachable_peer_is_a_miss() {
    let tenant = TenantId::new_test_id(1);
    let block = b"an embedding block".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let transport = SimulatedPeerTransport::new();
    transport.give(PeerId(7), tenant, HELD_FILE, file_blake3, block.clone());
    transport.set_behaviour(PeerId(7), PeerBehaviour::Unreachable);
    let cache = PeerCache::new(transport, 3);

    assert!(
        cache
            .fetch(&[PeerId(7)], &request(tenant, file_blake3, whole(block.len() as u64)))
            .is_none()
    );
}

#[test]
fn cold_cache_falls_through_to_none() {
    let tenant = TenantId::new_test_id(1);
    let cache = PeerCache::new(SimulatedPeerTransport::new(), 3);
    let members = vec![PeerId(1), PeerId(2)];
    assert!(cache.fetch(&members, &request(tenant, [4u8; 32], whole(8))).is_none());
}

#[test]
fn empty_membership_is_a_miss() {
    let tenant = TenantId::new_test_id(1);
    let cache = PeerCache::new(SimulatedPeerTransport::new(), 3);
    assert!(cache.fetch(&[], &request(tenant, [4u8; 32], whole(8))).is_none());
}

#[test]
fn fetch_stamps_the_caches_accepted_transports_onto_the_request() {
    use std::sync::Mutex;

    use crate::cache::peer::api::PeerCacheTransport;
    use crate::cache::peer::error::PeerCacheError;
    use crate::cache::peer::model::CacheRangeResponse;

    /// Records the request it is handed so the test can see what actually left the cache.
    #[derive(Default)]
    struct RecordingTransport {
        seen: Mutex<Option<TransportCapabilities>>,
    }

    impl PeerCacheTransport for RecordingTransport {
        fn fetch_range(
            &self,
            _peers: &[PeerId],
            request: &CacheRangeRequest,
        ) -> Result<Option<CacheRangeResponse>, PeerCacheError> {
            *self.seen.lock().unwrap() = Some(request.accepts);
            Ok(None)
        }
    }

    let tenant = TenantId::new_test_id(1);
    let transport = RecordingTransport::default();
    let cache = PeerCache::with_accepted_transports(transport, 3, TransportCapabilities::all());

    // The caller left the conservative default on the request; the cache stamps its own probed capabilities over it.
    let sent = request(tenant, [4u8; 32], whole(8));
    assert_eq!(sent.accepts, TransportCapabilities::inline_only());
    assert!(cache.fetch(&[PeerId(1)], &sent).is_none());
    assert_eq!(
        cache.transport.seen.lock().unwrap().unwrap(),
        TransportCapabilities::all()
    );
}

#[test]
fn verified_range_proves_a_slice_through_the_tree() {
    let object = multi_group_object();
    let file_blake3 = *blake3::hash(&object).as_bytes();
    let tree = build_outboard_tree(&object, CHUNK_GROUP_BYTES).expect("multi-group object has a tree");

    let range = ObjectRange { length: 32, offset: 10 };
    assert_eq!(
        verified_range(&object, Some(&tree), &file_blake3, range),
        object.get(10..42).map(<[u8]>::to_vec)
    );
}

#[test]
fn verified_range_rejects_bytes_the_tree_disproves() {
    let object = multi_group_object();
    let file_blake3 = *blake3::hash(&object).as_bytes();
    let tree = build_outboard_tree(&object, CHUNK_GROUP_BYTES).expect("multi-group object has a tree");

    let mut corrupted = object.clone();
    if let Some(first) = corrupted.first_mut() {
        *first ^= 0xFF;
    }
    let range = ObjectRange { length: 32, offset: 0 };
    assert!(verified_range(&corrupted, Some(&tree), &file_blake3, range).is_none());
}

#[test]
fn verified_range_falls_back_to_whole_content_when_the_tree_is_unusable() {
    // A small object never needs a tree, so any tree handed alongside it is unusable — verification must fall back to
    // hashing the whole content rather than rejecting.
    let block = b"an embedding block".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();

    let range = ObjectRange { length: 4, offset: 3 };
    assert_eq!(
        verified_range(&block, Some(&[0u8; 64]), &file_blake3, range),
        Some(b"embe".to_vec())
    );
}

#[test]
fn verified_range_rejects_content_that_misses_the_root() {
    let block = b"x".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();
    assert!(verified_range(b"y", None, &file_blake3, ObjectRange { length: 1, offset: 0 }).is_none());
}

/// A transport that takes longer than the fill deadline to answer, and counts how many peers it was asked about.
struct SlowTransport {
    calls: std::sync::atomic::AtomicUsize,
    delay: std::time::Duration,
}

impl crate::cache::peer::api::PeerCacheTransport for SlowTransport {
    fn fetch_range(
        &self,
        _peers: &[PeerId],
        _request: &CacheRangeRequest,
    ) -> Result<Option<crate::cache::peer::model::CacheRangeResponse>, crate::cache::peer::error::PeerCacheError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::thread::sleep(self.delay);
        Ok(None)
    }
}

#[test]
fn the_preferred_holders_share_one_fill_deadline_rather_than_each_getting_their_own() {
    let tenant = TenantId::new_test_id(1);
    let members = vec![PeerId(2), PeerId(3), PeerId(5)];
    let cache = PeerCache::new(
        SlowTransport {
            calls: std::sync::atomic::AtomicUsize::new(0),
            delay: crate::cache::peer::constant::PEER_FILL_DEADLINE,
        },
        3,
    );

    // The first holder alone spends the whole deadline, so the read falls through to the durable tier instead of
    // starting the deadline over on each of the three replicas.
    let started = std::time::Instant::now();
    assert_eq!(cache.fetch(&members, &request(tenant, [7u8; 32], whole(8))), None);
    assert!(
        started.elapsed() < 2 * crate::cache::peer::constant::PEER_FILL_DEADLINE,
        "the fall-through waited {:?}, past the one fill deadline it is allowed",
        started.elapsed()
    );
}

/// Regression test for #13695: `PeerCache::fetch` used to check its deadline only before calling the transport, never
/// telling it how much of the deadline was actually left — so a transport had no way to bound its own wait to less
/// than the whole deadline for every holder it tried. `fetch_range_within` must now be handed the true remaining
/// budget, which strictly shrinks as each earlier holder spends real time.
#[test]
fn fetch_range_within_receives_the_shrinking_remaining_budget() {
    use std::sync::Mutex;

    #[derive(Default)]
    struct BudgetRecordingTransport {
        seen_budgets: Mutex<Vec<std::time::Duration>>,
    }

    impl crate::cache::peer::api::PeerCacheTransport for BudgetRecordingTransport {
        fn fetch_range(
            &self,
            _peers: &[PeerId],
            _request: &CacheRangeRequest,
        ) -> Result<Option<crate::cache::peer::model::CacheRangeResponse>, crate::cache::peer::error::PeerCacheError>
        {
            panic!("fetch_range must not be called when fetch_range_within is available");
        }

        fn fetch_range_within(
            &self,
            _peers: &[PeerId],
            _request: &CacheRangeRequest,
            budget: std::time::Duration,
        ) -> Result<Option<crate::cache::peer::model::CacheRangeResponse>, crate::cache::peer::error::PeerCacheError>
        {
            self.seen_budgets.lock().unwrap().push(budget);
            std::thread::sleep(std::time::Duration::from_millis(20));
            Ok(None)
        }
    }

    let tenant = TenantId::new_test_id(1);
    let members = vec![PeerId(2), PeerId(3), PeerId(5)];
    let cache = PeerCache::new(BudgetRecordingTransport::default(), 3);

    assert_eq!(cache.fetch(&members, &request(tenant, [7u8; 32], whole(8))), None);

    let seen = cache.transport.seen_budgets.lock().unwrap();
    assert_eq!(seen.len(), 3, "all three preferred holders were asked");
    assert!(
        seen[0] <= crate::cache::peer::constant::PEER_FILL_DEADLINE,
        "the first holder's budget must never exceed the whole fill deadline, got {:?}",
        *seen
    );
    for pair in seen.windows(2) {
        assert!(
            pair[1] < pair[0],
            "the remaining budget must shrink for each later holder, got {:?}",
            *seen
        );
    }
}

#[test]
fn verified_range_rejects_a_range_past_the_end() {
    let block = b"an embedding block".to_vec();
    let file_blake3 = *blake3::hash(&block).as_bytes();
    let range = ObjectRange {
        length: 1,
        offset: block.len() as u64,
    };
    assert!(verified_range(&block, None, &file_blake3, range).is_none());
}
