//! Checks that the extra state changes tied to publishing a query file commit or roll back together with the publish
//! itself. The winning attempt applies its staged changes exactly once, right after the file is published and just
//! before peers are notified; a losing or failed attempt undoes them before anything is served.
use hef::writer::publish::{HefPublisher, NoopPeerNotices, RecordingObserver};

use crate::support;
use hef::invariants::sim::{SerialEncodeExecutor, SimulatedPublishedSet};
/// conformance: hef-write-path/hef-publish-side-effect-transaction-discipline/side-effect-rolled-back-on-lost-publish
#[test]
fn side_effect_rolled_back_on_lost_publish() {
    // Staged derived-state changes are recorded under the publish attempt id; a winning attempt promotes them at
    // after_hef_publish_before_peer_notice, and a losing/failed attempt rolls them back before anything is served.
    let mut world = support::World::new(131);
    let range = world.ingest(2);
    // Losing attempt: staged verification fails.
    let mut loser = HefPublisher::new();
    loser.quota_bytes = Some(1);
    let mut set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let config = support::build_config();
    let _ = loser
        .publish_range(
            &world.storage,
            support::SHARD,
            1,
            1,
            None,
            range,
            &config,
            &mut set,
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap_err();
    assert_eq!(observer.staged, vec![1]);
    assert_eq!(observer.rolled_back, vec![1], "rollback before serving reads");
    assert!(observer.promoted.is_empty());
    // Winning attempt: promotion happens exactly once, after publication, before the peer notice.
    let mut winner = HefPublisher::new();
    let mut observer = RecordingObserver::default();
    winner
        .publish_range(
            &world.storage,
            support::SHARD,
            1,
            1,
            None,
            range,
            &config,
            &mut set,
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    assert_eq!(observer.staged, vec![1]);
    assert_eq!(observer.promoted, vec![1]);
    assert!(observer.rolled_back.is_empty());
}
