use super::super::batch::tests::{sample_events, sample_frame};
use super::super::batch::{EventInput, PayloadInput, build_batch, decode_batch};
use super::super::frame::{FrameBuildInput, build_frame, decode_frame};
use super::super::segment::ReplayTail;
use super::*;
use crate::lifecycle::{FileType, HefFileEntry, ManifestGeneration, PartState};
use crate::typed_id::TypedIdTestExt;

fn payload_refs_of(events: &[EventInput]) -> Vec<u64> {
    let payload = build_batch(events, 1, 0).unwrap();
    let frame_bytes = build_frame(
        &FrameBuildInput {
            flags: 0,
            tenant_id: TenantId::new_test_id(7),
            writer_id: 3,
            epoch: 1,
            first_sequence: 1,
            last_sequence: events.len() as u64,
            event_count: events.len() as u32,
            durable_batch_id: 1,
            writer_local_batch_id: 1,
            schema_generation: 1,
            dictionary_generation_hint: 0,
            created_at_physical: 1,
            committed_at_physical: 2,
        },
        &payload,
    )
    .unwrap();
    let (header, payload) = decode_frame(&frame_bytes).unwrap();
    let decoded = decode_batch(payload, header.event_count).unwrap();
    let segment = convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("not covered");
    segment
        .batch
        .column(21)
        .as_any()
        .downcast_ref::<arrow_array::UInt64Array>()
        .unwrap()
        .values()
        .to_vec()
}

/// The conversion contract's schemas are built once and shared by every frame that converts, instead of being rebuilt
/// (fields and Arrow's field-name lookup map alike) per batch.
#[test]
fn the_conversion_schemas_are_built_once_and_shared_by_every_frame() {
    assert!(Arc::ptr_eq(&live_overlay_schema(), &live_overlay_schema()));
    assert!(Arc::ptr_eq(
        &live_overlay_schema_with_provenance(),
        &live_overlay_schema_with_provenance()
    ));
    assert_ne!(
        live_overlay_schema(),
        live_overlay_schema_with_provenance(),
        "the provenance schema stays a distinct contract"
    );
    // A converted frame carries the shared schema itself, not a fresh copy of the same fields.
    let segment = overlay_segment(1, 1, 2);
    assert!(Arc::ptr_eq(&segment.batch.schema(), &live_overlay_schema()));
}

#[test]
fn payload_ref_distinguishes_no_payload_from_a_payload_at_arena_offset_zero() {
    // Two logically different frames: (row 0 payload-less, row 1 payload at arena offset 0) versus (row 0 payload at
    // arena offset 0, row 1 payload-less). Before payload_ref carried the length, both produced identical payload_ref
    // columns, so a consumer would serve another row's payload for a payload-less row.
    let mut none_then_payload = sample_events(2);
    none_then_payload[0].payload = PayloadInput::None;
    let mut payload_then_none = sample_events(2);
    payload_then_none[1].payload = PayloadInput::None;

    let first = payload_refs_of(&none_then_payload);
    let second = payload_refs_of(&payload_then_none);
    assert_ne!(first, second, "the two frames must stay distinguishable");

    // payload_ref = (payload_len << 32) | payload_offset: a payload-less row is exactly 0; a real payload at arena
    // offset 0 carries its non-zero length in the high half.
    assert_eq!(first[0], 0);
    assert!(first[1] >> 32 > 0);
    assert_eq!(first[1] & 0xFFFF_FFFF, 0);
    assert!(second[0] >> 32 > 0);
    assert_eq!(second[0] & 0xFFFF_FFFF, 0);
    assert_eq!(second[1], 0);
}

fn overlay_segment(epoch: u64, first_sequence: u64, count: usize) -> LiveOverlaySegment {
    let frame_bytes = sample_frame(count, epoch, first_sequence);
    let (header, payload) = decode_frame(&frame_bytes).unwrap();
    let decoded = decode_batch(payload, header.event_count).unwrap();
    convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("an event frame converts to a segment")
}

fn replayed_frame(frame_bytes: Vec<u8>, offset: u64) -> ReplayedFrame {
    let (header, _) = decode_frame(&frame_bytes).unwrap();
    ReplayedFrame {
        frame_bytes,
        header,
        offset,
    }
}

