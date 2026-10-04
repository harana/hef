//! Checks the journal batch append: admission is decided for the whole batch before any event of it is queued, so a
//! refused batch leaves nothing queued, durable, or visible and is retryable as one unit, and an admitted batch
//! commits one contiguous `(epoch, sequence)` sub-range in submission order with one receipt per event.

use crate::support;
use hef::artifacts::batch::EventInput;
use hef::writer::error::QueueError;
use hef::writer::pipeline::{FlushReason, RouteDependency, Submission, WorkerCommitPipeline};
use hef::writer::queue::PendingRecord;

/// conformance: hef-apis/journal-batch-append-admits-a-caller-batch-whole-or-not-at-all/a-batch-that-does-not-fit-admits-nothing
#[test]
fn a_batch_that_does_not_fit_admits_nothing() {
    let mut world = support::World::new(7);
    // A small bounded queue so a handful of filler events leaves too little room for the batch as a whole.
    let mut worker = WorkerCommitPipeline::new(
        1,
        support::SHARD,
        support::tenant(),
        RouteDependency::AppendOnly,
        8 * 1024,
    );
    let batch: Vec<EventInput> = (100..106).map(support::event).collect();
    let batch_records: Vec<PendingRecord> = batch
        .iter()
        .cloned()
        .map(|event| PendingRecord {
            epoch: 1,
            event,
            tenant_id: support::tenant(),
        })
        .collect();

    // Fill with single submits until the whole batch no longer fits.
    let mut fillers = 0u64;
    while worker.queue().can_admit(&batch_records).unwrap() {
        worker
            .submit(support::event(fillers), 1, &mut world.retry, &world.clock)
            .unwrap();
        fillers += 1;
    }

    // The batch is refused whole: nothing from it is queued and the queue's cursors are untouched.
    let clean_before = worker.queue().clean_cursor();
    let dirty_before = worker.queue().dirty_cursor();
    let pending_before = worker.queue().pending_bytes();
    assert_eq!(
        worker.submit_batch(batch.clone(), 1, &mut world.retry, &world.clock),
        Err(QueueError::Full),
        "a batch the queue cannot hold whole is refused as one unit"
    );
    assert_eq!(worker.queue().clean_cursor(), clean_before);
    assert_eq!(worker.queue().dirty_cursor(), dirty_before);
    assert_eq!(worker.queue().pending_bytes(), pending_before);

    // The flush that drains the queue carries only the fillers: no prefix of the refused batch ever became durable.
    let result = worker
        .flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &mut world.overlay,
            &world.clock,
        )
        .unwrap();
    assert_eq!(result.receipts.len(), fillers as usize);

    // Room now exists: the identical batch is admitted whole and flushes as one contiguous sub-range.
    assert_eq!(
        worker.submit_batch(batch, 1, &mut world.retry, &world.clock).unwrap(),
        vec![Submission::Ready; 6]
    );
    let result = worker
        .flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &mut world.overlay,
            &world.clock,
        )
        .unwrap();
    assert_eq!(result.receipts.len(), 6);
    assert_eq!(
        result.range.last_sequence - result.range.first_sequence + 1,
        6,
        "the admitted batch occupies one contiguous sequence range"
    );
}

/// conformance: hef-apis/journal-batch-append-admits-a-caller-batch-whole-or-not-at-all/an-admitted-batch-commits-one-contiguous-sub-range-in-submission-order
#[test]
fn an_admitted_batch_commits_one_contiguous_sub_range_in_submission_order() {
    let mut world = support::World::new(11);
    // Two single events ahead of the batch, so the batch's sub-range starts mid-frame rather than at the frame start.
    world
        .worker
        .submit(support::event(0), 1, &mut world.retry, &world.clock)
        .unwrap();
    world
        .worker
        .submit(support::event(1), 1, &mut world.retry, &world.clock)
        .unwrap();
    let batch: Vec<EventInput> = (200..203).map(support::event).collect();
    assert_eq!(
        world
            .worker
            .submit_batch(batch, 1, &mut world.retry, &world.clock)
            .unwrap(),
        vec![Submission::Ready; 3]
    );

    let result = world
        .worker
        .flush(
            FlushReason::Target,
            &mut world.allocator,
            &mut world.storage,
            &mut world.watermarks,
            &mut world.retry,
            &mut world.overlay,
            &world.clock,
        )
        .unwrap();
    assert_eq!(result.range.first_sequence, 1);
    assert_eq!(result.range.last_sequence, 5);
    assert_eq!(result.receipts.len(), 5, "one receipt position per submitted event");

    // The batch's receipts sit at sequences 3..=5 in the caller's submission order, identified by each event's
    // connector delivery identity (`support::event(i)` uses 5000 + i).
    for (row, receipt) in result.receipts[2..].iter().enumerate() {
        assert_eq!(receipt.commit.sequence, 3 + row as u64);
        assert_eq!(receipt.delivery_identity, (5_000 + 200 + row as u64, 1));
    }
}
