//! One worker's path from accepting an event to acknowledging it, all the way through making it durable.
//!
//! The stages are: validate → serialize into the worker's lock-free queue (READY) → reach the flush target (16 KiB
//! default; 4 KiB force-commit) → claim and reserve a contiguous `(epoch, sequence)` range → submit an aligned HEJ
//! frame → verify CRC-64/NVME + BLAKE3 → HARDENED → record safe-retry metadata → COMMITTED (append-only: immediately;
//! no GSN/RFA/barrier machinery) → decode into LiveOverlay → acknowledge per the route contract.
//!
//! There is no global group-commit writer and no global acknowledgement thread: each pipeline instance is one worker
//! flushing autonomously.

use super::error::{FlushError, LeaseError, QueueError};
use super::queue::{CommitQueue, PendingRecord, peek_steal};
use super::reserve::{FORCE_COMMIT_IDLE_NANOS, SequenceAllocator};
use super::retry::{RetryReceipt, SafeRetryStore, StatusClass};
use crate::artifacts::batch::{EventInput, build_batch};
use crate::artifacts::frame::{FLAG_VOID_RECORD, FrameBuildInput, build_frame};
use crate::artifacts::watermark::WatermarkTracker;
use crate::error::FormatError;
use crate::events::{SequencePoint, SequenceRange, TenantId};
use crate::invariants::{JournalStorage, MonotonicClock, ShardId};

/// How long an acknowledgement guard stays valid, chosen to comfortably outlast any realistic replay window so a
/// replay can never land after the guard protecting it has expired.
const ACK_REPLAY_GUARD_NANOS: i64 = 60_000_000_000;

/// The force-commit idle jitter is this fraction of the idle target — enough to keep workers from synchronizing into
/// periodic bursts without meaningfully delaying the flush.
const FORCE_COMMIT_JITTER_DIVISOR: u64 = 4;

/// Commit states of the autonomous-commit model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitState {
    /// Acknowledgeable. For ordinary append-only ingest HARDENED and COMMITTED are the same state.
    Committed,
    /// The containing frame is durable under the selected policy.
    Hardened,
    /// Validated and serialized into the worker queue; not durable, no public visibility.
    Ready,
}

/// Route dependency classes. Ordinary append-only ingest pays no extra coordination overhead; that machinery is
/// permitted only for routes with real transactional dependencies (this is the hook, kept off the ordinary record
/// path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteDependency {
    AppendOnly,
    /// Acknowledgement additionally waits for the route's autonomous dependency acknowledgement.
    Transactional,
}

/// Read-after-ack visibility contract of the acknowledging route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckVisibility {
    /// Acknowledge on durability (HEJ completion).
    Durability,
    /// Acknowledge only once the event is included in `visibility_watermark` (or fresh queries block until replay).
    ReadAfterAck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushReason {
    /// Low-load force commit: trickle traffic must not wait indefinitely.
    ForceCommit,
    /// Pending bytes reached the flush target.
    Target,
}

/// The outcome of one committed frame.
#[derive(Debug)]
pub struct FlushResult {
    pub frame_len: u32,
    pub frame_offset: u64,
    pub range: SequenceRange,
    pub reason: FlushReason,
    pub receipts: Vec<RetryReceipt>,
    pub state: CommitState,
}

/// One worker's autonomous commit pipeline over the interfaces.
#[derive(Debug)]
pub struct WorkerCommitPipeline {
    /// Replay-guard window for safe-retry receipts.
    ack_guard_nanos: i64,
    durable_batch_counter: u64,
    flush_target_bytes: u64,
    last_activity_monotonic: u64,
    queue: CommitQueue,
    pub route: RouteDependency,
    schema_generation: u64,
    pub shard: ShardId,
    pub tenant_id: TenantId,
    pub worker_id: u32,
    writer_local_batch_counter: u64,
}

impl WorkerCommitPipeline {
    /// A fresh pipeline for one worker, bound to its shard and tenant, with an empty queue of the given capacity.
    pub fn new(
        worker_id: u32,
        shard: ShardId,
        tenant_id: TenantId,
        route: RouteDependency,
        queue_capacity: usize,
    ) -> Self {
        Self {
            worker_id,
            shard,
            tenant_id,
            route,
            queue: CommitQueue::new(queue_capacity),
            flush_target_bytes: u64::from(super::super::artifacts::DEFAULT_FLUSH_TARGET),
            last_activity_monotonic: 0,
            durable_batch_counter: 0,
            writer_local_batch_counter: 0,
            schema_generation: 1,
            ack_guard_nanos: ACK_REPLAY_GUARD_NANOS,
        }
    }

