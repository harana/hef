//! Checks that a write is only told "saved" once it is truly safe. There are two promises a client can ask for: "your
//! event is on durable storage" is confirmed the moment the event log lands, while "you can read it back now" waits
//! until the event is also visible to readers. An event that is durable but not yet visible is never silently dropped
//! from results.
use crate::support;
use hef::events::SequenceRange;
use hef::writer::pipeline::{AckVisibility, CommitState, FlushReason, WorkerCommitPipeline};

/// conformance: hef-physical-artifacts/acknowledge-only-after-hej-durability/strict-read-after-ack-visibility
#[test]
fn strict_read_after_ack_visibility() {
    // Under the strict contract, acknowledgement waits until the event is included in visibility_watermark;
    // acknowledged-but-not-yet-visible events are never silently omitted (the gate reports not-ready).
    let mut world = support::World::new(71);
    world.worker.submit(support::event(0), 1, &world.clock).unwrap();
    let result = world
        .worker
        .flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &world.clock,
        )
        .unwrap();
    assert_eq!(result.state, CommitState::Committed);
    let receipt = &result.receipts[0];
    // Durability-mode ack is ready immediately after HEJ durability.
    assert!(WorkerCommitPipeline::ack_ready(
        receipt,
        result.state,
        AckVisibility::Durability,
        &world.watermarks
    ));
    // Strict mode waits until the range is published into LiveOverlay.
    assert!(!WorkerCommitPipeline::ack_ready(
        receipt,
        result.state,
        AckVisibility::ReadAfterAck,
        &world.watermarks
    ));
    world.watermarks.record_visible(SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 1,
    });
    assert!(WorkerCommitPipeline::ack_ready(
        receipt,
        result.state,
        AckVisibility::ReadAfterAck,
        &world.watermarks
    ));
}
