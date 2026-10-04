use super::super::build::BuildLifecycle;
use super::super::pipeline::{FlushReason, RouteDependency, WorkerCommitPipeline};
use super::super::reserve::SequenceAllocator;
use super::super::retry::SafeRetryStore;
use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::artifacts::overlay::LiveOverlayStore;
use crate::artifacts::watermark::WatermarkTracker;
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::invariants::sim::{SerialEncodeExecutor, SimClock, SimJournalStorage, SimulatedPublishedSet};
use crate::layout::LayoutTargets;
use crate::layout::reader::HefFile;
use crate::object_store::sim::{ObjectFault, SimObjectStore};
use crate::typed_id::TypedIdTestExt;

fn event(i: u64) -> EventInput {
    EventInput {
        envelope: EventEnvelope {
            event_id: EventId::new_test_id(0xCAFE + u128::from(i)),
            tenant_id: TenantId::new_test_id(9),
            stream_id: StreamId(1),
            stream_sequence: i,
            occurred_at: TimestampValue::from_physical_nanos(100 + i as i64),
            ingested_at: TimestampValue::from_physical_nanos(200 + i as i64),
            source: "crm".into(),
            event_type: "deal.updated".into(),
            entity_type: "opportunity".into(),
            entity_id_hash_low: i,
            entity_id_hash_high: 0,
            entity_id: Some(format!("opp-{i}")),
            actor_id_hash_low: 0,
            actor_id: None,
            account_id_hash_low: 0,
            account_id: None,
            trace_id_hash_low: 0,
            dedupe_hash_low: 1000 + i,
            dedupe_hash_high: 7,
            schema_version: 1,
            flags: EventFlags(0),
        },
        payload: PayloadInput::Variant(VariantValue::Int(i as i64)),
        source_schema: None,
        source_delivery: None,
        connector_delivery_hash_low: 5000 + i,
        connector_delivery_hash_high: 1,
        provenance: None,
        relationships: None,
    }
}

fn build_config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 1,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 0,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets::default(),
        tenant_id: TenantId::new_test_id(9),
    }
}

struct World {
    allocator: SequenceAllocator,
    clock: SimClock,
    retry: SafeRetryStore,
    storage: SimJournalStorage,
    watermarks: WatermarkTracker,
    worker: WorkerCommitPipeline,
}

fn ingest(count: u64) -> (World, SequenceRange) {
    let clock = SimClock::new(99);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SafeRetryStore::new();
    let mut worker = WorkerCommitPipeline::new(
        1,
        ShardId(0),
        TenantId::new_test_id(9),
        RouteDependency::AppendOnly,
        1 << 20,
    );
    for i in 0..count {
        worker.submit(event(i), 1, &clock).unwrap();
    }
    let result = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &clock,
        )
        .unwrap();
    let range = result.range;
    (
        World {
            clock,
            storage,
            allocator,
            watermarks,
            retry,
            worker,
        },
        range,
    )
}

#[test]
fn publish_boundary_end_to_end() {
    let (world, range) = ingest(5);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    // The entry is Active, manifest-visible, and the staged side effects were promoted before the (single) peer notice.
    assert_eq!(published.entry.part_state, PartState::Active);
    assert_eq!(observer.promoted, vec![1]);
    assert!(observer.rolled_back.is_empty());
    assert_eq!(notices.published.len(), 1);
    let (generation_id, head) = published_set.head().unwrap();
    assert_eq!(generation_id, published.generation);
    assert!(head.covers(&range, TenantId::new_test_id(9)));
    // The published file opens and validates against the manifest entry's authoritative segment seal.
    let file = HefFile::open(published.file_bytes.clone(), Some(&published.entry.file_seal)).unwrap();
    assert_eq!(file.header().row_count, 5);
    // LiveOverlay segments for the covered range become evictable only now; an unpublished range stays.
    let mut overlay = LiveOverlayStore::new();
    let replay = crate::artifacts::segment::replay_segment(&world.storage, ShardId(0), 1, 1).unwrap();
    overlay.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert_eq!(overlay.segment_count(), 1);
    let evicted = overlay.evict_covered(&head);
    assert_eq!(evicted, 1);
    // Retention gate: published coverage + safety window.
    assert!(!journal_retention_can_advance(
        &head,
        &range,
        TenantId::new_test_id(9),
        false,
        false
    ));
    assert!(journal_retention_can_advance(
        &head,
        &range,
        TenantId::new_test_id(9),
        false,
        true
    ));
}

