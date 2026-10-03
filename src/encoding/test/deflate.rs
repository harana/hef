use super::*;
use crate::file::bytes::Writer;

/// Bytes with a short repeating pattern span several granules and compress well, without being trivially empty.
fn multi_granule_bytes() -> Vec<u8> {
    let unit: [u8; 7] = [0x11, 0x22, 0x9E, 0x00, 0xFF, 0x7A, 0x5C];
    (0..GRANULE_BYTES * 3 + 123).map(|i| unit[i % unit.len()]).collect()
}

#[test]
fn round_trip_recovers_the_original_bytes() {
    for raw in [Vec::new(), b"tiny".to_vec(), multi_granule_bytes()] {
        let packed = compress(&raw);
        assert_eq!(decompress(&packed).unwrap(), raw);
        assert_eq!(plain_len(&packed).unwrap(), raw.len());
    }
}

#[test]
fn every_granule_stays_within_the_history_window() {
    let raw = multi_granule_bytes();
    let packed = compress(&raw);
    let granules = raw.len().div_ceil(GRANULE_BYTES);
    for granule in 0..granules {
        let (_, len) = granule_byte_range(&packed, granule).unwrap();
        // A granule's compressed bytes can never exceed its plaintext window, since raw miniz_oxide deflate never
        // expands a same-size-or-smaller input by more than a handful of stored-block bytes.
        assert!(len <= GRANULE_BYTES + 16);
    }
}

#[test]
fn decompressing_one_granule_matches_the_corresponding_slice_of_the_full_decode() {
    let raw = multi_granule_bytes();
    let packed = compress(&raw);
    for granule in 0..raw.len().div_ceil(GRANULE_BYTES) {
        let start = granule * GRANULE_BYTES;
        let end = (start + GRANULE_BYTES).min(raw.len());
        assert_eq!(decompress_granule(&packed, granule).unwrap(), raw[start..end]);
    }
}

#[test]
fn decompressing_one_granule_does_not_need_any_other_granule_s_bytes() {
    let raw = multi_granule_bytes();
    let packed = compress(&raw);
    let (body_start, _) = granule_byte_range(&packed, 0).unwrap();
    let (target_start, target_len) = granule_byte_range(&packed, 1).unwrap();

    let mut corrupted = packed.clone();
    for (index, byte) in corrupted.iter_mut().enumerate().skip(body_start) {
        if index < target_start || index >= target_start + target_len {
            *byte = 0xFF;
        }
    }

    let start = GRANULE_BYTES;
    let end = (start + GRANULE_BYTES).min(raw.len());
    assert_eq!(decompress_granule(&corrupted, 1).unwrap(), raw[start..end]);
}

#[test]
fn an_out_of_range_granule_index_is_rejected() {
    let packed = compress(&multi_granule_bytes());
    assert!(decompress_granule(&packed, 999).is_err());
}

#[test]
fn a_page_decodes_granule_runs_from_one_directory_parse() {
    let raw = multi_granule_bytes();
    let packed = compress(&raw);
    let page = Page::open(&packed).unwrap();
    assert_eq!(page.plain_len(), raw.len());

    let granule_count = raw.len().div_ceil(GRANULE_BYTES);
    for first in 0..granule_count {
        for last in first..granule_count {
            let start = first * GRANULE_BYTES;
            let end = ((last + 1) * GRANULE_BYTES).min(raw.len());
            assert_eq!(page.decompress_granules(first, last).unwrap(), raw[start..end]);
        }
    }
    assert!(page.decompress_granules(0, granule_count).is_err());
    assert!(page.decompress_granules(2, 1).is_err());
}

/// A granule that inflates to a length other than the one its position implies breaks the fixed-stride arithmetic
/// range readers rely on, so the run decoder must reject it.
#[test]
fn a_page_rejects_a_granule_that_inflates_to_the_wrong_length() {
    let raw = multi_granule_bytes();
    // Re-split the plaintext off the granule grid: the directory's total plain length stays truthful, but granule 0
    // inflates to more than GRANULE_BYTES.
    let split = GRANULE_BYTES + 100;
    let granules = [&raw[..split], &raw[split..]];
    let compressed: Vec<Vec<u8>> = granules
        .iter()
        .map(|granule| miniz_oxide::deflate::compress_to_vec(granule, 6))
        .collect();
    let lens: Vec<usize> = compressed.iter().map(Vec::len).collect();
    let mut packed = super::mini_block_directory::encode(raw.len(), &lens);
    for granule in &compressed {
        packed.extend_from_slice(granule);
    }

    let page = Page::open(&packed).unwrap();
    assert!(page.decompress_granules(0, 0).is_err());
    assert!(page.decompress_granules(0, 1).is_err());
}

/// A directory that claims far more plaintext than its granule count could hold is a forged header. Decompress rejects
/// it up front rather than trusting the length as an allocation size or an accumulation bound.
#[test]
fn a_forged_plain_length_is_rejected() {
    let mut forged = Writer::with_capacity(8);
    forged.put_u32(0);
    forged.put_u32(u32::MAX);
    let packed = forged.into_bytes();

    let err = decompress(&packed).unwrap_err();
    assert!(matches!(err, FormatError::Structural { .. }));
}
