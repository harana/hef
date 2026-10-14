use super::*;
use crate::events::variant::VariantValue;
use crate::typed_id::TypedIdTestExt;
use std::collections::BTreeMap;

fn record(i: u64, epoch: u64) -> PendingRecord {
    PendingRecord {
        epoch,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xBB00 + u128::from(i)),
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
                entity_id: None,
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
            payload: PayloadInput::None,
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: 5000 + i,
            connector_delivery_hash_high: 1,
            provenance: None,
            relationships: None,
        },
        tenant_id: TenantId::new_test_id(9),
    }
}

#[test]
fn a_queued_payload_comes_back_encoded_and_still_means_the_same_value() {
    // A pending record's payload is carried on in the encoded form it was stored in rather than inflated into a value
    // tree, so what comes back out must still decode to exactly what went in — and re-serializing it must reproduce
    // the same record bytes, since a steal pushes a copied record straight back into another queue.
    let value = VariantValue::Object(BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(4_200)),
        (
            "nested".to_owned(),
            VariantValue::Array(vec![VariantValue::String("x".to_owned()), VariantValue::Null]),
        ),
    ]));
    let mut original = record(0, 1);
    original.event.payload = PayloadInput::Variant(value.clone());

    let bytes = encode_pending(&original).unwrap();
    let decoded = decode_pending(&bytes).unwrap();
    let PayloadInput::Encoded(payload) = &decoded.event.payload else {
        panic!("a queued variant payload comes back encoded");
    };
    assert_eq!(
        VariantRef::new(&payload.bytes).decode(&payload.dictionary).unwrap(),
        value
    );
    assert_eq!(encode_pending(&decoded).unwrap(), bytes);
}

#[test]
fn a_corrupt_queued_payload_refuses_instead_of_travelling_on() {
    // The payload is no longer proven decodable by being decoded, so a record whose payload bytes are damaged must be
    // rejected here rather than copied into a frame no reader can decode.
    let mut original = record(0, 1);
    original.event.payload = PayloadInput::Variant(VariantValue::Object(BTreeMap::from([(
        "amount".to_owned(),
        VariantValue::Int(4_200),
    )])));

    let bytes = encode_pending(&original).unwrap();
    let mut damaged = bytes.clone();
    let last = damaged.len() - 1;
    damaged[last] ^= 0xFF;
    assert!(decode_pending(&damaged).is_err());
}

#[test]
fn hardened_cursor_advances_past_a_stolen_region() {
    // Regression: a stolen region got no `mark_hardened` on the source, so the durability boundary stalled at its
    // start forever and every later self-hardened region piled up in the pending list unbounded.
    let mut queue = CommitQueue::new(1 << 16);
    queue.push(&record(0, 1)).unwrap();
    let (stolen, _) = queue.copy_pending().unwrap();
    assert!(queue.commit_claim(stolen)); // a peer steals the region
    queue.push(&record(1, 1)).unwrap();
    let (own, _) = queue.copy_pending().unwrap();
    assert!(queue.claim_for_flush(own));
    queue.mark_hardened(own);
    assert_eq!(
        queue.hardened_cursor(),
        own.end,
        "the durability boundary must jump the stolen gap instead of stalling at its start"
    );
}

#[test]
fn hardened_cursor_advances_past_an_aborted_region() {
    let mut queue = CommitQueue::new(1 << 16);
    queue.push(&record(0, 1)).unwrap();
    let (aborted, _) = queue.copy_pending().unwrap();
    assert!(queue.claim_for_flush(aborted));
    queue.abort_flush(aborted);
    assert_eq!(
        queue.hardened_cursor(),
        aborted.end,
        "an aborted region is resolved (its range is closed by a void), not pinned"
    );
    queue.push(&record(1, 1)).unwrap();
    let (own, _) = queue.copy_pending().unwrap();
    assert!(queue.claim_for_flush(own));
    queue.mark_hardened(own);
    assert_eq!(queue.hardened_cursor(), own.end);
}

#[test]
fn an_earlier_in_flight_region_still_gates_the_durability_boundary() {
    // Stolen and aborted gaps are jumped, but a region this queue itself still has in flight is not: a later
    // completion must never make an earlier still-in-flight region look durable.
    let mut queue = CommitQueue::new(1 << 16);
    queue.push(&record(0, 1)).unwrap();
    let (first, _) = queue.copy_pending().unwrap();
    assert!(queue.claim_for_flush(first));
    queue.push(&record(1, 1)).unwrap();
    let (second, _) = queue.copy_pending().unwrap();
    assert!(queue.claim_for_flush(second));
    queue.mark_hardened(second);
    assert_eq!(queue.hardened_cursor(), first.start);
    queue.mark_hardened(first);
    assert_eq!(queue.hardened_cursor(), second.end);
}