#[test]
fn republish_same_range_is_idempotent_with_an_encrypted_footer() {
    // Regression: the footer nonce was drawn at random per build, so rebuilding an identical range produced the same
    // deterministic `file_id` but a different BLAKE3. A retry after a successful publish then failed permanently as
    // "different content" (issue #9610). The seal is deterministic, so the retry matches the manifest entry.
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let mut config = build_config();
    config.footer_encryption = crate::security::FooterEncryption::Encrypted;
    config.footer_dek = Some([7u8; 32]);
    let publish = |publisher: &mut HefPublisher,
                   published_set: &mut SimulatedPublishedSet,
                   observer: &mut RecordingObserver,
                   notices: &mut NoopPeerNotices| {
        publisher.publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            published_set,
            &SimObjectStore::new(),
            observer,
            notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
    };
    let first = publish(&mut publisher, &mut published_set, &mut observer, &mut notices).unwrap();
    let second = publish(&mut publisher, &mut published_set, &mut observer, &mut notices)
        .expect("a retry of an encrypted-footer publication rebuilds byte-identical content");
    assert_eq!(first.entry.file_id, second.entry.file_id);
    assert_eq!(first.entry.file_seal, second.entry.file_seal);
    assert_eq!(first.file_bytes, second.file_bytes);
    assert_eq!(published_set.head().unwrap().0, first.generation);
    assert_eq!(notices.published.len(), 1);
}

#[test]
fn republish_same_range_is_idempotent() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let config = build_config();
    let first = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    let second = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    // Same file identity, no second manifest generation, no double-count, no second peer notice.
    assert_eq!(first.entry.file_id, second.entry.file_id);
    assert_eq!(published_set.head().unwrap().0, first.generation);
    assert_eq!(notices.published.len(), 1);
    assert_eq!(
        published_set.head().unwrap().1.files.len(),
        1,
        "re-publication must not add a second covering file"
    );
    // The returned bytes must actually hash-validate against the returned entry.
    HefFile::open(second.file_bytes, Some(&second.entry.file_seal)).unwrap();
}

#[test]
fn republish_with_changed_build_config_conflicts_instead_of_pairing_mismatched_bytes() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let first_config = build_config();
    publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &first_config,
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    // A changed build config (here, the created-at timestamp) rebuilds the same range into different bytes.
    let mut second_config = build_config();
    second_config.created_at_physical = 2;
    let result = publisher.publish_range(
        &world.storage,
        ShardId(0),
        1,
        1,
        None,
        range,
        &second_config,
        &mut published_set,
        &SimObjectStore::new(),
        &mut observer,
        &mut notices,
        &world.clock,
        &SerialEncodeExecutor,
    );
    // The rebuilt bytes do not hash-validate against the already-covering entry, so the mismatch must surface as a
    // failure rather than a `Published` pairing the existing entry with bytes it doesn't validate against.
    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    assert_eq!(
        published_set.head().unwrap().1.files.len(),
        1,
        "the conflicting rebuild must not be published as a second file"
    );
}

fn covering_entry(range: SequenceRange, tenant: TenantId, part_state: PartState) -> HefFileEntry {
    HefFileEntry {
        coverage: range,
        feature_metadata: None,
        file_seal: [0u8; 32],
        file_id: 0,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state,
        required_feature_flags: 0,
        size_bytes: 0,
        tenant_id: tenant,
        tree_len: None,
    }
}

#[test]
fn a_covering_entry_of_another_tenant_or_a_retired_entry_does_not_short_circuit() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();

    // Seed a manifest generation that covers the range with entries that must NOT satisfy idempotency: one owned by a
    // different tenant, and one owned by this tenant but already retired. Neither is a live coverage of this tenant's
    // range, so publication must proceed and actually publish rather than returning one of them as already-covered.
    let seeded = ManifestGeneration {
        files: vec![
            covering_entry(range, TenantId::new_test_id(999), PartState::Active),
            covering_entry(range, TenantId::new_test_id(9), PartState::DeleteOnDestroy),
        ],
        generation: 1,
        ..Default::default()
    };
    published_set.put_generation(seeded).unwrap();
    published_set.advance_head(0, 1).unwrap();

    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();

    // A real publication happened instead of short-circuiting: the returned entry is this tenant's freshly built Active
    // file (not a seeded stub), a peer notice fired, and the manifest gained the new covering file.
    assert_eq!(published.entry.tenant_id, TenantId::new_test_id(9));
    assert_eq!(published.entry.part_state, PartState::Active);
    assert_ne!(
        published.entry.file_id, 0,
        "the published entry is the freshly built file, not the seeded stub"
    );
    assert_eq!(
        notices.published.len(),
        1,
        "short-circuiting on a foreign/retired entry would emit no notice"
    );
    let head = published_set.head().unwrap().1;
    assert!(
        head.files.iter().any(|e| {
            e.tenant_id == TenantId::new_test_id(9)
                && e.part_state == PartState::Active
                && e.file_id == published.entry.file_id
        }),
        "the newly published Active file must be in the manifest"
    );
}

/// A `PublishedSet` that injects a competing publication between the loser's head read and its CAS, through the
/// interface — production code runs unmodified.
struct RacingSet {
    inner: SimulatedPublishedSet,
    race_armed: bool,
}

