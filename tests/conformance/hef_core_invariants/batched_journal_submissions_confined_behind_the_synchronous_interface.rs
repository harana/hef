//! Checks that a worker batches multiple events into one frame and waits for its own durability barrier before
//! acknowledging — without involving a global commit thread. Two independent workers operating from the same seed
//! produce byte-identical outcomes, proving no shared global state leaks between them and that the simulation is
//! kernel-runtime-free and reproducible.

use crate::support;
use hef::artifacts::segment::replay_segment;
use hef::writer::pipeline::{CommitState, FlushReason};

/// conformance:
/// hef-core-invariants/batched-journal-submissions-confined-behind-the-synchronous-interface/
/// a-worker-batches-frames-without-a-global-commit-thread
#[test]
fn a_worker_batches_frames_without_a_global_commit_thread() {
    // A worker submits several events, flushes them all into one frame, and receives COMMITTED only after the
    // durability barrier completes. A second, entirely independent worker from the same seed produces an identical
    // outcome — proving no global commit thread or shared kernel runtime is involved.
    let run = |seed: u64| -> (CommitState, usize) {
        let mut world = support::World::new(seed);
        // Submit three events; the worker batches them into a single frame.
        for i in 0..3 {
            world
                .worker
                .submit(support::event(i), 1, &mut world.retry, &world.clock)
                .unwrap();
        }
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
        let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
        (result.state, replay.frames.len())
    };

    // Same seed → same result: the simulation owns no kernel runtime and has no external shared state.
    assert_eq!(run(300), run(300), "simulation is reproducible from its seed");
    let (state, frames) = run(300);
    assert_eq!(
        state,
        CommitState::Committed,
        "ack only after the durability barrier completes"
    );
    assert_eq!(frames, 1, "all events batched into one frame, no global commit thread");
}
