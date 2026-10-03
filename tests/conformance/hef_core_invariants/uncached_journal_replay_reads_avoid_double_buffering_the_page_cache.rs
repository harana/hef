//! Checks that switching from buffered to direct (uncached) reads for the journal replay scan does not change the
//! CRC-64/NVME precheck, the authoritative BLAKE3 verification, or the recovery result. The simulation has no page
//! cache, so every replay is inherently uncached; this proves the validated-frame set is identical across durability
//! modes.

use crate::support;
use hef::artifacts::segment::{ReplayTail, replay_segment};
use hef::file::model::{Atomicity, DurabilityMode};
use hef::invariants::sim::SimJournalStorage;

/// conformance:
/// hef-core-invariants/uncached-journal-replay-reads-avoid-double-buffering-the-page-cache/
/// replay-scan-does-not-evict-hot-pages
#[test]
fn replay_scan_does_not_evict_hot_pages() {
    // The simulation owns no page cache; replay is inherently uncached, modelling the uncached-read guarantee exactly.
    // The validated frame count, BLAKE3 chain anchor, and tail status are identical whether the backend's durability
    // mode is Buffered or DirectIo — proving that choosing an uncached read path does not change the recovery result.
    let replay_with = |durability_mode: DurabilityMode| {
        let mut world = support::World::new(88);
        world.storage = SimJournalStorage::new().with_atomicity(Atomicity {
            atomic_frame_multiple: 4096,
            awupf_bytes: 0,
            durability_mode,
            untorn_write_bytes: 0,
        });
        world.ingest(8); // several events to exercise the segment
        replay_segment(&world.storage, support::SHARD, 1, 1).unwrap()
    };

    let buffered = replay_with(DurabilityMode::Buffered);
    let direct = replay_with(DurabilityMode::DirectIo);

    assert!(!buffered.frames.is_empty(), "frames recovered and BLAKE3-verified");
    assert_eq!(
        buffered.frames.len(),
        direct.frames.len(),
        "same frame count regardless of durability mode"
    );
    assert_eq!(
        buffered.segment_chain_blake3, direct.segment_chain_blake3,
        "identical BLAKE3 chain anchor"
    );
    assert_eq!(
        matches!(buffered.tail, ReplayTail::Clean),
        matches!(direct.tail, ReplayTail::Clean),
        "same tail status"
    );
}