impl PublishedSet for RacingSet {
    fn head(&self) -> Result<(u64, ManifestGeneration), PublishError> {
        self.inner.head()
    }
    fn put_generation(&mut self, generation: ManifestGeneration) -> Result<(), PublishError> {
        if self.race_armed {
            self.race_armed = false;
            // The rival wins the same generation id first.
            let (head_id, head) = self.inner.head()?;
            let rival = ManifestGeneration {
                generation: head_id + 1,
                files: head.files,
                ..Default::default()
            };
            self.inner.put_generation(rival)?;
            self.inner.advance_head(head_id, head_id + 1)?;
        }
        self.inner.put_generation(generation)
    }
    fn advance_head(&mut self, expected: u64, next: u64) -> Result<(), PublishError> {
        self.inner.advance_head(expected, next)
    }
    fn generation(&self, id: u64) -> Result<ManifestGeneration, PublishError> {
        self.inner.generation(id)
    }
}

#[test]
fn lost_cas_rebases_and_retries_never_overwrites() {
    let (world, range) = ingest(3);
    let mut publisher = HefPublisher::new();
    let mut racing = RacingSet {
        inner: SimulatedPublishedSet::new(),
        race_armed: true,
    };
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut racing,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    // The loser rebased onto the rival's generation and won the next one; the rival's generation object was never
    // overwritten.
    assert_eq!(published.generation, 2);
    let rival = racing.inner.generation(1).unwrap();
    assert!(rival.files.is_empty());
    let winner = racing.inner.generation(2).unwrap();
    assert_eq!(winner.files.len(), 1);
}

#[test]
fn failed_publish_leaks_nothing() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    publisher.quota_bytes = Some(16); // staged verification must fail
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let error = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap_err();
    assert!(matches!(error, PublishFailure::Verification(_)));
    // No peer frame, staged side effects discarded, no public read can observe the attempt (the manifest never
    // changed).
    assert!(notices.published.is_empty());
    assert_eq!(observer.rolled_back, vec![1]);
    assert!(observer.promoted.is_empty());
    let (generation_id, head) = published_set.head().unwrap();
    assert_eq!(generation_id, 0);
    assert!(head.files.is_empty());
}

#[test]
fn dual_roll_trigger_bytes_or_time() {
    let clock = SimClock::new(5);
    let policy = RollPolicy::default();
    // High volume: the byte target fires before the window.
    let state = OpenFileState {
        opened_at_monotonic_nanos: clock.monotonic_nanos(),
        compressed_bytes: policy.byte_target,
    };
    assert_eq!(should_roll(&state, &policy, &clock), Some(RollTrigger::ByteTarget));
    // Low volume: the open-time window fires even far below the byte target, so retention can advance.
    let state = OpenFileState {
        opened_at_monotonic_nanos: clock.monotonic_nanos(),
        compressed_bytes: 1024,
    };
    assert_eq!(should_roll(&state, &policy, &clock), None);
    clock.advance(policy.max_open_nanos + 1);
    assert_eq!(should_roll(&state, &policy, &clock), Some(RollTrigger::TimeWindow));
}

#[test]
fn unpublished_file_is_not_visible() {
    let (world, range) = ingest(3);
    // A file is built and exists "on storage" but no manifest entry references it: snapshots see nothing; the journal
    // range is still served from HEJ/LiveOverlay.
    let head = SimulatedPublishedSet::new().head().unwrap().1;
    assert!(!head.covers(&range, TenantId::new_test_id(9)));
    assert_eq!(head.snapshot_files().count(), 0);
    let mut overlay = LiveOverlayStore::new();
    let replay = crate::artifacts::segment::replay_segment(&world.storage, ShardId(0), 1, 1).unwrap();
    overlay
        .rebuild_from_replay(&replay, 1, 1, |t, r| head.covers(r, t))
        .unwrap();
    assert_eq!(overlay.segment_count(), 1, "range still served from LiveOverlay");
    assert_eq!(overlay.evict_covered(&head), 0, "premature eviction prevented");
}

#[test]
fn publish_skips_voids_but_requires_contiguity() {
    let (mut world, range) = ingest(2);
    // Abandon a reservation and close it with a void record, then ingest more events after the gap.
    let lease = world.allocator.reserve(3, world.clock.monotonic_nanos());
    assert_eq!(lease.range.first_sequence, range.last_sequence + 1);
    world.clock.advance(super::super::reserve::LEASE_NANOS + 1);
    world
        .worker
        .commit_voids(
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &world.clock,
        )
        .unwrap();
    world.worker.submit(event(10), 1, &world.clock).unwrap();
    let tail = world
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
    // Publish the whole durable range including the interior void.
    let full = SequenceRange {
        epoch: 1,
        first_sequence: range.first_sequence,
        last_sequence: tail.range.last_sequence,
    };
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            full,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    // Voided sequences are permanently skipped: 3 published rows from 6 covered sequences, and the void range is never
    // a HEF row.
    let file = HefFile::open(published.file_bytes, None).unwrap();
    assert_eq!(file.header().row_count, 3);
    assert!(published_set.head().unwrap().1.covers(&full, TenantId::new_test_id(9)));
}

