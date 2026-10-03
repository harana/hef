//! Checks the deflate compression family: it joins the adaptive candidate set purely on measured size, its on-disk
//! bytes are a portable RFC-1951 stream any host decodes without Intel IAA, and a reader can decode one granule of a
//! page without inflating the rest.

use hef::encoding::deflate::{GRANULE_BYTES, granule_byte_range};
use hef::encoding::{ColumnData, Compression, decode_block, decode_block_range, encode_block};

/// A handful of widely-spread `u64` constants, repeated in a fixed cycle. Cycling through such different-looking
/// values defeats FastLanes FOR/DELTA (their deltas span nearly the full 64-bit range, just like the plain baseline)
/// and RLE (no two consecutive values are ever equal), so the plain transform wins the size sample — while the tiny
/// repeating unit still makes the plain bytes highly compressible within any 4 KiB deflate granule.
const CYCLE: [u64; 8] = [
    0x9E37_79B9_7F4A_7C15,
    0x2545_F491_4F6C_DD1D,
    0xBF58_476D_1CE4_E5B9,
    0x94D0_49BB_1331_11EB,
    0xD6E8_FEB8_6659_FD93,
    0xA5A5_5A5A_5A5A_5A5A,
    0x1234_5678_9ABC_DEF0,
    0x0F0E_0D0C_0B0A_0908,
];

fn repetitive_column(rows: usize) -> Vec<u64> {
    (0..rows).map(|i| CYCLE[i % CYCLE.len()]).collect()
}

/// A simple LCG: enough to defeat deflate's 4 KiB window without pulling in a dependency, and just as high-entropy a
/// `u64` stream as the repetitive cycle above, so the two inputs pick the same top-level transform and differ only in
/// how compressible their bytes are.
fn incompressible_column(rows: usize) -> Vec<u64> {
    let mut state: u64 = 0x243F_6A88_85A3_08D3;
    (0..rows)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state
        })
        .collect()
}

/// conformance:
/// hef-encodings-and-compression/qpl-deflate-as-an-iaa-accelerated-software-parity-compression-family/deflate-chosen-only-by-the-sampler-never-by-config
#[test]
fn deflate_chosen_only_by_the_sampler_never_by_config() {
    let compressible = encode_block(&ColumnData::U64(repetitive_column(4096)), true);
    assert_eq!(
        compressible.pipeline.compression().unwrap(),
        Compression::Deflate,
        "a highly repetitive random-access column should win the sampler's size comparison"
    );

    // There is no parameter here that could force deflate on: `encode_block` takes only the data and a random-access
    // flag. With data that fails the same size comparison, the pipeline keeps no compression.
    let random = encode_block(&ColumnData::U64(incompressible_column(4096)), true);
    assert_eq!(
        random.pipeline.compression().unwrap(),
        Compression::None,
        "incompressible data must not be forced through deflate"
    );
}

/// conformance:
/// hef-encodings-and-compression/qpl-deflate-as-an-iaa-accelerated-software-parity-compression-family/deflate-page-decodes-in-software-without-an-accelerator
#[test]
fn deflate_page_decodes_in_software_without_an_accelerator() {
    let values = repetitive_column(4096);
    let block = encode_block(&ColumnData::U64(values.clone()), true);
    assert_eq!(
        block.pipeline.compression().unwrap(),
        Compression::Deflate,
        "the test data must exercise the deflate family"
    );

    // decode_block always runs through the process-wide decompressor, which defaults to (and in this process, with no
    // accelerator installed, always is) the pure-software miniz_oxide inflater — the same engine whether or not an
    // IAA host produced these bytes.
    let decoded = decode_block(block.pipeline, &block.bytes).unwrap();
    assert_eq!(decoded, ColumnData::U64(values));
}

/// conformance:
/// hef-encodings-and-compression/qpl-deflate-as-an-iaa-accelerated-software-parity-compression-family/single-granule-random-access-on-a-deflate-page
#[test]
fn single_granule_random_access_on_a_deflate_page() {
    let rows_per_granule = GRANULE_BYTES / 8;
    let values = repetitive_column(rows_per_granule * 4);
    let block = encode_block(&ColumnData::U64(values.clone()), true);
    assert_eq!(block.pipeline.compression().unwrap(), Compression::Deflate);

    // Rows [start, end) sit entirely inside one granule of the plain body (row i is at byte offset 4 + i * 8).
    let start = rows_per_granule * 2 + 3;
    let end = start + 5;
    let target_granule = (4 + start * 8) / GRANULE_BYTES;
    assert_eq!(
        (4 + (end - 1) * 8) / GRANULE_BYTES,
        target_granule,
        "the range must stay inside one granule"
    );

    // Corrupt every compressed granule except the target one — the per-granule offset table itself is left intact, so
    // the reader can still find the target granule, it just can no longer inflate any other one. Decoding the range
    // must still succeed and be correct, proving only the target granule's bytes were read.
    let (body_start, _) = granule_byte_range(&block.bytes, 0).unwrap();
    let (granule_start, granule_len) = granule_byte_range(&block.bytes, target_granule).unwrap();
    let mut corrupted = block.bytes.clone();
    for (index, byte) in corrupted.iter_mut().enumerate().skip(body_start) {
        if index < granule_start || index >= granule_start + granule_len {
            *byte = 0xFF;
        }
    }

    let decoded = decode_block_range(block.pipeline, &corrupted, start, end).unwrap();
    assert_eq!(decoded, ColumnData::U64(values[start..end].to_vec()));
}