    /// This worker's commit queue, read-only.
    pub fn queue(&self) -> &CommitQueue {
        &self.queue
    }

    /// This worker's commit queue, for a topology-local peer to steal pending records from — claiming a region
    /// advances the source queue's clean cursor, which needs mutable access.
    pub fn queue_mut(&mut self) -> &mut CommitQueue {
        &mut self.queue
    }

    /// READY: validates and serializes the event into the worker queue. No durability, no visibility, no final sequence
    /// yet.
    pub fn submit(
        &mut self,
        event: EventInput,
        epoch: u64,
        clock: &dyn MonotonicClock,
    ) -> Result<CommitState, QueueError> {
        if event.envelope.tenant_id != self.tenant_id {
            return Err(QueueError::TenantMismatch);
        }
        self.queue.push(&PendingRecord {
            tenant_id: self.tenant_id,
            epoch,
            event,
        })?;
        self.last_activity_monotonic = clock.monotonic_nanos();
        Ok(CommitState::Ready)
    }

    /// READY for a whole caller batch, or nothing: validates every event and queues the batch only when the queue can
    /// hold all of it. A refused batch leaves nothing queued and the queue's cursors untouched, so the caller retries
    /// the same batch as one unit once there is room — never a duplicated prefix, never a lost suffix. Admitted events
    /// keep their submission order, so the flush that claims them commits one contiguous sequence sub-range with one
    /// receipt per event.
    pub fn submit_batch(
        &mut self,
        events: Vec<EventInput>,
        epoch: u64,
        clock: &dyn MonotonicClock,
    ) -> Result<CommitState, QueueError> {
        if events.iter().any(|event| event.envelope.tenant_id != self.tenant_id) {
            return Err(QueueError::TenantMismatch);
        }
        let records: Vec<PendingRecord> = events
            .into_iter()
            .map(|event| PendingRecord {
                tenant_id: self.tenant_id,
                epoch,
                event,
            })
            .collect();
        // Reclaim storage peers have stolen before measuring room, as `push` does, so a drained queue is not refused
        // on stale accounting.
        self.queue.release_hardened();
        let Some(admission) = self.queue.admit(&records)? else {
            return Err(QueueError::Full);
        };
        self.queue.push_admitted(&admission)?;
        self.last_activity_monotonic = clock.monotonic_nanos();
        Ok(CommitState::Ready)
    }

    /// Steals same-tenant/same-epoch pending records from a topology-local peer queue into this worker's queue. The peer
    /// region is claimed only after this queue confirms it has room for every record, so a claimed record is never
    /// dropped between the two queues, and the source reclaims the stolen storage once the claim lands.
    pub fn steal_from(
        &mut self,
        peer: &mut CommitQueue,
        epoch: u64,
        same_steal_group: bool,
    ) -> Result<usize, QueueError> {
        let Some((region, records)) = peek_steal(peer, self.tenant_id, epoch, same_steal_group) else {
            return Ok(0);
        };
        let count = records.len();
        // Confirm room before taking ownership: a steal must not claim records this queue cannot hold, or they would
        // vanish from the source without ever landing here. The stolen bytes are already in this queue's own wire
        // format (peer and thief encode records identically), so they are copied in rather than decoded and
        // re-encoded. `records` borrows from `peer`, so it is not touched again past this call — the source is
        // claimed from immediately below.
        let Some(admission) = self.queue.admit_encoded(&records)? else {
            return Ok(0);
        };
        // Claim the region on the source. A failed claim means another worker took it first; discard the copy and
        // take nothing. Only after a won claim — with room already guaranteed — do the pushes run.
        if !peer.commit_claim(region) {
            return Ok(0);
        }
        self.queue.push_admitted(&admission)?;
        Ok(count)
    }

    /// Decides whether to flush now. The force-commit trigger is jittered (probabilistic) so workers do not synchronize
    /// into periodic bursts; the idle target is an internal runtime policy, not an operator knob.
    pub fn should_flush(&self, clock: &dyn MonotonicClock) -> Option<FlushReason> {
        let pending = self.queue.pending_bytes();
        if pending == 0 {
            return None;
        }
        if pending >= self.flush_target_bytes {
            return Some(FlushReason::Target);
        }
        let jitter = clock.jitter(FORCE_COMMIT_IDLE_NANOS / FORCE_COMMIT_JITTER_DIVISOR);
        let idle_threshold = FORCE_COMMIT_IDLE_NANOS - jitter;
        let idle = clock.monotonic_nanos().saturating_sub(self.last_activity_monotonic);
        if idle >= idle_threshold {
            return Some(FlushReason::ForceCommit);
        }
        None
    }