fn covering_manifest(segment: &LiveOverlaySegment) -> ManifestGeneration {
    ManifestGeneration {
        files: vec![HefFileEntry {
            coverage: segment.range(),
            feature_metadata: None,
            file_seal: [0u8; 32],
            file_id: 0,
            file_type: FileType::HefFile,
            footer_len: None,
            optional_feature_flags: 0,
            part_index: 0,
            part_state: PartState::Active,
            required_feature_flags: 0,
            size_bytes: 0,
            tenant_id: segment.meta.tenant_id,
            tree_len: None,
        }],
        footer_mirror: None,
        generation: 1,
        index_artifacts: Vec::new(),
    }
}

fn seq_range(epoch: u64, first_sequence: u64, last_sequence: u64) -> SequenceRange {
    SequenceRange {
        epoch,
        first_sequence,
        last_sequence,
    }
}

#[test]
fn fresh_read_serves_a_covered_range_and_reports_the_gap_otherwise() {
    let mut store = LiveOverlayStore::new();
    store.publish(overlay_segment(1, 1, 4)); // covers 1..=4
    store.publish(overlay_segment(1, 5, 4)); // covers 5..=8

    // A range spanned by contiguous segments is served in full.
    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 1, 8)) {
        FreshRead::Ready(hits) => assert_eq!(hits.len(), 2),
        FreshRead::Behind { missing } => panic!("expected Ready, got Behind {missing:?}"),
    }

    // A range extending past the last segment reports exactly the uncovered tail.
    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 1, 12)) {
        FreshRead::Behind { missing } => assert_eq!(missing, seq_range(1, 9, 12)),
        FreshRead::Ready(_) => panic!("expected Behind past the covered tail"),
    }

    // Another epoch is entirely uncovered.
    match store.fresh_read(TenantId::new_test_id(7), seq_range(2, 1, 3)) {
        FreshRead::Behind { missing } => assert_eq!(missing, seq_range(2, 1, 3)),
        FreshRead::Ready(_) => panic!("a different epoch must be Behind"),
    }
}

#[test]
fn rebuild_from_replay_publishes_event_frames_and_skips_covered_ranges() {
    let frame_a = sample_frame(3, 1, 1); // covers 1..=3
    let frame_b = sample_frame(3, 1, 4); // covers 4..=6
    let replay = ReplayOutcome {
        frames: vec![
            replayed_frame(frame_a.clone(), 0),
            replayed_frame(frame_b, frame_a.len() as u64),
        ],
        segment_chain_blake3: None,
        tail: ReplayTail::Clean,
    };

    let mut store = LiveOverlayStore::new();
    store.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert_eq!(store.segment_count(), 2, "both event frames rebuild into segments");

    // When the manifest already covers frame B's range, only frame A is rebuilt.
    let mut covered = LiveOverlayStore::new();
    covered
        .rebuild_from_replay(&replay, 1, 1, |_, range| range.first_sequence == 4)
        .unwrap();
    assert_eq!(covered.segment_count(), 1, "an HEF-covered range is skipped");
}

fn void_frame(epoch: u64, first_sequence: u64, last_sequence: u64) -> Vec<u8> {
    build_frame(
        &FrameBuildInput {
            flags: super::super::frame::FLAG_VOID_RECORD,
            tenant_id: TenantId::new_test_id(7),
            writer_id: 3,
            epoch,
            first_sequence,
            last_sequence,
            event_count: 0,
            durable_batch_id: 901,
            writer_local_batch_id: 2,
            schema_generation: 1,
            dictionary_generation_hint: 0,
            created_at_physical: 1,
            committed_at_physical: 2,
        },
        &[],
    )
    .unwrap()
}

