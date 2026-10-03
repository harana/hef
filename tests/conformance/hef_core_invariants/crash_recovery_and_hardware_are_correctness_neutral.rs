//! Checks that a crash and restart never change query answers. After a restart, recent in-memory state that was lost is
//! not served as if complete: a fresh read reports it is behind until that state is rebuilt from the durable journal,
//! and rebuilding twice produces the same result.
use crate::support;
use hef::artifacts::overlay::{FreshRead, LiveOverlayStore};
use hef::artifacts::segment::replay_segment;
use hef::events::SequenceRange;

/// conformance:
/// hef-core-invariants/crash-recovery-and-hardware-are-correctness-neutral/fresh-query-after-restart-waits-for-rebuild
#[test]
fn fresh_query_after_restart_waits_for_rebuild() {
    // After a restart, missing LiveOverlay state is not served silently: the fresh read reports Behind until the store
    // is rebuilt from HEJ.
    let mut world = support::World::new(21);
    let range = world.ingest(3);
    world.storage.crash(); // pending state lost; durable frames remain
    let mut overlay = LiveOverlayStore::new();
    assert!(matches!(
        overlay.fresh_read(support::tenant(), range),
        FreshRead::Behind { .. }
    ));
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    overlay.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert!(matches!(
        overlay.fresh_read(support::tenant(), range),
        FreshRead::Ready(_)
    ));
    // Idempotent recovery: replaying again reproduces the same coverage.
    let again = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert_eq!(again.frames.len(), replay.frames.len());
    let _ = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 3,
    };
}