#[test]
fn retention_advances_over_a_range_that_holds_only_voids() {
    // A sealed range whose every frame is a void record can never be published — a HEF file must carry a row — so
    // gating its journal segment on manifest coverage pinned it forever. The voids themselves release it (issue
    // #7491).
    let (mut world, range) = ingest(2);
    let lease = world.allocator.reserve(3, world.clock.monotonic_nanos());
    let void_range = lease.range;
    assert_eq!(void_range.first_sequence, range.last_sequence + 1);
    world.clock.advance(super::super::reserve::LEASE_NANOS + 1);
    world
        .worker
        .commit_voids(
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &world.clock,
        )
        .unwrap();

    // The all-void range is durable, and publishing it is refused: there is nothing to put in a file.
    let replay = crate::artifacts::segment::replay_segment(&world.storage, ShardId(0), 1, 1).unwrap();
    assert!(range_holds_no_events(&replay, TenantId::new_test_id(9), &void_range).unwrap());
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let failure = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            void_range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut RecordingObserver::default(),
            &mut NoopPeerNotices::default(),
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap_err();
    assert!(matches!(failure, PublishFailure::Verification(_)));

    // No manifest entry covers it, and none ever will — retention advances on the void coverage once the safety
    // window has elapsed, and not before.
    let head = published_set.head().unwrap().1;
    assert!(!head.covers(&void_range, TenantId::new_test_id(9)));
    assert!(!journal_retention_can_advance(
        &head,
        &void_range,
        TenantId::new_test_id(9),
        true,
        false
    ));
    assert!(journal_retention_can_advance(
        &head,
        &void_range,
        TenantId::new_test_id(9),
        true,
        true
    ));

    // A range with events in it is not all-void, so it still waits for its file.
    assert!(!range_holds_no_events(&replay, TenantId::new_test_id(9), &range).unwrap());
}

/// A build configuration with a small stripe target so a few dozen rows form several stripes, exercising the
/// stripe-aligned upload segmentation.
fn multi_stripe_config() -> HefBuildConfig {
    HefBuildConfig {
        targets: LayoutTargets {
            index_granularity: 8,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 512,
        },
        ..build_config()
    }
}

#[test]
fn hef_upload_segments_cut_the_file_at_stripe_boundaries() {
    let rows: Vec<HefRow> = (0..40)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: event(i),
        })
        .collect();
    let built = crate::writer::build::build_hef_file(rows, &multi_stripe_config()).unwrap();
    assert!(built.footer.stripes.len() >= 2, "test needs a multi-stripe file");

    let segments = hef_upload_segments(&built);
    // The segments partition the whole file exactly.
    assert_eq!(segments.iter().sum::<u64>(), built.bytes.len() as u64);

    // Every stripe begins on a segment boundary, so a multipart upload aligned to these segments never crosses a stripe.
    let mut boundaries = vec![0u64];
    let mut running = 0u64;
    for len in &segments {
        running += len;
        boundaries.push(running);
    }
    for stripe in &built.footer.stripes {
        assert!(
            boundaries.contains(&stripe.file_offset),
            "stripe offset {} falls on a segment boundary",
            stripe.file_offset
        );
    }
}

#[test]
fn publish_rejects_replayed_chain_that_does_not_match_recorded_anchor() {
    let (world, range) = ingest(4);
    let replay = crate::artifacts::segment::replay_segment(&world.storage, ShardId(0), 1, 1).unwrap();
    let recorded = replay.segment_chain_blake3.unwrap();

    // A recorded anchor the replayed chain actually produces publishes normally.
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            Some(recorded),
            range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    assert_eq!(published_set.head().unwrap().1.files.len(), 1);

    // A recorded anchor the replayed chain does NOT match (stale/wrong anchor, or reordered/removed frames) is refused
    // before anything is staged.
    let mut wrong = recorded;
    wrong[0] ^= 0xFF;
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let result = publisher.publish_range(
        &world.storage,
        ShardId(0),
        1,
        1,
        Some(wrong),
        range,
        &build_config(),
        &mut published_set,
        &SimObjectStore::new(),
        &mut observer,
        &mut notices,
        &world.clock,
        &SerialEncodeExecutor,
    );
    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    // Rejected before staging: no manifest change, no staged side effects, no peer notice.
    assert_eq!(published_set.head().unwrap().0, 0);
    assert!(observer.staged.is_empty());
    assert!(observer.rolled_back.is_empty());
    assert!(notices.published.is_empty());
}