#[test]
fn a_stale_claim_fails_after_the_cursor_moved() {
    let mut queue = CommitQueue::new(1 << 16);
    queue.push(&record(0, 1)).unwrap();
    let (region, _) = queue.copy_pending().unwrap();
    assert!(queue.commit_claim(region));
    assert!(
        !queue.commit_claim(region),
        "a stale claim must fail once the cursor moved"
    );
}

#[test]
fn copy_matching_prefix_splits_at_the_epoch_boundary() {
    let tenant = TenantId::new_test_id(9);
    let mut queue = CommitQueue::new(1 << 16);
    queue.push(&record(0, 1)).unwrap();
    queue.push(&record(1, 1)).unwrap();
    queue.push(&record(2, 2)).unwrap();

    let (prefix, records) = queue.copy_matching_prefix(tenant, 1, u64::MAX).unwrap();
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|record| record.epoch == 1));
    assert!(queue.claim_for_flush(prefix));

    // The epoch-2 record is untouched and still pending after the prefix claim.
    let (rest, remaining) = queue.copy_pending().unwrap();
    assert_eq!(rest.start, prefix.end);
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].epoch, 2);

    // A front record of another epoch yields an empty prefix: nothing to claim, nothing dropped.
    let (empty, none) = queue.copy_matching_prefix(tenant, 1, u64::MAX).unwrap();
    assert!(none.is_empty());
    assert_eq!(empty.start, empty.end);
}

#[test]
fn copy_matching_prefix_stops_once_it_reaches_the_byte_limit() {
    let tenant = TenantId::new_test_id(9);
    let mut queue = CommitQueue::new(1 << 16);
    for i in 0..4 {
        queue.push(&record(i, 1)).unwrap();
    }
    let (one, _) = queue.copy_matching_prefix(tenant, 1, 1).unwrap();
    let record_bytes = one.end - one.start;

    // The record that reaches the limit is the last one copied.
    let (prefix, records) = queue.copy_matching_prefix(tenant, 1, record_bytes + 1).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(prefix.end - prefix.start, 2 * record_bytes);
    let (_, all) = queue.copy_matching_prefix(tenant, 1, u64::MAX).unwrap();
    assert_eq!(all.len(), 4);
}

/// A batch admitted in one step lands exactly what pushing its records one by one would, and a batch too large for
/// the queue is refused whole, with nothing written.
#[test]
fn an_admitted_batch_lands_exactly_what_per_record_pushes_would() {
    let records: Vec<PendingRecord> = (0..4).map(|i| record(i, 1)).collect();

    let mut per_record = CommitQueue::new(1 << 16);
    for pending in &records {
        per_record.push(pending).unwrap();
    }
    let mut batched = CommitQueue::new(1 << 16);
    let admission = batched.admit(&records).unwrap().expect("the batch fits");
    batched.push_admitted(&admission).unwrap();

    assert_eq!(batched.dirty_cursor(), per_record.dirty_cursor());
    assert_eq!(batched.copy_pending().unwrap(), per_record.copy_pending().unwrap());

    let mut small = CommitQueue::new(1024);
    let too_many: Vec<PendingRecord> = (0..64).map(|i| record(i, 1)).collect();
    assert!(
        small.admit(&too_many).unwrap().is_none(),
        "a batch that does not fit is refused whole"
    );
    assert_eq!(small.dirty_cursor(), 0, "a refused batch writes nothing");
}

/// The source queue for the raw-steal tests: `count` epoch-1 records whose pending region crosses the buffer end, so
/// a walk over it has to step over a skip marker.
fn wrapped_source(count: u64) -> CommitQueue {
    let mut queue = CommitQueue::new(2048);
    // One record a cache line longer than the rest, so the later ones no longer end exactly on the buffer end.
    let mut longer = record(0, 1);
    longer.event.envelope.entity_id = Some("x".repeat(RECORD_ALIGN as usize));
    queue.push(&longer).unwrap();
    let mut i = 1;
    while queue.push(&record(i, 1)).is_ok() {
        i += 1;
    }
    let (stolen, _) = queue.copy_pending().unwrap();
    assert!(queue.commit_claim(stolen));
    for i in 0..count {
        queue.push(&record(10 + i, 1)).unwrap();
    }
    let (region, _) = queue.copy_pending().unwrap();
    assert_ne!(
        region.start / queue.capacity,
        (region.end - 1) / queue.capacity,
        "the pending region wraps"
    );
    queue
}

