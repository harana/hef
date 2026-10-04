use super::*;
use crate::artifacts::batch::PayloadInput;
use crate::artifacts::overlay::FreshRead;
use crate::artifacts::segment::{ReplayTail, replay_segment};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::invariants::Clock;
use crate::invariants::sim::{Fault, SimClock, SimJournalStorage};
use crate::typed_id::TypedIdTestExt;
use crate::writer::sim::SimSafeRetryStore;

fn event(i: u64) -> EventInput {
    EventInput {
        envelope: EventEnvelope {
            event_id: EventId::new_test_id(0xAA00 + u128::from(i)),
            tenant_id: TenantId::new_test_id(9),
            stream_id: StreamId(1),
            stream_sequence: i,
            occurred_at: TimestampValue::from_physical_nanos(10 + i as i64),
            ingested_at: TimestampValue::from_physical_nanos(20 + i as i64),
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

fn pipeline() -> WorkerCommitPipeline {
    WorkerCommitPipeline::new(
        3,
        ShardId(0),
        TenantId::new_test_id(9),
        RouteDependency::AppendOnly,
        1 << 20,
    )
}

#[test]
fn append_only_flush_commits_after_durability() {
    let clock = SimClock::new(42);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    for i in 0..3 {
        assert_eq!(
            worker.submit(event(i), 1, &mut retry, &clock).unwrap(),
            Submission::Ready
        );
    }
    let result = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    // Append-only: HARDENED == COMMITTED, no dependency machinery.
    assert_eq!(result.state, CommitState::Committed);
    assert_eq!(result.range.first_sequence, 1);
    assert_eq!(result.range.last_sequence, 3);
    assert_eq!(retry.len(), 3);
    assert_eq!(
        watermarks.commit_watermark(),
        Some(crate::events::SequencePoint { epoch: 1, sequence: 3 })
    );
    // The frame on disk replays to the same events.
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.tail, ReplayTail::Clean);
    assert_eq!(replay.frames.len(), 1);
    assert_eq!(replay.frames[0].header.event_count, 3);
}

#[test]
fn submit_rejects_an_event_whose_tenant_does_not_match_the_worker() {
    let clock = SimClock::new(42);
    let mut retry = SimSafeRetryStore::new();
    let mut worker = pipeline();
    let mut foreign = event(0);
    foreign.envelope.tenant_id = TenantId::new_test_id(42);
    assert_eq!(
        worker.submit(foreign, 1, &mut retry, &clock),
        Err(QueueError::TenantMismatch),
        "a cross-tenant event is diagnosed as a tenant mismatch, not a codec failure"
    );
    // The rejected event never entered the queue.
    assert_eq!(worker.queue().pending_bytes(), 0);
}

#[test]
fn a_zeroed_preallocated_tail_is_never_replayed_or_acknowledged() {
    use crate::invariants::JournalStorage;

    // The write-zeroes fast path preallocates and zeroes a segment ahead of the append head. A crash can leave that
    // zeroed segment on media past the committed frame; when recovery scans the shard the zeroed tail is a clean segment
    // end — it never decodes as a frame (the CRC-64/NVME precheck and authoritative BLAKE3 both fail on zeros) and is
    // never replayed or acknowledged, exactly as an unpreallocated tail behaves.
    let clock = SimClock::new(42);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    for i in 0..3 {
        worker.submit(event(i), 1, &mut retry, &clock).unwrap();
    }
    worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();

    // Model the preallocated-and-zeroed segment sitting in the written extent past the committed frame. (Preallocation
    // normally keeps this out of the written extent entirely; this is the stricter case where recovery scans it anyway.)
    storage.append(ShardId(0), &vec![0u8; 4096]).unwrap();
    storage.sync(ShardId(0)).unwrap();

    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(
        replay.tail,
        ReplayTail::Clean,
        "a zeroed preallocated tail is a clean segment end, not a torn or corrupt frame",
    );
    assert_eq!(
        replay.frames.len(),
        1,
        "only the committed frame replays; the zeroed preallocated tail is never acknowledged as a frame",
    );
    assert_eq!(replay.frames[0].header.event_count, 3);
}

#[test]
fn force_commit_fires_on_idle_not_immediately() {
    let clock = SimClock::new(7);
    let mut retry = SimSafeRetryStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    // Fresh submission: under the idle target, no flush.
    assert_eq!(worker.should_flush(&clock), None);
    // Past the (jittered) idle target, the force commit fires.
    clock.advance(FORCE_COMMIT_IDLE_NANOS * 2);
    assert_eq!(worker.should_flush(&clock), Some(FlushReason::ForceCommit));
}

#[test]
fn force_commit_produces_minimum_frame() {
    let clock = SimClock::new(7);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    clock.advance(FORCE_COMMIT_IDLE_NANOS * 2);
    let result = worker
        .flush(
            FlushReason::ForceCommit,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(result.frame_len, 4096);
}

#[test]
fn expired_lease_closed_by_void_record() {
    let clock = SimClock::new(11);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    // A reservation is taken but never hardened (stalled worker).
    let lease = allocator.reserve(5, clock.monotonic_nanos());
    assert_eq!(lease.range.first_sequence, 1);
    clock.advance(super::super::reserve::LEASE_NANOS + 1);
    // The stale worker may not harden under the expired lease.
    assert_eq!(allocator.reserve(1, clock.monotonic_nanos()).range.first_sequence, 6);
    let voided = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap();
    assert_eq!(
        voided,
        vec![SequenceRange {
            epoch: 1,
            first_sequence: 1,
            last_sequence: 5
        }]
    );
    // The void record is a real chained frame with zero events.
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1);
    assert!(replay.frames[0].header.is_void_record());
    assert_eq!(replay.frames[0].header.event_count, 0);
    // commit_watermark advances past the voided range.
    assert_eq!(
        watermarks.commit_watermark(),
        Some(crate::events::SequencePoint { epoch: 1, sequence: 5 })
    );
}

/// Storage whose `sync` takes simulated wall-clock time, so tests can model a durability barrier that outlasts a
/// reservation lease deadline without needing two separate `flush` calls.
struct LatencySimJournalStorage<'a> {
    clock: &'a SimClock,
    inner: SimJournalStorage,
    sync_latency_nanos: u64,
}

impl crate::invariants::JournalStorage for LatencySimJournalStorage<'_> {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.inner.append(shard, frame)
    }

    fn sync(&mut self, shard: ShardId) -> Result<(), crate::error::StorageError> {
        let result = self.inner.sync(shard);
        self.clock.advance(self.sync_latency_nanos);
        result
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        self.inner.read(shard, offset, len)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn durability_barrier_outlasting_the_lease_deadline_does_not_void_the_durable_frame() {
    let clock = SimClock::new(5);
    let mut storage = LatencySimJournalStorage {
        clock: &clock,
        inner: SimJournalStorage::new(),
        sync_latency_nanos: super::super::reserve::LEASE_NANOS + 1,
    };
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    // `sync` inside `build_and_persist` advances the clock past the lease deadline before `harden` is ever called.
    let result = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .expect("a frame made durable before its lease deadline check must not be rejected");
    assert_eq!(result.state, CommitState::Committed);

    // The lease was consumed despite the expired deadline, so no outstanding lease is left for `commit_voids` to close.
    assert_eq!(allocator.outstanding_leases(), 0);
    let voided = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap();
    assert!(
        voided.is_empty(),
        "the already-durable range must not also be closed by a void record"
    );

    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1);
    assert_eq!(
        replay.frames[0].header.event_count, 1,
        "the real event frame must survive, not a void"
    );
    assert!(!replay.frames[0].header.is_void_record());
}