#[test]
fn publish_is_not_short_circuited_by_another_tenants_covering_entry() {
    let (world, range) = ingest(4);
    // A manifest already carries a covering entry for a DIFFERENT tenant over the same range.
    let mut published_set = SimulatedPublishedSet::new();
    let other_tenant_entry = HefFileEntry {
        coverage: range,
        feature_metadata: None,
        file_seal: [7u8; 32],
        file_id: 999,
        file_type: FileType::HefFile,
        footer_len: None,
        optional_feature_flags: 0,
        part_index: 0,
        part_state: PartState::Active,
        required_feature_flags: 0,
        size_bytes: 1,
        tenant_id: TenantId::new_test_id(42),
        tree_len: None,
    };
    published_set
        .put_generation(ManifestGeneration {
            files: vec![other_tenant_entry],
            generation: 1,
            ..Default::default()
        })
        .unwrap();
    published_set.advance_head(0, 1).unwrap();

    // Publishing our tenant's identical range must not be short-circuited by the other tenant's entry.
    let mut publisher = HefPublisher::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    assert_eq!(published.entry.tenant_id, TenantId::new_test_id(9));
    let (_, head) = published_set.head().unwrap();
    assert_eq!(
        head.files.len(),
        2,
        "our file is published alongside the other tenant's entry"
    );
    assert_eq!(
        head.files
            .iter()
            .filter(|e| e.tenant_id == TenantId::new_test_id(9))
            .count(),
        1
    );
}

/// A manifest holding a single `Active` entry covering `range` for `tenant_id`.
fn manifest_covering(range: SequenceRange, tenant_id: TenantId) -> ManifestGeneration {
    ManifestGeneration {
        files: vec![HefFileEntry {
            coverage: range,
            feature_metadata: None,
            file_seal: [0u8; 32],
            file_id: 1,
            file_type: FileType::HefFile,
            footer_len: None,
            optional_feature_flags: 0,
            part_index: 0,
            part_state: PartState::Active,
            required_feature_flags: 0,
            size_bytes: 1,
            tenant_id,
            tree_len: None,
        }],
        generation: 1,
        ..Default::default()
    }
}

/// A crash between `put_generation` and `advance_head` leaves generation 1 written but the head at 0. A republish of
/// the same range whose rebuilt content differs (a changed build config) must refuse to adopt the orphan: advancing
/// the head would publish an entry this attempt never built while peers are told about the freshly rebuilt one.
#[test]
fn interrupted_publish_with_changed_content_is_refused_not_adopted() {
    let (world, range) = ingest(4);
    let mut published_set = SimulatedPublishedSet::new();
    // The orphan's covering entry carries a different file identity than the deterministic rebuild produces.
    published_set
        .put_generation(manifest_covering(range, TenantId::new_test_id(9)))
        .unwrap();

    let mut publisher = HefPublisher::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let result = publisher.publish_range(
        &world.storage,
        ShardId(0),
        1,
        1,
        None,
        range,
        &build_config(),
        &mut published_set,
        &SimObjectStore::new(),
        &mut observer,
        &mut notices,
        &world.clock,
        &SerialEncodeExecutor,
    );
    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    assert_eq!(
        published_set.head().unwrap().0,
        0,
        "the head must not advance over mismatched content"
    );
    assert!(notices.published.is_empty());
    assert_eq!(observer.rolled_back, vec![1]);
}

/// The recovery counterpart: when the orphaned generation holds exactly the entry this attempt rebuilt (same config,
/// same bytes), the republish completes the interrupted publication by advancing the head to it.
#[test]
fn interrupted_publish_of_identical_content_completes_by_advancing_the_head() {
    let (world, range) = ingest(4);
    let config = build_config();
    // A scratch publication yields the exact entry the pre-crash attempt wrote.
    let mut scratch = SimulatedPublishedSet::new();
    let mut publisher = HefPublisher::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let first = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            &mut scratch,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();

    // Model the crash: generation 1 holds that entry, but the head still points at 0.
    let mut published_set = SimulatedPublishedSet::new();
    published_set
        .put_generation(ManifestGeneration {
            files: vec![first.entry.clone()],
            generation: 1,
            ..Default::default()
        })
        .unwrap();

    let republished = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();
    assert_eq!(republished.entry.file_id, first.entry.file_id);
    assert_eq!(republished.generation, 1);
    assert_eq!(
        published_set.head().unwrap().0,
        1,
        "the head catches up to the already-written generation"
    );
}

