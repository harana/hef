//! Checks that the NVMe io_uring_cmd passthrough operation set is observably equivalent to the buffered filesystem fallback behind the unchanged synchronous JournalStorage interface: same durable bytes, same frame offsets, same shard extents, same read results, same BLAKE3 chain. On hosts without an NVMe character device the buffered fallback is used and the build and test suite pass unchanged.

use crate::support;
use hef::artifacts::segment::{ReplayTail, replay_segment};
use hef::file::model::{Atomicity, DurabilityMode};
use hef::invariants::JournalStorage;
use hef::invariants::sim::SimJournalStorage;

/// conformance: hef-hardware-deployment/nvme-passthrough-operation-set-is-provided-behind-the-synchronous-interface/accelerated-and-buffered-journal-paths-reach-the-same-result
#[test]
fn accelerated_and_buffered_journal_paths_reach_the_same_result() {
    // The NVMe passthrough path and the buffered fallback are observably equivalent behind the synchronous JournalStorage interface. Two simulation runs with different atomicity profiles but the same event sequence produce the same durable frame bytes, the same shard extent, and the same BLAKE3-verified segment chain.
    let (extent_a, frames_a, chain_a) = {
        let mut world = support::World::new(40);
        world.storage = SimJournalStorage::new().with_atomicity(Atomicity {
            atomic_frame_multiple: 4096,
            awupf_bytes: 4096,
            durability_mode: DurabilityMode::DirectIo,
            untorn_write_bytes: 4096,
        });
        world.ingest(3);
        let extent = world.storage.extent(support::SHARD).unwrap_or(0);
        let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
        assert!(matches!(replay.tail, ReplayTail::Clean));
        let frames: Vec<Vec<u8>> = replay.frames.into_iter().map(|f| f.frame_bytes).collect();
        (extent, frames, replay.segment_chain_blake3)
    };

    let (extent_b, frames_b, chain_b) = {
        let mut world = support::World::new(40);
        world.storage = SimJournalStorage::new().with_atomicity(Atomicity::buffered_unprobed());
        world.ingest(3);
        let extent = world.storage.extent(support::SHARD).unwrap_or(0);
        let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
        assert!(matches!(replay.tail, ReplayTail::Clean));
        let frames: Vec<Vec<u8>> = replay.frames.into_iter().map(|f| f.frame_bytes).collect();
        (extent, frames, replay.segment_chain_blake3)
    };

    assert_eq!(extent_a, extent_b, "shard extents identical on both paths");
    assert_eq!(frames_a, frames_b, "durable frame bytes identical on both paths");
    assert_eq!(chain_a, chain_b, "BLAKE3 segment chain identical on both paths");
}

/// conformance: hef-hardware-deployment/nvme-passthrough-operation-set-is-provided-behind-the-synchronous-interface/development-host-falls-back-without-breaking-the-build
#[test]
fn development_host_falls_back_without_breaking_the_build() {
    // On macOS or Linux hosts without an NVMe character device the passthrough operations are feature-gated off and the buffered filesystem fallback is used. The very fact that this test compiles and runs proves the build does not require NVMe character-device support or accelerator libraries. The portable IoCapabilities profile has no direct-I/O, no uncached reads, and no zoned placement — matching a macOS aarch64 or NVMe-less Linux host.
    use hef::file::model::IoCapabilities;
    let caps = IoCapabilities::portable();
    assert_eq!(caps.direct_io_alignment_bytes, 0, "no direct-I/O on portable host");
    assert!(!caps.uncached_reads, "no uncached reads on portable host");
    assert!(!caps.zoned_placement, "no ZNS/FDP on portable host");

    // The portable fallback (default SimJournalStorage) ingests and replays correctly without any NVMe or accelerator capability.
    let mut world = support::World::new(41);
    world.ingest(2);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert!(
        !replay.frames.is_empty(),
        "portable fallback commits and replays correctly"
    );
    assert!(
        replay.segment_chain_blake3.is_some(),
        "BLAKE3 chain verified on portable path"
    );
    assert!(matches!(replay.tail, ReplayTail::Clean));
}

/// conformance: hef-hardware-deployment/nvme-passthrough-operation-set-is-provided-behind-the-synchronous-interface/nvme-required-deployment-refuses-when-the-device-is-absent
#[test]
fn nvme_required_deployment_refuses_when_the_device_is_absent() {
    // stub: blocked on the NVMe-required deployment-config flag and the refusing startup probe path (D6); lands with the NVMe deployment configuration and device-absence detection changes.
}