#[test]
fn steal_claim_failure_discards_copied_bytes() {
    let clock = SimClock::new(3);
    let mut retry = SimSafeRetryStore::new();
    let mut owner = pipeline();
    owner.submit(event(0), 1, &mut retry, &clock).unwrap();
    owner.submit(event(1), 1, &mut retry, &clock).unwrap();
    // The stealer copies, then the owner claims first: the stale claim must fail and the stolen copy is discarded.
    let (region, copied) = owner.queue().copy_pending().unwrap();
    assert_eq!(copied.len(), 2);
    assert!(owner.queue_mut().commit_claim(region)); // owner wins
    assert!(!owner.queue_mut().commit_claim(region)); // stale claim fails
    let mut thief = pipeline();
    assert_eq!(thief.steal_from(owner.queue_mut(), 1, true).unwrap(), 0);
}

#[test]
fn steal_rules_tenant_epoch_and_group() {
    let clock = SimClock::new(3);
    let mut retry = SimSafeRetryStore::new();
    let mut owner = pipeline();
    owner.submit(event(0), 1, &mut retry, &clock).unwrap();
    let mut thief = pipeline();
    // Outside the steal group: refused.
    assert_eq!(thief.steal_from(owner.queue_mut(), 1, false).unwrap(), 0);
    // Wrong epoch: refused.
    assert_eq!(thief.steal_from(owner.queue_mut(), 2, true).unwrap(), 0);
    // Same tenant, epoch, and group: stolen.
    assert_eq!(thief.steal_from(owner.queue_mut(), 1, true).unwrap(), 1);
    assert_eq!(owner.queue().pending_bytes(), 0);
    assert!(thief.queue().pending_bytes() > 0);
}

