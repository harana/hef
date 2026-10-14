//! Unit tests for deletes, corrections, and per-subject crypto-shredding: the payload key never reuses an AEAD nonce,
//! and rebuilds stay erasure- and tamper-aware.

use super::*;
use crate::security::SealedContentKey;
use crate::typed_id::TypedIdTestExt;
use std::collections::BTreeSet;

const FILE_ID: u128 = 0xF11E_D00D;

#[test]
fn a_field_redaction_withholds_the_whole_raw_payload_of_its_row_only() {
    let mut redaction = FieldDeletionVector::new(vec!["payload.body".to_owned()]);
    redaction.mark_affected(4);
    assert!(redaction.withholds_raw_payload(4));
    assert!(!redaction.withholds_raw_payload(5));
}

fn correction_of(event_id: EventId) -> CorrectionMetadata {
    CorrectionMetadata {
        correction_epoch: 1,
        correction_generation: 1,
        correction_sequence: 1,
        correction_type: CorrectionType::Replacement,
        corrects_event_id: event_id,
    }
}

#[test]
fn corrections_suppress_only_their_superseded_events() {
    // Regression for the indexed correction lookup: each correction suppresses exactly the ordinal of the event it
    // supersedes, a correction of an event outside the file marks nothing, and duplicate corrections stay idempotent.
    let events = [
        (0, EventId::new_test_id(10)),
        (1, EventId::new_test_id(11)),
        (2, EventId::new_test_id(12)),
    ];
    let corrections = [
        correction_of(EventId::new_test_id(11)),
        correction_of(EventId::new_test_id(999)),
        correction_of(EventId::new_test_id(11)),
    ];

    let vector = latest_corrected_deletion_vector(&events, &corrections);
    assert!(!vector.is_deleted(0));
    assert!(vector.is_deleted(1), "the superseded event's ordinal is suppressed");
    assert!(!vector.is_deleted(2));
    assert_eq!(
        vector.deleted_count(),
        1,
        "a correction of an absent event marks nothing"
    );
}

#[test]
fn bulk_correction_vector_matches_marking_each_correction_in_turn() {
    // Differential check of the sort-then-fold construction against marking each match one at a time: same runs,
    // count and wire bytes for shuffled, reversed, duplicated, contiguous and unknown targets, repeated event ids, and
    // ordinals near the top of the range.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    let high = u64::MAX - 1;
    for case in 0..300u64 {
        let event_count = next(40);
        let events: Vec<(u64, EventId)> = (0..event_count)
            .map(|i| {
                let ordinal = match case % 3 {
                    0 => next(4096),
                    1 => next(48),
                    _ => high - next(8),
                };
                (ordinal, EventId::new_test_id(u128::from(i % 30)))
            })
            .collect();
        let corrections: Vec<CorrectionMetadata> = (0..next(60))
            .map(|_| correction_of(EventId::new_test_id(u128::from(next(35)))))
            .collect();

        let mut expected = DeletionVector::new();
        let mut first_ordinal = HashMap::new();
        for (ordinal, id) in &events {
            first_ordinal.entry(*id).or_insert(*ordinal);
        }
        for correction in &corrections {
            if let Some(ordinal) = first_ordinal.get(&correction.corrects_event_id) {
                expected.mark_deleted(*ordinal);
            }
        }

        let actual = latest_corrected_deletion_vector(&events, &corrections);
        assert_eq!(actual.deleted_runs, expected.deleted_runs, "case {case}");
        assert_eq!(actual.deleted_count(), expected.deleted_count(), "case {case}");
        assert_eq!(
            encode_deletion_vector(&actual),
            encode_deletion_vector(&expected),
            "case {case}"
        );
    }
}

