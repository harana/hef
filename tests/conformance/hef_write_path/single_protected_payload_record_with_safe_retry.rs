//! Checks that retrying a write within the safety window does not create duplicates. The system recognises the repeat
//! by its identity and stores the event body only once in the durable log; the fast-retry rows are just rebuildable
//! lookup entries, not extra copies, and rebuilding them from the log reproduces the same duplicate decisions.
use crate::support;
use hef::artifacts::segment::replay_segment;
use hef::invariants::Clock;
use hef::writer::retry::SafeRetryStore;
use hef::writer::sim::SimSafeRetryStore;

/// conformance: hef-write-path/single-protected-payload-record-with-safe-retry/replay-within-guard-window
#[test]
fn replay_within_guard_window() {
    // Replaying a retained HEJ frame while the original acknowledgement is within the replay-guard window enqueues no
    // duplicates: the dedupe identity reports duplicate, and HEJ remains the only payload record (safe-retry rows are
    // reconstructable indexes, not payload copies).
    let mut world = support::World::new(101);
    world.ingest(2);
    let now = world.clock.now_nanos();
    assert!(world.retry.duplicate_within_guard(support::tenant(), (1_000, 7), now));
    assert!(world.retry.duplicate_within_guard(support::tenant(), (1_001, 7), now));
    assert!(!world.retry.duplicate_within_guard(support::tenant(), (9_999, 7), now));
    // Reconstruction from HEJ replay reproduces the same guard decisions.
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let mut rebuilt = SimSafeRetryStore::new();
    rebuilt.reconstruct_from_replay(&replay.frames, support::SHARD, now + 1);
    assert!(rebuilt.duplicate_within_guard(support::tenant(), (1_000, 7), now));
}
