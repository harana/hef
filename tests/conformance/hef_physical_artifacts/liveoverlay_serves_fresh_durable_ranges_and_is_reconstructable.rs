//! Checks the in-memory layer that serves the most recent events not yet folded into a query file. If a node's copy is
//! missing a range it reports exactly what it is missing (there is no "ask the tenant's EventManager" shortcut) and can
//! rebuild that range from the durable log. It also refuses to drop a range until a published query file actually
//! covers it, so nothing is evicted too early.

use crate::support;
use hef::artifacts::overlay::{FreshRead, LiveOverlayStore};
use hef::artifacts::segment::replay_segment;
use hef::events::SequenceRange;
use hef::lifecycle::ManifestGeneration;

/// conformance:
/// hef-physical-artifacts/liveoverlay-serves-fresh-durable-ranges-and-is-reconstructable/node-behind-on-a-range
#[test]
fn node_behind_on_a_range() {
    // A node whose local overlay does not cover a fresh range reports Behind with the missing range — there is no path
    // that forwards the read to the tenant's EventManager, by construction — and serves after rebuilding from HEJ.
    let mut world = support::World::new(81);
    let range = world.ingest(3);
    let mut overlay = LiveOverlayStore::new();
    let FreshRead::Behind { missing } = overlay.fresh_read(support::tenant(), range) else {
        panic!("must report behind");
    };
    assert_eq!(missing, range);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    overlay.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert!(matches!(
        overlay.fresh_read(support::tenant(), range),
        FreshRead::Ready(_)
    ));
}

/// conformance:
/// hef-physical-artifacts/liveoverlay-serves-fresh-durable-ranges-and-is-reconstructable/premature-eviction-prevented
#[test]
fn premature_eviction_prevented() {
    // A segment whose range is not covered by a visible manifest-published HEF is not evicted.
    let mut world = support::World::new(83);
    let range = world.ingest(2);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let mut overlay = LiveOverlayStore::new();
    overlay.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    let empty = ManifestGeneration::default();
    assert!(!empty.covers(&range, support::tenant()));
    assert_eq!(overlay.evict_covered(&empty), 0);
    assert_eq!(overlay.segment_count(), 1);
    let _ = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 2,
    };
}