#[test]
fn ledger_keeps_invalidation_order_and_never_double_queues() {
    // Regression for the keyed ledger: membership moved to hash lookups, but the queue must still read back in
    // invalidation order with no duplicates, and a republish must clear exactly its own event.
    let first = EventId::new_test_id(1);
    let neighbor = EventId::new_test_id(2);
    let second = EventId::new_test_id(3);

    let mut ledger = DerivedColumnLedger::new();
    ledger.invalidate(first, &[neighbor, neighbor]);
    ledger.invalidate(second, &[first]);
    assert_eq!(
        ledger.pending_recomputes(),
        &[first, neighbor, second],
        "invalidation order is kept and re-invalidation never queues an event twice"
    );
    assert_eq!(ledger.published_value(first), None);
    assert_eq!(ledger.published_value(neighbor), None);

    ledger.republish(neighbor, 0.5, 9);
    assert_eq!(ledger.pending_recomputes(), &[first, second]);
    assert_eq!(ledger.published_value(neighbor), Some(0.5));
    assert_eq!(
        ledger.published_value(first),
        None,
        "still pending until its own republish"
    );
}

#[test]
fn republish_tombstones_in_place_and_a_later_reinvalidation_queues_once() {
    // Regression for the O(1) republish: a republished event's old queue slot is cleared rather than searched for
    // and removed, so re-invalidating that same event later must not resurrect the stale slot or queue it twice.
    let event = EventId::new_test_id(1);
    let mut ledger = DerivedColumnLedger::new();

    ledger.invalidate(event, &[]);
    ledger.republish(event, 1.0, 1);
    assert!(ledger.pending_recomputes().is_empty());

    ledger.invalidate(event, &[]);
    assert_eq!(
        ledger.pending_recomputes(),
        &[event],
        "queued exactly once, not duplicated"
    );
    assert_eq!(ledger.published_value(event), None, "pending again until republished");
}

#[test]
fn churning_one_event_keeps_the_queue_bounded_by_outstanding_work() {
    // Regression for tombstone build-up: invalidating and republishing the same event over and over while another
    // event stays pending must not grow the queue with every cycle, and once nothing is pending the queue is empty
    // so a poll inspects no slots at all.
    let waiting = EventId::new_test_id(1);
    let hot = EventId::new_test_id(2);
    let mut ledger = DerivedColumnLedger::new();
    ledger.invalidate(waiting, &[]);
    for cycle in 0..10_000 {
        ledger.invalidate(hot, &[hot]);
        ledger.republish(hot, 1.0, cycle);
        assert!(ledger.recompute_queue.len() <= RECOMPUTE_QUEUE_COMPACTION_FLOOR + 1);
    }
    assert_eq!(ledger.pending_recomputes(), &[waiting]);

    ledger.invalidate(hot, &[]);
    assert_eq!(
        ledger.pending_recomputes(),
        &[waiting, hot],
        "re-invalidating a completed event appends it after the jobs already waiting"
    );
    ledger.republish(waiting, 2.0, 1);
    ledger.republish(hot, 3.0, 1);
    assert!(ledger.recompute_queue.is_empty());
    assert_eq!(ledger.published_value(waiting), Some(2.0));
    assert_eq!(ledger.published_value(hot), Some(3.0));
}

#[test]
fn ledger_matches_a_plain_ordered_list_under_random_invalidations_and_republishes() {
    // Differential check of the compacting queue against the obvious model: an ordered list of pending events plus
    // each event's last published value. Enough distinct events are used to push the queue past the compaction floor.
    let mut ledger = DerivedColumnLedger::new();
    let mut pending: Vec<EventId> = Vec::new();
    let mut published: HashMap<EventId, f64> = HashMap::new();
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |bound: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % bound
    };
    for step in 0..20_000u64 {
        let event = EventId::new_test_id(u128::from(next(200)));
        if next(2) == 0 {
            let neighbors: Vec<EventId> = (0..next(4))
                .map(|_| EventId::new_test_id(u128::from(next(200))))
                .collect();
            ledger.invalidate(event, &neighbors);
            for id in std::iter::once(event).chain(neighbors) {
                published.remove(&id);
                if !pending.contains(&id) {
                    pending.push(id);
                }
            }
        } else {
            let value = step as f64;
            ledger.republish(event, value, step);
            pending.retain(|id| *id != event);
            published.insert(event, value);
        }

        assert_eq!(ledger.pending_recomputes(), pending, "step {step}");
        for (id, position) in &ledger.queue_position {
            assert_eq!(ledger.recompute_queue[*position], Some(*id), "step {step}");
        }
        assert_eq!(
            ledger.recompute_queue.iter().flatten().count(),
            ledger.queue_position.len()
        );
        for n in 0..200 {
            let id = EventId::new_test_id(n);
            assert_eq!(ledger.published_value(id), published.get(&id).copied(), "step {step}");
        }
    }
}

