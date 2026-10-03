//! Checks that what queries can see is decided by the published catalogue, not by which files happen to exist on
//! storage. A finished file that no published catalogue entry references yet is invisible, and that range keeps being
//! served from the recent in-memory store instead.
use crate::support;
use hef::artifacts::overlay::LiveOverlayStore;
use hef::artifacts::segment::replay_segment;
use hef::invariants::PublishedSet;
use hef::invariants::sim::SimulatedPublishedSet;

/// conformance: hef-core-invariants/source-of-truth-and-visibility-ordering/unpublished-hef-is-not-visible
#[test]
fn unpublished_hef_is_not_visible() {
    // A built HEF file "exists on storage" but no manifest references it: the manifest, not file existence, defines
    // visibility, and the journal range is still served from HEJ/LiveOverlay.
    let mut world = support::World::new(11);
    let range = world.ingest(4);
    let _file_on_storage = support::built_file(4);
    let head = SimulatedPublishedSet::new().head().unwrap().1;
    assert!(!head.covers(&range, support::tenant()));
    assert_eq!(head.snapshot_files().count(), 0);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let mut overlay = LiveOverlayStore::new();
    overlay
        .rebuild_from_replay(&replay, 1, 1, |t, r| head.covers(r, t))
        .unwrap();
    assert_eq!(overlay.segment_count(), 1, "still served from LiveOverlay");
}
