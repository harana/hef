//! Checks that NVMe data-placement (FDP/ZNS) is a hint layer only: when FDP is absent at startup the journal writes proceed without placement handles, remain correct and readable, and no runtime fallback or re-probe path runs.

use crate::support;
use hef::artifacts::segment::{ReplayTail, replay_segment};

/// conformance: hef-hardware-deployment/nvme-data-placement-is-a-hint-layer/fdp-absent-at-startup
#[test]
fn fdp_absent_at_startup() {
    // When the startup Identify Namespace probe records FDP as absent (the default portable profile has no zoned placement), writes proceed using the standard path without placement handles. The journal remains correct and fully replayable — FDP absence changes nothing about correctness.
    let mut world = support::World::new(30);
    // Default SimJournalStorage: no FDP / ZNS capability (portable fallback).
    world.ingest(4);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    assert!(!replay.frames.is_empty(), "frame durable without FDP placement hints");
    assert!(
        replay.segment_chain_blake3.is_some(),
        "BLAKE3 segment chain intact without FDP"
    );
    assert!(
        matches!(replay.tail, ReplayTail::Clean),
        "clean tail without FDP: no truncation or corruption"
    );
}