#[test]
fn steal_into_a_full_thief_claims_nothing_and_loses_no_records() {
    // Regression: the source region must be claimed only after the thief confirms it has room. A thief that cannot
    // admit the records claims nothing, so every record stays with the source rather than vanishing between the queues.
    let clock = SimClock::new(3);
    let mut retry = SimSafeRetryStore::new();
    let mut owner = pipeline();
    for i in 0..40 {
        owner.submit(event(i), 1, &mut retry, &clock).unwrap();
    }
    let before = owner.queue().pending_bytes();
    // A minimal-capacity thief cannot hold all forty records.
    let mut thief = WorkerCommitPipeline::new(3, ShardId(0), TenantId::new_test_id(9), RouteDependency::AppendOnly, 1);
    assert_eq!(thief.steal_from(owner.queue_mut(), 1, true).unwrap(), 0);
    assert_eq!(
        owner.queue().pending_bytes(),
        before,
        "the source keeps its records when the steal cannot be admitted"
    );
    let (_, still_there) = owner.queue().copy_pending().unwrap();
    assert_eq!(still_there.len(), 40, "no record was dropped by a rejected steal");
}

#[test]
fn steal_lets_the_source_reclaim_its_storage() {
    // Regression: once a peer steals a region, the source must reclaim that storage. Without this the source's head
    // never advances over stolen regions, and a small queue soon rejects new records though every record has been
    // drained. Sixty-four single-record rounds through a queue that holds only a few records at once proves the space
    // is reclaimed after each steal.
    let clock = SimClock::new(3);
    let mut retry = SimSafeRetryStore::new();
    let mut owner = WorkerCommitPipeline::new(
        3,
        ShardId(0),
        TenantId::new_test_id(9),
        RouteDependency::AppendOnly,
        4 * 1024,
    );
    let mut thief = pipeline();
    for i in 0..64 {
        owner.submit(event(i), 1, &mut retry, &clock).unwrap();
        assert_eq!(thief.steal_from(owner.queue_mut(), 1, true).unwrap(), 1);
        assert_eq!(owner.queue().pending_bytes(), 0);
    }
}

#[test]
fn crash_loses_unsynced_frame_and_replay_is_idempotent() {
    let clock = SimClock::new(5);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    // The sync barrier fails (injected): flush errors, nothing durable.
    storage.inject(Fault::FailSync { shard: ShardId(0) });
    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::Storage(_)));
    storage.crash();
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert!(replay.frames.is_empty());
    // Re-running replay yields the identical outcome (idempotent).
    let again = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(again.frames.len(), replay.frames.len());
    assert_eq!(again.tail, replay.tail);
}

#[test]
fn torn_tail_truncates_and_safe_retry_rebuilds_from_hej() {
    let clock = SimClock::new(5);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    // Second frame tears at the crash: only 100 bytes reach media.
    worker.submit(event(1), 1, &mut retry, &clock).unwrap();
    storage.inject(Fault::TornTail {
        shard: ShardId(0),
        keep_bytes: 100,
    });
    storage.inject(Fault::FailSync { shard: ShardId(0) });
    let _ = worker.flush(
        FlushReason::Target,
        &mut allocator,
        &mut storage,
        &mut watermarks,
        &mut retry,
        &mut overlay,
        &clock,
    );
    storage.crash();
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1);
    assert!(matches!(replay.tail, ReplayTail::Truncated { .. }));
    // Safe-retry rows rebuilt from HEJ coverage agree with the journal.
    let mut rebuilt = SimSafeRetryStore::new();
    rebuilt.reconstruct_from_replay(&replay.frames, ShardId(0), clock.now_nanos() + 1);
    assert_eq!(rebuilt.len(), 1);
    let receipt = rebuilt.lookup(TenantId::new_test_id(9), (5000, 1)).unwrap();
    assert_eq!(receipt.commit.sequence, 1);
    assert!(rebuilt.duplicate_within_guard(TenantId::new_test_id(9), (1000, 7), clock.now_nanos()));
}