    /// Claims the pending records that match this worker's tenant and the allocator's epoch — a queue spanning an
    /// epoch roll is split at the boundary, claiming only the matching prefix and leaving the rest pending — reserves
    /// one contiguous sequence range, writes one aligned HEJ frame, reads it back to verify what landed on media, and
    /// records durability plus safe-retry receipts. Append-only routes are COMMITTED on durability; transactional
    /// routes stay HARDENED for the dependency hook.
    pub fn flush(
        &mut self,
        reason: FlushReason,
        allocator: &mut SequenceAllocator,
        storage: &mut dyn JournalStorage,
        watermarks: &mut WatermarkTracker,
        retry: &mut SafeRetryStore,
        clock: &dyn MonotonicClock,
    ) -> Result<FlushResult, FlushError> {
        let epoch = allocator.epoch();
        let (region, records) = self
            .queue
            .copy_matching_prefix(self.tenant_id, epoch)
            .map_err(FlushError::Queue)?;
        if records.is_empty() {
            // An empty prefix with records still pending means the queue front belongs to another tenant or epoch.
            // Nothing was claimed, so those records stay queued for a matching flush instead of being dropped.
            if self.queue.pending_bytes() > 0 {
                return Err(FlushError::MixedTenantOrEpoch);
            }
            return Err(FlushError::NothingToFlush);
        }
        if !self.queue.claim_for_flush(region) {
            // Lost the claim to a stealer: the copied bytes are discarded.
            return Err(FlushError::NothingToFlush);
        }

        // Reserve the final (epoch, sequence) range under a lease, but do NOT consume the lease yet: the range is used
        // to build the frame while the lease stays outstanding, so if any step below fails before the frame is durable,
        // the lease is left to expire into a void record (closing the sequence hole) instead of leaving a permanent gap.
        let lease = allocator.reserve(records.len() as u64, clock.monotonic_nanos());
        let range = lease.range;

        let events: Vec<EventInput> = records.into_iter().map(|record| record.event).collect();
        let now = clock.now_nanos();
        let (frame_bytes, frame_offset) = match self.build_and_persist(&events, range, now, storage) {
            Ok(result) => result,
            Err(FlushError::SyncAfterAppend { error, frame_offset }) => {
                // The appended bytes are present but unsynced, so a later sync can still persist them. Voiding this
                // range would then overlap the frame if it lands, so consume the lease as potentially-durable — keeping
                // it out of `expire`/`commit_voids`. Receipts are recorded as `Indeterminate` so a client retry of the
                // same delivery is recognised instead of appending a second copy of these events; they are never an
                // acknowledgement, and the failure is still reported so the caller does not acknowledge.
                let _ = allocator.accept_late(lease.lease_id);
                self.record_receipts(&events, range, frame_offset, now, StatusClass::Indeterminate, retry);
                self.queue.abort_flush(region);
                return Err(FlushError::Storage(error));
            }
            Err(error) => {
                // The append never happened, so the frame never reached durability. Unpin the claimed queue region and
                // leave the lease outstanding so its range is closed by a void record — no pinned storage, no permanent
                // sequence hole.
                self.queue.abort_flush(region);
                return Err(error);
            }
        };

        // Verification against what actually landed on media, not the in-memory buffer just built: read the frame back
        // and require it to be byte-identical to the frame just appended, before anything is receipted or acknowledged.
        // Decoding alone is not enough — a stale or misdirected read can return some *other* valid frame of the same
        // size, and accepting it would acknowledge events replay will not find. A failed read-back or a mismatch is
        // handled like a failed durability barrier: the appended bytes may be durable, so the lease is consumed as
        // potentially-durable — never closed by a void — and the failure is reported so the caller does not
        // acknowledge. `sync` is the durability barrier and it succeeded, so the frame can already be durable with only
        // the verification read at fault; `Indeterminate` receipts are recorded for the same reason as the failed-sync
        // branch, so a client retry of the same delivery is recognised instead of appending a second copy.
        if let Err(error) = storage
            .read(self.shard, frame_offset, frame_bytes.len() as u32)
            .map_err(FlushError::Storage)
            .and_then(|stored| {
                if stored == frame_bytes {
                    Ok(())
                } else {
                    Err(FlushError::Format(FormatError::Structural {
                        rule: "frame read back after sync differs from the frame appended",
                    }))
                }
            })
        {
            let _ = allocator.accept_late(lease.lease_id);
            self.record_receipts(&events, range, frame_offset, now, StatusClass::Indeterminate, retry);
            self.queue.abort_flush(region);
            return Err(error);
        }

        // Durable and verified: only now consume the lease, then advance the durable watermark and release the queue.
        // A deadline crossed by the durability barrier itself (append + sync routinely outlasts the lease window) does
        // not mean anything raced ahead of us — this call has held `allocator` exclusively since `reserve`, so nobody
        // could have voided this range yet. Accept the already-durable frame rather than voiding a range that already
        // holds a real event frame.
        if let Err(error) = allocator.harden(lease.lease_id, clock.monotonic_nanos()) {
            if error != LeaseError::Expired || allocator.accept_late(lease.lease_id).is_err() {
                self.queue.abort_flush(region);
                return Err(FlushError::Lease(error));
            }
        }
        watermarks.record_durable(range);
        self.queue.mark_hardened(region);
        self.queue.release_hardened();

        // Safe-retry receipts: reconstructable indexes pointing at HEJ.
        let receipts = self.record_receipts(&events, range, frame_offset, now, StatusClass::Acknowledged, retry);

        // Append-only: HARDENED becomes COMMITTED immediately — no dependency machinery on the ordinary ingest path.
        let state = match self.route {
            RouteDependency::AppendOnly => CommitState::Committed,
            RouteDependency::Transactional => CommitState::Hardened,
        };
        Ok(FlushResult {
            range,
            frame_offset,
            frame_len: frame_bytes.len() as u32,
            state,
            receipts,
            reason,
        })
    }