/// A retired entry over the same range must not stand in for the entry this attempt is adding. A new generation copies
/// every head entry, so a colliding generation can hold a `DeleteOnDestroy` copy of an earlier file over this range;
/// adopting it would advance the head, promote side effects, and report success for a range no live file covers
/// (issue #6458).
#[test]
fn a_collided_generation_holding_only_a_retired_entry_is_not_adopted() {
    let (world, range) = ingest(4);
    let config = build_config();
    let mut scratch = SimulatedPublishedSet::new();
    let mut publisher = HefPublisher::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let first = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            &mut scratch,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();

    // Generation 1 exists but carries only the retired copy of that same file; the head is still at 0.
    let mut retired = first.entry.clone();
    retired.part_state = PartState::DeleteOnDestroy;
    let mut published_set = SimulatedPublishedSet::new();
    published_set
        .put_generation(ManifestGeneration {
            files: vec![retired],
            generation: 1,
            ..Default::default()
        })
        .unwrap();

    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let republished = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &config,
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();

    // The publisher does not claim the retired entry as its own: it moves past the stalled generation and publishes a
    // generation whose entry is live and covers the range.
    assert!(republished.generation > 1);
    let (head_id, head) = published_set.head().unwrap();
    assert_eq!(head_id, republished.generation);
    assert!(
        head.files
            .iter()
            .any(|entry| entry.part_state == PartState::Active && entry.coverage.contains(&range)),
        "the range ends up covered by a live entry, not a retired one"
    );
}

/// A crashed publisher's orphaned generation for an unrelated range must not wedge every other publish: retrying
/// against the unmoved head recomputes the same colliding id forever. The publisher helps the stalled publication
/// over, rebases onto it, and publishes its own range in the next generation.
#[test]
fn orphaned_generation_for_another_range_does_not_wedge_the_publish() {
    let (world, range) = ingest(4);
    let orphan_range = SequenceRange {
        epoch: 1,
        first_sequence: 900,
        last_sequence: 950,
    };
    let mut published_set = SimulatedPublishedSet::new();
    published_set
        .put_generation(manifest_covering(orphan_range, TenantId::new_test_id(9)))
        .unwrap();

    let mut publisher = HefPublisher::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut published_set,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .expect("an orphaned foreign generation must not block other ranges");

    assert_eq!(published.generation, 2);
    let (head_id, head) = published_set.head().unwrap();
    assert_eq!(head_id, 2);
    assert!(head.covers(&range, TenantId::new_test_id(9)));
    assert!(
        head.covers(&orphan_range, TenantId::new_test_id(9)),
        "helping over the stalled generation publishes its entry too"
    );
    // Advancing the head over the stalled generation made its entry query-visible, so this publisher notifies peers of
    // that entry on the crashed publisher's behalf, then notifies its own — both newly-live files are announced (issue
    // #9854), never left query-visible with no peer notice.
    assert_eq!(
        notices.published.len(),
        2,
        "both the rescued orphan's entry and this publisher's own entry are notified"
    );
}

/// A `PublishedSet` that lets a rival advance the head over the caller's just-written generation before the caller's
/// own `advance_head` runs: the first `advance_head` performs the move itself (making the caller's entry live) and then
/// reports `CasLost`, exactly as a concurrent publisher completing a different range would.
struct HeadStolenSet {
    inner: SimulatedPublishedSet,
    steal_armed: bool,
}

impl PublishedSet for HeadStolenSet {
    fn head(&self) -> Result<(u64, ManifestGeneration), PublishError> {
        self.inner.head()
    }
    fn put_generation(&mut self, generation: ManifestGeneration) -> Result<(), PublishError> {
        self.inner.put_generation(generation)
    }
    fn advance_head(&mut self, expected: u64, next: u64) -> Result<(), PublishError> {
        if self.steal_armed {
            self.steal_armed = false;
            // A rival advances the head over the generation this attempt just wrote, then this attempt loses the CAS.
            self.inner.advance_head(expected, next)?;
            return Err(PublishError::CasLost {
                current_generation: next,
            });
        }
        self.inner.advance_head(expected, next)
    }
    fn generation(&self, id: u64) -> Result<ManifestGeneration, PublishError> {
        self.inner.generation(id)
    }
}

/// The core of issue #9854: publisher A writes its generation, and a concurrent publisher advances the head over it
/// before A's own `advance_head` runs. A's entry is now live and query-visible, but A never promoted its staged side
/// effects or notified peers. On rediscovering its own live entry, A must complete the publication it won — promote and
/// notify — rather than rolling the side effects back and leaving a live file its derived state and peers never saw.
#[test]
fn a_winning_generation_the_head_was_advanced_over_is_completed_not_rolled_back() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut stolen = HeadStolenSet {
        inner: SimulatedPublishedSet::new(),
        steal_armed: true,
    };
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut stolen,
            &SimObjectStore::new(),
            &mut observer,
            &mut notices,
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();

    // The entry this attempt built is the one that is live, and the publication was completed rather than rolled back.
    assert_eq!(published.generation, 1);
    assert_eq!(
        observer.promoted,
        vec![1],
        "the won publication promotes its staged side effects"
    );
    assert!(
        observer.rolled_back.is_empty(),
        "a live, query-visible file must not have had its side effects rolled back"
    );
    assert_eq!(notices.published.len(), 1, "peers are notified of the live file");
    let (_, head) = stolen.head().unwrap();
    assert!(head.covers(&range, TenantId::new_test_id(9)));
    assert!(
        head.files
            .iter()
            .any(|entry| entry.part_state == PartState::Active && entry.file_id == published.entry.file_id),
        "the attempt's own file is the live covering entry"
    );
}

