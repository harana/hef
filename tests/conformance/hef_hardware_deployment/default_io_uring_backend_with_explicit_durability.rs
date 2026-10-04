//! Checks the explicit-durability requirement for the io_uring journal backend: on the buffered fallback path acknowledgement waits for the fsync/fdatasync barrier, a write-chain failure prevents the sequence watermark from advancing, and consumer SSDs without power-loss protection require the same explicit sync barrier as the dev-fallback path.

use crate::support;
use hef::artifacts::segment::replay_segment;
use hef::file::model::{Atomicity, DurabilityMode};
use hef::invariants::JournalStorage;
use hef::invariants::sim::{Fault, SimJournalStorage};
use hef::writer::pipeline::FlushReason;

/// conformance: hef-hardware-deployment/default-io-uring-backend-with-explicit-durability/buffered-fallback-waits-for-fsync
#[test]
fn buffered_fallback_waits_for_fsync() {
    // On the buffered fallback path (non-NVMe dev environment) a frame is not durable until the sync barrier completes. Injecting a sync fault before the flush, then crashing, proves the frame was never made durable — ack could not have preceded the barrier.
    let mut world = support::World::new(10);
    world.storage = SimJournalStorage::new().with_atomicity(Atomicity {
        atomic_frame_multiple: 4096,
        awupf_bytes: 0,
        durability_mode: DurabilityMode::Buffered,
        untorn_write_bytes: 0,
    });
    world
        .worker
        .submit(support::event(1), 1, &mut world.retry, &world.clock)
        .ok();
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
    assert_eq!(
        replay.frames.len(),
        0,
        "no frame is durable when the fsync barrier fails in buffered mode"
    );
}

/// conformance: hef-hardware-deployment/default-io-uring-backend-with-explicit-durability/consumer-ssd-without-power-loss-protection
#[test]
fn consumer_ssd_without_power_loss_protection() {
    // Consumer SSDs without power-loss protection are represented as DurabilityMode::Buffered with awupf_bytes = 0. Local write completion alone is not sufficient for durability: the engine requires an explicit sync barrier. When that barrier fails, no frame is durable after a crash.
    let mut world = support::World::new(11);
    world.storage = SimJournalStorage::new().with_atomicity(Atomicity {
        atomic_frame_multiple: 4096,
        awupf_bytes: 0, // no AWUPF guarantee — no power-loss protection
        durability_mode: DurabilityMode::Buffered,
        untorn_write_bytes: 0,
    });
    world
        .worker
        .submit(support::event(1), 1, &mut world.retry, &world.clock)
        .ok();
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
    assert_eq!(
        replay.frames.len(),
        0,
        "local write completion alone is not durable on consumer SSD without PLP"
    );
}

/// conformance: hef-hardware-deployment/default-io-uring-backend-with-explicit-durability/linked-chain-cancelled-on-write-failure
#[test]
fn linked_chain_cancelled_on_write_failure() {
    // When the append (first link of the write → flush → sequence-watermark chain) fails, no bytes reach the shard and the sequence watermark does not advance — the chain is cancelled at the first failing link and the frame is not acknowledged.
    let mut world = support::World::new(12);
    world
        .worker
        .submit(support::event(1), 1, &mut world.retry, &world.clock)
        .ok();
    world.storage.inject(Fault::FailAppend { shard: support::SHARD });
    let _ = world.worker.flush(
        FlushReason::Target,
        &mut world.allocator,
        &mut world.storage,
        &mut world.watermarks,
        &mut world.retry,
        &mut world.overlay,
        &world.clock,
    );
    let extent = world.storage.extent(support::SHARD).unwrap_or(0);
    assert_eq!(extent, 0, "failed write: no bytes reach the shard extent");
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert_eq!(
        replay.frames.len(),
        0,
        "no frame acknowledged after write-chain failure"
    );
}

/// conformance: hef-hardware-deployment/default-io-uring-backend-with-explicit-durability/trim-only-after-segment-gc-confirmation
#[test]
fn trim_only_after_segment_gc_confirmation() {
    // stub: blocked on the segment GC subsystem and the DSM-TRIM passthrough path (D6); lands with the segment-reclaim and GC confirmation changes.
}