#[test]
fn a_fresh_read_crosses_a_void_range_to_reach_the_events_behind_it() {
    // A void record closes an abandoned reservation: its sequences are durable and permanently rowless, so a request
    // spanning event–void–event is fully served. Before this, the void read as a gap and the acknowledged events after
    // it were unreachable through this API (issue #7490).
    let events_before = sample_frame(3, 1, 1); // covers 1..=3
    let void = void_frame(1, 4, 5); // covers 4..=5, no rows
    let events_after = sample_frame(3, 1, 6); // covers 6..=8
    let replay = ReplayOutcome {
        frames: vec![
            replayed_frame(events_before.clone(), 0),
            replayed_frame(void.clone(), events_before.len() as u64),
            replayed_frame(events_after, (events_before.len() + void.len()) as u64),
        ],
        segment_chain_blake3: None,
        tail: ReplayTail::Clean,
    };

    let mut store = LiveOverlayStore::new();
    store.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert_eq!(store.segment_count(), 2, "a void record produces no segment");

    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 1, 8)) {
        FreshRead::Ready(hits) => assert_eq!(hits.len(), 2, "both event segments serve the read"),
        FreshRead::Behind { missing } => panic!("expected Ready across the void, got Behind {missing:?}"),
    }

    // A read of the void range alone is served too — with no rows, because there are none to serve.
    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 4, 5)) {
        FreshRead::Ready(hits) => assert!(hits.is_empty()),
        FreshRead::Behind { missing } => panic!("expected Ready over the void, got Behind {missing:?}"),
    }

    // A genuine gap past the last frame is still reported.
    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 1, 12)) {
        FreshRead::Behind { missing } => assert_eq!(missing, seq_range(1, 9, 12)),
        FreshRead::Ready(_) => panic!("expected Behind past the covered tail"),
    }
}

#[test]
fn evict_covered_drops_only_published_ranges() {
    let mut store = LiveOverlayStore::new();
    let segment = overlay_segment(1, 1, 4);
    store.publish(segment.clone());
    store.publish(overlay_segment(1, 5, 4));

    // An empty manifest covers nothing: premature eviction is prevented.
    let evicted = store.evict_covered(&ManifestGeneration::default());
    assert_eq!(evicted, 0);
    assert_eq!(store.segment_count(), 2);

    // A manifest that covers the first segment's range evicts exactly that segment.
    let evicted = store.evict_covered(&covering_manifest(&segment));
    assert_eq!(evicted, 1);
    assert_eq!(store.segment_count(), 1);
}

#[test]
fn two_tenants_using_the_same_epoch_and_sequences_keep_their_own_segments() {
    // Sequence identity includes the tenant, but segments and voids were keyed by epoch and first sequence alone, so
    // one tenant's segment displaced another's and a fresh read could be answered from the wrong tenant's rows
    // (issue #8922).
    let mut store = LiveOverlayStore::new();
    let mine = overlay_segment(1, 1, 4);
    let tenant = mine.meta.tenant_id;
    let other_tenant = TenantId::new_test_id(8);
    let mut theirs = overlay_segment(1, 1, 4);
    theirs.meta.tenant_id = other_tenant;

    store.publish(mine);
    store.publish(theirs);
    assert_eq!(
        store.segment_count(),
        2,
        "one tenant's segment must not displace another's"
    );

    match store.fresh_read(tenant, seq_range(1, 1, 4)) {
        FreshRead::Ready(hits) => {
            assert_eq!(hits.len(), 1);
            assert_eq!(
                hits[0].meta.tenant_id, tenant,
                "a read is served from its own tenant's rows"
            );
        }
        FreshRead::Behind { missing } => panic!("expected Ready, got Behind {missing:?}"),
    }

    // A tenant with no segment at all is Behind, even where another tenant covers the same sequences.
    match store.fresh_read(TenantId::new_test_id(99), seq_range(1, 1, 4)) {
        FreshRead::Behind { missing } => assert_eq!(missing, seq_range(1, 1, 4)),
        FreshRead::Ready(_) => panic!("another tenant's segment must not satisfy this read"),
    }

    // The same holds for voids: another tenant's void never covers this tenant's sequences.
    let mut voids = LiveOverlayStore::new();
    voids.publish_void(other_tenant, seq_range(1, 1, 4));
    match voids.fresh_read(tenant, seq_range(1, 1, 4)) {
        FreshRead::Behind { missing } => assert_eq!(missing, seq_range(1, 1, 4)),
        FreshRead::Ready(_) => panic!("another tenant's void must not satisfy this read"),
    }
}