#[test]
fn covers_and_retention_are_tenant_scoped() {
    let range = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 10,
    };
    // Only a tenant-B entry covers the range.
    let manifest = manifest_covering(range, TenantId::new_test_id(2));
    // Tenant A's identical range is not covered by tenant B's file; tenant B's is.
    assert!(!manifest.covers(&range, TenantId::new_test_id(1)));
    assert!(manifest.covers(&range, TenantId::new_test_id(2)));
    // Retention for tenant A cannot advance on tenant B's coverage, even with the safety window elapsed.
    assert!(!journal_retention_can_advance(
        &manifest,
        &range,
        TenantId::new_test_id(1),
        false,
        true
    ));
    assert!(journal_retention_can_advance(
        &manifest,
        &range,
        TenantId::new_test_id(2),
        false,
        true
    ));
}

#[test]
fn evict_covered_does_not_cross_tenants() {
    // The overlay segment is built from tenant-9 frames (see `event`).
    let (world, range) = ingest(3);
    let mut overlay = LiveOverlayStore::new();
    let replay = crate::artifacts::segment::replay_segment(&world.storage, ShardId(0), 1, 1).unwrap();
    overlay.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert_eq!(overlay.segment_count(), 1);

    // A different tenant's covering entry over the identical range must not evict our segment.
    let other_tenant = manifest_covering(range, TenantId::new_test_id(42));
    assert_eq!(overlay.evict_covered(&other_tenant), 0);
    assert_eq!(overlay.segment_count(), 1);

    // Our own tenant's covering entry does evict it.
    let own_tenant = manifest_covering(range, TenantId::new_test_id(9));
    assert_eq!(overlay.evict_covered(&own_tenant), 1);
    assert_eq!(overlay.segment_count(), 0);
}

/// One event frame for `tenant`, covering `first_sequence..=first_sequence + count - 1` in epoch 1.
fn frame_for(tenant: u128, first_sequence: u64, count: u64) -> Vec<u8> {
    let events: Vec<EventInput> = (0..count)
        .map(|i| {
            let mut event = event(first_sequence + i);
            event.envelope.tenant_id = TenantId::new_test_id(tenant);
            event
        })
        .collect();
    let payload = crate::artifacts::batch::build_batch(&events, 1, 0).unwrap();
    crate::artifacts::frame::build_frame(
        &crate::artifacts::frame::FrameBuildInput {
            flags: 0,
            tenant_id: TenantId::new_test_id(tenant),
            writer_id: 1,
            epoch: 1,
            first_sequence,
            last_sequence: first_sequence + count - 1,
            event_count: count as u32,
            durable_batch_id: first_sequence,
            writer_local_batch_id: first_sequence,
            schema_generation: 1,
            dictionary_generation_hint: 0,
            created_at_physical: 1,
            committed_at_physical: 2,
        },
        &payload,
    )
    .unwrap()
}

/// Publishes `range` from a journal holding exactly `frames`, in the order given.
fn publish_frames(frames: &[Vec<u8>], range: SequenceRange) -> Result<Published, PublishFailure> {
    let clock = SimClock::new(99);
    let mut storage = SimJournalStorage::new();
    for frame in frames {
        storage.append(ShardId(0), frame).unwrap();
    }
    storage.sync(ShardId(0)).unwrap();
    HefPublisher::new().publish_range(
        &storage,
        ShardId(0),
        1,
        1,
        None,
        range,
        &build_config(),
        &mut SimulatedPublishedSet::new(),
        &SimObjectStore::new(),
        &mut RecordingObserver::default(),
        &mut NoopPeerNotices::default(),
        &clock,
        &SerialEncodeExecutor,
    )
}

#[test]
fn frames_made_durable_out_of_sequence_order_still_publish() {
    // Contiguity used to be checked in durable-commit order, so a range whose later sequences were made durable first
    // was rejected as `RangeNotDurable` and kept its journal segment pinned. Autonomous workers and replication both
    // complete out of order, so the accepted frames must be sorted by sequence first (issue #8461).
    let range = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 6,
    };
    let published = publish_frames(&[frame_for(9, 4, 3), frame_for(9, 1, 3)], range).expect("the range is durable");
    let file = HefFile::open(published.file_bytes.clone(), Some(&published.entry.file_seal)).unwrap();
    assert_eq!(file.header().row_count, 6, "every row of the range is published");
}

