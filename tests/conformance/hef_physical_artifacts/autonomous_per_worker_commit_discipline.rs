//! Checks that each writer worker can commit its own batch of events without coordinating with the others, yet never
//! corrupts shared state. It covers a worker that tries to "steal" another's pending bytes but loses the race and
//! throws its copy away, a reserved-but-abandoned slot that gets closed off cleanly so no phantom rows appear, and a
//! slow trickle of traffic that still gets written out promptly instead of waiting forever for a full batch.
use crate::support;
use hef::artifacts::segment::replay_segment;
use hef::events::{SequencePoint, SequenceRange};
use hef::invariants::MonotonicClock;
use hef::writer::pipeline::{FlushReason, RouteDependency, WorkerCommitPipeline};
use hef::writer::reserve::LEASE_NANOS;

/// conformance: hef-physical-artifacts/autonomous-per-worker-commit-discipline/log-steal-cas-fails
#[test]
fn log_steal_cas_fails() {
    // The stealer copies clean..dirty bytes, then loses the clean-cursor CAS to the owner: the copied bytes are
    // discarded and never framed.
    let mut world = support::World::new(61);
    world.worker.submit(support::event(0), 1, &world.clock).unwrap();
    world.worker.submit(support::event(1), 1, &world.clock).unwrap();
    let (region, copied) = world.worker.queue().copy_pending().unwrap();
    assert_eq!(copied.len(), 2);
    assert!(world.worker.queue_mut().commit_claim(region), "owner claims first");
    let mut thief = WorkerCommitPipeline::new(
        2,
        support::SHARD,
        support::tenant(),
        RouteDependency::AppendOnly,
        1 << 20,
    );
    assert_eq!(
        thief.steal_from(world.worker.queue_mut(), 1, true).unwrap(),
        0,
        "failed CAS discards the copy"
    );
}

/// conformance:
/// hef-physical-artifacts/autonomous-per-worker-commit-discipline/abandoned-reservation-closed-by-void-record
#[test]
fn abandoned_reservation_closed_by_void_record() {
    // A reserved range whose lease expires is closed by an internal void record covering exactly that range;
    // commit_watermark advances past it, and the voided sequences are never public rows.
    let mut world = support::World::new(63);
    let lease = world.allocator.reserve(4, world.clock.monotonic_nanos());
    world.clock.advance(LEASE_NANOS + 1);
    let voided = world
        .worker
        .commit_voids(
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &world.clock,
        )
        .unwrap();
    assert_eq!(voided, vec![lease.range]);
    assert_eq!(
        world.watermarks.commit_watermark(),
        Some(SequencePoint { epoch: 1, sequence: 4 })
    );
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert!(replay.frames[0].header.is_void_record());
    assert_eq!(replay.frames[0].header.event_count, 0);
    let _ = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 4,
    };
}

/// conformance: hef-physical-artifacts/autonomous-per-worker-commit-discipline/low-load-force-commit
#[test]
fn low_load_force_commit() {
    // Trickle traffic does not wait for a full 16 KiB frame: past the jittered idle target the worker force-commits a 4
    // KiB-aligned frame.
    use hef::writer::reserve::FORCE_COMMIT_IDLE_NANOS;
    let mut world = support::World::new(65);
    world.worker.submit(support::event(0), 1, &world.clock).unwrap();
    assert_eq!(world.worker.should_flush(&world.clock), None);
    world.clock.advance(FORCE_COMMIT_IDLE_NANOS * 2);
    assert_eq!(world.worker.should_flush(&world.clock), Some(FlushReason::ForceCommit));
    let result = world
        .worker
        .flush(
            FlushReason::ForceCommit,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &world.clock,
        )
        .unwrap();
    assert_eq!(result.frame_len, 4096);
}
