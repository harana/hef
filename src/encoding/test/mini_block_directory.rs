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

/// Every lookup must agree with ranges recomputed from the raw length list by a running prefix sum, including
/// zero-length entries, on a metadata-only buffer with no compressed bodies appended.
#[test]
fn lookups_match_a_prefix_sum_over_the_raw_lengths() {
    let cases: [&[usize]; 5] = [&[], &[9], &[0, 0, 0], &[4, 0, 17, 0, 1, 250], &[u32::MAX as usize, 3]];
    for block_lens in cases {
        let bytes = encode(123, block_lens);
        let directory = decode(&bytes).unwrap();
        let mut offset = bytes.len();
        let expected: Vec<(usize, usize)> = block_lens
            .iter()
            .map(|&len| {
                let range = (offset, len);
                offset += len;
                range
            })
            .collect();

        assert_eq!(directory.body_start(), bytes.len());
        assert_eq!(directory.block_count(), block_lens.len());
        assert_eq!(directory.plain_len(), 123);
        assert_eq!(directory.ranges().collect::<Vec<_>>(), expected);
        for (index, range) in expected.iter().enumerate() {
            assert_eq!(directory.byte_range(index).unwrap(), *range);
        }
        assert!(directory.byte_range(block_lens.len()).is_err());
        for first in 0..=block_lens.len() + 1 {
            let suffix: Vec<(usize, usize)> = directory.ranges_from(first).collect();
            assert_eq!(suffix, expected.get(first..).unwrap_or(&[]));
        }
    }
}

#[test]
fn a_truncated_length_table_is_rejected() {
    let bytes = encode(64, &[5, 6, 7]);
    for cut in 0..bytes.len() {
        assert!(decode(&bytes[..cut]).is_err(), "cut at {cut}");
    }
}