fn tenant_frame(tenant_id: TenantId, count: usize, epoch: u64, first_sequence: u64) -> Vec<u8> {
    let events = sample_events(count);
    let payload = build_batch(&events, 1, 0).unwrap();
    build_frame(
        &FrameBuildInput {
            flags: 0,
            tenant_id,
            writer_id: 3,
            epoch,
            first_sequence,
            last_sequence: first_sequence + count as u64 - 1,
            event_count: count as u32,
            durable_batch_id: 900,
            writer_local_batch_id: 1,
            schema_generation: 1,
            dictionary_generation_hint: 0,
            created_at_physical: 1,
            committed_at_physical: 2,
        },
        &payload,
    )
    .unwrap()
}

#[test]
fn published_coverage_is_asked_about_each_frames_own_tenant() {
    // Regression: `hef_covers` took only a range, so a caller had to hard-code one tenant. A replay holding two
    // tenants' identically numbered ranges then let one tenant's publication suppress the other tenant's
    // still-unpublished frame (issue #9611). The closure is asked about each frame's own tenant instead.
    let published = TenantId::new_test_id(7);
    let unpublished = TenantId::new_test_id(8);
    let frame_a = tenant_frame(published, 3, 1, 1);
    let frame_b = tenant_frame(unpublished, 3, 1, 1);
    let replay = ReplayOutcome {
        frames: vec![
            replayed_frame(frame_a.clone(), 0),
            replayed_frame(frame_b, frame_a.len() as u64),
        ],
        segment_chain_blake3: None,
        tail: ReplayTail::Clean,
    };

    let mut store = LiveOverlayStore::new();
    store
        .rebuild_from_replay(&replay, 1, 1, |tenant_id, _| tenant_id == published)
        .unwrap();
    assert_eq!(
        store.segment_count(),
        1,
        "only the tenant whose range is published is skipped",
    );
    assert!(matches!(
        store.fresh_read(unpublished, seq_range(1, 1, 3)),
        FreshRead::Ready(_),
    ));
    assert!(matches!(
        store.fresh_read(published, seq_range(1, 1, 3)),
        FreshRead::Behind { .. },
    ));
}

#[test]
fn a_rebuilt_overlay_rejects_an_event_frame_a_void_already_covered() {
    // Regression: the rebuild inserted every replayed frame, and a fresh read preferred an event segment over a
    // covering void. A stalled worker's frame landing after the void that closed its range was therefore served live
    // even though publication's first-durable-wins arbitration drops it (issue #9612). The rebuild now arbitrates the
    // same way, so the overlay and the published history agree.
    let void = void_frame(1, 1, 3);
    let late_event = sample_frame(3, 1, 1); // the stalled worker's frame over the same range
    let replay = ReplayOutcome {
        frames: vec![
            replayed_frame(void.clone(), 0),
            replayed_frame(late_event, void.len() as u64),
        ],
        segment_chain_blake3: None,
        tail: ReplayTail::Clean,
    };

    let mut store = LiveOverlayStore::new();
    store.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert_eq!(
        store.segment_count(),
        0,
        "the void won the range, so no segment is built"
    );
    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 1, 3)) {
        FreshRead::Ready(hits) => assert!(hits.is_empty(), "the voided range serves no rows"),
        FreshRead::Behind { missing } => panic!("expected Ready over the void, got Behind {missing:?}"),
    }
}

#[test]
fn a_rebuilt_overlay_rejects_a_void_that_overlaps_an_earlier_event_frame() {
    // The mirror case: the event frame reached durability first, so it wins and a later overlapping void is the
    // rejected duplicate. The overlay must keep serving the event's rows.
    let event = sample_frame(3, 1, 1);
    let late_void = void_frame(1, 1, 3);
    let replay = ReplayOutcome {
        frames: vec![
            replayed_frame(event.clone(), 0),
            replayed_frame(late_void, event.len() as u64),
        ],
        segment_chain_blake3: None,
        tail: ReplayTail::Clean,
    };

    let mut store = LiveOverlayStore::new();
    store.rebuild_from_replay(&replay, 1, 1, |_, _| false).unwrap();
    assert_eq!(store.segment_count(), 1);
    match store.fresh_read(TenantId::new_test_id(7), seq_range(1, 1, 3)) {
        FreshRead::Ready(hits) => assert_eq!(hits.len(), 1, "the first-durable event frame still serves its rows"),
        FreshRead::Behind { missing } => panic!("expected Ready, got Behind {missing:?}"),
    }
}
