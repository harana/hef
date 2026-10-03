use super::*;
use crate::events::TenantId;
use crate::typed_id::TypedIdTestExt;

/// A header for a file spanning epochs `min_epoch..=max_epoch`, starting at `min_sequence` in its first epoch and ending
/// at `max_sequence` in its last. When the file spans epochs the two sequence bounds live in different epochs, so the
/// flat `[min_sequence, max_sequence]` interval is inverted and meaningless on its own.
fn spanning_header(min_epoch: u64, max_epoch: u64, min_sequence: u64, max_sequence: u64) -> HefHeader {
    HefHeader {
        created_at_physical: 0,
        feature_flags: 0,
        file_id: 0,
        footer_pointer_hint: 0,
        generation_id: 0,
        layout_class: LayoutClass::Compact,
        max_epoch,
        max_ingested_at_physical: 0,
        max_occurred_at_physical: 0,
        max_sequence,
        min_epoch,
        min_ingested_at_physical: 0,
        min_occurred_at_physical: 0,
        min_sequence,
        projection_count: 1,
        row_count: 1,
        tenant_id: TenantId::new_test_id(1),
        version_major: 1,
        version_minor: 0,
    }
}

#[test]
fn may_contain_sequence_is_epoch_aware_for_a_file_spanning_epochs() {
    // The file starts at sequence 100 in epoch 3 and ends at sequence 50 in epoch 5 (sequences reset per epoch).
    let header = spanning_header(3, 5, 100, 50);

    // An epoch strictly between the boundaries is covered in full: any sequence may be present. The flat interval
    // `[100, 50]` is inverted here, so the old whole-file test wrongly rejected these.
    assert!(header.may_contain_sequence(4, 1, 1));
    assert!(header.may_contain_sequence(4, 100, 100));
    assert!(header.may_contain_sequence(4, 999, 1000));

    // First epoch: coverage runs from min_sequence upward. A query above the start may match; one entirely below it
    // cannot.
    assert!(header.may_contain_sequence(3, 200, 300));
    assert!(header.may_contain_sequence(3, 90, 100));
    assert!(
        !header.may_contain_sequence(3, 1, 50),
        "below the file's first-epoch start is rejected"
    );

    // Last epoch: coverage runs up to max_sequence. A query below it may match; one entirely above it cannot.
    assert!(header.may_contain_sequence(5, 10, 20));
    assert!(header.may_contain_sequence(5, 50, 80));
    assert!(
        !header.may_contain_sequence(5, 60, 70),
        "above the file's last-epoch end is rejected"
    );

    // Epochs outside the file's span are always rejected.
    assert!(!header.may_contain_sequence(2, 1, u64::MAX));
    assert!(!header.may_contain_sequence(6, 1, u64::MAX));
}

#[test]
fn may_contain_sequence_within_a_single_epoch_is_the_flat_interval() {
    // A file confined to one epoch: the boundary epoch is both first and last, so both bounds apply.
    let header = spanning_header(7, 7, 100, 200);
    assert!(header.may_contain_sequence(7, 150, 160));
    assert!(header.may_contain_sequence(7, 50, 100));
    assert!(header.may_contain_sequence(7, 200, 250));
    assert!(!header.may_contain_sequence(7, 1, 99), "below the interval is rejected");
    assert!(
        !header.may_contain_sequence(7, 201, 300),
        "above the interval is rejected"
    );
    assert!(!header.may_contain_sequence(8, 150, 160), "wrong epoch is rejected");
}

/// Every presence form round-trips through `encode_presence`/`decode_presence` to exactly the bitmap it replaced, and
/// the zero-byte forms truly store zero payload bytes. Implements `hef-encodings-and-compression` — "Presence and
/// null bitmaps are encoded side streams".
#[test]
fn presence_side_stream_forms_round_trip_and_zero_byte_forms_store_nothing() {
    use crate::file::bytes::{Reader, Writer};

    let round_trip = |presence: &[u8], rows: u32| {
        let mut out = Writer::new();
        super::encode_presence(presence, rows, &mut out);
        let bytes = out.into_bytes();
        let decoded = super::decode_presence(&mut Reader::new(&bytes), rows)
            .unwrap()
            .into_owned();
        (bytes, decoded)
    };

    // A plain dense column stays the empty in-memory convention at one stored byte.
    let (bytes, decoded) = round_trip(&[], 100);
    assert_eq!(bytes, vec![super::presence_forms::EMPTY]);
    assert!(decoded.is_empty());

    // Fully present: zero payload bytes, rebuilt as the all-set bitmap over the row count.
    let all_set = vec![0xFF, 0x03];
    let (bytes, decoded) = round_trip(&all_set, 10);
    assert_eq!(bytes, vec![super::presence_forms::ALL_PRESENT]);
    assert_eq!(decoded, all_set);

    // Fully absent: zero payload bytes, rebuilt as the all-zero bitmap.
    let (bytes, decoded) = round_trip(&[0u8; 2], 10);
    assert_eq!(bytes, vec![super::presence_forms::ALL_ABSENT]);
    assert_eq!(decoded, vec![0u8; 2]);

    // One clustered run across a big block: the run form wins and rebuilds the exact bitmap.
    let rows = 4096u32;
    let mut clustered = vec![0u8; 512];
    for row in 100..140 {
        clustered[row / 8] |= 1 << (row % 8);
    }
    let (bytes, decoded) = round_trip(&clustered, rows);
    assert_eq!(bytes[0], super::presence_forms::SET_RUNS);
    assert_eq!(bytes.len(), 1 + 4 + 8, "one run costs one (start, len) pair");
    assert_eq!(decoded, clustered);

    // Scattered single rows: the position form wins.
    let mut scattered = vec![0u8; 512];
    for row in [3usize, 900, 2001, 3777] {
        scattered[row / 8] |= 1 << (row % 8);
    }
    let (bytes, decoded) = round_trip(&scattered, rows);
    assert_eq!(bytes[0], super::presence_forms::SET_POSITIONS);
    assert_eq!(decoded, scattered);

    // Dense noise keeps the raw fallback, so no form ever costs more than the bitmap.
    let noisy: Vec<u8> = (0..512).map(|i| (i as u8).wrapping_mul(37) | 1).collect();
    let (bytes, decoded) = round_trip(&noisy, rows);
    assert_eq!(bytes[0], super::presence_forms::RAW);
    assert_eq!(decoded, noisy);

    // A forged form tag refuses instead of misframing the block.
    assert!(super::decode_presence(&mut Reader::new(&[9u8]), 10).is_err());
}