#[test]
fn safe_retry_receipts_do_not_collide_across_tenants() {
    // Two tenants share one connector delivery identity and one dedupe hash. Neither tenant's receipt may overwrite or
    // suppress the other's: the store is keyed by tenant, and the replay guard matches on tenant too.
    let tenant_a = TenantId::new_test_id(9);
    let tenant_b = TenantId::new_test_id(10);
    let delivery = (5000, 1);
    let dedupe = (1000, 7);
    let receipt = |tenant, sequence| RetryReceipt {
        commit: SequencePoint { epoch: 1, sequence },
        dedupe,
        delivery_identity: delivery,
        expiry_physical_nanos: 1_000,
        frame_offset: 0,
        shard: ShardId(0),
        status_class: StatusClass::Acknowledged,
        tenant_id: tenant,
    };
    let mut retry = SimSafeRetryStore::new();
    retry.record(receipt(tenant_a, 1));
    retry.record(receipt(tenant_b, 2));
    // Both rows coexist; each tenant reads its own commit position rather than the other's.
    assert_eq!(retry.len(), 2);
    assert_eq!(retry.lookup(tenant_a, delivery).unwrap().commit.sequence, 1);
    assert_eq!(retry.lookup(tenant_b, delivery).unwrap().commit.sequence, 2);
    // The replay guard is per tenant: one tenant's acknowledgement never suppresses another tenant's event.
    assert!(retry.duplicate_within_guard(tenant_a, dedupe, 0));
    assert!(retry.duplicate_within_guard(tenant_b, dedupe, 0));
    // A third tenant sharing the same identities sees no receipt and no guard hit.
    assert!(retry.lookup(TenantId::new_test_id(11), delivery).is_none());
    assert!(!retry.duplicate_within_guard(TenantId::new_test_id(11), dedupe, 0));
}

/// Storage that fails the Nth `append` on any shard (1-based) and delegates everything else, so a test can model a void
/// write failing partway through a batch of expired ranges.
struct FailNthAppendStorage {
    fail_on_append: usize,
    inner: SimJournalStorage,
    seen_appends: usize,
}

impl crate::invariants::JournalStorage for FailNthAppendStorage {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.seen_appends += 1;
        if self.seen_appends == self.fail_on_append {
            return Err(crate::error::StorageError::InjectedFault {
                kind: "fail-nth-append",
            });
        }
        self.inner.append(shard, frame)
    }

    fn sync(&mut self, shard: ShardId) -> Result<(), crate::error::StorageError> {
        self.inner.sync(shard)
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        self.inner.read(shard, offset, len)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn a_failed_void_write_keeps_later_expired_ranges_outstanding() {
    // Three reservations abandoned together. The void write for the second fails, so only the first is durably voided;
    // the second and third must stay outstanding for a later pass to close, never dropped as permanent sequence holes.
    let clock = SimClock::new(11);
    let mut storage = FailNthAppendStorage {
        fail_on_append: 2,
        inner: SimJournalStorage::new(),
        seen_appends: 0,
    };
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    allocator.reserve(1, clock.monotonic_nanos());
    allocator.reserve(1, clock.monotonic_nanos());
    allocator.reserve(1, clock.monotonic_nanos());
    clock.advance(super::super::reserve::LEASE_NANOS + 1);

    // The second void append fails; the pass errors after durably voiding only the first range.
    let error = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap_err();
    assert!(matches!(error, FlushError::Storage(_)));
    assert_eq!(
        allocator.outstanding_leases(),
        2,
        "a failed void write drops no still-uncovered range",
    );

    // A later pass with storage healthy closes the two ranges that were left outstanding.
    let voided = worker
        .commit_voids(
            &mut allocator,
            &mut storage.inner,
            &mut watermarks,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(voided.len(), 2);
    assert_eq!(allocator.outstanding_leases(), 0);
}

#[test]
fn a_sync_failure_after_a_successful_append_is_not_closed_by_a_void() {
    use crate::invariants::JournalStorage;

    // `append` places the frame's bytes as pending; the durability barrier then fails. Those bytes can still be
    // persisted by a later sync, so the range must never be closed by a void record — a void would overlap the frame if
    // it lands. The lease is accepted as potentially-durable instead of being left for `commit_voids`.
    let clock = SimClock::new(9);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    storage.inject(Fault::FailSync { shard: ShardId(0) });
    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::Storage(_)));
    assert_eq!(
        allocator.outstanding_leases(),
        0,
        "an appended range is accepted as potentially-durable, not left outstanding for a void",
    );

    // Even past the lease deadline, no void record covers the range.
    clock.advance(super::super::reserve::LEASE_NANOS + 1);
    let voided = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap();
    assert!(
        voided.is_empty(),
        "no void may cover a range whose appended frame can still become durable",
    );

    // A later successful sync persists the appended frame: it is the real event frame, never shadowed by a void.
    storage.sync(ShardId(0)).unwrap();
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1);
    assert!(!replay.frames[0].header.is_void_record());
    assert_eq!(replay.frames[0].header.event_count, 1);
}