#[test]
fn a_key_resumed_from_a_sealing_checkout_stays_inside_its_epoch_range() {
    // A subject key built from a keystore checkout owns only its reserved epochs: re-seals roll inside the range,
    // sealing refuses once it is spent, and the next checkout's key seals the same block under fresh nonces — so no
    // two runs (restart or concurrent node) can ever repeat a nonce.
    let scheme = AeadScheme::default();
    let mut resumed = SubjectContentKey::from_sealing_key(
        SealedContentKey::with_epoch_range(ContentKey::new([5u8; 32]), 0, 2, scheme).unwrap(),
    );
    let first = resumed.encrypt(7, FILE_ID, b"original").unwrap();
    let second = resumed.encrypt(7, FILE_ID, b"corrected").unwrap();
    assert!(
        resumed.encrypt(7, FILE_ID, b"again").is_none(),
        "a spent epoch range refuses to seal rather than enter another checkout's epochs"
    );

    let mut next_checkout = SubjectContentKey::from_sealing_key(
        SealedContentKey::with_epoch_range(ContentKey::new([5u8; 32]), 2, 4, scheme).unwrap(),
    );
    let third = next_checkout.encrypt(7, FILE_ID, b"after restart").unwrap();
    assert_eq!(
        next_checkout.key_epoch(),
        2,
        "the resumed key seals from its reserved epoch"
    );

    let nonce_len = scheme.nonce_len();
    let nonces: hashbrown::HashSet<_> = [&first, &second, &third]
        .iter()
        .map(|blob| blob.get(..nonce_len))
        .collect();
    assert_eq!(nonces.len(), 3, "no nonce repeats within or across checkouts");

    // Every generation of the block still opens under the same key material.
    assert_eq!(
        rebuild_payload(&next_checkout, 7, FILE_ID, &first),
        RebuiltPayload::Payload(b"original".to_vec())
    );
    assert_eq!(
        rebuild_payload(&next_checkout, 7, FILE_ID, &third),
        RebuiltPayload::Payload(b"after restart".to_vec())
    );
}

#[test]
fn resealing_a_block_id_never_reuses_a_nonce() {
    // A caller that seals the same block id twice (e.g. a corrected block) must not drive an AES-GCM nonce reuse. The
    // key rolls its epoch so the second seal gets a fresh nonce, and both blobs still open to their own plaintext.
    let mut key = SubjectContentKey::generate([5u8; 32]);
    let first = key.encrypt(7, FILE_ID, b"original").expect("first seal succeeds");
    let second = key
        .encrypt(7, FILE_ID, b"corrected")
        .expect("a re-seal of the same block id succeeds under a fresh nonce");

    let nonce_len = key.scheme().nonce_len();
    assert_ne!(
        first.get(..nonce_len),
        second.get(..nonce_len),
        "re-sealing one block id must roll to a fresh nonce, never repeat one"
    );

    assert_eq!(
        rebuild_payload(&key, 7, FILE_ID, &first),
        RebuiltPayload::Payload(b"original".to_vec())
    );
    assert_eq!(
        rebuild_payload(&key, 7, FILE_ID, &second),
        RebuiltPayload::Payload(b"corrected".to_vec())
    );
}

#[test]
fn cloning_a_subject_key_cannot_reuse_a_nonce() {
    // #3924: a `SubjectContentKey` holds a shared sealing guard, so cloning it must share the nonce history rather than
    // fork a private copy. Two clones sealing the same block id must not both seal their differing plaintext under one
    // nonce.
    let key = SubjectContentKey::generate([3u8; 32]);
    let mut original = key.clone();
    let mut clone = key;

    let sealed_a = original.encrypt(5, FILE_ID, b"from-original").unwrap();
    let sealed_b = clone.encrypt(5, FILE_ID, b"from-clone").unwrap();

    let nonce_len = original.scheme().nonce_len();
    assert_ne!(
        sealed_a.get(..nonce_len),
        sealed_b.get(..nonce_len),
        "two clones of one subject key must never seal different plaintexts under the same nonce"
    );
}