    /// Records one safe-retry receipt per event — pointers into HEJ at `frame_offset`, one sequence per row — under
    /// `status_class`, and returns them. `Acknowledged` is the durable-and-verified path; `Indeterminate` is the path
    /// where the append may or may not have persisted, and the receipt exists only so a client retry of the same
    /// delivery is recognised rather than appended twice.
    fn record_receipts(
        &self,
        events: &[EventInput],
        range: SequenceRange,
        frame_offset: u64,
        now: i64,
        status_class: StatusClass,
        retry: &mut SafeRetryStore,
    ) -> Vec<RetryReceipt> {
        let mut receipts = Vec::with_capacity(events.len());
        for (row, event) in events.iter().enumerate() {
            let receipt = RetryReceipt {
                delivery_identity: (event.connector_delivery_hash_low, event.connector_delivery_hash_high),
                tenant_id: self.tenant_id,
                commit: SequencePoint {
                    epoch: range.epoch,
                    sequence: range.first_sequence + row as u64,
                },
                shard: self.shard,
                frame_offset,
                status_class,
                expiry_physical_nanos: now + self.ack_guard_nanos,
                dedupe: (event.envelope.dedupe_hash_low, event.envelope.dedupe_hash_high),
            };
            retry.record(receipt.clone());
            receipts.push(receipt);
        }
        // Record-heavy flushes must not grow the retry store without bound; sweep receipts whose guard window has
        // already passed even when no replay ever consults them.
        retry.evict_expired(now);
        receipts
    }

    /// Builds the batch payload and HEJ frame for `events` under the leased `range`, then appends and syncs it.
    /// Returns the frame bytes and its storage offset once durable; the caller verifies the stored bytes by reading
    /// them back before receipting. The batch-id counters advance only on success, so a failed, aborted flush leaves
    /// no numbering gap. A failure before `append` returns `Storage` and the range can be safely voided; a `sync`
    /// failure after a successful `append` returns `SyncAfterAppend`, because the appended bytes may still become
    /// durable and the range must not be voided.
    fn build_and_persist(
        &mut self,
        events: &[EventInput],
        range: SequenceRange,
        now: i64,
        storage: &mut dyn JournalStorage,
    ) -> Result<(Vec<u8>, u64), FlushError> {
        let payload = build_batch(events, self.schema_generation, 0).map_err(FlushError::Format)?;
        let durable_batch_id = self.durable_batch_counter + 1;
        let writer_local_batch_id = self.writer_local_batch_counter + 1;
        let frame_bytes = build_frame(
            &FrameBuildInput {
                flags: 0,
                tenant_id: self.tenant_id,
                writer_id: self.worker_id,
                epoch: range.epoch,
                first_sequence: range.first_sequence,
                last_sequence: range.last_sequence,
                event_count: events.len() as u32,
                durable_batch_id,
                writer_local_batch_id,
                schema_generation: self.schema_generation,
                dictionary_generation_hint: 0,
                created_at_physical: now,
                committed_at_physical: now,
            },
            &payload,
        )
        .map_err(FlushError::Format)?;

        let frame_offset = storage.append(self.shard, &frame_bytes).map_err(FlushError::Storage)?;
        // Durability barrier per the selected policy (direct-I/O completion or buffered fdatasync-equivalent). A sync
        // failure leaves the appended bytes in one of two states, and which one it is decides whether the range may be
        // voided, so the offset is read back to tell them apart: bytes that are not there cannot become durable and the
        // range is reported as a plain `Storage` failure the caller may void; bytes that are there may still be
        // persisted by a later sync, and `SyncAfterAppend` keeps the caller from voiding a range the frame may occupy.
        // Verification of the durable path happens in `flush`, against the bytes read back after a successful sync.
        if let Err(error) = storage.sync(self.shard) {
            // Only a read that succeeds and returns *other* bytes proves the frame is absent. A failed read proves
            // nothing — `append` already succeeded, so the bytes may be there and a later sync may persist them — and
            // treating it as absence would void a range the frame can still occupy, leaving overlapping event and void
            // coverage. An unreadable offset is therefore reported as `SyncAfterAppend` too.
            let absent = storage
                .read(self.shard, frame_offset, frame_bytes.len() as u32)
                .is_ok_and(|stored| stored != frame_bytes);
            return Err(if absent {
                FlushError::Storage(error)
            } else {
                FlushError::SyncAfterAppend { error, frame_offset }
            });
        }

        self.durable_batch_counter = durable_batch_id;
        self.writer_local_batch_counter = writer_local_batch_id;
        Ok((frame_bytes, frame_offset))
    }