/// Storage whose `sync` and `read` both fail while `append` succeeds, modelling the case where the durability barrier
/// fails and the writer cannot read the offset back to find out whether the appended bytes are there.
struct UnreadableAfterFailedSyncStorage {
    inner: SimJournalStorage,
}

impl crate::invariants::JournalStorage for UnreadableAfterFailedSyncStorage {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.inner.append(shard, frame)
    }

    fn sync(&mut self, _shard: ShardId) -> Result<(), crate::error::StorageError> {
        Err(crate::error::StorageError::Io {
            op: "fail-sync",
            detail: String::new(),
        })
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, _shard: ShardId, _offset: u64, _len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        Err(crate::error::StorageError::Io {
            op: "fail-read",
            detail: String::new(),
        })
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn an_unreadable_offset_after_a_failed_sync_is_not_treated_as_proof_the_append_is_absent() {
    // Regression: a failed read-back after a failed sync used to be read as "the frame never landed", leaving the lease
    // outstanding for a void. `append` already succeeded, so a failed read proves nothing — a later sync can still
    // persist the frame, and a void over the same range would then overlap it. Only a read that succeeds and returns
    // other bytes proves absence.
    let clock = SimClock::new(17);
    let mut storage = UnreadableAfterFailedSyncStorage {
        inner: SimJournalStorage::new(),
    };
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::Storage(_)));
    assert_eq!(
        allocator.outstanding_leases(),
        0,
        "an append whose presence cannot be disproved is accepted as potentially-durable",
    );
    clock.advance(super::super::reserve::LEASE_NANOS + 1);
    let voided = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap();
    assert!(
        voided.is_empty(),
        "no void may cover a range the appended frame may still occupy",
    );
}

#[test]
fn a_mixed_epoch_flush_commits_the_matching_prefix_and_drops_nothing() {
    // Regression: a queue spanning an epoch roll used to be claimed whole and then aborted, silently discarding every
    // record. The flush must instead split at the epoch boundary: commit the current-epoch prefix, keep the rest.
    let clock = SimClock::new(21);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    worker.submit(event(1), 1, &mut retry, &clock).unwrap();
    worker.submit(event(2), 2, &mut retry, &clock).unwrap();

    let result = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(result.range.epoch, 1);
    assert_eq!(result.range.first_sequence, 1);
    assert_eq!(result.range.last_sequence, 2);
    // The epoch-2 record stays queued, not silently discarded.
    let (_, remaining) = worker.queue().copy_pending().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].epoch, 2);
    // Once the allocator rolls to epoch 2 the remainder flushes normally.
    let mut next_allocator = SequenceAllocator::new(2);
    let result = worker
        .flush(
            FlushReason::Target,
            &mut next_allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(result.range.epoch, 2);
    assert_eq!(result.range.first_sequence, 1);
    assert_eq!(retry.len(), 3, "every accepted record is eventually receipted");
}

#[test]
fn a_flush_whose_queue_front_mismatches_claims_and_drops_nothing() {
    let clock = SimClock::new(22);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    // An epoch-2 record at the queue front while the allocator is still at epoch 1.
    worker.submit(event(0), 2, &mut retry, &clock).unwrap();

    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::MixedTenantOrEpoch));
    let (_, remaining) = worker.queue().copy_pending().unwrap();
    assert_eq!(
        remaining.len(),
        1,
        "the mismatched record stays queued instead of being claimed and dropped"
    );
    assert_eq!(
        allocator.outstanding_leases(),
        0,
        "no lease is taken for an unflushable queue"
    );
}

/// Storage whose read-back returns corrupted bytes while append and sync succeed, modelling storage-side corruption
/// that only reading the stored bytes back can catch.
struct CorruptReadStorage {
    inner: SimJournalStorage,
}

