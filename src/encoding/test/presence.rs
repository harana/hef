use super::*;

/// The per-bit form the word-at-a-time count must reproduce exactly.
fn count_bits_one_at_a_time(bitmap: &[u8], len: usize) -> usize {
    (0..len)
        .filter(|bit| bitmap.get(bit / 8).is_some_and(|byte| byte & (1 << (bit % 8)) != 0))
        .count()
}

fn every_third_row(rows: usize) -> Vec<u8> {
    let mut bitmap = vec![0u8; rows.div_ceil(8)];
    for row in (0..rows).step_by(3) {
        bitmap[row / 8] |= 1 << (row % 8);
    }
    bitmap
}

/// Whole words, leftover bytes, and a partial tail byte all have to agree with the per-bit count, so every length
/// across several word boundaries is checked against it.
#[test]
fn counting_by_word_matches_counting_bit_by_bit() {
    let rows = 300;
    let bitmap = every_third_row(rows);
    for len in 0..=rows {
        assert_eq!(
            count_set_bits(&bitmap, len),
            count_bits_one_at_a_time(&bitmap, len),
            "len {len}"
        );
    }
}

/// Bits past `len` inside the tail byte are not part of the count, whatever the byte holds.
#[test]
fn bits_past_the_length_never_count() {
    let bitmap = vec![0xFFu8; 3];
    for len in 0..=24 {
        assert_eq!(count_set_bits(&bitmap, len), len, "len {len}");
    }
}

/// A bitmap shorter than the length asked for treats its missing bits as unset rather than reading past its end.
#[test]
fn a_short_bitmap_counts_its_missing_bits_as_unset() {
    let bitmap = vec![0xFFu8; 2];
    assert_eq!(count_set_bits(&bitmap, 1000), 16);
    assert_eq!(count_set_bits(&[], 64), 0);
    assert_eq!(count_set_bits(&bitmap, 0), 0);
}

/// `count_present_before` is the same count taken over the prefix, and stops at the end of the bitmap.
#[test]
fn present_before_counts_the_prefix() {
    let rows = 200;
    let bitmap = every_third_row(rows);
    for row in 0..rows {
        assert_eq!(
            count_present_before(&bitmap, row),
            count_bits_one_at_a_time(&bitmap, row),
            "row {row}"
        );
    }
    let all = count_bits_one_at_a_time(&bitmap, bitmap.len() * 8);
    assert_eq!(count_present_before(&bitmap, rows * 2), all, "row past the bitmap");
}