#[test]
fn another_tenants_overlapping_frame_neither_blocks_publication_nor_joins_its_coverage() {
    // Frame selection ignored `tenant_id`, so another tenant's frame over the same epoch and sequences arbitrated
    // against this range: it could be treated as the durable cover for sequences 1..=3 and block publication of the
    // real ones (issue #8922).
    let range = SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 3,
    };
    let published =
        publish_frames(&[frame_for(42, 1, 3), frame_for(9, 1, 3)], range).expect("our own frame covers the range");
    let file = HefFile::open(published.file_bytes.clone(), Some(&published.entry.file_seal)).unwrap();
    assert_eq!(file.header().row_count, 3, "only this tenant's rows are published");

    // With only the other tenant's frame on the journal, the range is not durable for us at all.
    assert!(matches!(
        publish_frames(&[frame_for(42, 1, 3)], range),
        Err(PublishFailure::RangeNotDurable)
    ));
}

#[test]
fn a_failed_upload_never_reaches_put_generation() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let objects = SimObjectStore::new();
    objects.inject(ObjectFault::FailPutIfAbsent);
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();

    let result = publisher.publish_range(
        &world.storage,
        ShardId(0),
        1,
        1,
        None,
        range,
        &build_config(),
        &mut published_set,
        &objects,
        &mut observer,
        &mut notices,
        &world.clock,
        &SerialEncodeExecutor,
    );

    assert!(matches!(result, Err(PublishFailure::Storage(_))));
    assert_eq!(
        published_set.generation(1),
        Err(PublishError::UnknownGeneration),
        "no generation was written"
    );
    assert_eq!(published_set.head().unwrap().0, 0);
    assert_eq!(observer.rolled_back, vec![1]);
    assert!(notices.published.is_empty());
    assert!(objects.keys().is_empty());
}

#[test]
fn a_published_file_is_uploaded_under_its_object_key() {
    let (world, range) = ingest(4);
    let mut publisher = HefPublisher::new();
    let mut published_set = SimulatedPublishedSet::new();
    let objects = SimObjectStore::new();
    let published = publisher
        .publish_range(
            &world.storage,
            ShardId(0),
            1,
            1,
            None,
            range,
            &build_config(),
            &mut published_set,
            &objects,
            &mut RecordingObserver::default(),
            &mut NoopPeerNotices::default(),
            &world.clock,
            &SerialEncodeExecutor,
        )
        .unwrap();

    let key = hef_object_key(TenantId::new_test_id(9), published.entry.file_id);
    assert_eq!(objects.object(&key).unwrap(), published.file_bytes);
}

/// A `PublishedSet` where a rival publishes a different file over the same range between the caller's head read and
/// its own generation write, so the caller loses after it has already uploaded.
struct RivalCoversSet {
    inner: SimulatedPublishedSet,
    rival: Option<ManifestGeneration>,
}

impl PublishedSet for RivalCoversSet {
    fn head(&self) -> Result<(u64, ManifestGeneration), PublishError> {
        self.inner.head()
    }
    fn put_generation(&mut self, generation: ManifestGeneration) -> Result<(), PublishError> {
        if let Some(rival) = self.rival.take() {
            let rival_id = rival.generation;
            self.inner.put_generation(rival)?;
            self.inner.advance_head(rival_id - 1, rival_id)?;
        }
        self.inner.put_generation(generation)
    }
    fn advance_head(&mut self, expected: u64, next: u64) -> Result<(), PublishError> {
        self.inner.advance_head(expected, next)
    }
    fn generation(&self, id: u64) -> Result<ManifestGeneration, PublishError> {
        self.inner.generation(id)
    }
}

#[test]
fn a_lost_race_after_upload_leaves_the_object_unreferenced() {
    let (world, range) = ingest(4);
    let tenant = TenantId::new_test_id(9);
    let mut publisher = HefPublisher::new();
    let mut racing = RivalCoversSet {
        inner: SimulatedPublishedSet::new(),
        rival: Some(manifest_covering(range, tenant)),
    };
    let objects = SimObjectStore::new();
    let mut observer = RecordingObserver::default();
    let mut notices = NoopPeerNotices::default();

    let result = publisher.publish_range(
        &world.storage,
        ShardId(0),
        1,
        1,
        None,
        range,
        &build_config(),
        &mut racing,
        &objects,
        &mut observer,
        &mut notices,
        &world.clock,
        &SerialEncodeExecutor,
    );

    assert!(matches!(result, Err(PublishFailure::Verification(_))));
    // The upload happened and is left in place; the catalogue only names the rival's file.
    let keys = objects.keys();
    assert_eq!(keys.len(), 1, "the uploaded object is kept, not deleted");
    let (_, head) = racing.inner.head().unwrap();
    assert_eq!(head.files.len(), 1);
    assert_ne!(keys[0], hef_object_key(tenant, head.files[0].file_id));
    assert_eq!(observer.rolled_back, vec![1]);
    assert!(notices.published.is_empty());
}