impl crate::invariants::JournalStorage for CorruptReadStorage {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.inner.append(shard, frame)
    }

    fn sync(&mut self, shard: ShardId) -> Result<(), crate::error::StorageError> {
        self.inner.sync(shard)
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        let mut bytes = self.inner.read(shard, offset, len)?;
        if let Some(byte) = bytes.get_mut(200) {
            *byte ^= 0xFF;
        }
        Ok(bytes)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn post_sync_verification_reads_back_the_stored_bytes() {
    // Regression: the post-sync check used to decode the in-memory frame buffer, so corruption on the storage side
    // sailed through to receipt and acknowledgement. The verification must fail when the stored bytes are corrupt.
    let clock = SimClock::new(13);
    let mut storage = CorruptReadStorage {
        inner: SimJournalStorage::new(),
    };
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::Format(_)));
    // `sync` succeeded, so the frame can already be durable and only the verification read is at fault. The delivery is
    // receipted `Indeterminate` so a client retry is recognised instead of appending a second copy — never as an
    // acknowledgement.
    let receipt = retry
        .lookup(TenantId::new_test_id(9), (5000, 1))
        .expect("an unverified but possibly durable frame leaves retry metadata");
    assert_eq!(receipt.status_class, StatusClass::Indeterminate);
    assert!(
        !retry.duplicate_within_guard(TenantId::new_test_id(9), (1000, 7), clock.now_nanos()),
        "an indeterminate receipt is never an acknowledgement",
    );
    assert_eq!(
        watermarks.commit_watermark(),
        None,
        "durability is not recorded for an unverified frame"
    );
    // The frame's bytes are on media, so its range is consumed as potentially durable — never closed by a void.
    assert_eq!(allocator.outstanding_leases(), 0);
    clock.advance(super::super::reserve::LEASE_NANOS + 1);
    let voided = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap();
    assert!(voided.is_empty());
}

#[test]
fn read_after_ack_acknowledges_an_earlier_epoch_once_it_is_visible() {
    // Regression: the acknowledgement gate compared epochs for equality against the single visibility-watermark
    // point, so an epoch-1 event could never acknowledge once epoch 2 published anything.
    let mut watermarks = WatermarkTracker::new();
    watermarks.record_visible(SequenceRange {
        epoch: 1,
        first_sequence: 1,
        last_sequence: 3,
    });
    watermarks.record_visible(SequenceRange {
        epoch: 2,
        first_sequence: 1,
        last_sequence: 1,
    });
    let receipt = RetryReceipt {
        commit: SequencePoint { epoch: 1, sequence: 2 },
        dedupe: (1000, 7),
        delivery_identity: (5000, 1),
        expiry_physical_nanos: 1_000,
        frame_offset: 0,
        shard: ShardId(0),
        status_class: StatusClass::Acknowledged,
        tenant_id: TenantId::new_test_id(9),
    };
    assert!(
        WorkerCommitPipeline::ack_ready(
            &receipt,
            CommitState::Committed,
            AckVisibility::ReadAfterAck,
            &watermarks
        ),
        "an event visible in its own (earlier) epoch acknowledges even after a later epoch begins"
    );
    // A sequence beyond its epoch's visible prefix still waits.
    let unpublished = RetryReceipt {
        commit: SequencePoint { epoch: 1, sequence: 4 },
        ..receipt.clone()
    };
    assert!(!WorkerCommitPipeline::ack_ready(
        &unpublished,
        CommitState::Committed,
        AckVisibility::ReadAfterAck,
        &watermarks
    ));
}

/// Storage whose `append` reports success but writes zeros instead of the frame, and whose `sync` then fails: the model
/// of a durability barrier failing over bytes that never reached the device at all.
struct LosingAppendStorage {
    inner: SimJournalStorage,
}