/// A block that already stores its presence as a bitmap hands those bytes back borrowed, so a raw read that only
/// reads the bitmap — the point-probe path — copies nothing. The forms the decoder has to rebuild stay owned.
#[test]
fn presence_stored_as_a_bitmap_is_borrowed_from_the_block_bytes() {
    use crate::file::bytes::{Reader, Writer};
    use std::borrow::Cow;

    // Dense noise: the raw form is the one that stores smallest, so the bitmap is in the block verbatim.
    let noisy: Vec<u8> = (0..512).map(|i| (i as u8).wrapping_mul(37) | 1).collect();
    let mut out = Writer::new();
    super::encode_presence(&noisy, 4096, &mut out);
    let bytes = out.into_bytes();
    assert_eq!(bytes[0], super::presence_forms::RAW);
    let decoded = super::decode_presence_frame(&mut Reader::new(&bytes), 4096, true).unwrap();
    assert!(matches!(decoded, Cow::Borrowed(_)), "the raw form must not be copied");
    assert_eq!(decoded, noisy);

    // The legacy frame is a bitmap too, and is borrowed the same way.
    let mut legacy = Writer::new();
    legacy.put_u32(noisy.len() as u32);
    legacy.put_slice(&noisy);
    let legacy_bytes = legacy.into_bytes();
    let decoded = super::decode_presence_frame(&mut Reader::new(&legacy_bytes), 4096, false).unwrap();
    assert!(
        matches!(decoded, Cow::Borrowed(_)),
        "the legacy frame must not be copied"
    );
    assert_eq!(decoded, noisy);

    // A form the decoder has to rebuild has nothing to borrow and stays owned.
    let mut runs = Writer::new();
    let mut clustered = vec![0u8; 512];
    for row in 100..140 {
        clustered[row / 8] |= 1 << (row % 8);
    }
    super::encode_presence(&clustered, 4096, &mut runs);
    let runs_bytes = runs.into_bytes();
    assert_eq!(runs_bytes[0], super::presence_forms::SET_RUNS);
    let decoded = super::decode_presence_frame(&mut Reader::new(&runs_bytes), 4096, true).unwrap();
    assert!(matches!(decoded, Cow::Owned(_)));
    assert_eq!(decoded, clustered);
}

/// A file written before the `compressed_presence` feature frames presence as `u32 length | bitmap`; the framing
/// decoder picks the right parse from the file's declared features, so old blocks are not misread as form tags.
#[test]
fn presence_frame_decoding_follows_the_declared_feature() {
    use crate::file::bytes::{Reader, Writer};

    // The legacy frame for a two-byte bitmap: little-endian length 2, then the bitmap bytes.
    let bitmap = vec![0b0000_0101u8, 0b0000_0010];
    let mut legacy = Writer::new();
    legacy.put_u32(bitmap.len() as u32);
    legacy.put_slice(&bitmap);
    let legacy_bytes = legacy.into_bytes();

    let decoded = super::decode_presence_frame(&mut Reader::new(&legacy_bytes), 10, false).unwrap();
    assert_eq!(decoded, bitmap, "a legacy frame must decode to its exact bitmap");

    // The same bytes read under the compressed framing would misparse (length byte 2 = ALL_PRESENT), which is
    // exactly why the feature bit, not the bytes, picks the parse.
    let misread = super::decode_presence_frame(&mut Reader::new(&legacy_bytes), 10, true).unwrap();
    assert_ne!(misread, bitmap);

    // A compressed frame still routes through the tagged decoder.
    let mut compressed = Writer::new();
    super::encode_presence(&bitmap, 10, &mut compressed);
    let compressed_bytes = compressed.into_bytes();
    let decoded = super::decode_presence_frame(&mut Reader::new(&compressed_bytes), 10, true).unwrap();
    assert_eq!(decoded, bitmap);
}