/// The record bytes an admission lands, read back after pushing it into a fresh queue.
fn landed(admission: &QueueAdmission) -> Vec<Vec<u8>> {
    let mut queue = CommitQueue::new(1 << 16);
    queue.push_admitted(admission).unwrap();
    let (_, records) = queue.copy_pending_raw().unwrap();
    records.into_iter().map(<[u8]>::to_vec).collect()
}

#[test]
fn walking_raw_records_matches_the_collected_copy() {
    let empty = CommitQueue::new(1024);
    let (region, mut records) = empty.pending_raw();
    assert_eq!(region.start, region.end);
    assert!(records.next().is_none());

    for queue in [wrapped_source(1), wrapped_source(3)] {
        let (walked_region, walked) = queue.pending_raw();
        let walked: Vec<&[u8]> = walked.map(Result::unwrap).collect();
        assert_eq!((walked_region, walked), queue.copy_pending_raw().unwrap());
    }
}

#[test]
fn admitting_raw_records_lands_the_same_bytes_as_admitting_a_collected_copy() {
    let source = wrapped_source(3);
    let (_, collected) = source.copy_pending_raw().unwrap();
    let (_, walk) = source.pending_raw();
    let destination = CommitQueue::new(1 << 16);

    let (admission, count) = destination.admit_raw(walk).unwrap().expect("the region fits");
    assert_eq!(count, 3);
    let expected = destination.admit_encoded(&collected).unwrap().expect("the region fits");
    assert_eq!(landed(&admission), landed(&expected));
    let originals: Vec<Vec<u8>> = collected.iter().map(|record| record.to_vec()).collect();
    assert_eq!(landed(&admission), originals);
}

#[test]
fn a_raw_region_that_does_not_fit_whole_is_refused() {
    let slot = (LEN_HEADER + encode_pending(&record(0, 1)).unwrap().len() as u64).next_multiple_of(RECORD_ALIGN);
    let room = 1024 / slot;
    let source = wrapped_source(room);
    let destination = CommitQueue::new(1024);

    // The region uses every slot an empty destination has.
    let (_, walk) = source.pending_raw();
    let (_, count) = destination
        .admit_raw(walk)
        .unwrap()
        .expect("the region fills the destination");
    assert_eq!(count as u64, room);

    // A destination one record short refuses the last record and so the whole region.
    let mut short = CommitQueue::new(1024);
    short.push(&record(99, 1)).unwrap();
    let (_, walk) = source.pending_raw();
    assert!(short.admit_raw(walk).unwrap().is_none());
    let (_, collected) = source.copy_pending_raw().unwrap();
    assert!(short.admit_encoded(&collected).unwrap().is_none());

    // A full destination refuses the first record.
    let mut full = CommitQueue::new(1024);
    while full.push(&record(99, 1)).is_ok() {}
    let dirty = full.dirty_cursor();
    let (_, walk) = source.pending_raw();
    assert!(full.admit_raw(walk).unwrap().is_none());
    assert_eq!(full.dirty_cursor(), dirty, "a refused admission writes nothing");
}

#[test]
fn a_steal_refuses_a_region_with_another_epoch_or_broken_framing() {
    let tenant = TenantId::new_test_id(9);
    let mut queue = wrapped_source(2);
    assert!(peek_steal_raw(&queue, tenant, 1, true).is_some());
    assert!(peek_steal_raw(&queue, tenant, 2, true).is_none());
    assert!(peek_steal_raw(&queue, TenantId::new_test_id(8), 1, true).is_none());
    assert!(peek_steal_raw(&queue, tenant, 1, false).is_none());

    // A record whose length runs past the buffer ends the walk with an error, and the region is not stealable.
    let start = queue.clean_cursor();
    let corrupt_len = queue.capacity * 2;
    queue.write_at(start, &corrupt_len.to_le_bytes());
    let (_, mut walk) = queue.pending_raw();
    assert!(matches!(walk.next(), Some(Err(QueueError::Codec))));
    assert!(walk.next().is_none());
    assert!(peek_steal_raw(&queue, tenant, 1, true).is_none());
    assert!(peek_steal(&queue, tenant, 1, true).is_none());
    let (_, walk) = queue.pending_raw();
    assert!(CommitQueue::new(1 << 16).admit_raw(walk).is_err());
}

#[test]
fn a_steal_refuses_a_record_too_short_to_carry_its_tenant_and_epoch() {
    let mut queue = CommitQueue::new(1024);
    queue.push_encoded(&[0; 8]).unwrap();
    assert!(peek_steal_raw(&queue, TenantId::new_test_id(9), 1, true).is_none());
}