impl crate::invariants::JournalStorage for LosingAppendStorage {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.inner.append(shard, &vec![0u8; frame.len()])
    }

    fn sync(&mut self, _shard: ShardId) -> Result<(), crate::error::StorageError> {
        Err(crate::error::StorageError::InjectedFault { kind: "fail-sync" })
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        self.inner.read(shard, offset, len)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn a_sync_failure_over_bytes_that_never_landed_leaves_its_range_to_be_voided() {
    // A sync failure used to consume the lease unconditionally, so a range whose bytes are not on the device at all
    // became a permanent watermark hole no later pass could close. Reading the offset back tells the two cases apart:
    // absent bytes can never become durable, so the lease stays outstanding and a void closes the hole (issue #8923).
    let clock = SimClock::new(23);
    let mut storage = LosingAppendStorage {
        inner: SimJournalStorage::new(),
    };
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::Storage(_)));
    assert_eq!(
        allocator.outstanding_leases(),
        1,
        "a range whose bytes never landed stays outstanding so a void can close it",
    );
    assert_eq!(retry.len(), 0, "nothing was appended, so nothing is receipted");

    // A later pass over healthy storage closes the range with a void record: no permanent sequence hole.
    clock.advance(super::super::reserve::LEASE_NANOS + 1);
    let voided = worker
        .commit_voids(
            &mut allocator,
            &mut storage.inner,
            &mut watermarks,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(voided.len(), 1);
    assert_eq!(allocator.outstanding_leases(), 0);
}

#[test]
fn a_sync_failure_over_appended_bytes_leaves_an_indeterminate_receipt_to_guard_a_retry() {
    // The events were appended and may still become durable, but nothing acknowledged them. Without a receipt a client
    // retry appended a second copy of the same delivery; with one, the retry path can see where the first attempt
    // landed. The receipt is never an acknowledgement, so it does not arm the replay guard (issue #8923).
    let clock = SimClock::new(29);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    let submitted = event(0);
    let delivery = (
        submitted.connector_delivery_hash_low,
        submitted.connector_delivery_hash_high,
    );
    let dedupe = (submitted.envelope.dedupe_hash_low, submitted.envelope.dedupe_hash_high);
    worker.submit(submitted, 1, &mut retry, &clock).unwrap();

    storage.inject(Fault::FailSync { shard: ShardId(0) });
    assert!(matches!(
        worker
            .flush(
                FlushReason::Target,
                &mut allocator,
                &mut storage,
                &mut watermarks,
                &mut retry,
                &mut overlay,
                &clock,
            )
            .unwrap_err(),
        FlushError::Storage(_)
    ));

    let receipt = retry
        .lookup(worker.tenant_id, delivery)
        .expect("a retry of this delivery must find where the appended events went");
    assert_eq!(receipt.status_class, StatusClass::Indeterminate);
    assert_eq!(receipt.commit.sequence, 1);
    assert!(
        !retry.duplicate_within_guard(worker.tenant_id, dedupe, 0),
        "an indeterminate receipt is not an acknowledgement, so it never arms the replay guard",
    );
    assert_eq!(
        watermarks.commit_watermark(),
        None,
        "durability is not recorded for an unsynced frame"
    );
}

/// Storage whose read-back returns a *different* frame — itself perfectly valid and the same size — modelling a stale or
/// misdirected read.
struct MisdirectedReadStorage {
    inner: SimJournalStorage,
    other_frame: Vec<u8>,
}

