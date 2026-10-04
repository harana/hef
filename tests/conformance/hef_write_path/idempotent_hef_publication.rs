//! Checks that publishing the same range of events twice is harmless: the second publish reuses the same file identity,
//! adds no new catalogue version, sends only one notice to peers, and never double-counts the data. Cleanup of the
//! durable log only advances once the published file plus a safety margin cover the range.
use hef::invariants::sim::{SerialEncodeExecutor, SimulatedPublishedSet};
use hef::writer::publish::{HefPublisher, NoopPeerNotices, RecordingObserver, journal_retention_can_advance};

use crate::support;
use hef::invariants::PublishedSet;
/// conformance: hef-write-path/idempotent-hef-publication/re-publish-same-range
#[test]
fn re_publish_same_range() {
    // Re-publishing an already-published journal range yields the same file identity, no new manifest generation, and
    // no double-counting; LiveOverlay eviction and HEJ retention gate on the published coverage plus the safety window.
    let mut world = support::World::new(111);
    let range = world.ingest(4);
    let mut publisher = HefPublisher::new();
    let mut set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let config = support::build_config();
    let first = publisher
        .publish_range(
            &world.storage,
            support::SHARD,
            1,
            1,
            None,
            range,
            &config,
            &mut set,
            &hef::object_store::sim::SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    let second = publisher
        .publish_range(
            &world.storage,
            support::SHARD,
            1,
            1,
            None,
            range,
            &config,
            &mut set,
            &hef::object_store::sim::SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    assert_eq!(first.entry.file_id, second.entry.file_id);
    let (head_id, head) = set.head().unwrap();
    assert_eq!(head_id, first.generation, "no extra generation");
    assert_eq!(head.files.len(), 1, "no double-counted coverage");
    assert_eq!(notices.published.len(), 1, "one peer notice");
    assert!(!journal_retention_can_advance(
        &head,
        &range,
        support::tenant(),
        false,
        false
    ));
    assert!(journal_retention_can_advance(
        &head,
        &range,
        support::tenant(),
        false,
        true
    ));
}
