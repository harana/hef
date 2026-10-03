//! Checks the path a new event takes from arrival to "saved". Once its log frame is on durable storage and its
//! fast-retry receipt is recorded, an append-only write is treated as committed right away and the client can be told
//! the write succeeded.

use crate::support;
use hef::writer::pipeline::{CommitState, FlushReason};

/// conformance: hef-write-path/ingest-to-acknowledgement-pipeline/append-only-commit
#[test]
fn append_only_commit() {
    // Once the HEJ frame is durable and the safe-retry receipt recorded, HARDENED becomes COMMITTED immediately for
    // append-only ingest and the client may be acknowledged.
    let mut world = support::World::new(91);
    assert_eq!(
        world.worker.submit(support::event(0), 1, &world.clock).unwrap(),
        CommitState::Ready
    );
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
    assert_eq!(result.receipts.len(), 1);
    assert!(
        world.retry.lookup(support::tenant(), (5_000, 1)).is_some(),
        "receipt recorded"
    );
}