impl crate::invariants::JournalStorage for MisdirectedReadStorage {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.inner.append(shard, frame)
    }

    fn sync(&mut self, shard: ShardId) -> Result<(), crate::error::StorageError> {
        self.inner.sync(shard)
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        if len as usize == self.other_frame.len() {
            return Ok(self.other_frame.clone());
        }
        self.inner.read(shard, offset, len)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn post_sync_verification_rejects_a_different_valid_frame_read_back() {
    // Verification only checked that the read-back bytes decoded as *some* valid frame, so a stale or misdirected read
    // returning another valid frame of the same size passed — and the events were marked durable and acknowledged even
    // though replay would never find them. The read-back must equal the frame just appended (issue #8460).
    let clock = SimClock::new(31);
    let other_frame = {
        let mut worker = pipeline();
        let mut allocator = SequenceAllocator::new(1);
        let mut watermarks = WatermarkTracker::new();
        let mut retry = SimSafeRetryStore::new();
        let mut overlay = LiveOverlayStore::new();
        let mut storage = SimJournalStorage::new();
        worker.submit(event(41), 1, &mut retry, &clock).unwrap();
        let result = worker
            .flush(
                FlushReason::Target,
                &mut allocator,
                &mut storage,
                &mut watermarks,
                &mut retry,
                &mut overlay,
                &clock,
            )
            .unwrap();
        crate::invariants::JournalStorage::read(&storage, ShardId(0), result.frame_offset, result.frame_len).unwrap()
    };

    let mut storage = MisdirectedReadStorage {
        inner: SimJournalStorage::new(),
        other_frame,
    };
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    let error = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap_err();
    assert!(matches!(error, FlushError::Format(_)));
    // The durability barrier succeeded, so the frame may well be on media and only the read is misdirected: the
    // delivery keeps `Indeterminate` retry metadata so a client retry is recognised rather than appended twice.
    assert_eq!(
        retry
            .lookup(TenantId::new_test_id(9), (5000, 1))
            .map(|receipt| receipt.status_class),
        Some(StatusClass::Indeterminate),
    );
    assert_eq!(
        watermarks.commit_watermark(),
        None,
        "durability is not recorded for a frame that could not be verified"
    );
}

#[test]
fn a_committed_event_is_readable_through_fresh_read_as_soon_as_flush_returns() {
    let clock = SimClock::new(37);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    for i in 0..3 {
        worker.submit(event(i), 1, &mut retry, &clock).unwrap();
    }

    let result = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();

    let FreshRead::Ready(segments) = overlay.fresh_read(worker.tenant_id, result.range) else {
        panic!("a committed frame must be in the overlay when flush returns");
    };
    assert_eq!(segments.len(), 1);
    assert_eq!(segments[0].range(), result.range);
    assert_eq!(segments[0].batch.num_rows(), 3);
}

#[test]
fn a_hardened_transactional_frame_is_not_published_to_the_overlay() {
    let clock = SimClock::new(41);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = WorkerCommitPipeline::new(
        3,
        ShardId(0),
        TenantId::new_test_id(9),
        RouteDependency::Transactional,
        1 << 20,
    );
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();

    let result = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(result.state, CommitState::Hardened);
    assert_eq!(overlay.segment_count(), 0, "only COMMITTED frames become visible");
}

#[test]
fn a_committed_void_is_passed_over_by_fresh_read() {
    let clock = SimClock::new(43);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    allocator.reserve(2, clock.monotonic_nanos());
    clock.advance(super::super::reserve::LEASE_NANOS + 1);

    let voided = worker
        .commit_voids(&mut allocator, &mut storage, &mut watermarks, &mut overlay, &clock)
        .unwrap();
    assert_eq!(voided.len(), 1);
    assert!(matches!(
        overlay.fresh_read(worker.tenant_id, voided[0]),
        FreshRead::Ready(segments) if segments.is_empty()
    ));
}

#[test]
fn a_retry_inside_the_guard_window_commits_once_and_returns_the_original_receipt() {
    let clock = SimClock::new(47);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    assert_eq!(
        worker.submit(event(0), 1, &mut retry, &clock).unwrap(),
        Submission::Ready
    );
    let first = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    let original = first.receipts[0].clone();

    assert_eq!(
        worker.submit(event(0), 1, &mut retry, &clock).unwrap(),
        Submission::Duplicate(original),
        "the retry is answered with the first commit's receipt"
    );
    assert_eq!(worker.queue().pending_bytes(), 0, "the retry is not queued");
    assert!(matches!(
        worker.flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        ),
        Err(FlushError::NothingToFlush)
    ));
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1, "the event is stored once");
}

#[test]
fn a_batch_retry_skips_only_the_events_already_committed() {
    let clock = SimClock::new(53);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    let first = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();

    let submissions = worker
        .submit_batch(vec![event(0), event(1)], 1, &mut retry, &clock)
        .unwrap();
    assert_eq!(
        submissions,
        vec![Submission::Duplicate(first.receipts[0].clone()), Submission::Ready]
    );
    let second = worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();
    assert_eq!(second.receipts.len(), 1, "only the new event commits");
    assert_eq!(second.receipts[0].dedupe, (1001, 7));
}

#[test]
fn a_retry_after_the_guard_window_is_queued_again() {
    let clock = SimClock::new(59);
    let mut storage = SimJournalStorage::new();
    let mut allocator = SequenceAllocator::new(1);
    let mut watermarks = WatermarkTracker::new();
    let mut retry = SimSafeRetryStore::new();
    let mut overlay = LiveOverlayStore::new();
    let mut worker = pipeline();
    worker.submit(event(0), 1, &mut retry, &clock).unwrap();
    worker
        .flush(
            FlushReason::Target,
            &mut allocator,
            &mut storage,
            &mut watermarks,
            &mut retry,
            &mut overlay,
            &clock,
        )
        .unwrap();

    clock.advance(ACK_REPLAY_GUARD_NANOS as u64 + 1);
    assert_eq!(
        worker.submit(event(0), 1, &mut retry, &clock).unwrap(),
        Submission::Ready
    );
}
