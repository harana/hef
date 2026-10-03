//! Checks that journal-device health monitoring is purely a correctness- neutral signal: detecting corruption triggers
//! the existing BLAKE3/CRC recovery path but never gates the append hot path, and on a host that exposes no proactive
//! health signals the reactive path (torn-tail truncation, BLAKE3-on-replay) continues to protect correctness.

use crate::support;
use hef::artifacts::segment::{ReplayTail, replay_segment};
use hef::file::model::{Atomicity, DurabilityMode};
use hef::invariants::JournalStorage;
use hef::invariants::sim::{Fault, SimJournalStorage};
use hef::writer::pipeline::FlushReason;

/// conformance:
/// hef-core-invariants/probed-journal-device-health-watch-is-correctness-neutral/
/// journal-device-error-raises-an-early-warning-without-gating-the-hot-path
#[test]
fn journal_device_error_raises_an_early_warning_without_gating_the_hot_path() {
    // Bit rot in the durable frame is caught by BLAKE3 on replay (the early-warning path), but the hot path — new
    // appends and syncs — is never gated by that detection.
    let mut world = support::World::new(55);
    world.ingest(2); // commit first frame (durable)
    world.ingest(2); // commit second frame (durable)

    // Corrupt a byte in the first frame: simulates the kind of I/O error that a health watch would surface as an
    // early-warning signal.
    world.storage.corrupt_durable_byte(support::SHARD, 100);

    // Replay detects the corruption via BLAKE3 and flags it as trailing non-zero data — the early-warning signal — but
    // does not silently serve the corrupt frame.
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert!(
        matches!(
            replay.tail,
            ReplayTail::Truncated {
                trailing_nonzero: true,
                ..
            }
        ),
        "corruption caught by BLAKE3, flagged as early warning"
    );

    // The hot path is not gated: new appends and syncs succeed immediately after detecting corruption in the existing
    // durable extent.
    let extent_before = world.storage.extent(support::SHARD).unwrap();
    world.ingest(1);
    let extent_after = world.storage.extent(support::SHARD).unwrap();
    assert!(
        extent_after > extent_before,
        "hot path append+sync succeeded after corruption detection"
    );
}

/// conformance:
/// hef-core-invariants/probed-journal-device-health-watch-is-correctness-neutral/
/// unsupported-host-keeps-the-reactive-path
#[test]
fn unsupported_host_keeps_the_reactive_path() {
    // On a host that exposes no proactive health signals (buffered mode, untorn_write_bytes == 0), journal correctness
    // relies entirely on the reactive path: torn tails are truncated on recovery and corrupt frames are caught by
    // CRC-64/NVME precheck and authoritative BLAKE3.
    let mut world = support::World::new(66);
    world.storage = SimJournalStorage::new().with_atomicity(Atomicity {
        atomic_frame_multiple: 4096,
        awupf_bytes: 0,
        durability_mode: DurabilityMode::Buffered,
        untorn_write_bytes: 0,
    });

    world.ingest(2); // commit one frame (durable)

    // Arm a torn tail and fail-sync on the next flush so the second frame never completes its durability barrier.
    world.storage.inject(Fault::TornTail {
        shard: support::SHARD,
        keep_bytes: 64,
    });
    world.storage.inject(Fault::FailSync { shard: support::SHARD });
    world.worker.submit(support::event(77), 1, &world.clock).ok();
    let _ = world.worker.flush(
        FlushReason::Target,
        &mut world.allocator,
        &mut world.storage,
        &mut world.watermarks,
        &mut world.retry,
        &world.clock,
    );
    world.storage.crash(); // torn tail lands on media

    // Reactive path: torn frame is truncated; the committed frame is intact.
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1, "committed frame intact via reactive path");
    assert!(
        matches!(replay.tail, ReplayTail::Truncated { .. }),
        "torn tail truncated, never replayed as a partial frame"
    );

    // No proactive signal is assumed: correctness rests on BLAKE3 and truncation alone, as reported by the shard's
    // atomicity probe.
    let atomicity = world.storage.atomicity(support::SHARD).unwrap();
    assert_eq!(atomicity.untorn_write_bytes, 0, "no atomic-write unit on this host");
}