#[test]
fn distinct_block_ids_seal_under_distinct_nonces() {
    let mut key = SubjectContentKey::generate([9u8; 32]);
    let a = key.encrypt(1, FILE_ID, b"a").unwrap();
    let b = key.encrypt(2, FILE_ID, b"b").unwrap();
    let nonce_len = key.scheme().nonce_len();
    assert_ne!(a.get(..nonce_len), b.get(..nonce_len));
}

#[test]
fn a_destroyed_key_seals_nothing_and_rebuilds_as_a_tombstone() {
    let mut key = SubjectContentKey::generate([1u8; 32]);
    let sealed = key.encrypt(1, FILE_ID, b"pii").unwrap();
    key.destroy();
    assert!(
        key.encrypt(2, FILE_ID, b"more").is_none(),
        "a destroyed key seals nothing"
    );
    assert_eq!(rebuild_payload(&key, 1, FILE_ID, &sealed), RebuiltPayload::Tombstone);
}

#[test]
fn a_tampered_block_is_rejected_not_returned_altered() {
    let mut key = SubjectContentKey::generate([2u8; 32]);
    let mut sealed = key.encrypt(4, FILE_ID, b"single-subject payload").unwrap();
    let last = sealed.len() - 1;
    if let Some(byte) = sealed.get_mut(last) {
        *byte ^= 0xff;
    }
    assert_eq!(rebuild_payload(&key, 4, FILE_ID, &sealed), RebuiltPayload::Rejected);
}

#[test]
fn subtract_sum_reports_overflow_instead_of_panicking() {
    // Regression: both the total and deleted sums can be valid i128 aggregate metadata while their difference falls
    // outside i128, e.g. total_sum near MIN and deleted_sum positive (issue #9858). The caller must fall back to a
    // scan rather than the process aborting on an unchecked subtraction.
    let delta = DeletionAggregateDelta {
        deleted_count: 1,
        deleted_sum: Some(1),
    };
    assert_eq!(delta.subtract_sum(i128::MIN), None);

    let delta = DeletionAggregateDelta {
        deleted_count: 2,
        deleted_sum: Some(-1),
    };
    assert_eq!(delta.subtract_sum(i128::MAX), None);
}

#[test]
fn subtract_sum_adjusts_within_range() {
    let delta = DeletionAggregateDelta {
        deleted_count: 3,
        deleted_sum: Some(450),
    };
    assert_eq!(delta.subtract_sum(10_000), Some(9_550));

    let no_sum = DeletionAggregateDelta {
        deleted_count: 3,
        deleted_sum: None,
    };
    assert_eq!(no_sum.subtract_sum(5_000), None);
}

#[test]
fn a_block_sealed_for_one_file_is_rejected_when_opened_as_another() {
    // The seal binds the file identity as associated data, so a sealed block relocated into a different file — its
    // ciphertext copied verbatim under a mishandled key — fails authentication rather than decrypting as that file's
    // block (issue #4056).
    let mut key = SubjectContentKey::generate([4u8; 32]);
    let sealed = key.encrypt(3, FILE_ID, b"single-subject pii").unwrap();

    assert_eq!(
        rebuild_payload(&key, 3, FILE_ID, &sealed),
        RebuiltPayload::Payload(b"single-subject pii".to_vec()),
        "the block opens as block 3 of its own file"
    );
    assert_eq!(
        rebuild_payload(&key, 3, FILE_ID ^ 1, &sealed),
        RebuiltPayload::Rejected,
        "the same block opened under a different file id is rejected — ciphertext cannot be relocated across files"
    );
}

fn vector_of(ordinals: &[u64]) -> DeletionVector {
    let mut vector = DeletionVector::new();
    for ordinal in ordinals {
        vector.mark_deleted(*ordinal);
    }
    vector
}

