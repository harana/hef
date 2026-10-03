//! Checks that the journal backend's probed fast paths are purely a performance optimisation: whether a host supports a
//! large atomic-write unit or not, the committed frames are durable, BLAKE3-verified, and replayable — and a torn tail
//! on a non-atomic host is always truncated, never replayed as a partial frame.

use crate::support;
use hef::artifacts::segment::{ReplayTail, replay_segment};
use hef::file::model::{Atomicity, DurabilityMode};
use hef::invariants::sim::{Fault, SimJournalStorage};
use hef::writer::pipeline::FlushReason;

/// conformance:
/// hef-core-invariants/probed-journal-storage-capabilities-with-a-portable-fallback/
/// unsupported-journal-fast-path-falls-back
#[test]
fn unsupported_journal_fast_path_falls_back() {
    // A host that reports no fast-path capabilities (buffered mode, untorn_write_bytes == 0) still produces frames that
    // are durable, replayable, and BLAKE3-chain-verified — observably identical to a direct-I/O fast-path host.
    for durability_mode in [DurabilityMode::Buffered, DurabilityMode::DirectIo] {
        let atomicity = Atomicity {
            atomic_frame_multiple: 4096,
            awupf_bytes: 0,
            durability_mode,
            untorn_write_bytes: 0,
        };
        let mut world = support::World::new(42);
        world.storage = SimJournalStorage::new().with_atomicity(atomicity);
        world.ingest(3);
        let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
        assert!(!replay.frames.is_empty(), "frame durable on {durability_mode:?} path");
        assert!(
            replay.segment_chain_blake3.is_some(),
            "BLAKE3 chain verified on {durability_mode:?} path"
        );
        assert!(
            matches!(replay.tail, ReplayTail::Clean),
            "clean tail on {durability_mode:?} path"
        );
    }
}

/// conformance:
/// hef-core-invariants/probed-journal-storage-capabilities-with-a-portable-fallback/
/// atomic-append-and-torn-tail-recovery-reach-the-same-result
#[test]
fn atomic_append_and_torn_tail_recovery_reach_the_same_result() {
    // On any host, only frames whose durability barrier completed before a crash are recovered. On a host with a large
    // atomic-write unit the pending frame is written untorn; on a host without, a torn tail is truncated. In both cases
    // the committed set is the same: exactly the frames that finished their barrier, and no partial frame is ever
    // replayed.
    let committed_frames_after_crash = |untorn_bytes: u32| -> usize {
        let mut world = support::World::new(77);
        world.storage = SimJournalStorage::new().with_atomicity(Atomicity {
            atomic_frame_multiple: 4096,
            awupf_bytes: untorn_bytes,
            durability_mode: DurabilityMode::DirectIo,
            untorn_write_bytes: untorn_bytes,
        });
        world.ingest(2); // one frame committed and durable
        // Arm a torn tail and fail the sync on the next flush so the second frame never completes its durability
        // barrier.
        world.storage.inject(Fault::TornTail {
            shard: support::SHARD,
            keep_bytes: 64,
        });
        world.storage.inject(Fault::FailSync { shard: support::SHARD });
        world.worker.submit(support::event(50), 1, &world.clock).ok();
        let _ = world.worker.flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &world.clock,
        );
        world.storage.crash();
        let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
        assert!(
            matches!(replay.tail, ReplayTail::Truncated { .. }),
            "torn frame is truncated, never replayed"
        );
        replay.frames.len()
    };

    // Both an atomic host (4096-byte write unit) and a non-atomic host (0) recover exactly the same number of committed
    // frames.
    let with_atomic_unit = committed_frames_after_crash(4096);
    let without_atomic_unit = committed_frames_after_crash(0);
    assert_eq!(
        with_atomic_unit, without_atomic_unit,
        "committed-frame count is the same on both hosts"
    );
    assert_eq!(with_atomic_unit, 1, "exactly the one committed frame survives");
}
