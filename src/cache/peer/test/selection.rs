use hashbrown::HashMap;

use super::{
    negotiate_fill_transport, preferred_peers, preferred_peers_with_capacity, select_fill_transport,
    select_inline_encoding,
};
use crate::cache::peer::constant::{COMPRESSED_INLINE_MIN_BYTES, ONE_SIDED_MIN_BYTES};
use crate::cache::peer::model::{FillTransport, InlineEncoding, PeerId, Residency, TransportCapabilities};

#[test]
fn ranking_is_identical_regardless_of_member_order() {
    let members = vec![PeerId(1), PeerId(2), PeerId(3), PeerId(4), PeerId(5)];
    let key = [9u8; 32];

    let forward = preferred_peers(&members, &key, 3);

    let mut reversed = members.clone();
    reversed.reverse();
    let backward = preferred_peers(&reversed, &key, 3);

    assert_eq!(forward, backward);
    assert_eq!(forward.len(), 3);
}

#[test]
fn different_objects_spread_across_peers() {
    let members = vec![PeerId(1), PeerId(2), PeerId(3), PeerId(4)];
    let mut tops = std::collections::BTreeSet::new();
    for seed in 0..64u8 {
        let mut key = [0u8; 32];
        if let Some(first) = key.first_mut() {
            *first = seed;
        }
        tops.extend(preferred_peers(&members, &key, 1));
    }
    assert!(
        tops.len() > 1,
        "rendezvous hashing should not map every object to one peer"
    );
}

#[test]
fn removing_a_peer_only_rehomes_the_objects_it_held() {
    let full = vec![PeerId(1), PeerId(2), PeerId(3), PeerId(4)];
    let without_three = vec![PeerId(1), PeerId(2), PeerId(4)];
    let key = [5u8; 32];

    let top_full = preferred_peers(&full, &key, 1);
    let top_without = preferred_peers(&without_three, &key, 1);

    // Removing a peer only changes the top holder for objects that peer ranked first.
    if top_full != vec![PeerId(3)] {
        assert_eq!(top_full, top_without);
    }
}

#[test]
fn capacity_aware_selection_skips_full_peers() {
    let members = vec![PeerId(1), PeerId(2), PeerId(3), PeerId(4)];
    let key = [5u8; 32];

    // Capacity-blind: every peer is a candidate.
    let blind = preferred_peers(&members, &key, 4);

    // Every peer reported full: none is chosen.
    let all_full: HashMap<PeerId, u64> = members.iter().map(|peer| (*peer, 0)).collect();
    assert!(preferred_peers_with_capacity(&members, &key, 4, &all_full, 100).is_empty());

    // Only the top-ranked peer reported full: it drops out, and the next-ranked peer does *not* take its place —
    // a read only ever asks the capacity-blind top-`count` holders, so a replacement copy would never be found.
    let top = blind[0];
    let mut only_top_full = HashMap::default();
    only_top_full.insert(top, 0u64);
    assert!(preferred_peers_with_capacity(&members, &key, 1, &only_top_full, 100).is_empty());

    // With room for two holders, the pressured top peer is dropped and the rest of the read set is kept as-is.
    assert_eq!(
        preferred_peers_with_capacity(&members, &key, 2, &only_top_full, 100),
        vec![blind[1]]
    );
}

#[test]
fn capacity_aware_selection_treats_a_peer_with_no_gossip_as_available() {
    let members = vec![PeerId(1), PeerId(2), PeerId(3)];
    let key = [8u8; 32];
    let empty: HashMap<PeerId, u64> = HashMap::default();
    assert_eq!(
        preferred_peers_with_capacity(&members, &key, 3, &empty, 1_000_000),
        preferred_peers(&members, &key, 3)
    );
}