#[test]
fn excluding_a_granule_span_matches_the_per_ordinal_check() {
    // The runs here start before the span, end inside it, sit wholly within it, straddle its end, and lie past it —
    // every way a run can meet the span a granule occupies. Checked against `is_deleted`, the search-per-row form it
    // replaces, and over every span offset so a partial overlap at either edge cannot pass by luck.
    let mut vector = vector_of(&[3, 4, 5, 20, 21, 40, 60, 61, 62, 63]);
    vector.mark_deleted(22);
    for first_ordinal in 0..70u64 {
        for rows in 0..12usize {
            let mut keep = vec![true; rows];
            vector.exclude_deleted(first_ordinal, &mut keep);
            let expected: Vec<bool> = (0..rows as u64)
                .map(|row| !vector.is_deleted(first_ordinal + row))
                .collect();
            assert_eq!(
                keep, expected,
                "span [{first_ordinal}, +{rows}) disagrees with is_deleted"
            );
        }
    }
}

#[test]
fn excluding_leaves_rows_already_dropped_dropped() {
    // A scan clears flags for several reasons — the deletion vector is only one — so exclusion must never revive a
    // row another rule has already dropped.
    let vector = vector_of(&[11]);
    let mut keep = vec![false, true, true, false];
    vector.exclude_deleted(10, &mut keep);
    assert_eq!(keep, vec![false, false, true, false]);
}

#[test]
fn marking_ordinals_in_any_order_matches_the_per_ordinal_set() {
    // The runs are built by merging spans, so the order marks arrive in — and repeats, neighbours, and overlaps —
    // must not change the set. Checked against the per-ordinal set the runs replaced.
    // `1` arrives last, closing the gap between two established runs, which must fold them into one.
    let ordinals: [u64; 15] = [7, 3, 4, 100, 99, 3, 5, 2, 101, 50, 6, 8, 0, 100, 1];
    let vector = vector_of(&ordinals);
    let expected: BTreeSet<u64> = ordinals.into_iter().collect();

    assert_eq!(
        vector.iter().collect::<Vec<_>>(),
        expected.iter().copied().collect::<Vec<_>>()
    );
    assert_eq!(vector.deleted_count(), expected.len() as u64);
    for ordinal in 0..110 {
        assert_eq!(
            vector.is_deleted(ordinal),
            expected.contains(&ordinal),
            "ordinal {ordinal} disagrees with the per-ordinal set"
        );
    }
    let rows: Vec<u64> = (0..110).collect();
    assert_eq!(
        vector.apply(&rows),
        rows.iter()
            .copied()
            .filter(|row| !expected.contains(row))
            .collect::<Vec<_>>()
    );
    // 0..=8 and 99..=101 each collapsed into one run; 50 stands alone.
    assert_eq!(vector.deleted_runs.len(), 3, "touching ordinals merge into one run");
}

#[test]
fn a_contiguous_deleted_span_decodes_into_one_run() {
    // Regression for the per-ordinal representation: a vector deleting a large contiguous span used to decode into one
    // entry per row. It decodes into the single run the bytes describe, whatever the span's length.
    let mut vector = DeletionVector::new();
    vector.mark_range(RowRange {
        start: 0,
        end: 8_000_000,
    });
    let (form, bytes) = encode_deletion_vector(&vector);
    assert_eq!(form, DeletionVectorWireForm::PositionalRoaring);

    let decoded = decode_deletion_vector(&bytes).unwrap();
    assert_eq!(decoded.deleted_runs.len(), 1);
    assert_eq!(decoded.deleted_count(), 8_000_000);
    assert!(decoded.is_deleted(7_999_999) && !decoded.is_deleted(8_000_000));
}

#[test]
fn a_sparse_vector_encodes_as_the_ordinal_array_and_round_trips() {
    let vector = vector_of(&[3, 17, 4_000, 90_000]);
    let (form, bytes) = encode_deletion_vector(&vector);
    assert_eq!(form, DeletionVectorWireForm::OrdinalArray);
    let decoded = decode_deletion_vector(&bytes).unwrap();
    assert_eq!(decoded.iter().collect::<Vec<_>>(), vector.iter().collect::<Vec<_>>());
    // The array is smaller than the run-bitmap form would be for scattered singletons: 4 runs cost 16 bytes each.
    assert!(bytes.len() < 4 * 16);
}

