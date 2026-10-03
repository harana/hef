use super::*;
use crate::file::error::CodecError;

#[test]
fn writer_then_reader_round_trips_every_width() {
    let mut writer = Writer::new();
    writer.put_u8(0x12);
    writer.put_u16(0x3456);
    writer.put_u32(0x789a_bcde);
    writer.put_u64(0x0102_0304_0506_0708);
    writer.put_u128(0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100);
    writer.put_slice(b"tail");
    let bytes = writer.into_bytes();

    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.u8("u8").unwrap(), 0x12);
    assert_eq!(reader.u16("u16").unwrap(), 0x3456);
    assert_eq!(reader.u32("u32").unwrap(), 0x789a_bcde);
    assert_eq!(reader.u64("u64").unwrap(), 0x0102_0304_0506_0708);
    assert_eq!(reader.u128("u128").unwrap(), 0x0f0e_0d0c_0b0a_0908_0706_0504_0302_0100);
    assert_eq!(reader.take(4, "tail").unwrap(), b"tail");
    assert_eq!(reader.remaining(), 0);
}

#[test]
fn take_past_end_is_truncated_not_a_panic() {
    let mut reader = Reader::new(&[1, 2, 3]);
    assert_eq!(reader.take(4, "field"), Err(CodecError::Truncated { what: "field" }));
}

#[test]
fn slice_bounds_are_checked() {
    assert_eq!(slice(&[1, 2, 3, 4], 1, 2, "mid").unwrap(), &[2, 3]);
    assert!(slice(&[1, 2], 1, 5, "over").is_err());
    assert!(slice(&[1, 2], usize::MAX, 1, "overflow").is_err());
}

#[test]
fn capacity_hint_never_exceeds_what_the_input_can_hold() {
    let reader = Reader::new(&[0u8; 10]);
    // A forged count of one billion cannot drive a billion-element allocation.
    assert_eq!(reader.capacity_hint(1_000_000_000, 4), 2);
    assert_eq!(reader.capacity_hint(1, 4), 1);
}

#[test]
fn pad_to_rounds_up_to_alignment() {
    let mut writer = Writer::new();
    writer.put_slice(&[1, 2, 3]);
    writer.pad_to(4);
    assert_eq!(writer.len(), 4);
    writer.pad_to(0); // no-op
    assert_eq!(writer.len(), 4);
}

#[test]
fn bulk_u64_matches_one_at_a_time() {
    let values = [0u64, 1, 0x0102_0304_0506_0708, u64::MAX, 42];
    let mut bulk = Writer::new();
    bulk.put_u64_slice(&values);
    let mut one_by_one = Writer::new();
    for value in values {
        one_by_one.put_u64(value);
    }
    assert_eq!(bulk.bytes(), one_by_one.bytes(), "bulk write is the same stream");

    let bytes = bulk.into_bytes();
    let mut reader = Reader::new(&bytes);
    assert_eq!(reader.u64_vec(values.len(), "values").unwrap(), values);
    assert_eq!(reader.remaining(), 0);
}

#[test]
fn bulk_u128_matches_one_at_a_time() {
    let values = [0u128, 1, 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10, u128::MAX, 42];
    let mut bulk = Writer::new();
    bulk.put_u128_slice(&values);
    let mut one_by_one = Writer::new();
    for value in values {
        one_by_one.put_u128(value);
    }
    assert_eq!(bulk.bytes(), one_by_one.bytes(), "bulk write is the same stream");

    let bytes = bulk.into_bytes();
    let mut reader = Reader::new(&bytes);
    for value in values {
        assert_eq!(reader.u128("value").unwrap(), value);
    }
    assert_eq!(reader.remaining(), 0);
}

#[test]
fn clear_empties_the_buffer_without_dropping_its_capacity() {
    let mut writer = Writer::with_capacity(64);
    writer.put_u64(u64::MAX);
    writer.put_slice(b"hello");
    writer.clear();
    assert!(writer.is_empty());
    assert_eq!(writer.len(), 0);
    writer.put_u32(7);
    assert_eq!(writer.into_bytes(), 7u32.to_le_bytes());
}

#[test]
fn bulk_u64_read_past_end_is_truncated_not_a_panic() {
    let mut reader = Reader::new(&[0u8; 12]);
    assert_eq!(
        reader.u64_vec(2, "values"),
        Err(CodecError::Truncated { what: "values" })
    );
    assert_eq!(reader.u64_vec(0, "values").unwrap(), Vec::<u64>::new());
}
