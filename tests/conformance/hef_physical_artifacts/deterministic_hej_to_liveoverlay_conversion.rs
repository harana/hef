//! Checks that turning durable event-log frames into the in-memory table that serves fresh reads is exact and
//! repeatable. A frame that claims a different number of rows than it actually decodes is rejected, a frame whose rows
//! are already covered by a published query file is skipped rather than loaded twice, and two independent replays of
//! the same stored bytes produce
use crate::support;
use arrow_array::UInt64Array;
use hef::artifacts::batch::decode_batch;
use hef::artifacts::frame::decode_frame;
use hef::artifacts::overlay::{convert_frame, select_representation, validate_segment};
use hef::artifacts::segment::replay_segment;

/// conformance: hef-physical-artifacts/deterministic-hej-to-liveoverlay-conversion/row-count-mismatch
#[test]
fn row_count_mismatch() {
    // A decoded segment whose Arrow row count differs from the frame's event_count is rejected.
    let mut world = support::World::new(51);
    world.ingest(3);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let (header, payload) = decode_frame(&replay.frames[0].frame_bytes).unwrap();
    let decoded = decode_batch(payload, header.event_count).unwrap();
    let segment = convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("converted");
    assert!(validate_segment(&segment, &header).is_ok());
    let mut lying_header = header.clone();
    lying_header.event_count = 2;
    assert!(validate_segment(&segment, &lying_header).is_err());
}

/// conformance: hef-physical-artifacts/deterministic-hej-to-liveoverlay-conversion/range-already-hef-covered
#[test]
fn range_already_hef_covered() {
    // A frame whose complete sequence range is covered by the selected manifest-published HEF snapshot is skipped, not
    // decoded into the overlay.
    let mut world = support::World::new(53);
    world.ingest(3);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let (header, payload) = decode_frame(&replay.frames[0].frame_bytes).unwrap();
    let decoded = decode_batch(payload, header.event_count).unwrap();
    let skipped = convert_frame(&header, &decoded, 1, 1, None, |_, _| true).unwrap();
    assert!(skipped.is_none());
}

/// conformance:
/// hef-physical-artifacts/deterministic-hej-to-liveoverlay-conversion/
/// compressed-array-segment-yields-identical-query-results
#[test]
fn compressed_array_segment_yields_identical_query_results() {
    // Decoding the same HEJ frame twice produces two Arrow RecordBatch segments (the only representation currently
    // available). An identical query — reading the sequence column — returns the same rows, values, and order from
    // each, proving the logical-equivalence property that any future Vortex compressed-array representation would also
    // have to satisfy.
    let mut world = support::World::new(57);
    world.ingest(5);
    let replay = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let (header, payload) = decode_frame(&replay.frames[0].frame_bytes).unwrap();
    let decoded = decode_batch(payload, header.event_count).unwrap();

    let seg_a = convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("converted A");
    let seg_b = convert_frame(&header, &decoded, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("converted B");

    let read_sequences = |seg: &hef::artifacts::overlay::LiveOverlaySegment| {
        seg.batch
            .column_by_name("sequence")
            .unwrap()
            .as_any()
            .downcast_ref::<UInt64Array>()
            .unwrap()
            .values()
            .to_vec()
    };

    let sequences_a = read_sequences(&seg_a);
    let sequences_b = read_sequences(&seg_b);

    assert_eq!(sequences_a, sequences_b);
    assert_eq!(sequences_a.len(), 5);
    // Rows are in HEJ sequence order, ascending.
    assert!(sequences_a.windows(2).all(|w| w[0] < w[1]));
}

/// conformance:
/// hef-physical-artifacts/deterministic-hej-to-liveoverlay-conversion/representation-is-deterministic-across-nodes
#[test]
fn representation_is_deterministic_across_nodes() {
    // Two nodes independently replay the same stored HEJ bytes. Each calls select_representation for the frame's range,
    // gets the same result, and convert_frame produces byte-identical segment metadata and column values.
    let mut world = support::World::new(59);
    world.ingest(4);

    // Node A.
    let replay_a = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let (header_a, payload_a) = decode_frame(&replay_a.frames[0].frame_bytes).unwrap();
    let rep_a = select_representation(header_a.epoch, header_a.first_sequence);
    let decoded_a = decode_batch(payload_a, header_a.event_count).unwrap();
    let seg_a = convert_frame(&header_a, &decoded_a, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("node A converted");

    // Node B — independent replay of the identical stored bytes.
    let replay_b = replay_segment(&world.storage, support::SHARD, 1, 1).unwrap();
    let (header_b, payload_b) = decode_frame(&replay_b.frames[0].frame_bytes).unwrap();
    let rep_b = select_representation(header_b.epoch, header_b.first_sequence);
    let decoded_b = decode_batch(payload_b, header_b.event_count).unwrap();
    let seg_b = convert_frame(&header_b, &decoded_b, 1, 1, None, |_, _| false)
        .unwrap()
        .expect("node B converted");

    // Both nodes selected the same representation.
    assert_eq!(rep_a, rep_b);

    // Segment metadata is byte-identical (same BLAKE3 hashes, same range).
    assert_eq!(seg_a.meta, seg_b.meta);

    // All columns carry identical values in the same row order.
    assert_eq!(seg_a.batch.num_rows(), seg_b.batch.num_rows());
    assert_eq!(seg_a.batch.num_columns(), seg_b.batch.num_columns());
    for col_idx in 0..seg_a.batch.num_columns() {
        assert_eq!(
            seg_a.batch.column(col_idx).to_data(),
            seg_b.batch.column(col_idx).to_data(),
            "column {col_idx} differs between nodes"
        );
    }
}