#[test]
fn a_dense_vector_encodes_as_positional_roaring_and_round_trips() {
    let vector = vector_of(&(0..10_000).collect::<Vec<_>>());
    let (form, bytes) = encode_deletion_vector(&vector);
    assert_eq!(form, DeletionVectorWireForm::PositionalRoaring);
    // One contiguous run stores in a handful of bytes where the array would cost 40 KB.
    assert!(bytes.len() < 64);
    let decoded = decode_deletion_vector(&bytes).unwrap();
    assert_eq!(decoded.deleted_count(), 10_000);
    assert!(decoded.is_deleted(0) && decoded.is_deleted(9_999) && !decoded.is_deleted(10_000));
}

#[test]
fn an_ordinal_beyond_u32_forces_the_roaring_form() {
    let vector = vector_of(&[5, u64::from(u32::MAX) + 10]);
    let (form, _) = encode_deletion_vector(&vector);
    assert_eq!(form, DeletionVectorWireForm::PositionalRoaring);
}

#[test]
fn both_wire_forms_apply_identically() {
    // The same deleted set pushed through each form filters a row list identically — the encoding changes bytes,
    // never which rows are excluded.
    let ordinals: Vec<u64> = (0..6_000).map(|i| i * 3).collect();
    let vector = vector_of(&ordinals);
    let (dense_form, dense_bytes) = encode_deletion_vector(&vector);
    assert_eq!(dense_form, DeletionVectorWireForm::PositionalRoaring);

    let sparse = vector_of(&ordinals[..100]);
    let (sparse_form, sparse_bytes) = encode_deletion_vector(&sparse);
    assert_eq!(sparse_form, DeletionVectorWireForm::OrdinalArray);

    let rows: Vec<u64> = (0..600).collect();
    let via_dense = decode_deletion_vector(&dense_bytes).unwrap().apply(&rows);
    let via_sparse = decode_deletion_vector(&sparse_bytes).unwrap().apply(&rows);
    assert_eq!(via_dense, vector.apply(&rows));
    assert_eq!(via_sparse, sparse.apply(&rows));
}

#[test]
fn forged_deletion_vector_bytes_refuse_instead_of_decoding_wrong() {
    assert!(decode_deletion_vector(&[9]).is_err(), "unknown wire-form tag");
    // Ordinal array claiming descending ordinals.
    let (_, mut bytes) = encode_deletion_vector(&vector_of(&[1, 2]));
    let len = bytes.len();
    bytes.swap(len - 8, len - 4);
    bytes.swap(len - 7, len - 3);
    bytes.swap(len - 6, len - 2);
    bytes.swap(len - 5, len - 1);
    assert!(decode_deletion_vector(&bytes).is_err(), "unsorted ordinals refuse");
    // A forged run amplifying to billions of rows refuses at the decode bound instead of allocating them.
    let mut forged = vec![2u8];
    forged.extend_from_slice(&1u32.to_le_bytes());
    forged.extend_from_slice(&0u64.to_le_bytes());
    forged.extend_from_slice(&(1u64 << 33).to_le_bytes());
    assert!(decode_deletion_vector(&forged).is_err(), "decode bomb refuses");
}

/// The encoder as it stood before the positional form wrote runs directly: the ordinal array, or a
/// `RoaringRangeBitmap` built from the runs, encoded, and appended after the tag.
fn encoding_through_a_built_bitmap(vector: &DeletionVector) -> (DeletionVectorWireForm, Vec<u8>) {
    let ordinals: Vec<u64> = vector.iter().collect();
    if ordinals.len() < DELETION_VECTOR_ORDINAL_ARRAY_MAX as usize
        && ordinals.iter().all(|ordinal| *ordinal <= u64::from(u32::MAX))
    {
        let mut out = vec![1u8];
        out.extend_from_slice(&(ordinals.len() as u32).to_le_bytes());
        for ordinal in ordinals {
            out.extend_from_slice(&(ordinal as u32).to_le_bytes());
        }
        return (DeletionVectorWireForm::OrdinalArray, out);
    }
    let mut out = vec![2u8];
    out.extend_from_slice(&RoaringRangeBitmap::from_ranges(vector.deleted_runs.iter().copied()).encode());
    (DeletionVectorWireForm::PositionalRoaring, out)
}

