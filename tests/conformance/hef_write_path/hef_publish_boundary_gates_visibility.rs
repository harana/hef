//! Checks that publishing a query file is all-or-nothing for readers. When a publish attempt fails its checks partway
//! through, it sends no notice to peers, throws away any half-staged side effects, and leaves the catalogue untouched,
//! so no reader can ever observe the failed attempt.
use hef::invariants::sim::{SerialEncodeExecutor, SimulatedPublishedSet};
use hef::writer::publish::{HefPublisher, NoopPeerNotices, PublishFailure, RecordingObserver};

use crate::support;
use hef::invariants::PublishedSet;
/// conformance: hef-write-path/hef-publish-boundary-gates-visibility/failed-publish-leaks-nothing
#[test]
fn failed_publish_leaks_nothing() {
    // A publish attempt that fails staged verification sends no peer frame, discards staged side effects, and leaves
    // the manifest untouched: no public read can observe the attempt.
    let mut world = support::World::new(121);
    let range = world.ingest(3);
    let mut publisher = HefPublisher::new();
    publisher.quota_bytes = Some(16);
    let mut set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let error = publisher
        .publish_range(
            &world.storage,
            support::SHARD,
            1,
            1,
            None,
            range,
            &support::build_config(),
            &mut set,
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap_err();
    assert!(matches!(error, PublishFailure::Verification(_)));
    assert!(notices.published.is_empty(), "no peer notice leaked");
    assert_eq!(observer.staged, vec![1]);
    assert_eq!(observer.rolled_back, vec![1]);
    assert!(observer.promoted.is_empty());
    let (generation, head) = set.head().unwrap();
    assert_eq!(generation, 0);
    assert!(head.files.is_empty(), "manifest unchanged");
}
