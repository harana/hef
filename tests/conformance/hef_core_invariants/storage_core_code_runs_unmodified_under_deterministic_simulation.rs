//! Checks that the real storage code can be tested under simulated faults without being changed for testing. Faults
//! like a half-written journal record or a lost publish race are injected only through the swappable interfaces, the
//! production code runs as-is, and a run is reproducible from its random seed.
use crate::support;
use hef::artifacts::segment::{ReplayTail, replay_segment};
use hef::invariants::sim::Fault;
use hef::writer::pipeline::FlushReason;

/// conformance:
/// hef-core-invariants/storage-core-code-runs-unmodified-under-deterministic-simulation/
/// fault-injection-requires-no-production-code-changes
#[test]
fn fault_injection_requires_no_production_code_changes() {
    // A torn HEJ frame tail and a lost publication CAS race are both injected purely through the interfaces
    // (JournalStorage, PublishedSet); the production pipeline, replay, and publisher code run unmodified, and the run
    // is reproducible from its seed.
    let run = |seed: u64| -> (usize, bool) {
        let mut world = support::World::new(seed);
        world.ingest(2);
        world
            .worker
            .submit(support::event(7), 1, &mut world.retry, &world.clock)
            .unwrap();
        world.storage.inject(Fault::TornTail {
            shard: support::SHARD,
            keep_bytes: 64,
        });
        world.storage.inject(Fault::FailSync { shard: support::SHARD });
        let _ = world.worker.flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &mut world.overlay,
            &world.clock,
        );
        world.storage.crash();
        let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
        let torn = matches!(replay.tail, ReplayTail::Truncated { .. });
        (replay.frames.len(), torn)
    };
    // Reproducible from the seed: identical outcome on identical seeds.
    assert_eq!(run(42), run(42));
    let (frames, torn) = run(42);
    assert_eq!(frames, 1, "the durable frame survives; the torn one is truncated");
    assert!(torn);
    // The lost-CAS injection path is exercised in
    // conditional_manifest_publication...::lost_manifest_cas_rebases_instead_of_overwriting, also entirely through the
    // interface.
}
