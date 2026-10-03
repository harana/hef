use super::*;

#[test]
fn round_trip_recovers_block_count_plain_len_and_byte_ranges() {
    let block_lens = [10usize, 3, 0, 7];
    let plain_len = 4096 * 3 + 123;
    let mut bytes = encode(plain_len, &block_lens);
    let body_start = bytes.len();
    for len in block_lens {
        bytes.extend(std::iter::repeat_n(0xABu8, len));
    }

    let directory = decode(&bytes).unwrap();
    assert_eq!(directory.block_count(), block_lens.len());
    assert_eq!(directory.plain_len(), plain_len);
    assert_eq!(directory.body_start(), body_start);

    let mut offset = body_start;
    for (index, len) in block_lens.into_iter().enumerate() {
        assert_eq!(directory.byte_range(index).unwrap(), (offset, len));
        offset += len;
    }
}

#[test]
fn ranges_matches_byte_range_called_per_index() {
    let block_lens = [5usize, 0, 12, 1];
    let bytes = encode(64, &block_lens);
    let directory = decode(&bytes).unwrap();

    let via_ranges: Vec<(usize, usize)> = directory.ranges().collect();
    let via_byte_range: Vec<(usize, usize)> = (0..directory.block_count())
        .map(|index| directory.byte_range(index).unwrap())
        .collect();
    assert_eq!(via_ranges, via_byte_range);
}

#[test]
fn an_out_of_range_block_index_is_rejected() {
    let bytes = encode(0, &[]);
    let directory = decode(&bytes).unwrap();
    assert!(directory.byte_range(0).is_err());
}

#[test]
fn a_forged_plain_length_does_not_force_an_unbounded_allocation() {
    let mut forged = Writer::with_capacity(8);
    forged.put_u32(0);
    forged.put_u32(u32::MAX);
    let bytes = forged.into_bytes();

    let directory = decode(&bytes).unwrap();

    assert_eq!(directory.block_count(), 0);
    assert!(directory.plain_capacity_hint() <= bytes.len());
}