    /// Closes expired reservation leases with internal void records: zero events, the void flag, the abandoned range;
    /// in the hash chain like any frame, never a user event or public output. A lease is retired only after its void
    /// record is durable, so a failed append or sync leaves that lease and every later one still outstanding for a
    /// subsequent pass to close rather than dropping their ranges uncovered. Returns the ranges durably voided.
    pub fn commit_voids(
        &mut self,
        allocator: &mut SequenceAllocator,
        storage: &mut dyn JournalStorage,
        watermarks: &mut WatermarkTracker,
        clock: &dyn MonotonicClock,
    ) -> Result<Vec<SequenceRange>, FlushError> {
        let now = clock.now_nanos();
        let mut voided = Vec::new();
        for (lease_id, range) in allocator.expired_leases(clock.monotonic_nanos()) {
            let durable_batch_id = self.durable_batch_counter + 1;
            let writer_local_batch_id = self.writer_local_batch_counter + 1;
            let frame_bytes = build_frame(
                &FrameBuildInput {
                    flags: FLAG_VOID_RECORD,
                    tenant_id: self.tenant_id,
                    writer_id: self.worker_id,
                    epoch: range.epoch,
                    first_sequence: range.first_sequence,
                    last_sequence: range.last_sequence,
                    event_count: 0,
                    durable_batch_id,
                    writer_local_batch_id,
                    schema_generation: self.schema_generation,
                    dictionary_generation_hint: 0,
                    created_at_physical: now,
                    committed_at_physical: now,
                },
                &[],
            )
            .map_err(FlushError::Format)?;
            storage.append(self.shard, &frame_bytes).map_err(FlushError::Storage)?;
            storage.sync(self.shard).map_err(FlushError::Storage)?;
            // Durable void record written: only now retire the lease. A failure above returns before this line, so the
            // lease stays outstanding and its range remains expirable on a later pass.
            allocator.close_voided(lease_id);
            self.durable_batch_counter = durable_batch_id;
            self.writer_local_batch_counter = writer_local_batch_id;
            // Voided sequences are permanently skipped: durable and visible coverage advance (they publish zero rows).
            watermarks.record_durable(range);
            watermarks.record_visible(range);
            voided.push(range);
        }
        Ok(voided)
    }

    /// The acknowledgement gate for a committed receipt under the route's visibility contract. Acknowledged HEJ events
    /// not yet HEF-covered are never silently omitted: under `ReadAfterAck`, acknowledgement waits for
    /// `visibility_watermark` to include the event instead.
    pub fn ack_ready(
        receipt: &RetryReceipt,
        state: CommitState,
        visibility: AckVisibility,
        watermarks: &WatermarkTracker,
    ) -> bool {
        if state != CommitState::Committed {
            return false;
        }
        match visibility {
            AckVisibility::Durability => true,
            // Coverage is answered within the receipt's own epoch, so an event published in an earlier epoch stays
            // acknowledgeable after later epochs begin.
            AckVisibility::ReadAfterAck => watermarks.visibly_covers(receipt.commit),
        }
    }
}

#[cfg(test)]
#[path = "test/pipeline.rs"]
mod tests;