#[test]
fn small_ranges_always_go_inline_whatever_the_residency() {
    let small = ONE_SIDED_MIN_BYTES - 1;
    assert_eq!(
        select_fill_transport(small, Residency::RegisteredRam),
        FillTransport::Inline
    );
    assert_eq!(select_fill_transport(small, Residency::Nvme), FillTransport::Inline);
}

#[test]
fn a_large_range_takes_the_one_sided_road_its_residency_allows() {
    let large = ONE_SIDED_MIN_BYTES;
    assert_eq!(
        select_fill_transport(large, Residency::RegisteredRam),
        FillTransport::RdmaRead
    );
    assert_eq!(
        select_fill_transport(large, Residency::Nvme),
        FillTransport::NvmeOverFabrics
    );
}

#[test]
fn the_one_sided_threshold_is_the_exact_cut_over() {
    assert_eq!(
        select_fill_transport(ONE_SIDED_MIN_BYTES - 1, Residency::RegisteredRam),
        FillTransport::Inline
    );
    assert_eq!(
        select_fill_transport(ONE_SIDED_MIN_BYTES, Residency::RegisteredRam),
        FillTransport::RdmaRead
    );
}

#[test]
fn negotiation_takes_the_one_sided_road_only_when_both_sides_can_drive_it() {
    let large = ONE_SIDED_MIN_BYTES;
    let all = TransportCapabilities::all();
    let none = TransportCapabilities::inline_only();

    assert_eq!(
        negotiate_fill_transport(large, Residency::RegisteredRam, all, all),
        FillTransport::RdmaRead
    );
    assert_eq!(
        negotiate_fill_transport(large, Residency::Nvme, all, all),
        FillTransport::NvmeOverFabrics
    );
    // Either side missing the capability drops the fill to the always-available inline path.
    assert_eq!(
        negotiate_fill_transport(large, Residency::RegisteredRam, none, all),
        FillTransport::Inline
    );
    assert_eq!(
        negotiate_fill_transport(large, Residency::RegisteredRam, all, none),
        FillTransport::Inline
    );
    assert_eq!(
        negotiate_fill_transport(large, Residency::Nvme, all, none),
        FillTransport::Inline
    );
}

#[test]
fn negotiation_matches_the_capability_to_the_residency() {
    let large = ONE_SIDED_MIN_BYTES;
    // A requester that can only redeem RDMA does not help an NVMe-resident range, and vice versa: the road must match
    // where the holder keeps the bytes.
    let rdma_only = TransportCapabilities {
        compressed_inline: false,
        nvme_over_fabrics: false,
        rdma_read: true,
    };
    let nvme_only = TransportCapabilities {
        compressed_inline: false,
        nvme_over_fabrics: true,
        rdma_read: false,
    };
    assert_eq!(
        negotiate_fill_transport(large, Residency::Nvme, TransportCapabilities::all(), rdma_only),
        FillTransport::Inline
    );
    assert_eq!(
        negotiate_fill_transport(large, Residency::RegisteredRam, TransportCapabilities::all(), nvme_only),
        FillTransport::Inline
    );
}

#[test]
fn negotiation_keeps_small_ranges_inline_whatever_both_sides_offer() {
    assert_eq!(
        negotiate_fill_transport(
            ONE_SIDED_MIN_BYTES - 1,
            Residency::Nvme,
            TransportCapabilities::all(),
            TransportCapabilities::all()
        ),
        FillTransport::Inline
    );
}

#[test]
fn inline_encoding_compresses_only_by_mutual_consent_past_the_threshold() {
    let big = COMPRESSED_INLINE_MIN_BYTES;
    let all = TransportCapabilities::all();
    let none = TransportCapabilities::inline_only();

    assert_eq!(select_inline_encoding(big, all, all), InlineEncoding::Deflate);
    assert_eq!(select_inline_encoding(big - 1, all, all), InlineEncoding::Identity);
    assert_eq!(select_inline_encoding(big, none, all), InlineEncoding::Identity);
    assert_eq!(select_inline_encoding(big, all, none), InlineEncoding::Identity);
}
