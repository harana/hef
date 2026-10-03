//! Checks that the durable event log is the single source of truth for replaying events. The fast-retry lookup index is
//! only a convenience copy: if one of its rows disagrees with the log it is repaired from the log, and if a row is
//! missing entirely the whole index is rebuilt from the log alone.
use crate::support;
use hef::artifacts::segment::replay_segment;
use hef::events::SequencePoint;
use hef::writer::retry::{RetryReceipt, SafeRetryStore, StatusClass};

/// conformance: hef-physical-artifacts/hej-is-the-durable-event-replay-source/safe-retry-row-disagrees-with-hej
#[test]
fn safe_retry_row_disagrees_with_hej() {
    // A safe-retry row that disagrees with the HEJ event bytes is repaired from the journal: HEJ wins for event replay.
    let mut world = support::World::new(41);
    world.ingest(2);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let mut store = SafeRetryStore::new();
    store.record(RetryReceipt {
        delivery_identity: (5_000, 1),
        tenant_id: support::tenant(),
        commit: SequencePoint {
            epoch: 9,
            sequence: 999,
        }, // wrong
        shard: support::SHARD,
        frame_offset: 0,
        status_class: StatusClass::Acknowledged,
        expiry_physical_nanos: i64::MAX,
        dedupe: (0, 0), // wrong
    });
    store.reconstruct_from_replay(&replay.frames, support::SHARD, i64::MAX);
    let repaired = store.lookup(support::tenant(), (5_000, 1)).unwrap();
    assert_eq!(repaired.commit, SequencePoint { epoch: 1, sequence: 1 });
    assert_eq!(repaired.dedupe, (1_000, 7));
}

/// conformance: hef-physical-artifacts/hej-is-the-durable-event-replay-source/missing-safe-retry-row-after-recovery
#[test]
fn missing_safe_retry_row_after_recovery() {
    // A missing row is rebuilt from HEJ dedupe_hash fields and sequence positions alone.
    let mut world = support::World::new(43);
    world.ingest(3);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let mut store = SafeRetryStore::new();
    assert!(store.is_empty());
    store.reconstruct_from_replay(&replay.frames, support::SHARD, i64::MAX);
    assert_eq!(store.len(), 3);
    let receipt = store.lookup(support::tenant(), (5_001, 1)).unwrap();
    assert_eq!(receipt.commit.sequence, 2);
    assert_eq!(receipt.dedupe, (1_001, 7));
}