/// Marks `ordinals` in a scrambled order, each one twice, so the result exercises out-of-order and repeated marks.
fn scrambled_vector_of(ordinals: &[u64]) -> DeletionVector {
    let mut order: Vec<u64> = ordinals.iter().chain(ordinals.iter()).copied().collect();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for i in (1..order.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        order.swap(i, (state >> 33) as usize % (i + 1));
    }
    vector_of(&order)
}

fn assert_encodes_like_a_built_bitmap(name: &str, vector: &DeletionVector, form: DeletionVectorWireForm) {
    let (actual_form, bytes) = encode_deletion_vector(vector);
    let (expected_form, expected_bytes) = encoding_through_a_built_bitmap(vector);
    assert_eq!(actual_form, form, "{name}: form");
    assert_eq!(actual_form, expected_form, "{name}: form matches the reference");
    assert_eq!(bytes, expected_bytes, "{name}: bytes match the reference");
    if form == DeletionVectorWireForm::PositionalRoaring {
        assert_eq!(
            bytes.len(),
            5 + 16 * vector.deleted_runs.len(),
            "{name}: tag, count, and 16 bytes per run"
        );
    }
    let decoded = decode_deletion_vector(&bytes).unwrap();
    assert_eq!(decoded.deleted_runs, vector.deleted_runs, "{name}: round trip");
    assert_eq!(
        decoded.deleted_count(),
        vector.deleted_count(),
        "{name}: round-trip count"
    );
}

#[test]
fn the_positional_form_writes_the_same_bytes_as_a_built_bitmap() {
    let array = DeletionVectorWireForm::OrdinalArray;
    let roaring = DeletionVectorWireForm::PositionalRoaring;
    let scattered = |count: u64| (0..count).map(|i| i * 2).collect::<Vec<_>>();

    assert_encodes_like_a_built_bitmap("empty", &DeletionVector::new(), array);
    assert_encodes_like_a_built_bitmap("sparse", &vector_of(&[3, 17, 4_000, 90_000]), array);
    assert_encodes_like_a_built_bitmap("4095 rows", &vector_of(&scattered(4_095)), array);
    assert_encodes_like_a_built_bitmap("4096 rows", &vector_of(&scattered(4_096)), roaring);
    assert_encodes_like_a_built_bitmap("4097 rows", &vector_of(&scattered(4_097)), roaring);
    assert_encodes_like_a_built_bitmap("4096 contiguous", &vector_of(&(0..4_096).collect::<Vec<_>>()), roaring);

    let mut spans = DeletionVector::new();
    spans.mark_range(RowRange {
        start: 10,
        end: 5_000_000,
    });
    spans.mark_range(RowRange {
        start: 6_000_000,
        end: 9_000_000,
    });
    spans.mark_range(RowRange {
        start: 1 << 40,
        end: (1 << 40) + 100,
    });
    assert_encodes_like_a_built_bitmap("long spans", &spans, roaring);

    let mut disjoint = DeletionVector::new();
    for i in 0..50_000u64 {
        disjoint.mark_range(RowRange {
            start: i * 10,
            end: i * 10 + 1 + i % 7,
        });
    }
    assert_eq!(disjoint.deleted_runs.len(), 50_000);
    assert_encodes_like_a_built_bitmap("many disjoint runs", &disjoint, roaring);

    let beyond_u32 = vector_of(&[5, u64::from(u32::MAX), u64::from(u32::MAX) + 1, u64::MAX - 1]);
    assert_encodes_like_a_built_bitmap("ordinal above u32", &beyond_u32, roaring);
}

#[test]
fn scrambled_and_repeated_marks_encode_to_the_same_bytes_as_sorted_marks() {
    let mut ordinals: Vec<u64> = (0..3_000).map(|i| i * 3).collect();
    ordinals.extend(10_000..12_000);
    ordinals.push(u64::from(u32::MAX) + 7);
    for count in [100, 4_095, 4_096, 4_097, ordinals.len()] {
        let sorted = vector_of(&ordinals[..count]);
        let scrambled = scrambled_vector_of(&ordinals[..count]);
        let name = format!("{count} ordinals");
        assert_eq!(
            encode_deletion_vector(&scrambled),
            encode_deletion_vector(&sorted),
            "{name}"
        );
        let form = encode_deletion_vector(&sorted).0;
        assert_encodes_like_a_built_bitmap(&name, &scrambled, form);
    }
}
