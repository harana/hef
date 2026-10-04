//! Checks the fast path for accepting ordinary events into the write-ahead journal (HEJ, the append-only log of
//! incoming events). A plain append is confirmed as soon as it is safely on disk, with no extra coordination steps, and
//! each journal record is written at one of the fixed sizes.
use crate::support;
use hef::artifacts::NORMAL_FRAME_SIZES;
use hef::artifacts::segment::replay_segment;
use hef::writer::pipeline::{CommitState, FlushReason};

/// conformance:
/// hef-core-invariants/hej-frame-sizing-and-acknowledgement-hot-path/append-only-ingest-avoids-dependency-machinery
#[test]
fn append_only_ingest_avoids_dependency_machinery() {
    // An ordinary append-only event is acknowledged via the worker's own autonomous flush: COMMITTED directly on
    // durability, no GSN/RFA/ barrier step, and the frame is one of the exact normal sizes.
    let mut world = support::World::new(31);
    world
        .worker
        .submit(support::event(0), 1, &mut world.retry, &world.clock)
        .unwrap();
    let result = world
        .worker
        .flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &mut world.overlay,
            &world.clock,
        )
        .unwrap();
    assert_eq!(result.state, CommitState::Committed);
    assert!(NORMAL_FRAME_SIZES.contains(&result.frame_len));
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1, "one worker-flushed frame, no barriers");
}
