use super::*;
use crate::file::bytes::{Reader, Writer};
use arrow_array::Array;
use miniz_oxide::deflate::compress_to_vec;

/// A transform chosen from a sample can encode the full block larger than simply writing the values out — which for
/// the format's own metadata columns, a handful of values each, was the normal case rather than the unlucky one.
/// Whatever the sampler picks, the block stored must never be bigger than its plain form.
#[test]
fn a_block_that_encodes_larger_than_plain_is_stored_plain() {
    // A few values each: the shape of a marks page's columns, where a transform's framing costs more than the values.
    for values in [
        vec![7u64, 9],
        vec![1_000_000u64, 1_000_064, 1_000_128],
        vec![0u64],
        vec![u64::MAX, 3, 17, 9_000_000_000],
    ] {
        let data = ColumnData::U64(values.clone());
        let block = encode_block(&data, false);
        let plain = plain_form(&data, false, CascadeStrategy::DecodeOptimized, None);
        assert!(
            block.bytes.len() <= plain.bytes.len(),
            "{} values encoded to {} bytes, past the plain form's {}",
            values.len(),
            block.bytes.len(),
            plain.bytes.len()
        );
        assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
    }
}

/// The gate must not cost a block that genuinely earns its encoding: a long run-heavy column stays on the transform
/// that shrinks it rather than being pushed onto the plain form.
#[test]
fn a_block_that_earns_its_encoding_keeps_it() {
    let data = ColumnData::U64((0..4096u64).map(|i| i / 64).collect());
    let block = encode_block(&data, false);
    let plain = plain_form(&data, false, CascadeStrategy::DecodeOptimized, None);
    assert!(
        block.bytes.len() * 10 < plain.bytes.len(),
        "a run-heavy block must still encode far under plain: {} vs {}",
        block.bytes.len(),
        plain.bytes.len()
    );
    assert_ne!(block.pipeline.transform().unwrap(), Transform::PlainU64);
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
}

fn round_trip(data: ColumnData, random_access: bool) -> (Transform, Compression) {
    let encoded = encode_block(&data, random_access);
    let decoded = decode_block(encoded.pipeline, &encoded.bytes).unwrap();
    assert_eq!(decoded, data);
    (
        encoded.pipeline.transform().unwrap(),
        encoded.pipeline.compression().unwrap(),
    )
}

#[test]
fn monotonic_columns_choose_for_or_delta() {
    let values: Vec<u64> = (0..8192u64).map(|i| 1_700_000_000_000 + i * 17).collect();
    let (transform, _) = round_trip(ColumnData::U64(values), false);
    assert!(
        matches!(transform, Transform::ForBitpack | Transform::DeltaBitpack),
        "monotonic data must select a FastLanes-style candidate, got {transform:?}"
    );
}

#[test]
fn repeated_values_choose_rle() {
    // Long runs of distinct, widely spread values: RLE beats FOR (which pays the value spread) and DELTA (which pays
    // the run edges).
    let values: Vec<u64> = (0..64u64)
        .flat_map(|run| std::iter::repeat_n(run * 1_000_003, 64))
        .collect();
    let (transform, _) = round_trip(ColumnData::U64(values), false);
    assert_eq!(transform, Transform::Rle);
    // A constant block still round-trips through whatever wins.
    round_trip(ColumnData::U64(vec![42; 4096]), false);
}

#[test]
fn transform_sampling_sees_past_a_misleading_prefix() {
    // A constant head exactly as long as the sample, then high-entropy values. A leading-prefix sample sees one run
    // and picks RLE, which then pays 12 bytes per distinct tail value over the whole block; the stratified sample
    // spans both regions, so selection selects for the block that actually gets encoded.
    let mut values = vec![42u64; TRANSFORM_SAMPLE_SIZE];
    let mut state = 0x1234_5678_u64;
    for _ in 0..7 * 1024 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        values.push(state >> 32);
    }
    let encoded = encode_block(&ColumnData::U64(values.clone()), true);
    let transform = encoded.pipeline.transform().unwrap();
    assert_ne!(
        transform,
        Transform::Rle,
        "a misleading constant prefix must not pick RLE for a high-entropy block"
    );
    let mut rle = Writer::new();
    encode_rle(&values, &mut rle);
    assert!(
        encoded.bytes.len() * 2 < rle.into_bytes().len(),
        "the stratified pick must beat the prefix pick by a wide margin"
    );
    assert_eq!(
        decode_block(encoded.pipeline, &encoded.bytes).unwrap(),
        ColumnData::U64(values)
    );
}

#[test]
fn monotonic_block_still_chooses_delta_across_sample_run_boundaries() {
    // The stratified sample concatenates runs from across the block, so a monotonic column's sample carries a few
    // large between-run jumps. Those jumps must not push selection off the FastLanes candidates.
    let values: Vec<u64> = (0..8192u64).map(|i| 10_000 + i * 3).collect();
    let (transform, _) = round_trip(ColumnData::U64(values), true);
    assert!(
        matches!(transform, Transform::ForBitpack | Transform::DeltaBitpack),
        "monotonic data must keep a FastLanes-style candidate under stratified sampling, got {transform:?}"
    );
}

#[test]
fn metric_f64_uses_alp_with_exact_round_trip() {
    let values: Vec<f64> = (0..2048).map(|i| (i as f64) * 0.25 + 10.5).collect();
    let (transform, _) = round_trip(ColumnData::F64(values), false);
    assert_eq!(transform, Transform::Alp);
    // Pathological doubles fall back without corruption.
    let weird: Vec<f64> = (0..256).map(|i| (i as f64).sqrt() * std::f64::consts::PI).collect();
    round_trip(ColumnData::F64(weird), false);
}

/// Doubles with a fixed sign, exponent, and top mantissa byte (a small, constant magnitude around `2^-17`) and the
/// remaining 40 mantissa bits pseudo-random. ALP cannot decimal-scale such fine-grained noise into any of its 19
/// powers of ten without exceptions on nearly every row, so it rejects the column — like the ML feature values or
/// sensor readings the issue targets — while the top 3 bytes (sign, exponent, top mantissa byte) still repeat
/// identically across every row, exactly what byte-stream-split needs to win.
fn narrow_range_noisy_floats(count: usize) -> Vec<f64> {
    const FIXED_TOP_24_BITS: u64 = 0x3EE0_0000_0000_0000;
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..count)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            f64::from_bits(FIXED_TOP_24_BITS | (state & 0xFF_FFFF_FFFF))
        })
        .collect()
}

#[test]
fn noisy_float_column_alp_rejects_prefers_byte_stream_split_over_plain() {
    let values = narrow_range_noisy_floats(4096);
    let (transform, _) = round_trip(ColumnData::F64(values), false);
    assert_eq!(
        transform,
        Transform::ByteStreamSplit,
        "high-entropy floats in a narrow magnitude range should prefer byte-stream-split over raw plain storage"
    );
}

#[test]
fn byte_stream_split_deflate_page_supports_single_granule_range_decode() {
    let values = narrow_range_noisy_floats(4096);
    let block = encode_block(&ColumnData::F64(values.clone()), true);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::ByteStreamSplit);
    assert_eq!(block.pipeline.compression().unwrap(), Compression::SeekableZstd);

    let start = 100;
    let end = 137;
    let decoded = decode_block_range(block.pipeline, &block.bytes, start, end).unwrap();
    assert_eq!(decoded, ColumnData::F64(values[start..end].to_vec()));
}

/// Builds a forged ALP block body (`[exponent][exception count = 0][bitpack count][bitpack width]`, no packed words)
/// so a test can drive the bitpack header past its valid range without a real encoder.
fn forged_alp_block(exponent_index: u8, count: u32, width: u8) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u8(exponent_index);
    out.put_u32(0);
    out.put_u32(count);
    out.put_u8(width);
    out.into_bytes()
}

/// `decode_alp_range` must reject the same forged bitpack headers `bitunpack` already rejects instead of trusting
/// them straight from untrusted bytes (issue #9855): a width beyond 64 would shift a u64 by 64 while reassembling the
/// high half, and a zero-width block claiming `u32::MAX` values would allocate an amplified `Vec` from a handful of
/// input bytes.
#[test]
fn alp_range_decode_rejects_forged_bit_width_and_amplified_count() {
    let pipeline = PipelineId::new(Transform::Alp, Compression::None, ValueKind::F64);

    let too_wide = forged_alp_block(0, 4, 65);
    assert!(decode_block_range(pipeline, &too_wide, 0, 4).is_err());

    let amplified = forged_alp_block(0, u32::MAX, 0);
    assert!(decode_block_range(pipeline, &amplified, 0, 2_000_000_000).is_err());

    // The same shape with a valid width and an in-bound count still decodes (proves the guards reject only the
    // forged cases). One FastLanes vector's worth of packed words (FASTLANES_LANES words at width 1) follows the
    // header.
    let mut valid_bytes = forged_alp_block(0, 4, 1);
    for _ in 0..FASTLANES_LANES {
        valid_bytes.extend_from_slice(&0u64.to_le_bytes());
    }
    assert!(decode_block_range(pipeline, &valid_bytes, 0, 4).is_ok());
}

#[test]
fn money_is_decimal128_never_float() {
    let (transform, _) = round_trip(
        ColumnData::Decimal {
            values: vec![123_456_789_000_000_000_000_000i128, -5_00],
            scale: 2,
        },
        false,
    );
    assert_eq!(transform, Transform::Decimal128);
}

#[test]
fn low_cardinality_strings_choose_dictionary() {
    let values: Vec<Option<String>> = (0..2000)
        .map(|i| Some(["EUR", "USD", "NOK"][i % 3].to_owned()))
        .collect();
    let (transform, _) = round_trip(ColumnData::Strings(values.into()), false);
    assert_eq!(transform, Transform::DictionaryString);
}

#[test]
fn high_cardinality_short_strings_choose_fsst() {
    let values: Vec<Option<String>> = (0..2000)
        .map(|i| {
            if i % 17 == 0 {
                None
            } else {
                Some(format!("https://example.com/opportunity/{i}/stage"))
            }
        })
        .collect();
    let (transform, _) = round_trip(ColumnData::Strings(values.into()), false);
    assert_eq!(transform, Transform::FsstString);
}

/// A random-access column never takes whole-block LZ4 or Zstandard, whose compressed bytes are not addressable by
/// row. The granular deflate family is the exception it may still take, and only for the layouts that stay
/// addressable under it: a dictionary block, which does not, keeps no trailing stage at all.
#[test]
fn random_access_blocks_keep_only_the_compression_that_preserves_range_access() {
    let values: Vec<Option<String>> = (0..512).map(|i| Some(format!("text {i} {i}"))).collect();
    let encoded = encode_block(&ColumnData::Strings(values.clone().into()), true);
    assert!(
        matches!(
            encoded.pipeline.compression().unwrap(),
            Compression::None | Compression::SeekableZstd
        ),
        "a random-access block took {:?}",
        encoded.pipeline.compression().unwrap()
    );
    assert!(encoded.pipeline.supports_byte_range_extraction().unwrap());
    assert_eq!(
        decode_block(encoded.pipeline, &encoded.bytes).unwrap(),
        ColumnData::Strings(values.into())
    );

    let statuses = ["lost", "open", "pending", "won"];
    let low_cardinality: Vec<Option<String>> = (0..512).map(|i| Some(statuses[i % 4].to_owned())).collect();
    let dictionary = encode_block(&ColumnData::Strings(low_cardinality.into()), true);
    assert_eq!(dictionary.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(dictionary.pipeline.compression().unwrap(), Compression::None);
}

#[test]
fn pipeline_id_is_recorded_and_decodable() {
    let encoded = encode_block(&ColumnData::U64((0..100).collect()), false);
    let id = encoded.pipeline;
    assert_eq!(
        PipelineId::new(
            id.transform().unwrap(),
            id.compression().unwrap(),
            id.value_kind().unwrap()
        ),
        id
    );
}

#[test]
fn byte_range_extraction_capability_matches_decode_block_range_s_fast_paths() {
    assert!(Transform::Alp.is_per_value_addressable());
    assert!(Transform::FsstString.is_per_value_addressable());
    assert!(!Transform::PlainU64.is_per_value_addressable());
    assert!(!Transform::PlainF64.is_per_value_addressable());
    assert!(!Transform::ByteStreamSplit.is_per_value_addressable());
    assert!(Transform::DictionaryString.is_per_value_addressable());
    assert!(Transform::RawString.is_per_value_addressable());

    assert!(Transform::PlainU64.supports_framed_range());
    assert!(Transform::PlainF64.supports_framed_range());
    assert!(Transform::ByteStreamSplit.supports_framed_range());
    assert!(!Transform::Alp.supports_framed_range());
    assert!(!Transform::DictionaryString.supports_framed_range());

    // The string arenas resolve a row's bytes through their offset table, so deflating them in granules keeps range
    // access; the dictionary and ALP layouts do not, so they stay off the granular family.
    assert!(Transform::FsstString.keeps_range_access_under_framing());
    assert!(Transform::RawString.keeps_range_access_under_framing());
    assert!(Transform::PlainU64.keeps_range_access_under_framing());
    assert!(!Transform::DictionaryString.keeps_range_access_under_framing());
    assert!(!Transform::Alp.keeps_range_access_under_framing());

    let raw_deflated = PipelineId::new(Transform::RawString, Compression::SeekableZstd, ValueKind::String);
    assert!(raw_deflated.supports_byte_range_extraction().unwrap());

    let plain_uncompressed = PipelineId::new(Transform::PlainU64, Compression::None, ValueKind::U64);
    assert!(!plain_uncompressed.supports_byte_range_extraction().unwrap());

    let plain_deflated = PipelineId::new(Transform::PlainU64, Compression::SeekableZstd, ValueKind::U64);
    assert!(plain_deflated.supports_byte_range_extraction().unwrap());

    let alp = PipelineId::new(Transform::Alp, Compression::None, ValueKind::F64);
    assert!(alp.supports_byte_range_extraction().unwrap());
}

/// A block larger than the full-trial size takes the sampling-cascade path, which compresses the whole block with only
/// one codec instead of both. Whichever codec the sample picks, the stored bytes must decode back to the exact input —
/// the cascade changes only how much work encoding does, never the bytes a reader recovers.
#[test]
fn large_block_sampling_cascade_round_trips_exactly() {
    // A repetitive body well above `TRAILING_FULL_TRIAL_MAX_BYTES`: both codecs shrink it, so a trailing stage is kept.
    let body: Vec<u8> = (0..400_000u32)
        .flat_map(|i| (u64::from(i % 512)).to_le_bytes())
        .collect();
    assert!(
        body.len() > TRAILING_FULL_TRIAL_MAX_BYTES,
        "test body must exercise the sampling path"
    );
    let (compression, stored) = apply_trailing(body.clone(), CascadeStrategy::DecodeOptimized);
    assert_ne!(
        compression,
        Compression::None,
        "a large, repetitive body must still gain a trailing stage"
    );
    assert_eq!(
        remove_trailing(compression, &stored).unwrap(),
        body,
        "the sampled trailing stage must round-trip byte-for-byte"
    );
}

/// A large body that neither codec can shrink past the threshold must be kept raw: the sampler picks one codec, sees its
/// real whole-block output miss the bar, and stores the block uncompressed rather than paying to store a larger form.
#[test]
fn large_incompressible_block_keeps_raw_bytes() {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let body: Vec<u8> = (0..400_000u32)
        .flat_map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state.to_le_bytes()
        })
        .collect();
    let (compression, stored) = apply_trailing(body.clone(), CascadeStrategy::SizeOptimized);
    assert_eq!(compression, Compression::None);
    assert_eq!(stored, body);
}

/// The benchmark's no-trailing control keeps every choice `DecodeOptimized` makes and drops only the trailing stage:
/// the same transform and side stream, the body `DecodeOptimized` would have compressed stored as it stands, and no
/// seekable frames on a random-access column either.
#[test]
fn the_no_trailing_strategy_stores_the_decode_optimized_body_uncompressed() {
    let strings = ColumnData::Strings(
        (0..4096)
            .map(|i| Some(format!("row {i} settled without incident")))
            .collect(),
    );
    let ints = ColumnData::U64((0..4096u64).map(|i| i % 512).collect());
    for (data, random_access) in [(&strings, false), (&strings, true), (&ints, false), (&ints, true)] {
        let optimized = encode_block_with_strategy(data, random_access, CascadeStrategy::DecodeOptimized);
        let control = encode_block_with_strategy(data, random_access, CascadeStrategy::NoTrailing);
        assert_eq!(
            control.pipeline.transform().unwrap(),
            optimized.pipeline.transform().unwrap()
        );
        assert_eq!(
            control.pipeline.side_stream().unwrap(),
            optimized.pipeline.side_stream().unwrap()
        );
        assert_eq!(control.pipeline.compression().unwrap(), Compression::None);
        assert_eq!(control.bytes.len() as u64, control.uncompressed_len);
        assert_eq!(control.uncompressed_len, optimized.uncompressed_len);
        assert_eq!(
            remove_trailing(optimized.pipeline.compression().unwrap(), &optimized.bytes).unwrap(),
            control.bytes,
            "the control stores exactly the body the optimized block compresses"
        );
        assert_eq!(decode_block(control.pipeline, &control.bytes).unwrap(), *data);
    }
    // A repetitive string body is one `DecodeOptimized` does compress, so the control is a real difference.
    let optimized = encode_block_with_strategy(&strings, false, CascadeStrategy::DecodeOptimized);
    assert_ne!(optimized.pipeline.compression().unwrap(), Compression::None);
}

/// The FastLanes transposed layout must round-trip exactly for every bit width and at every vector boundary — including
/// counts that are not a multiple of the 1024-value vector, where the final vector is zero-padded.
#[test]
fn fastlanes_bitpack_round_trips_across_widths_and_block_boundaries() {
    // A small deterministic generator so the test needs no rng dependency.
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    let counts = [
        0usize, 1, 7, 8, 9, 16, 17, 31, 63, 64, 127, 128, 129, 511, 512, 1023, 1024, 1025, 2047, 2048, 2049, 3000,
    ];
    for width in 0u32..=64 {
        let limit = mask(width);
        for &count in &counts {
            let values: Vec<u64> = (0..count)
                .map(|_| if width == 0 { 0 } else { next() & limit })
                .collect();
            let mut writer = Writer::new();
            bitpack(&values, width, &mut writer);
            let bytes = writer.into_bytes();
            let mut reader = Reader::new(&bytes);
            let decoded = bitunpack(&mut reader).unwrap();
            assert_eq!(decoded, values, "round-trip failed at width {width}, count {count}");
        }
    }
}

/// Reading one value straight from its packed words must give exactly what unpacking the whole vector around it gives,
/// at every bit width and at every position inside a vector — including the spill widths, where a value straddles two
/// 64-bit words. A range decode picks between the two paths on width alone, so a disagreement would surface as a
/// point read and a scan returning different values for the same row.
#[test]
fn reading_one_fastlanes_value_matches_unpacking_its_whole_vector() {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        state
    };
    // Two whole vectors plus a partial one, so the check covers the padded tail as well as the interior.
    let count = 2 * 1024 + 300;
    for width in 0u32..=64 {
        let limit = mask(width);
        let values: Vec<u64> = (0..count)
            .map(|_| if width == 0 { 0 } else { next() & limit })
            .collect();
        let mut writer = Writer::new();
        bitpack(&values, width, &mut writer);
        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        reader.u32("count").unwrap();
        reader.u8("width").unwrap();
        let packed = reader.take(reader.remaining(), "packed").unwrap();
        // Every position of the first vector, the sub-block and lane boundaries of the second, and the padded tail.
        let probes = (0..1024)
            .chain((1024..2048).step_by(17))
            .chain(2048..count)
            .chain([count - 1]);
        for index in probes {
            assert_eq!(
                fastlanes_value_at(packed, width, index),
                values[index],
                "width {width}, index {index}"
            );
        }
    }
}

/// The packed body is a whole number of 1024-value FastLanes vectors, each exactly `16 * width` 64-bit words — the
/// property a SIMD/hardware decoder relies on. This pins the on-disk size so an accidental return to a non-FastLanes
/// layout is caught.
#[test]
fn fastlanes_layout_packs_whole_1024_value_vectors() {
    for &(count, width) in &[(1usize, 7u32), (1024, 11), (1025, 11), (5000, 13)] {
        let values: Vec<u64> = (0..count as u64).map(|i| i & mask(width)).collect();
        let mut writer = Writer::new();
        bitpack(&values, width, &mut writer);
        let vectors = count.div_ceil(1024);
        // header: 4 (count) + 1 (width); body: vectors * 16 lanes * width words * 8 bytes.
        let expected = 5 + vectors * 16 * width as usize * 8;
        assert_eq!(writer.len(), expected, "count {count}, width {width}");
    }
}

/// String values whose null pattern drives the encoder to a specific side-stream form.
fn string_values_with_nulls(row_count: usize, present: impl Fn(usize) -> bool) -> StringColumn {
    (0..row_count)
        .map(|i| present(i).then(|| format!("value-{i:04}")))
        .collect()
}

/// The shared null side stream must round-trip every form — `all_present` and `all_absent` at zero payload bytes,
/// runs for clustered nulls, the raw bitmap for scattered nulls — and every form must answer presence, rank, and
/// count questions identically to the values it was derived from. Requirement: "Presence and null bitmaps are encoded
/// side streams".
#[test]
fn null_stream_round_trips_every_form_with_identical_presence_answers() {
    let patterns: Vec<(&str, StringColumn)> = vec![
        ("all present", string_values_with_nulls(300, |_| true)),
        ("all absent", string_values_with_nulls(300, |_| false)),
        ("clustered", string_values_with_nulls(300, |i| !(100..250).contains(&i))),
        ("scattered", string_values_with_nulls(300, |i| i % 2 == 0)),
        ("single row", string_values_with_nulls(1, |_| true)),
        ("trailing null", string_values_with_nulls(9, |i| i < 8)),
    ];
    for (name, values) in patterns {
        let stream = NullStream::from_values(&values);
        let mut out = Writer::new();
        stream.write(&mut out);
        let bytes = out.into_bytes();
        let mut reader = Reader::new(&bytes);
        let decoded = NullStream::read(&mut reader).unwrap();
        assert_eq!(decoded, stream, "{name}: round trip");
        assert_eq!(decoded.row_count(), values.len(), "{name}: row count");
        assert_eq!(
            decoded.present_count(),
            values.iter().filter(|value| value.is_some()).count(),
            "{name}: present count"
        );
        let mut rank = 0usize;
        for (row, value) in values.iter().enumerate() {
            assert_eq!(decoded.is_present(row), value.is_some(), "{name}: is_present({row})");
            assert_eq!(decoded.present_before(row), rank, "{name}: present_before({row})");
            if value.is_some() {
                rank += 1;
            }
        }
        let mut walked: Vec<bool> = Vec::new();
        decoded
            .each_row(|set| {
                walked.push(set);
                Ok(())
            })
            .unwrap();
        let expected: Vec<bool> = values.iter().map(|value| value.is_some()).collect();
        assert_eq!(walked, expected, "{name}: each_row");
    }
}

/// The zero-byte and run forms must actually be chosen where they win: an all-present block stores five null-stream
/// bytes (count + form tag), clustered nulls store a short run list, and only a scattered pattern falls back to the
/// raw bitmap.
#[test]
fn null_stream_picks_the_smallest_form() {
    let encoded_len = |values: &StringColumn| {
        let mut out = Writer::new();
        NullStream::from_values(values).write(&mut out);
        out.len()
    };
    // 4-byte count + 1-byte form tag, nothing else, for both zero-byte forms.
    assert_eq!(encoded_len(&string_values_with_nulls(4096, |_| true)), 5);
    assert_eq!(encoded_len(&string_values_with_nulls(4096, |_| false)), 5);
    // One null cluster splits presence into two runs: 5 + 4 (run count) + 2 * 8 bytes, far under the 512-byte bitmap.
    assert_eq!(
        encoded_len(&string_values_with_nulls(4096, |i| !(1000..3000).contains(&i))),
        25
    );
    // Alternating presence would need 2048 runs, so the raw one-bit-per-row bitmap wins: 5 + 512 bytes.
    assert_eq!(encoded_len(&string_values_with_nulls(4096, |i| i % 2 == 0)), 517);
}

/// Every string transform must round-trip under every null side-stream form, and the zero-copy view decode must
/// report the identical null pattern — the equivalence oracle for retiring the unconditional raw bitmap.
#[test]
fn string_transforms_round_trip_under_every_null_stream_form() {
    let presence_patterns: [(&str, fn(usize) -> bool); 4] = [
        ("all present", |_| true),
        ("all absent", |_| false),
        ("clustered", |i| !(200..900).contains(&i)),
        ("scattered", |i| i % 3 != 1),
    ];
    for (name, present) in presence_patterns {
        // Low-cardinality labels select the dictionary transform; short high-cardinality values select FSST; long
        // opaque bodies select the raw arena. An all-absent block always encodes raw.
        let dictionary: Vec<Option<String>> = (0..1200)
            .map(|i| present(i).then(|| format!("label-{}", i % 3)))
            .collect();
        let fsst: Vec<Option<String>> = (0..1200)
            .map(|i| present(i).then(|| format!("event-name-{i:05}")))
            .collect();
        let raw: Vec<Option<String>> = (0..1200)
            .map(|i| present(i).then(|| format!("{i:05}-").repeat(40)))
            .collect();
        for values in [dictionary, fsst, raw] {
            let encoded = encode_block(&ColumnData::Strings(values.clone().into()), false);
            let decoded = decode_block(encoded.pipeline, &encoded.bytes).unwrap();
            assert_eq!(
                decoded,
                ColumnData::Strings(values.clone().into()),
                "{name}: full decode"
            );
            if let Some(views) = decode_string_block_views(encoded.pipeline, &encoded.bytes).unwrap() {
                let view_nulls: Vec<bool> = (0..views.len()).map(|i| views.is_valid(i)).collect();
                let expected: Vec<bool> = values.iter().map(|value| value.is_some()).collect();
                assert_eq!(view_nulls, expected, "{name}: view nulls");
                let viewed: Vec<Option<String>> = (0..views.len())
                    .map(|row| views.is_valid(row).then(|| views.value(row).to_string()))
                    .collect();
                assert_eq!(viewed, values, "{name}: view values");
            }
        }
    }
}

/// A malformed null side stream must refuse, never decode: unknown form tags, runs that are empty, out of bounds, or
/// out of order, and a zero-byte form whose forged row count would amplify straight into the decoder's allocation.
#[test]
fn null_stream_refuses_malformed_input() {
    let read = |bytes: &[u8]| {
        let mut reader = Reader::new(bytes);
        NullStream::read(&mut reader)
    };
    let stream = |row_count: u32, form: u8, rest: &[u8]| {
        let mut out = Writer::new();
        out.put_u32(row_count);
        out.put_u8(form);
        out.put_slice(rest);
        out.into_bytes()
    };
    let run = |start: u32, end: u32| [start.to_le_bytes(), end.to_le_bytes()].concat();
    assert!(read(&stream(8, 9, &[])).is_err(), "unknown form tag");
    assert!(read(&stream(u32::MAX, 0, &[])).is_err(), "amplified all-present count");
    assert!(read(&stream(u32::MAX, 1, &[])).is_err(), "amplified all-absent count");
    let empty_run = [2u32.to_le_bytes().as_slice(), &run(3, 3)].concat();
    assert!(read(&stream(8, 2, &empty_run)).is_err(), "empty run");
    let beyond = [1u32.to_le_bytes().as_slice(), &run(0, 9)].concat();
    assert!(read(&stream(8, 2, &beyond)).is_err(), "run past the row count");
    let touching = [2u32.to_le_bytes().as_slice(), &run(0, 2), &run(2, 4)].concat();
    assert!(read(&stream(8, 2, &touching)).is_err(), "touching runs");
    let unsorted = [2u32.to_le_bytes().as_slice(), &run(4, 6), &run(0, 2)].concat();
    assert!(read(&stream(8, 2, &unsorted)).is_err(), "unsorted runs");
    assert!(read(&stream(64, 3, &[0xFF; 4])).is_err(), "truncated raw bitmap");
}

/// A hand-built FSST string block: one present row whose compressed bytes are `data`, under a symbol table with the
/// given lengths (each symbol's bytes are its index repeated).
fn forged_fsst_block(symbol_lengths: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Writer::new();
    // Null side stream: one row, `all_present` form (tag 0), zero stored null bytes.
    out.put_u32(1);
    out.put_u8(0);
    out.put_u16(symbol_lengths.len() as u16);
    for (index, len) in symbol_lengths.iter().enumerate() {
        out.put_u64(u64::from_le_bytes([index as u8; 8]));
        out.put_u8(*len);
    }
    out.put_u32(1);
    out.put_u32(0);
    out.put_u32(data.len() as u32);
    out.put_slice(data);
    out.into_bytes()
}

/// Like [`forged_fsst_block`] but with explicit symbol words, so a test can force two length-≥3 symbols whose low three
/// bytes collide in fsst-rs's lossy hash table.
fn forged_fsst_block_words(symbols: &[(u64, u8)], data: &[u8]) -> Vec<u8> {
    let mut out = Writer::new();
    // Null side stream: one row, `all_present` form (tag 0), zero stored null bytes.
    out.put_u32(1);
    out.put_u8(0);
    out.put_u16(symbols.len() as u16);
    for (word, len) in symbols {
        out.put_u64(*word);
        out.put_u8(*len);
    }
    out.put_u32(1);
    out.put_u32(0);
    out.put_u32(data.len() as u32);
    out.put_slice(data);
    out.into_bytes()
}

/// A forged FSST symbol table must refuse with a `FormatError`: `fsst::Compressor::rebuild_from` asserts on more
/// than 255 symbols or misordered lengths (a whole-process abort under `panic = "abort"`), and a length outside 1..=8
/// would walk the crate's unchecked decode loop out of bounds.
#[test]
fn a_forged_fsst_symbol_table_is_rejected_not_aborted() {
    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String);

    let too_many_symbols = forged_fsst_block(&vec![2u8; 300], &[0]);
    let zero_length = forged_fsst_block(&[0], &[0]);
    let over_length = forged_fsst_block(&[9], &[0]);
    let one_then_longer = forged_fsst_block(&[1, 3], &[0]);
    let decreasing = forged_fsst_block(&[3, 2], &[0]);
    for forged in [
        &too_many_symbols,
        &zero_length,
        &over_length,
        &one_then_longer,
        &decreasing,
    ] {
        assert!(decode_block(pipeline, forged).is_err());
        assert!(decode_string_block_views(pipeline, forged).is_err());
        assert!(decode_block_range(pipeline, forged, 0, 1).is_err());
    }
}

/// A forged FSST table whose length-≥3 symbols collide in fsst-rs's private lossy hash table would make
/// `rebuild_from` assert (a whole-process abort under `panic = "abort"`), the one forged-table abort the earlier
/// checks could not reach from outside the crate (issue #4054). The reader replicates the crate's slot function and
/// rejects the collision as a `FormatError` instead. Words `0x000001` and `0x000801` both land in slot 1249.
#[test]
fn a_forged_fsst_table_colliding_in_the_decoder_hash_is_rejected() {
    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String);

    let colliding = forged_fsst_block_words(&[(0x00_0001, 3), (0x00_0801, 3)], &[0]);
    assert!(decode_block(pipeline, &colliding).is_err());
    assert!(decode_string_block_views(pipeline, &colliding).is_err());
    assert!(decode_block_range(pipeline, &colliding, 0, 1).is_err());

    // The same shape whose two length-3 symbols land in different slots (0x000001 -> 1249, 0x000002 -> 450) has no
    // collision, so the reader accepts the table and decodes it — proving the guard rejects only the collision.
    let distinct = forged_fsst_block_words(&[(0x00_0001, 3), (0x00_0002, 3)], &[0]);
    assert!(decode_block(pipeline, &distinct).is_ok());
}

/// A hand-built FSST string block holding two present rows, whose compressed bytes are `first` and `second` under a
/// symbol table given as explicit `(word, length)` pairs.
fn forged_fsst_block_pair(symbols: &[(u64, u8)], first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut out = Writer::new();
    // Null side stream: two rows, `all_present` form (tag 0), zero stored null bytes.
    out.put_u32(2);
    out.put_u8(0);
    out.put_u16(symbols.len() as u16);
    for (word, len) in symbols {
        out.put_u64(*word);
        out.put_u8(*len);
    }
    out.put_u32(2);
    out.put_u32(0);
    out.put_u32(first.len() as u32);
    out.put_u32((first.len() + second.len()) as u32);
    out.put_slice(first);
    out.put_slice(second);
    out.into_bytes()
}

/// The block's values decompress in one pass into a single buffer, so the UTF-8 check runs over that whole buffer
/// rather than over each value. A whole that validates does not make every part valid — a two-byte character split
/// across two values concatenates into valid UTF-8 while neither value is — so every decode path must still refuse a
/// block whose value boundary lands mid-character.
#[test]
fn a_value_boundary_cutting_a_character_in_half_is_rejected() {
    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String);

    // Codes 0 and 1 emit the two bytes of `é` one each: together `é`, apart neither is a character.
    let split = forged_fsst_block_pair(&[(0xC3, 1), (0xA9, 1)], &[0], &[1]);
    assert!(decode_block(pipeline, &split).is_err());
    assert!(decode_string_block_views(pipeline, &split).is_err());
    assert!(decode_block_range(pipeline, &split, 0, 2).is_err());

    // The same shape whose two values are whole characters decodes, proving the guard rejects only the split.
    let whole = forged_fsst_block_pair(&[(0xA9C3, 2)], &[0], &[0]);
    assert_eq!(
        decode_block(pipeline, &whole).unwrap(),
        ColumnData::Strings(vec![Some("\u{e9}"), Some("\u{e9}")].into())
    );
}

/// A hand-built raw string block holding two present rows whose bytes are `first` and `second`.
fn forged_raw_string_block_pair(first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut out = Writer::new();
    // Null side stream: two rows, `all_present` form (tag 0), zero stored null bytes.
    out.put_u32(2);
    out.put_u8(0);
    out.put_u32(2);
    out.put_u32(0);
    out.put_u32(first.len() as u32);
    out.put_u32((first.len() + second.len()) as u32);
    out.put_slice(first);
    out.put_slice(second);
    out.into_bytes()
}

/// A hand-built block-local plain dictionary block: two entries whose bytes are `first` and `second`, then two present
/// rows storing codes 0 and 1.
fn forged_dictionary_block_pair(first: &[u8], second: &[u8]) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u32(2);
    out.put_u8(0);
    out.put_u32(2);
    out.put_u32(0);
    out.put_u32(first.len() as u32);
    out.put_u32((first.len() + second.len()) as u32);
    out.put_slice(first);
    out.put_slice(second);
    bitpack(&[0, 1], 1, &mut out);
    out.into_bytes()
}

/// The raw and dictionary arms carve their arena by stored offsets just as the FSST arm carves its decompressed one,
/// so a boundary landing mid-character must be refused there too, by the view decode and the full decode alike; and
/// a block whose null stream marks more rows present than it stores values for is refused, never padded.
#[test]
fn raw_and_dictionary_value_boundaries_cutting_a_character_in_half_are_rejected() {
    let raw = PipelineId::new(Transform::RawString, Compression::None, ValueKind::String);
    let split = forged_raw_string_block_pair(&[0xC3], &[0xA9]);
    assert!(decode_string_block_views(raw, &split).is_err());
    assert!(decode_block(raw, &split).is_err());
    let whole = forged_raw_string_block_pair("\u{e9}".as_bytes(), "\u{e9}".as_bytes());
    let views = decode_string_block_views(raw, &whole).unwrap().unwrap();
    assert_eq!((views.value(0), views.value(1)), ("\u{e9}", "\u{e9}"));

    let dictionary = PipelineId::new(Transform::DictionaryString, Compression::None, ValueKind::String);
    let split = forged_dictionary_block_pair(&[0xC3], &[0xA9]);
    assert!(decode_string_block_views(dictionary, &split).is_err());
    assert!(decode_block(dictionary, &split).is_err());
    let whole = forged_dictionary_block_pair("\u{e9}".as_bytes(), b"x");
    let views = decode_string_block_views(dictionary, &whole).unwrap().unwrap();
    assert_eq!((views.value(0), views.value(1)), ("\u{e9}", "x"));

    // Two rows marked present, one value stored.
    let mut short = Writer::new();
    short.put_u32(2);
    short.put_u8(0);
    short.put_u32(1);
    short.put_u32(0);
    short.put_u32(1);
    short.put_slice(b"a");
    let short = short.into_bytes();
    assert!(decode_string_block_views(raw, &short).is_err());
    assert!(decode_block(raw, &short).is_err());
}

/// The view decode lays its views straight into a row-sized vector from whichever form the null stream took, so every
/// form — all present, all absent, clustered runs, a scattered raw bitmap — must land exactly the values the full
/// decode does, for every string transform.
#[test]
fn string_view_decode_weaves_every_null_stream_form() {
    let rows = 700usize;
    let value = |i: usize| match i % 3 {
        0 => format!("k-{}", i % 5),
        1 => format!("value-{i:05}-{}", "\u{e9}".repeat(i % 4)),
        _ => String::new(),
    };
    let patterns: [(&str, fn(usize) -> bool); 4] = [
        ("all present", |_| true),
        ("all absent", |_| false),
        ("clustered runs", |i| (i / 100) % 2 == 0),
        ("scattered bitmap", |i| i % 3 != 1),
    ];
    for (name, present) in patterns {
        let values: Vec<Option<String>> = (0..rows).map(|i| present(i).then(|| value(i))).collect();
        let data = ColumnData::Strings(values.clone().into());
        for transform in [Transform::DictionaryString, Transform::FsstString, Transform::RawString] {
            let block =
                encode_block_forced_for_conformance(&data, false, CascadeStrategy::DecodeOptimized, transform).unwrap();
            assert_eq!(
                decode_block(block.pipeline, &block.bytes).unwrap(),
                data,
                "{name} {transform:?}"
            );
            let views = decode_string_block_views(block.pipeline, &block.bytes)
                .unwrap()
                .expect("a string block decodes to views");
            let viewed: Vec<Option<String>> = (0..views.len())
                .map(|row| views.is_valid(row).then(|| views.value(row).to_string()))
                .collect();
            assert_eq!(viewed, values, "{name} {transform:?}");
        }
    }
}

/// A stored FSST code byte at or past the symbol count would index the crate's symbol table without a bounds check
/// (an out-of-bounds read on a forged file), and an escape code with no literal byte after it trips a crate assert.
/// Both must surface as a `FormatError` from every FSST decode path.
#[test]
fn forged_fsst_code_bytes_are_rejected_before_the_unchecked_decode() {
    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String);

    let out_of_range_code = forged_fsst_block(&[2], &[0x7F]);
    let dangling_escape = forged_fsst_block(&[2], &[255]);
    for forged in [&out_of_range_code, &dangling_escape] {
        assert!(decode_block(pipeline, forged).is_err());
        assert!(decode_string_block_views(pipeline, forged).is_err());
        assert!(decode_block_range(pipeline, forged, 0, 1).is_err());
    }

    // The same shape with an in-range code decodes fine, proving the guard rejects only forged codes.
    let valid = forged_fsst_block(&[2], &[0]);
    assert!(decode_block(pipeline, &valid).is_ok());
}

/// The decoder is all that stands between a stored byte and a table lookup, so it must refuse exactly the two shapes
/// it cannot take — a code the table never defined and an escape with nothing after it — and decode everything else,
/// including escaped literals that happen to look like bad codes.
#[test]
fn fsst_decoding_refuses_only_undefined_codes_and_dangling_escapes() {
    let full = fsst::Compressor::rebuild_from(
        (0..FSST_MAX_SYMBOL_COUNT as u8)
            .map(fsst::Symbol::from_u8)
            .collect::<Vec<_>>(),
        vec![1u8; FSST_MAX_SYMBOL_COUNT],
    );
    // The crate keeps its longer symbols first, so code 0 is `bc` and code 1 is `a`.
    let two = fsst::Compressor::rebuild_from(
        [
            fsst::Symbol::from_slice(&[b'b', b'c', 0, 0, 0, 0, 0, 0]),
            fsst::Symbol::from_u8(b'a'),
        ],
        [2u8, 1],
    );
    let cases: [(&fsst::Compressor, &[u8], Option<&[u8]>); 19] = [
        (&full, &[], Some(&[])),
        (&full, &[0, 254], Some(&[0, 254])),
        (&full, &[255, 65], Some(b"A")),
        (&full, &[255, 255], Some(&[255])),
        (&full, &[7, 255, 255, 255, 255], Some(&[7, 255, 255])),
        (&full, &[255], None),
        (&full, &[7, 255], None),
        (&full, &[255, 255, 255], None),
        (&full, &[255, 65, 255], None),
        (&two, &[0, 1, 0], Some(b"bcabc")),
        (&two, &[255, 2], Some(&[2])),
        (&two, &[255, 255], Some(&[255])),
        (&two, &[255, 200, 1], Some(&[200, b'a'])),
        (&two, &[2], None),
        (&two, &[0, 200], None),
        (&two, &[255], None),
        (&two, &[1, 255], None),
        (&two, &[255, 255, 255], None),
        (&two, &[255, 2, 2], None),
    ];
    for (compressor, compressed, expected) in cases {
        let mut buffer = Vec::new();
        let decoded = fsst_decompress_value(&FsstDecodeTable::new(compressor), compressed, &mut buffer)
            .ok()
            .map(|()| buffer.as_slice());
        assert_eq!(
            decoded,
            expected,
            "{} symbols, codes {compressed:?}",
            compressor.symbol_lengths().len()
        );
    }
}

/// A small deterministic generator (xorshift) for the forged-input tests, so any failure replays exactly.
fn pseudo_random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Serializes a symbol table the way the writer does: the `u16` count, then each symbol's word and its length.
fn forged_fsst_table(symbols: &[(u64, u8)]) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u16(symbols.len() as u16);
    for (word, len) in symbols {
        out.put_u64(*word);
        out.put_u8(*len);
    }
    out.into_bytes()
}

/// The table language restated on its own, independently of the parser: at most 255 symbols, every length in
/// 1..=8, lengths non-decreasing until the first 1 and all 1 from there on, and no two length-≥3 symbols sharing a
/// slot of fsst-rs's lossy hash table.
fn fsst_table_is_well_formed(symbols: &[(u64, u8)]) -> bool {
    if symbols.len() > FSST_MAX_SYMBOL_COUNT {
        return false;
    }
    let lengths: Vec<u8> = symbols.iter().map(|(_, len)| *len).collect();
    if lengths.iter().any(|len| !(1..=8).contains(len)) {
        return false;
    }
    let first_one = lengths.iter().position(|&len| len == 1).unwrap_or(lengths.len());
    if lengths[..first_one].windows(2).any(|pair| pair[1] < pair[0]) || lengths[first_one..].iter().any(|&len| len != 1)
    {
        return false;
    }
    let mut slots: Vec<u64> = symbols
        .iter()
        .filter(|(_, len)| *len >= 3)
        .map(|(word, _)| fsst_lossy_pht_slot(*word))
        .collect();
    let long_symbols = slots.len();
    slots.sort_unstable();
    slots.dedup();
    slots.len() == long_symbols
}

/// A forged symbol table of `count` symbols: `shape` 0 draws lengths freely from 0..=9 (mostly refused), 1 keeps
/// them in FSST order with random words (which collide in the lossy hash once there are more than a few dozen long
/// symbols), 2 keeps them in order with words squeezed into few bits so collisions are the rule, and 3 keeps them in
/// order with every symbol on a distinct low three-byte prefix — the slot is that prefix times an odd number modulo
/// the table size, so such a table never collides and is always accepted.
fn forged_fsst_symbols(state: &mut u64, count: usize, shape: u64) -> Vec<(u64, u8)> {
    let mut symbols = Vec::with_capacity(count);
    let mut len = 2u8;
    for index in 0..count {
        let draw = pseudo_random(state);
        len = if shape == 0 {
            (draw % 10) as u8
        } else if len == 1 {
            1
        } else {
            match draw % 8 {
                0 => 1,
                1 | 2 => (len + 1).min(8),
                _ => len,
            }
        };
        let word = pseudo_random(state);
        let word = match shape {
            2 => word & 0x1FFF,
            3 => (word & !0xFF_FFFF) | ((index as u64 * 977) & 0x7FF),
            _ => word,
        };
        // A symbol's word is its bytes, little-endian, zero past its length — what the writer stores.
        let word = if len == 0 || len >= 8 {
            word
        } else {
            word & ((1u64 << (8 * u64::from(len))) - 1)
        };
        symbols.push((word, len));
    }
    symbols
}

/// The direct decode-table parser and the compressor parser must accept and reject exactly the same tables, agree
/// byte for byte on every table they accept, and match the language restated in `fsst_table_is_well_formed` —
/// so the accepted file language cannot drift between the two APIs or away from the rule set.
#[test]
fn the_direct_table_parser_and_the_compressor_parser_accept_the_same_tables() {
    let mut state = 0x5EED_u64;
    let mut tables: Vec<Vec<(u64, u8)>> = Vec::new();
    for round in 0..4000u64 {
        let draw = pseudo_random(&mut state);
        let count = match round % 4 {
            0 => (draw % 9) as usize,
            1 => 248 + (draw % 12) as usize,
            _ => (draw % 256) as usize,
        };
        tables.push(forged_fsst_symbols(&mut state, count, round % 4));
    }
    // Tables a real training run produces, in the writer's own layout.
    for corpus in [
        (0..300)
            .map(|i| format!("session-{i:07}@tenant-{:03}.example.com", i % 40))
            .collect::<Vec<_>>(),
        (0..300)
            .map(|i| format!("{i} \u{fc}n\u{ef}c\u{f6}d\u{e9} {}", "\u{2713}".repeat(i % 5)))
            .collect(),
        vec![String::from("a"); 20],
    ] {
        let corpus: Vec<&[u8]> = corpus.iter().map(String::as_bytes).collect();
        let compressor = fsst::Compressor::train(&corpus);
        let symbols = compressor.symbol_table();
        let lengths = compressor.symbol_lengths();
        tables.push(
            symbols
                .iter()
                .zip(lengths)
                .map(|(symbol, len)| (symbol.to_u64(), *len))
                .collect(),
        );
    }

    let (mut accepted, mut refused) = (0usize, 0usize);
    for (index, symbols) in tables.iter().enumerate() {
        let well_formed = fsst_table_is_well_formed(symbols);
        let bytes = forged_fsst_table(symbols);
        // Every table also goes in cut short, which both parsers must refuse alike.
        let cut = if index % 5 == 0 && !bytes.is_empty() {
            (pseudo_random(&mut state) % bytes.len() as u64) as usize
        } else {
            bytes.len()
        };
        let stored = &bytes[..cut];
        let mut direct_reader = Reader::new(stored);
        let mut compressor_reader = Reader::new(stored);
        let direct = FsstDecodeTable::read(&mut direct_reader);
        let via_compressor = read_fsst_compressor(&mut compressor_reader);
        assert_eq!(
            direct.as_ref().err(),
            via_compressor.as_ref().err(),
            "table {index}: {symbols:?} cut to {cut}"
        );
        assert_eq!(
            direct.is_ok(),
            well_formed && cut == bytes.len(),
            "table {index}: {symbols:?} cut to {cut}"
        );
        let (Ok(direct), Ok(compressor)) = (direct, via_compressor) else {
            refused += 1;
            continue;
        };
        accepted += 1;
        assert_eq!(direct_reader.position(), compressor_reader.position());
        assert_eq!(direct_reader.position(), bytes.len());
        let rebuilt = FsstDecodeTable::new(&compressor);
        assert_eq!(direct.symbol_count, symbols.len());
        assert_eq!(rebuilt.symbol_count, symbols.len());
        assert_eq!(direct.lengths, rebuilt.lengths);
        assert_eq!(direct.symbols, rebuilt.symbols);
        for (code, (word, len)) in symbols.iter().enumerate() {
            assert_eq!((direct.symbols[code], direct.lengths[code]), (*word, *len));
        }
        for code in symbols.len()..256 {
            assert_eq!((direct.symbols[code], direct.lengths[code]), (0, 0));
        }
    }
    assert!(
        accepted >= 500 && refused >= 500,
        "accepted {accepted}, refused {refused}"
    );
}

/// A plain one-code-at-a-time FSST decoder — the shape the chunked decoder replaced — as the reference it must match.
fn reference_fsst_decode(table: &FsstDecodeTable, codes: &[u8]) -> Result<Vec<u8>, FormatError> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(&code) = codes.get(at) {
        if code == fsst::ESCAPE_CODE {
            let literal = codes.get(at + 1).ok_or(FormatError::Truncated {
                what: "fsst escaped byte",
            })?;
            out.push(*literal);
            at += 2;
        } else {
            let len = table.lengths[usize::from(code)];
            if len == 0 {
                return Err(FormatError::RefOutOfRange { what: "fsst code" });
            }
            out.extend_from_slice(&table.symbols[usize::from(code)].to_le_bytes()[..usize::from(len)]);
            at += 1;
        }
    }
    Ok(out)
}

/// A random code stream over a table of `defined` symbols: `len` codes, each an escape pair (`escape` in 16),
/// a code the table never defined (`undefined` in 16, when the table leaves room), or a defined code. A stream may
/// end on a dangling escape.
fn forged_fsst_codes(state: &mut u64, defined: usize, len: usize, escape: u64, undefined: u64) -> Vec<u8> {
    let mut codes = Vec::with_capacity(len + 1);
    while codes.len() < len {
        let draw = pseudo_random(state);
        let kind = draw % 16;
        if kind < escape {
            codes.push(fsst::ESCAPE_CODE);
            if draw >> 8 & 0x3 != 0 || codes.len() < len {
                codes.push((draw >> 16) as u8);
            }
        } else if kind < escape + undefined && defined < 255 {
            codes.push(defined as u8 + ((draw >> 16) % (255 - defined as u64)) as u8);
        } else if defined > 0 {
            codes.push(((draw >> 16) % defined as u64) as u8);
        } else {
            codes.push(fsst::ESCAPE_CODE);
            codes.push((draw >> 16) as u8);
        }
    }
    codes
}

/// The chunked decoder must produce exactly what the one-at-a-time reference produces — the same bytes, or the same
/// error — over random tables and streams: no escapes, escapes anywhere in a word, escapes ending a word or a
/// value, undefined codes anywhere, empty values, values from one code to thousands, and the all-escape worst case.
/// Streams the reference accepts must also match the crate's own decoder.
#[test]
fn the_chunked_fsst_decoder_matches_the_reference_decoder_over_random_streams() {
    let mut state = 0xF55F_u64;
    let mut buffer = Vec::new();
    let (mut ok, mut undefined, mut dangling) = (0usize, 0usize, 0usize);
    for round in 0..6000u64 {
        let draw = pseudo_random(&mut state);
        let count = 1 + (draw % 255) as usize;
        let symbols = forged_fsst_symbols(&mut state, count, 3);
        assert!(fsst_table_is_well_formed(&symbols));
        let bytes = forged_fsst_table(&symbols);
        let table = FsstDecodeTable::read(&mut Reader::new(&bytes)).unwrap();
        let compressor = read_fsst_compressor(&mut Reader::new(&bytes)).unwrap();
        let len = match round % 8 {
            0 => 0,
            1..=4 => (draw >> 8) as usize % 9,
            5 | 6 => (draw >> 8) as usize % 64,
            _ => (draw >> 8) as usize % 3000,
        };
        let escape = match round % 5 {
            0 => 0,
            1 => 16,
            _ => (draw >> 20) % 5,
        };
        let undefined_rate = (draw >> 24) % 3;
        let codes = forged_fsst_codes(&mut state, count, len, escape, undefined_rate);
        let expected = reference_fsst_decode(&table, &codes);
        buffer.clear();
        buffer.extend_from_slice(b"kept");
        let decoded = fsst_decompress_value(&table, &codes, &mut buffer).map(|()| buffer[4..].to_vec());
        assert_eq!(decoded, expected, "table {symbols:?}, codes {codes:?}");
        match &expected {
            Ok(plain) => {
                ok += 1;
                assert_eq!(&buffer[..4], b"kept");
                assert_eq!(plain, &compressor.decompressor().decompress(&codes));
            }
            Err(FormatError::RefOutOfRange { .. }) => undefined += 1,
            Err(FormatError::Truncated { .. }) => dangling += 1,
            Err(other) => panic!("unexpected {other:?}"),
        }
    }
    assert!(
        ok >= 1000 && undefined >= 300 && dangling >= 100,
        "ok {ok}, undefined {undefined}, dangling {dangling}"
    );
}

/// The one-pass arena decode must land every value's bound where decoding the values one by one would, keep what the
/// buffer already held, and answer a bad offset table with the error the offset earns — `Structural` for an offset
/// running backwards, `Truncated` for one past the data — checking each value's offsets before decoding it, so an
/// earlier value's bad code still wins over a later bad offset. Repeated offsets are empty values, not errors.
#[test]
fn the_arena_fsst_decode_matches_decoding_the_values_one_by_one() {
    const WHAT: &str = "fsst value";
    const RULE: &str = "fsst offsets must be non-decreasing";
    let mut state = 0xA4E4_u64;
    let mut buffer = Vec::new();
    let (mut ok, mut backwards, mut past, mut bad_codes) = (0usize, 0usize, 0usize, 0usize);
    for round in 0..3000u64 {
        let draw = pseudo_random(&mut state);
        let count = 1 + (draw % 255) as usize;
        let symbols = forged_fsst_symbols(&mut state, count, 3);
        let table = FsstDecodeTable::read(&mut Reader::new(&forged_fsst_table(&symbols))).unwrap();
        // The arena may start past some leading bytes the offsets skip.
        let lead = (draw >> 8) as usize % 4;
        let mut data = vec![0xEE; lead];
        let mut offsets = vec![lead];
        let values = (draw >> 12) as usize % 40;
        for value in 0..values {
            let draw = pseudo_random(&mut state);
            let len = match draw % 4 {
                0 => 0,
                1 => draw as usize >> 8 & 0x7,
                _ => draw as usize >> 8 & 0x3F,
            };
            let escape = if round % 3 == 0 { 0 } else { draw >> 20 & 0x3 };
            let undefined = u64::from(value % 7 == 6 && draw >> 24 & 0x3 == 0);
            data.extend(forged_fsst_codes(&mut state, count, len, escape, undefined));
            offsets.push(data.len());
        }
        // Then a fault in the offsets, some of the time.
        let fault = pseudo_random(&mut state);
        if values > 0 && fault % 4 == 0 {
            let at = 1 + (fault >> 8) as usize % values;
            offsets[at] = offsets[at].saturating_sub(1 + (fault >> 16) as usize % 8);
        } else if fault % 4 == 1 {
            let at = (fault >> 8) as usize % offsets.len();
            offsets[at] = data.len() + 1 + (fault >> 16) as usize % 8;
        }

        let expected: Result<(Vec<usize>, Vec<u8>), FormatError> = (|| {
            let mut plain = b"kept".to_vec();
            let mut bounds = vec![plain.len()];
            if offsets[0] > data.len() {
                return Err(FormatError::Truncated { what: WHAT });
            }
            for pair in offsets.windows(2) {
                let (start, end) = (pair[0], pair[1]);
                if end < start {
                    return Err(FormatError::Structural { rule: RULE });
                }
                if end > data.len() {
                    return Err(FormatError::Truncated { what: WHAT });
                }
                plain.extend(reference_fsst_decode(&table, &data[start..end])?);
                bounds.push(plain.len());
            }
            Ok((bounds, plain))
        })();
        buffer.clear();
        buffer.extend_from_slice(b"kept");
        let decoded = fsst_decompress_values_with_table(&table, &data, &offsets, WHAT, RULE, &mut buffer)
            .map(|bounds| (bounds, buffer.clone()));
        assert_eq!(decoded, expected, "offsets {offsets:?}, data {data:?}");
        match &expected {
            Ok(_) => ok += 1,
            Err(FormatError::Structural { .. }) => backwards += 1,
            Err(FormatError::Truncated { what }) if *what == WHAT => past += 1,
            Err(_) => bad_codes += 1,
        }
    }
    assert!(
        ok >= 800 && backwards >= 150 && past >= 150 && bad_codes >= 100,
        "ok {ok}, backwards {backwards}, past {past}, bad codes {bad_codes}"
    );
}

/// The view decode reads every value's bound off the arena as it decompresses, so it must land on exactly the values
/// the full decode and the writer's input agree on — through empty values, values short enough to ride inside their
/// view, values long enough to point into the arena, characters the symbol table never learned, and absent rows.
#[test]
fn fsst_view_decode_matches_the_full_decode_over_empties_escapes_and_nulls() {
    let values: Vec<Option<String>> = (0..1400)
        .map(|i| match i % 7 {
            0 => None,
            1 => Some(String::new()),
            2 => Some(format!("id-{i}")),
            3 => Some(format!("session-{i:07}@tenant-{:03}.example.com", i % 400)),
            4 => Some(format!(
                "{i} \u{fc}n\u{ef}c\u{f6}d\u{e9} \u{2713} {}",
                "\u{1F600}".repeat(i % 3)
            )),
            5 => Some(char::from_u32(0x100 + (i as u32 % 0x300)).unwrap_or('?').to_string()),
            _ => Some((i % 5).to_string()),
        })
        .collect();
    let data = ColumnData::Strings(values.clone().into());
    for random_access in [false, true] {
        let block = encode_block_forced_for_conformance(
            &data,
            random_access,
            CascadeStrategy::DecodeOptimized,
            Transform::FsstString,
        )
        .unwrap();
        assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
        let views = decode_string_block_views(block.pipeline, &block.bytes)
            .unwrap()
            .expect("an FSST block decodes to views");
        let viewed: Vec<Option<String>> = (0..views.len())
            .map(|row| views.is_valid(row).then(|| views.value(row).to_string()))
            .collect();
        assert_eq!(viewed, values, "random access {random_access}");
    }

    // Hand-built blocks pin the shapes a trained table may or may not produce: a value that is nothing but escapes, an
    // empty value beside a full one, and a full 255-symbol table whose only refusable shape is the dangling escape.
    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String);
    let hello = u64::from_le_bytes(*b"hello\0\0\0");
    let escapes_only = forged_fsst_block_pair(&[(hello, 5)], &[255, b'a', 255, b'b'], &[0, 255, b'!', 0]);
    let views = decode_string_block_views(pipeline, &escapes_only).unwrap().unwrap();
    assert_eq!((views.value(0), views.value(1)), ("ab", "hello!hello"));
    let empty_then_full = forged_fsst_block_pair(&[(hello, 5)], &[], &[0]);
    let views = decode_string_block_views(pipeline, &empty_then_full).unwrap().unwrap();
    assert_eq!((views.value(0), views.value(1)), ("", "hello"));
    let full_table = vec![1u8; FSST_MAX_SYMBOL_COUNT];
    let views = decode_string_block_views(pipeline, &forged_fsst_block(&full_table, &[72, 105, 255, 33]))
        .unwrap()
        .unwrap();
    assert_eq!(views.value(0), "Hi!");
    assert!(decode_string_block_views(pipeline, &forged_fsst_block(&full_table, &[72, 255])).is_err());
    assert!(decode_block(pipeline, &forged_fsst_block(&full_table, &[72, 255])).is_err());
}

/// Every FSST decode path must answer a damaged block with an error, never a panic or an out-of-bounds read: the
/// block cut short at every length, and every byte of it overwritten with the values likeliest to forge a code past
/// the table, a dangling escape, or a broken offset.
#[test]
fn damaged_fsst_blocks_are_refused_not_decoded_out_of_bounds() {
    let values: Vec<Option<String>> = (0..60)
        .map(|i| (i % 9 != 4).then(|| format!("ticket-{i:04} \u{fc}n\u{ef}code {}", i % 13)))
        .collect();
    let block = encode_block_forced_for_conformance(
        &ColumnData::Strings(values.into()),
        false,
        CascadeStrategy::DecodeOptimized,
        Transform::FsstString,
    )
    .unwrap();
    // The damage goes into the FSST body itself, not into a trailing compression stage that would refuse it first.
    let body = remove_trailing(block.pipeline.compression().unwrap(), &block.bytes)
        .unwrap()
        .into_owned();
    let pipeline = PipelineId::new(Transform::FsstString, Compression::None, ValueKind::String)
        .with_side_stream(block.pipeline.side_stream().unwrap());
    assert!(decode_string_block_views(pipeline, &body).unwrap().is_some());

    let exercise = |bytes: &[u8]| {
        let _ = decode_block(pipeline, bytes);
        let _ = decode_string_block_views(pipeline, bytes);
        let _ = decode_block_range(pipeline, bytes, 1, 3);
    };
    for len in 0..body.len() {
        exercise(&body[..len]);
    }
    let mut damaged = body.clone();
    for position in 0..body.len() {
        for forged in [0x00, 0x7F, 0x80, 0xFE, 0xFF, body[position].wrapping_add(1)] {
            damaged[position] = forged;
            exercise(&damaged);
        }
        damaged[position] = body[position];
    }
}

/// An I64 column stored as `PlainU64 + Deflate` holds zigzag codes on disk; the range fast path must unzigzag them
/// back to `ColumnData::I64` exactly as the full decode does, not return the raw codes as `U64`.
#[test]
fn i64_plain_framed_range_decode_matches_the_full_decode() {
    let values: Vec<i64> = (0..1500i64).map(|i| (i - 750) * 12_345).collect();
    let mut body = Writer::new();
    body.put_u32(values.len() as u32);
    for value in &values {
        body.put_u64(zigzag(*value));
    }
    let bytes = seekable_zstd::compress(&body.into_bytes()).expect("the body compresses");
    let pipeline = PipelineId::new(Transform::PlainU64, Compression::SeekableZstd, ValueKind::I64);
    assert!(pipeline.supports_byte_range_extraction().unwrap());

    assert_eq!(decode_block(pipeline, &bytes).unwrap(), ColumnData::I64(values.clone()));
    // A range spanning the block's whole body, so the fast path stitches every frame it covers.
    let (start, end) = (100, 1400);
    assert_eq!(
        decode_block_range(pipeline, &bytes, start, end).unwrap(),
        ColumnData::I64(values[start..end].to_vec())
    );
    // An empty range keeps the I64 variant too.
    assert_eq!(
        decode_block_range(pipeline, &bytes, 5, 5).unwrap(),
        ColumnData::I64(Vec::new())
    );
}

/// An uncompressed block must decode from a borrow of the stored bytes — no whole-block copy on the hot read path.
#[test]
fn an_uncompressed_block_is_not_copied_when_its_trailing_stage_is_removed() {
    let bytes = vec![7u8; 64];
    let body = remove_trailing(Compression::None, &bytes).unwrap();
    assert!(matches!(body, std::borrow::Cow::Borrowed(_)));
}

/// The buffer-reusing entry points hand an uncompressed block through as the stored bytes themselves, and inflate a
/// compressed one to exactly what [`remove_trailing`] yields — into the buffer they were given.
#[test]
fn the_buffer_reusing_trailing_stage_removal_matches_remove_trailing() {
    let bytes = vec![7u8; 64];
    let mut scratch = vec![1u8; 8];
    let body = remove_trailing_into(Compression::None, &bytes, &mut scratch).unwrap();
    assert_eq!(body.as_ptr(), bytes.as_ptr());
    assert_eq!(scratch, vec![1u8; 8]);
    let borrowed = with_trailing_removed(Compression::None, &bytes, |body| Ok(body.as_ptr())).unwrap();
    assert_eq!(borrowed, bytes.as_ptr());

    let raw: Vec<u8> = (0..4096u32).map(|i| (i % 13) as u8).collect();
    let stored = compress_zstd_framed(&raw, 1);
    let inflated = remove_trailing_into(Compression::Zstd1, &stored, &mut scratch)
        .unwrap()
        .to_vec();
    assert_eq!(inflated, raw);
    assert_eq!(scratch, raw);
    assert_eq!(
        remove_trailing(Compression::Zstd1, &stored).unwrap().as_ref(),
        raw.as_slice()
    );
    let consumed = with_trailing_removed(Compression::Zstd1, &stored, |body| Ok(body.to_vec())).unwrap();
    assert_eq!(consumed, raw);
}

/// A framed page whose body length disagrees with its own row-count header is a forgery: a range read must refuse it
/// rather than serve rows a whole-block decode would never produce.
#[test]
fn a_framed_page_disagreeing_with_its_row_count_header_is_rejected_by_the_range_decoder() {
    let count = 1500usize;
    let mut body = Writer::new();
    // One row more than the block actually carries: the header promises 1501 rows of eight bytes, the body holds 1500.
    body.put_u32(count as u32 + 1);
    for i in 0..count {
        body.put_u64(i as u64);
    }
    let bytes = seekable_zstd::compress(&body.into_bytes()).expect("the body compresses");

    let pipeline = PipelineId::new(Transform::PlainU64, Compression::SeekableZstd, ValueKind::U64);
    assert!(decode_block_range(pipeline, &bytes, 600, 700).is_err());
}

/// The FOR, DELTA, and dictionary kernels all bit-pack through the FastLanes layout, so each must survive a round-trip
/// when the data spans several vectors and ends mid-vector (1025 and 3000 are not multiples of 1024).
#[test]
fn fastlanes_kernels_round_trip_across_vector_boundaries() {
    for &count in &[1025u64, 3000] {
        // Monotonic data selects a FOR/DELTA bit-packed candidate.
        let ramp: Vec<u64> = (0..count).map(|i| 1_700_000_000_000 + i * 17).collect();
        round_trip(ColumnData::U64(ramp), false);
        // Signed monotonic data exercises the zigzag + DELTA path.
        let signed: Vec<i64> = (0..count as i64).map(|i| -1_000_000 + i * 13).collect();
        round_trip(ColumnData::I64(signed), false);
        // Low-cardinality strings bit-pack their dictionary codes.
        let labels: Vec<Option<String>> = (0..count)
            .map(|i| Some(["EUR", "USD", "NOK", "GBP"][(i % 4) as usize].to_owned()))
            .collect();
        round_trip(ColumnData::Strings(labels.into()), false);
        // ALP packs its scaled integers, so its float path rides the layout too.
        let metric: Vec<f64> = (0..count).map(|i| (i as f64) * 0.25 + 10.5).collect();
        round_trip(ColumnData::F64(metric), false);
    }
}

/// The integer decode rebuilds the frame-of-reference base, the delta prefix sum, and the block's zigzag mapping
/// inside one pass over the packed lanes, so every integer transform must hand back exactly what it was given — for
/// signed columns (which reach the decoder as zigzag codes) as much as unsigned ones, including counts that end
/// mid-vector.
#[test]
fn integer_transforms_round_trip_signed_and_unsigned_blocks() {
    let transforms = [
        Transform::DeltaBitpack,
        Transform::ForBitpack,
        Transform::PlainU64,
        Transform::Rle,
    ];
    for count in [1i64, 1023, 1024, 1025, 3000] {
        // Values that swing either side of zero, so the delta stream carries negative differences and the block-level
        // zigzag mapping is not the identity.
        let signed: Vec<i64> = (0..count).map(|i| (i % 97 - 48) * 1_000_003 - i).collect();
        let unsigned: Vec<u64> = signed.iter().map(|value| value.unsigned_abs() + 7).collect();
        for transform in transforms {
            for data in [ColumnData::I64(signed.clone()), ColumnData::U64(unsigned.clone())] {
                let block =
                    encode_block_forced_for_conformance(&data, false, CascadeStrategy::SizeOptimized, transform)
                        .unwrap();
                assert_eq!(
                    decode_block(block.pipeline, &block.bytes).unwrap(),
                    data,
                    "{transform:?} at {count} values"
                );
            }
        }
    }
}

/// The fused i64 encoders ([`encode_for_bitpack_i64`], [`encode_delta_bitpack_i64`]) fold the zigzag mapping and the
/// frame-of-reference/delta step into the bitpack gather instead of materialising a zigzag-mapped copy first. They
/// must still write exactly the bytes the unfused, mapped-then-bitpacked path would.
#[test]
fn fused_i64_bitpack_encoders_match_the_unfused_mapped_path() {
    for count in [1i64, 1023, 1024, 1025, 3000] {
        let signed: Vec<i64> = (0..count).map(|i| (i % 97 - 48) * 1_000_003 - i).collect();
        let mapped: Vec<u64> = signed.iter().map(|value| zigzag(*value)).collect();

        let mut fused = Writer::new();
        encode_for_bitpack_i64(&signed, &mut fused);
        let mut unfused = Writer::new();
        encode_for_bitpack(&mapped, &mut unfused);
        assert_eq!(fused.into_bytes(), unfused.into_bytes(), "ForBitpack at {count} values");

        let mut fused = Writer::new();
        encode_delta_bitpack_i64(&signed, &mut fused);
        let mut unfused = Writer::new();
        encode_delta_bitpack(&mapped, &mut unfused);
        assert_eq!(
            fused.into_bytes(),
            unfused.into_bytes(),
            "DeltaBitpack at {count} values"
        );
    }
}

/// `i64::MIN` zigzags to `u64::MAX`, so a signed block holding it puts the frame-of-reference base within a packed
/// delta's reach of the top of the `u64` range. Such a block must still decode exactly, and a forged one whose base
/// plus delta leaves the range must be refused rather than wrapped.
#[test]
fn frame_of_reference_near_the_top_of_the_range_stays_exact() {
    let data = ColumnData::I64(vec![i64::MIN, -1, i64::MIN + 5, -3]);
    let block =
        encode_block_forced_for_conformance(&data, false, CascadeStrategy::SizeOptimized, Transform::ForBitpack)
            .unwrap();
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);

    let mut forged = Writer::new();
    forged.put_u64(u64::MAX);
    bitpack(&[1], 1, &mut forged);
    let pipeline = PipelineId::new(Transform::ForBitpack, Compression::None, ValueKind::U64);
    assert!(decode_block(pipeline, &forged.into_bytes()).is_err());
}

/// A `U128` block whose values straddle `i128::MAX` must record both bounds or neither: a half-converted pair is a
/// footer no reader can open.
#[test]
fn u128_stats_straddling_i128_max_are_both_present_or_both_absent() {
    let straddling = stats_for(&ColumnData::U128(vec![1, u128::MAX]));
    assert_eq!(straddling.min_i128, None);
    assert_eq!(straddling.max_i128, None);

    let within_range = stats_for(&ColumnData::U128(vec![1, i128::MAX as u128]));
    assert_eq!(within_range.min_i128, Some(1));
    assert_eq!(within_range.max_i128, Some(i128::MAX));

    let both_past_max = stats_for(&ColumnData::U128(vec![i128::MAX as u128 + 1, u128::MAX]));
    assert_eq!(both_past_max.min_i128, None);
    assert_eq!(both_past_max.max_i128, None);
}

/// Every `ColumnData` kind the shredded path carries round-trips through the random-access adaptive encoder, and the
/// per-value-addressable kinds range-decode a single granule exactly. These are the guarantees the shredded scan-path
/// columns rely on now that they use the same encoder and stored format as every other column.
#[test]
fn shredded_kinds_round_trip_and_range_decode_through_the_adaptive_encoder() {
    let u64_data = ColumnData::U64((0..5000u64).map(|i| i * 3 + 7).collect());
    let block = encode_block(&u64_data, true);
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), u64_data);
    assert_eq!(
        decode_block_range(block.pipeline, &block.bytes, 100, 132).unwrap(),
        ColumnData::U64((100..132).map(|i| i * 3 + 7).collect())
    );

    for data in [
        ColumnData::I64((-500..500i64).map(|i| i * 11).collect()),
        ColumnData::F64((0..1000).map(|i| i as f64 * 0.25).collect()),
        ColumnData::Decimal {
            values: (0..500i128).map(|i| i * 1000 - 250_000).collect(),
            scale: 2,
        },
        ColumnData::U128((0..64u128).map(|i| i << 64 | i).collect()),
    ] {
        let block = encode_block(&data, true);
        assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
    }
}

#[test]
fn shredded_low_cardinality_strings_round_trip_and_range_decode() {
    let statuses = ["lost", "open", "pending", "won"];
    let values: Vec<Option<String>> = (0..800)
        .map(|i| {
            if i % 13 == 0 {
                None
            } else {
                Some(statuses[i % statuses.len()].to_owned())
            }
        })
        .collect();
    let data = ColumnData::Strings(values.clone().into());
    let block = encode_block(&data, true);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
    assert_eq!(
        decode_block_range(block.pipeline, &block.bytes, 26, 39).unwrap(),
        ColumnData::Strings(values[26..39].to_vec().into())
    );
}

#[test]
fn shredded_high_cardinality_strings_round_trip() {
    let values: Vec<Option<String>> = (0..300).map(|i| Some(format!("event-{i:06}"))).collect();
    let data = ColumnData::Strings(values.into());
    let block = encode_block(&data, true);
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
}

#[test]
fn shredded_encoding_is_deterministic() {
    let data = ColumnData::U64((0..2048u64).collect());
    let a = encode_block(&data, true);
    let b = encode_block(&data, true);
    assert_eq!(a.bytes, b.bytes);
    assert_eq!(a.pipeline, b.pipeline);
}

/// Transform id 11 named the removed Vortex shredded serialization. A block still claiming it must be rejected as
/// structural corruption, never decoded as something else.
#[test]
fn retired_vortex_transform_id_is_rejected() {
    let pipeline = PipelineId(11);
    assert!(pipeline.transform().is_err());
    assert!(decode_block(pipeline, &[0u8; 16]).is_err());
    assert!(decode_block_range(pipeline, &[0u8; 16], 0, 4).is_err());
}

/// The dictionary range decoder must agree with the whole-block decode across FastLanes vector boundaries — the
/// extraction skips whole vectors, so an off-by-one there returns the wrong rows' codes rather than failing.
#[test]
fn dictionary_range_decode_matches_whole_block_across_vector_boundaries() {
    let statuses = ["lost", "open", "pending", "won"];
    let values: Vec<Option<String>> = (0..3_000)
        .map(|i| {
            if i % 7 == 0 {
                None
            } else {
                Some(statuses[i % statuses.len()].to_owned())
            }
        })
        .collect();
    let block = encode_block(&ColumnData::Strings(values.clone().into()), true);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::DictionaryString);
    for (start, end) in [(0, 40), (1_000, 1_100), (2_040, 2_060), (2_990, 3_000), (1_023, 1_025)] {
        assert_eq!(
            decode_block_range(block.pipeline, &block.bytes, start, end).unwrap(),
            ColumnData::Strings(values[start..end].to_vec().into()),
            "range {start}..{end}"
        );
    }
}

/// Long free-text bodies, the shape `choose_string_transform` sends to the raw arena: high cardinality, average
/// length past `FSST_MAX_AVERAGE_VALUE_LEN`, one row in nine absent.
fn long_free_text(rows: usize) -> Vec<Option<String>> {
    (0..rows)
        .map(|i| {
            (i % 9 != 0).then(|| {
                format!(
                    "note {i:06}: the renewal call covered pricing, the pilot rollout, and the security review; \
                     the account team agreed to send a revised quote by {}/{} and to schedule the migration \
                     workshop once procurement signs off.",
                    i % 28 + 1,
                    i % 12 + 1
                )
            })
        })
        .collect()
}

/// A raw-string block's arena is handed to Arrow as a string array's data buffer without Arrow re-checking one view
/// per row, so the decode itself has to refuse an arena that is not UTF-8 — otherwise a forged block would reach a
/// caller as a `&str` over arbitrary bytes.
#[test]
fn a_raw_string_block_holding_invalid_utf8_is_rejected() {
    let pipeline = PipelineId::new(Transform::RawString, Compression::None, ValueKind::String);
    let mut out = Writer::new();
    // Null side stream: one row, `all_present` form (tag 0), then one value of one byte that starts no character.
    out.put_u32(1);
    out.put_u8(0);
    out.put_u32(1);
    out.put_u32(0);
    out.put_u32(1);
    out.put_slice(&[0xFF]);
    let forged = out.into_bytes();

    assert!(decode_block(pipeline, &forged).is_err());
    assert!(decode_string_block_views(pipeline, &forged).is_err());
    assert!(decode_block_range(pipeline, &forged, 0, 1).is_err());
}

/// A raw-string block stores a byte arena behind an offset table, so one row's bytes are reachable without decoding
/// the block — the same guarantee FSST blocks already carried. Long free text is exactly the shape that lands here.
#[test]
fn raw_string_range_decode_matches_the_whole_block() {
    let values = long_free_text(2_000);
    let block = encode_block(&ColumnData::Strings(values.clone().into()), false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::RawString);
    assert!(block.pipeline.supports_byte_range_extraction().unwrap());
    for (start, end) in [(0, 2_000), (0, 1), (9, 10), (137, 138), (500, 640), (1_999, 2_000)] {
        assert_eq!(
            decode_block_range(block.pipeline, &block.bytes, start, end).unwrap(),
            ColumnData::Strings(values[start..end].to_vec().into()),
            "range {start}..{end}"
        );
    }
    // An empty range, and one running past the block, behave as they do for every other transform.
    assert_eq!(
        decode_block_range(block.pipeline, &block.bytes, 7, 7).unwrap(),
        ColumnData::Strings(StringColumn::new())
    );
    assert_eq!(
        decode_block_range(block.pipeline, &block.bytes, 1_995, 2_100).unwrap(),
        ColumnData::Strings(values[1_995..].to_vec().into())
    );
}

/// A point-accessible column keeps its trailing compression through the granular deflate family, and the range
/// decoder reads one row out of the deflate page without inflating the rest of it. Both string arenas take that
/// path; the values must match the whole-block decode exactly.
#[test]
fn string_arenas_range_decode_from_a_deflate_page() {
    let raw = long_free_text(2_000);
    let short: Vec<Option<String>> = (0..3_000)
        .map(|i| (i % 11 != 0).then(|| format!("session-{i:07}@tenant-{:03}.example.com", i % 400)))
        .collect();
    for (values, transform) in [(raw, Transform::RawString), (short, Transform::FsstString)] {
        let block = encode_block(&ColumnData::Strings(values.clone().into()), true);
        assert_eq!(block.pipeline.transform().unwrap(), transform);
        assert_eq!(
            block.pipeline.compression().unwrap(),
            Compression::SeekableZstd,
            "{transform:?} keeps a trailing stage that preserves range access"
        );
        assert!(block.pipeline.supports_byte_range_extraction().unwrap());
        assert_eq!(
            decode_block(block.pipeline, &block.bytes).unwrap(),
            ColumnData::Strings(values.clone().into())
        );
        let rows = values.len();
        for (start, end) in [(0, rows), (0, 1), (11, 12), (rows / 2, rows / 2 + 3), (rows - 1, rows)] {
            assert_eq!(
                decode_block_range(block.pipeline, &block.bytes, start, end).unwrap(),
                ColumnData::Strings(values[start..end].to_vec().into()),
                "{transform:?} range {start}..{end}"
            );
        }
    }
}

/// The point of the frame window is what it does *not* decompress: a single value's bytes come out of the frames that
/// hold them, leaving every other frame in the page compressed.
#[test]
fn a_frame_window_decompresses_only_the_frames_a_range_falls_in() {
    let frame = seekable_zstd::FRAME_BYTES as usize;
    let plain: Vec<u8> = (0..(frame * 6 + 40) as u32).map(|i| (i % 251) as u8).collect();
    let page = seekable_zstd::compress(&plain).expect("the arena compresses");

    let mut window = seekable_zstd::Window::open(&page).unwrap();
    assert_eq!(window.plain_len(), plain.len());
    assert_eq!(
        window.read(frame * 3 + 100, 40).unwrap(),
        plain[frame * 3 + 100..frame * 3 + 140]
    );
    assert_eq!(window.decompressed_frames(), 1);
    // A range straddling a frame boundary decompresses both sides of it, and nothing else.
    assert_eq!(window.read(frame - 3, 6).unwrap(), plain[frame - 3..frame + 3]);
    assert_eq!(window.decompressed_frames(), 3);

    // Reads still agree with the whole-page decompression everywhere, including the short final frame.
    assert_eq!(window.read(0, plain.len()).unwrap(), plain);
    assert_eq!(window.read(plain.len() - 5, 5).unwrap(), plain[plain.len() - 5..]);
    assert_eq!(window.read(7, 0).unwrap(), Vec::<u8>::new());
    assert!(window.read(plain.len() - 4, 5).is_err());
    assert_eq!(seekable_zstd::decompress_all(&page).unwrap(), plain);
}

/// The reused-scratch path must produce exactly the bytes `fsst::Compressor::compress` allocates per value, for every
/// value shape — so buffer reuse is invisible in the encoded block.
#[test]
fn fsst_buffer_reuse_matches_per_value_compress() {
    let values: Vec<String> = (0..512)
        .map(|i| match i % 5 {
            0 => String::new(),
            1 => format!("id{i}"),
            2 => format!("account-{i}-renewal-opportunity-{}", i * 31),
            3 => "übergroße Snowman ☃ value ".repeat(i % 7 + 1),
            _ => format!("{:x}", i * 2_654_435_761usize).repeat(200),
        })
        .collect();
    let corpus: Vec<&[u8]> = values.iter().map(|value| value.as_bytes()).collect();
    let compressor = fsst::Compressor::train(&corpus);
    let mut scratch = Vec::new();
    for value in &corpus {
        compress_fsst_value(&compressor, value, &mut scratch);
        assert_eq!(scratch, compressor.compress(value));
    }
}

/// A value the symbol table cannot cover at all compresses to the worst case — one escape pair per input byte —
/// exercising the exact capacity bound the safety comment in `compress_fsst_value` relies on.
#[test]
fn fsst_buffer_reuse_survives_the_all_escape_worst_case() {
    let corpus: Vec<&[u8]> = vec![b"aaaaaaaaaaaaaaaa"];
    let compressor = fsst::Compressor::train(&corpus);
    let plaintext = vec![b'z'; 4096];
    let mut scratch = Vec::new();
    compress_fsst_value(&compressor, &plaintext, &mut scratch);
    assert_eq!(scratch, compressor.compress(&plaintext));
    assert_eq!(scratch.len(), plaintext.len() * 2);
}

#[test]
fn the_decode_cost_multiplier_breaks_marginal_ties_toward_the_cheaper_family() {
    // 1024 runs of 4 values, each run a fresh 25-bit pseudo-random draw: RLE's estimate lands a couple of percent
    // under FOR's — inside the DecodeOptimized multiplier — so fresh publication takes the straight-line FastLanes
    // kernel while rewrite keeps the smaller RLE bytes. No wall-clock is consulted; the choice replays identically on
    // any node. Implements `hef-encodings-and-compression` — "Decode-cost preference is a deterministic score".
    let mut state = 0xACE1_u64;
    let mut values = Vec::with_capacity(4096);
    for _ in 0..1024 {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let value = (state >> 32) & 0x1FF_FFFF;
        for _ in 0..4 {
            values.push(value);
        }
    }
    assert_eq!(
        choose_u64_transform(&values, CascadeStrategy::SizeOptimized),
        Transform::Rle,
        "rewrite keeps the smaller bytes"
    );
    assert_ne!(
        choose_u64_transform(&values, CascadeStrategy::DecodeOptimized),
        Transform::Rle,
        "fresh publication must not keep a marginal RLE win over the FastLanes kernels"
    );
    let decode = encode_block_with_strategy(&ColumnData::U64(values.clone()), true, CascadeStrategy::DecodeOptimized);
    assert_eq!(
        decode_block(decode.pipeline, &decode.bytes).unwrap(),
        ColumnData::U64(values.clone())
    );
    let size = encode_block_with_strategy(&ColumnData::U64(values.clone()), true, CascadeStrategy::SizeOptimized);
    assert_eq!(
        decode_block(size.pipeline, &size.bytes).unwrap(),
        ColumnData::U64(values)
    );
}

#[test]
fn replay_skips_selection_and_the_trip_wire_rearms_on_drift() {
    // A stable column: the head's captured transform replays onto the next block.
    let head_values: Vec<u64> = (0..2048u64).map(|i| 10_000 + i * 3).collect();
    let head = encode_block(&ColumnData::U64(head_values.clone()), true);
    let capture = ReplayCapture::from_head(
        &ColumnData::U64(head_values),
        head.pipeline,
        head.uncompressed_len,
        None,
    )
    .unwrap();
    let next: Vec<u64> = (0..2048u64).map(|i| 500_000 + i * 3).collect();
    let (block, replayed) = encode_block_replayed(
        &ColumnData::U64(next.clone()),
        true,
        CascadeStrategy::DecodeOptimized,
        Some(&capture),
        None,
    );
    assert!(replayed, "a stable distribution keeps the replay");
    assert_eq!(block.pipeline.transform().unwrap(), capture.transform);
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::U64(next)
    );

    // A drifted column: the constant head compresses to almost nothing (width-0 FOR), so replaying its transform
    // onto high-entropy values blows the ratio past the trip-wire and full selection re-arms for this block instead
    // of the stale capture riding the rest of the column.
    let constant_head = encode_block(&ColumnData::U64(vec![7; 2048]), true);
    let stale = ReplayCapture::from_head(
        &ColumnData::U64(vec![7; 2048]),
        constant_head.pipeline,
        constant_head.uncompressed_len,
        None,
    )
    .unwrap();
    let mut state = 0x5EED_u64;
    let noise: Vec<u64> = (0..2048)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            state >> 16
        })
        .collect();
    let (block, replayed) = encode_block_replayed(
        &ColumnData::U64(noise.clone()),
        true,
        CascadeStrategy::DecodeOptimized,
        Some(&stale),
        None,
    );
    assert!(!replayed, "the trip-wire must re-arm full selection on drift");
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::U64(noise)
    );
}

/// The FSST symbol table a block stores, as `(symbol, length)` pairs read back out of its bytes.
fn stored_fsst_table(block: &EncodedBlock) -> Vec<(u64, u8)> {
    let body = remove_trailing(block.pipeline.compression().unwrap(), &block.bytes).unwrap();
    let mut reader = Reader::new(&body);
    NullStream::read(&mut reader).unwrap();
    fsst_table_entries(&FsstTable(read_fsst_compressor(&mut reader).unwrap()))
}

fn fsst_table_entries(table: &FsstTable) -> Vec<(u64, u8)> {
    table
        .0
        .symbol_table()
        .iter()
        .map(|symbol| symbol.to_u64())
        .zip(table.0.symbol_lengths().iter().copied())
        .collect()
}

fn fsst_user_value(i: u64) -> Option<String> {
    Some(format!("user-{}@example-{}.test", i * 7919 % 100_003, i % 13))
}

/// A later block of an FSST column compresses with the head's captured symbol table instead of training its own:
/// the table it stores is the head's — one its own training could never produce, since a quarter of the head's
/// values use bytes the later block never sees — it hands out no table of its own, and it decodes to its values. A
/// capture without a table trains one as before.
#[test]
fn a_replayed_fsst_block_stores_the_heads_table_instead_of_training_its_own() {
    let head_values: Vec<Option<String>> = (0..2048u64)
        .map(|i| {
            if i % 4 == 3 {
                Some(format!("order:{}/status=shipped", i * 104_729 % 99_991))
            } else {
                fsst_user_value(i)
            }
        })
        .collect();
    let head_data = ColumnData::Strings(head_values.into());
    let mut head = encode_block(&head_data, false);
    assert_eq!(head.pipeline.transform().unwrap(), Transform::FsstString);
    let head_table = head.fsst.take().expect("an FSST head hands out the table it trained");
    let capture = ReplayCapture::from_head(
        &head_data,
        head.pipeline,
        head.uncompressed_len,
        Some(head_table.clone()),
    )
    .unwrap();
    assert!(capture.fsst.is_some());

    let later = ColumnData::Strings((5_000..7_048u64).map(fsst_user_value).collect::<Vec<_>>().into());
    let (block, replayed) =
        encode_block_replayed(&later, false, CascadeStrategy::DecodeOptimized, Some(&capture), None);
    assert!(replayed, "the same distribution keeps the replay");
    assert!(block.fsst.is_none(), "a replayed block trains no table of its own");
    assert_eq!(stored_fsst_table(&block), fsst_table_entries(&head_table));
    let own = encode_block(&later, false);
    assert_ne!(
        stored_fsst_table(&own),
        fsst_table_entries(&head_table),
        "the block's own training would store a different table"
    );
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), later);

    let bare = ReplayCapture { fsst: None, ..capture };
    let (block, replayed) = encode_block_replayed(&later, false, CascadeStrategy::DecodeOptimized, Some(&bare), None);
    assert!(replayed);
    assert!(block.fsst.is_some(), "a capture without a table trains one");
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), later);
}

/// A distribution shift trips the replay's wire and the block trains its own table: the head's symbols cover almost
/// none of the shifted block's bytes, so compressing with them stores far more than the head's ratio allows, and full
/// selection runs instead — storing a table of the block's own — while still decoding exactly.
#[test]
fn a_shifted_distribution_retrains_its_fsst_table() {
    let head_data = ColumnData::Strings((0..2048u64).map(fsst_user_value).collect::<Vec<_>>().into());
    let mut head = encode_block(&head_data, false);
    let head_table = head.fsst.take().unwrap();
    let capture = ReplayCapture::from_head(
        &head_data,
        head.pipeline,
        head.uncompressed_len,
        Some(head_table.clone()),
    )
    .unwrap();
    let shifted = ColumnData::Strings(
        (0..2048u64)
            .map(|i| Some(format!("ORDER#{:X}#SHIPPED#{}", i * 2_654_435_761, i % 97)))
            .collect::<Vec<_>>()
            .into(),
    );
    let (block, replayed) =
        encode_block_replayed(&shifted, false, CascadeStrategy::DecodeOptimized, Some(&capture), None);
    assert!(!replayed, "a stale table must re-arm full selection");
    assert_eq!(block.pipeline.transform().unwrap(), Transform::FsstString);
    assert_ne!(stored_fsst_table(&block), fsst_table_entries(&head_table));
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), shifted);
}

/// The trailing stage must not be able to hide a transform that amplifies. A raw-arena head captured from one long
/// value replays onto a block of short repeated values as an offset table wider than the values it indexes — several
/// times what selection's dictionary block stores — and the granular deflate stage then squeezes those bytes back
/// under the head's ratio. Measured on the stored bytes the trip-wire keeps that replay and it rides the rest of the
/// replay segment; measured on the transform's own output it re-arms, and the block is encoded on its own data.
#[test]
fn a_trailing_stage_cannot_hide_an_amplifying_replay() {
    let long: String = (0..2_000u64)
        .map(|k| char::from(b'!' + (k.wrapping_mul(2_654_435_761) ^ (k >> 3)).wrapping_rem(90) as u8))
        .collect();
    let mut head_values: Vec<Option<String>> = vec![Some("small".to_owned()); 7];
    head_values.insert(0, Some(long));
    let head_data = ColumnData::Strings(head_values.into());
    let head = encode_block(&head_data, true);
    assert_eq!(head.pipeline.transform().unwrap(), Transform::RawString);
    let capture = ReplayCapture::from_head(&head_data, head.pipeline, head.uncompressed_len, None).unwrap();

    let later = ColumnData::Strings(vec![Some("small".to_owned()); 64].into());
    let (block, replayed) = encode_block_replayed(&later, true, CascadeStrategy::DecodeOptimized, Some(&capture), None);
    assert!(
        !replayed,
        "a replay whose transform amplifies the block must re-arm full selection, however well its bytes compress"
    );
    let selected = encode_block(&later, true);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(block.bytes.len(), selected.bytes.len());
    assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), later);
}

/// A replayed block can drift within the trip-wire yet still store more bytes than the plain form (a head at ratio
/// 0.9 tolerates replays up to 1.125): the plain-form acceptance gate rejects exactly that block, falling back to
/// full selection so a kept replay never regresses past plain.
#[test]
fn a_replay_past_the_plain_form_falls_back_to_full_selection() {
    // Average run length 1.4: RLE stores ~12 bytes per run = ~8.57 bytes per value, a few percent past plain's 8 —
    // inside a 0.9-ratio head's trip-wire, outside the plain form.
    let mut values = Vec::new();
    let mut next = 0u64;
    'outer: loop {
        for run_len in [2usize, 1, 1, 2, 1] {
            for _ in 0..run_len {
                values.push(next);
                if values.len() == 8192 {
                    break 'outer;
                }
            }
            next += 1;
        }
    }
    let data = ColumnData::U64(values.clone());
    let head = ReplayCapture {
        compression: Compression::None,
        decoded_len: 90,
        fsst: None,
        raw_len: 100,
        transform: Transform::Rle,
    };
    let (block, replayed) = encode_block_replayed(&data, true, CascadeStrategy::DecodeOptimized, Some(&head), None);
    assert!(
        !replayed,
        "a replay past the plain form must fall back to full selection"
    );
    let plain = encode_block_inner(
        &data,
        true,
        CascadeStrategy::DecodeOptimized,
        Some(Transform::PlainU64),
        Some(Compression::None),
        None,
        None,
    );
    assert!(
        block.bytes.len() <= plain.bytes.len(),
        "the kept block ({}) must never store more than the plain form ({})",
        block.bytes.len(),
        plain.bytes.len()
    );
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::U64(values)
    );
}

#[test]
fn conformance_forcing_exercises_every_family_and_rejects_invalid_ones() {
    let ints: Vec<u64> = (0..512u64).map(|i| 40 + i % 7).collect();
    let strings: Vec<Option<String>> = (0..64).map(|i| Some(format!("value-{}", i % 9))).collect();
    let floats: Vec<f64> = (0..512).map(|i| (i as f64) * 0.25 + 1.5).collect();
    let cases: Vec<(Transform, ColumnData)> = vec![
        (Transform::PlainU64, ColumnData::U64(ints.clone())),
        (Transform::ForBitpack, ColumnData::U64(ints.clone())),
        (Transform::DeltaBitpack, ColumnData::U64(ints.clone())),
        (Transform::Rle, ColumnData::U64(ints.clone())),
        (Transform::DictionaryString, ColumnData::Strings(strings.clone().into())),
        (Transform::FsstString, ColumnData::Strings(strings.clone().into())),
        (Transform::RawString, ColumnData::Strings(strings.clone().into())),
        (Transform::Alp, ColumnData::F64(floats.clone())),
        (Transform::AlpRd, ColumnData::F64(floats.clone())),
        (Transform::ByteStreamSplit, ColumnData::F64(floats.clone())),
        (Transform::PlainF64, ColumnData::F64(floats)),
        (
            Transform::Decimal128,
            ColumnData::Decimal {
                scale: 2,
                values: vec![1_00, 2_50, 99_99],
            },
        ),
        (Transform::PlainU128, ColumnData::U128(vec![1, u128::MAX, 42])),
    ];
    for (transform, data) in cases {
        let block =
            encode_block_forced_for_conformance(&data, true, CascadeStrategy::DecodeOptimized, transform).unwrap();
        assert_eq!(
            block.pipeline.transform().unwrap(),
            transform,
            "forcing {transform:?} must record it"
        );
        assert_eq!(decode_block(block.pipeline, &block.bytes).unwrap(), data);
    }
    // A transform invalid for the value kind is rejected like a corrupt recorded pipeline, never encoded by guess.
    assert!(
        encode_block_forced_for_conformance(
            &ColumnData::Strings(strings.into()),
            true,
            CascadeStrategy::DecodeOptimized,
            Transform::Rle
        )
        .is_err()
    );
    assert!(
        encode_block_forced_for_conformance(
            &ColumnData::U64(ints),
            true,
            CascadeStrategy::DecodeOptimized,
            Transform::DictionaryString
        )
        .is_err()
    );
}

/// Deterministic 64-bit scrambler (the splitmix64 finalizer) so tests synthesize noise without a random-number crate.
fn mix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Quarter-steps everywhere except rows 1024..2048 — one full FastLanes vector of mantissa noise ALP cannot scale
/// exactly, concentrated so the block as a whole is still worth ALP.
fn one_noisy_vector() -> Vec<f64> {
    (0..4096)
        .map(|i| {
            if (1024..2048).contains(&i) {
                f64::from_bits(0x3FF0_0000_0000_0000 | (mix(i as u64) & 0x000F_FFFF_FFFF_FFFF))
            } else {
                (i as f64) * 0.25
            }
        })
        .collect()
}

#[test]
fn alp_escapes_a_pathological_vector_instead_of_abandoning_the_block() {
    // A quarter of the stratified sample is exceptions — over the flat bound that used to disqualify ALP outright —
    // but they all sit in one vector, so the block encodes as ALP with that vector stored raw behind the sentinel.
    let values = one_noisy_vector();
    let block = encode_block(&ColumnData::F64(values.clone()), false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::Alp);
    let body = remove_trailing(block.pipeline.compression().unwrap(), &block.bytes).unwrap();
    assert_eq!(body[0], ALP_VECTOR_ESCAPE_SENTINEL);
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::F64(values)
    );
}

#[test]
fn alp_escaped_vectors_survive_range_decode_and_predicate_filtering() {
    let values = one_noisy_vector();
    let block = encode_block(&ColumnData::F64(values.clone()), true);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::Alp);
    for (start, end) in [
        (0, 4096),
        (1000, 1100),
        (1500, 1600),
        (2000, 2100),
        (900, 2100),
        (4090, 4096),
    ] {
        let ColumnData::F64(ranged) = decode_block_range(block.pipeline, &block.bytes, start, end).unwrap() else {
            panic!("alp block must range-decode to floats");
        };
        assert_eq!(ranged, values[start..end], "range [{start}, {end})");
    }
    let filter = predicate::FloatPredicate::Range {
        lower: Some(predicate::FloatBound {
            inclusive: true,
            value: 1.0,
        }),
        upper: Some(predicate::FloatBound {
            inclusive: false,
            value: 600.0,
        }),
    };
    let mask = predicate::filter_float_block(block.pipeline, &block.bytes, &filter)
        .unwrap()
        .expect("alp blocks answer float predicates from compressed bytes");
    assert_eq!(mask, filter.filter_decoded(&values));
}

#[test]
fn well_behaved_alp_blocks_keep_the_unescaped_layout() {
    let values: Vec<f64> = (0..4096).map(|i| (i as f64) * 0.25).collect();
    let block = encode_block(&ColumnData::F64(values.clone()), true);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::Alp);
    let body = remove_trailing(block.pipeline.compression().unwrap(), &block.bytes).unwrap();
    assert_ne!(body[0], ALP_VECTOR_ESCAPE_SENTINEL);
}

#[test]
fn alp_rd_encodes_high_entropy_doubles_with_shared_top_bits() {
    // 52 random mantissa bits under one shared sign/exponent, at a magnitude past ALP's scaled-integer guard so every
    // power rejects: no byte-level codec can shrink true noise — but the shared top 12 bits collapse into a one-entry
    // ALP-RD dictionary, so only the noisy 52 bits are stored per value.
    let values: Vec<f64> = (0..2048)
        .map(|i| f64::from_bits(0x43F0_0000_0000_0000 | (mix(i) >> 12)))
        .collect();
    let block = encode_block(&ColumnData::F64(values.clone()), false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::AlpRd);
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::F64(values)
    );
}

#[test]
fn alp_rd_round_trips_arbitrary_floats_when_forced() {
    let values: Vec<f64> = (0..1500)
        .map(|i| match i % 5 {
            0 => -(i as f64) * 3.5,
            1 => f64::from_bits(0x40E0_0000_0000_0000 | (mix(i as u64) & 0x000F_FFFF_FFFF_FFFF)),
            2 => 0.0,
            3 => (i as f64).sqrt(),
            _ => f64::MAX / (i as f64 + 1.0),
        })
        .collect();
    let block = encode_block_forced_for_conformance(
        &ColumnData::F64(values.clone()),
        true,
        CascadeStrategy::DecodeOptimized,
        Transform::AlpRd,
    )
    .unwrap();
    assert_eq!(block.pipeline.transform().unwrap(), Transform::AlpRd);
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::F64(values.clone())
    );
    let ColumnData::F64(ranged) = decode_block_range(block.pipeline, &block.bytes, 700, 900).unwrap() else {
        panic!("alp-rd block must range-decode to floats");
    };
    assert_eq!(ranged, values[700..900]);
}

#[test]
fn dictionary_blocks_fsst_compress_a_bulky_alphabet_and_keep_code_pushdown() {
    let distinct: Vec<String> = (0..200)
        .map(|i| {
            format!(
                "https://events.example.com/api/v1/tenants/tenant-{i:04}/streams/ingest/checkpoints/segment-{i:04}/manifest.json"
            )
        })
        .collect();
    let values: Vec<Option<String>> = (0..2000)
        .map(|i| (i % 13 != 0).then(|| distinct[i % 200].clone()))
        .collect();
    let block = encode_block(&ColumnData::Strings(values.clone().into()), false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::FsstDictionaryValues);
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::Strings(values.clone().into())
    );
    let ColumnData::Strings(ranged) = decode_block_range(block.pipeline, &block.bytes, 100, 250).unwrap() else {
        panic!("dictionary block must range-decode to strings");
    };
    assert_eq!(ranged, values[100..250].to_vec().into());
    let views = decode_string_block_views(block.pipeline, &block.bytes)
        .unwrap()
        .expect("dictionary blocks decode to views");
    for (row, expected) in values.iter().enumerate() {
        match expected {
            Some(text) => assert_eq!(views.value(row), text.as_str()),
            None => assert!(views.is_null(row)),
        }
    }
    // The alphabet is exposed in sorted order exactly as a plain dictionary block would expose it.
    let descriptor::PageDescriptor::Dictionary { entries } =
        descriptor::extract_descriptor(block.pipeline, &block.bytes).unwrap()
    else {
        panic!("dictionary block must describe its entries");
    };
    let mut sorted = distinct.clone();
    sorted.sort();
    assert_eq!(entries, sorted);
    // Code pushdown answers every predicate class without touching FSST bytes for the row stream.
    let filters = [
        predicate::StringPredicate::Equals(distinct[17].clone()),
        predicate::StringPredicate::NotEquals(distinct[17].clone()),
        predicate::StringPredicate::InSet(vec![distinct[3].clone(), distinct[150].clone(), "absent".to_owned()]),
        predicate::StringPredicate::Range {
            lower: Some(predicate::StringBound {
                inclusive: true,
                value: distinct[40].clone(),
            }),
            upper: Some(predicate::StringBound {
                inclusive: false,
                value: distinct[160].clone(),
            }),
        },
    ];
    for filter in filters {
        let mask = predicate::filter_string_block(block.pipeline, &block.bytes, &filter)
            .unwrap()
            .expect("dictionary blocks answer string predicates from codes");
        assert_eq!(mask, filter.filter_decoded(&values.clone().into()), "{filter:?}");
    }
}

#[test]
fn small_cheap_alphabets_keep_the_plain_dictionary_value_stream() {
    let values: Vec<Option<String>> = (0..600).map(|i| Some(format!("kind-{}", i % 8))).collect();
    let block = encode_block(&ColumnData::Strings(values.clone().into()), false);
    assert_eq!(block.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::None);
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::Strings(values.into())
    );
}

#[test]
fn a_replayed_block_runs_only_its_captured_trailing_codec() {
    // Structured, compressible u64s: after the FOR bitpack both trailing codecs shrink the body, so full selection
    // would run a two-codec trial and always land on the same winner. A capture pins the codec instead: whichever it
    // names is the one that runs — so identical input records three different trailing stages below, which a trial
    // could never produce.
    let values: Vec<u64> = (0..8192u64).map(|i| (i % 64) * 7).collect();
    let capture = |compression| ReplayCapture {
        compression,
        decoded_len: 8192 * 8,
        fsst: None,
        raw_len: 8192 * 8,
        transform: Transform::ForBitpack,
    };
    for captured in [Compression::Lz4, Compression::Zstd1, Compression::None] {
        let (block, replayed) = encode_block_replayed(
            &ColumnData::U64(values.clone()),
            false,
            CascadeStrategy::DecodeOptimized,
            Some(&capture(captured)),
            None,
        );
        assert!(replayed, "a generous capture head must keep the replay");
        assert_eq!(block.pipeline.compression().unwrap(), captured, "captured {captured:?}");
        assert_eq!(
            decode_block(block.pipeline, &block.bytes).unwrap(),
            ColumnData::U64(values.clone())
        );
    }
}

/// A dictionary block whose values all appear in the column's file-scope alphabet stores only its code stream: it
/// records the file scope, shrinks against a block-local dictionary, decodes only with the alphabet, and predicates
/// translate once per file and agree with the decode reference. A novel value keeps the local dictionary, and the
/// reserved external scope is rejected. Implements `hef-encodings-and-compression` — "Dictionary alphabets may be
/// shared at file scope".
#[test]
fn shared_scope_dictionary_blocks_store_codes_only_and_translate_once() {
    let alphabet: Vec<String> = ["blue", "green", "red", "yellow"].map(str::to_owned).to_vec();
    let values: Vec<Option<String>> = (0..600)
        .map(|i| (i % 7 != 0).then(|| alphabet[i % 4].clone()))
        .collect();
    let shared = encode_block_with_shared_dictionary(
        &ColumnData::Strings(values.clone().into()),
        false,
        CascadeStrategy::DecodeOptimized,
        Some(&alphabet),
    );
    assert_eq!(shared.pipeline.transform().unwrap(), Transform::DictionaryString);
    assert_eq!(shared.pipeline.side_stream().unwrap(), SideStream::FileScopeDictionary);
    let local = encode_block(&ColumnData::Strings(values.clone().into()), false);
    assert!(
        shared.bytes.len() < local.bytes.len(),
        "the shared scope must drop the value stream ({} vs {})",
        shared.bytes.len(),
        local.bytes.len()
    );

    assert!(
        decode_block(shared.pipeline, &shared.bytes).is_err(),
        "a shared-scope block must refuse to decode without its alphabet"
    );
    assert_eq!(
        decode_block_shared(shared.pipeline, &shared.bytes, Some(&alphabet)).unwrap(),
        ColumnData::Strings(values.clone().into())
    );
    assert_eq!(
        decode_block_range_shared(shared.pipeline, &shared.bytes, Some(&alphabet), 100, 130).unwrap(),
        ColumnData::Strings(values[100..130].to_vec().into())
    );
    let views = decode_string_block_views_shared(shared.pipeline, &shared.bytes, Some(&alphabet))
        .unwrap()
        .expect("dictionary blocks decode to views");
    let prepared = prepare_shared_string_view_dictionary(&alphabet).unwrap();
    let prepared_views =
        decode_string_block_views_shared_prepared(shared.pipeline, &shared.bytes, Some(&alphabet), Some(&prepared))
            .unwrap()
            .expect("dictionary blocks decode through the prepared alphabet");
    for (row, expected) in values.iter().enumerate() {
        match expected {
            Some(text) => {
                assert_eq!(views.value(row), text.as_str(), "row {row}");
                assert_eq!(prepared_views.value(row), text.as_str(), "prepared row {row}");
            }
            None => {
                assert!(views.is_null(row), "row {row}");
                assert!(prepared_views.is_null(row), "prepared row {row}");
            }
        }
    }

    let filters = [
        predicate::StringPredicate::Equals("green".to_owned()),
        predicate::StringPredicate::NotEquals("red".to_owned()),
        predicate::StringPredicate::Range {
            lower: Some(predicate::StringBound {
                inclusive: true,
                value: "blue".to_owned(),
            }),
            upper: Some(predicate::StringBound {
                inclusive: false,
                value: "red".to_owned(),
            }),
        },
    ];
    for filter in filters {
        let translated = predicate::translate_for_shared_dictionary(&filter, &alphabet);
        let mask = predicate::filter_string_block_shared(shared.pipeline, &shared.bytes, &filter, Some(&translated))
            .unwrap()
            .expect("shared-scope blocks answer from codes with the once-per-file translation");
        assert_eq!(mask, filter.filter_decoded(&values.clone().into()), "{filter:?}");
        assert!(
            predicate::filter_string_block(shared.pipeline, &shared.bytes, &filter)
                .unwrap()
                .is_none(),
            "without the translation the fast path declines rather than guessing"
        );
    }

    let mut novel = values.clone();
    novel[3] = Some("purple".to_owned());
    let block = encode_block_with_shared_dictionary(
        &ColumnData::Strings(novel.clone().into()),
        false,
        CascadeStrategy::DecodeOptimized,
        Some(&alphabet),
    );
    assert_ne!(
        block.pipeline.side_stream().unwrap(),
        SideStream::FileScopeDictionary,
        "a value outside the alphabet keeps a block-local dictionary"
    );
    assert_eq!(
        decode_block(block.pipeline, &block.bytes).unwrap(),
        ColumnData::Strings(novel.into())
    );

    let forged = PipelineId((shared.pipeline.0 & 0x00FF_FFFF) | (4 << 24));
    assert!(forged.side_stream().is_err(), "the external scope is reserved");
}

/// The scope decision is a per-block size trial, not a coverage rule: a block whose two distinct values sit at the
/// far ends of a 1024-entry alphabet would pay ten-bit codes at file scope, so its one-bit local dictionary wins —
/// while a block spanning the whole alphabet drops the value stream and takes the file scope.
#[test]
fn a_block_covering_a_corner_of_a_wide_alphabet_keeps_its_local_dictionary() {
    let alphabet: Vec<String> = (0..1024).map(|i| format!("v{i:04}")).collect();

    let corner: Vec<Option<String>> = (0..8192)
        .map(|i| Some(alphabet[if i % 2 == 0 { 0 } else { 1023 }].clone()))
        .collect();
    let block = encode_block_with_shared_dictionary(
        &ColumnData::Strings(corner.clone().into()),
        false,
        CascadeStrategy::DecodeOptimized,
        Some(&alphabet),
    );
    assert_ne!(
        block.pipeline.side_stream().unwrap(),
        SideStream::FileScopeDictionary,
        "wide-alphabet codes for a two-value block must lose the size trial"
    );
    assert_eq!(
        decode_block_shared(block.pipeline, &block.bytes, Some(&alphabet)).unwrap(),
        ColumnData::Strings(corner.into())
    );

    let narrow: Vec<String> = (0..64).map(|i| format!("v{i:04}")).collect();
    let spanning: Vec<Option<String>> = (0..8192).map(|i| Some(narrow[i % 64].clone())).collect();
    let block = encode_block_with_shared_dictionary(
        &ColumnData::Strings(spanning.clone().into()),
        false,
        CascadeStrategy::DecodeOptimized,
        Some(&narrow),
    );
    assert_eq!(
        block.pipeline.side_stream().unwrap(),
        SideStream::FileScopeDictionary,
        "a block spanning the alphabet drops its value stream and shares"
    );
    assert_eq!(
        decode_block_shared(block.pipeline, &block.bytes, Some(&narrow)).unwrap(),
        ColumnData::Strings(spanning.into())
    );
}

#[test]
fn a_block_shorter_than_one_fastlanes_vector_is_not_padded_to_one() {
    // A bit-packed stream is written as whole 1024-value FastLanes vectors, zero-padded, so a block holding a handful
    // of values occupies the same bytes as one holding a thousand. Columnar marks pages encode one such tiny array per
    // mark field, so a transform choice blind to that padding costs them hundreds of bytes — and, once whole-block
    // compression squashes the padding back down, a zstd decompress per field on every read.
    for count in [2usize, 3, 8, 64, 512] {
        let values: Vec<u64> = (0..count as u64).map(|i| i * 7 + 3).collect();
        let block = encode_block(&ColumnData::U64(values.clone()), true);
        let plain_bytes = 4 + count * 8;
        assert!(
            block.bytes.len() <= plain_bytes + 16,
            "a {count}-value block took {} bytes, more than the {plain_bytes} its values occupy written plainly: \
             the transform choice is paying for a whole padded vector",
            block.bytes.len()
        );
        assert_eq!(
            decode_block(block.pipeline, &block.bytes).unwrap(),
            ColumnData::U64(values),
            "a {count}-value block must round-trip whatever transform wins"
        );
    }
}

#[test]
fn a_trailing_codec_is_chosen_on_the_bytes_it_actually_stores() {
    // `pick_trailing` takes both candidates already in their final stored form: zstd behind the four-byte
    // uncompressed-length prefix `compress_zstd_framed` puts in front of it, LZ4 behind its own. Scored on the bare zstd
    // body, zstd would win here 45 to 47 — but it stores 49 bytes to LZ4's 47, and its decode pays a fixed setup
    // cost an LZ4 decode does not. Columnar marks pages are full of blocks in exactly this range.
    let body = vec![7u8; 68];
    let lz4 = vec![0u8; 47];
    let zstd_framed = vec![0u8; 45 + ZSTD_LENGTH_PREFIX_BYTES];
    let (compression, stored) = pick_trailing(body.clone(), 60, lz4.clone(), zstd_framed);
    assert_eq!(
        compression,
        Compression::Lz4,
        "the codec storing fewer bytes must win, prefix included"
    );
    assert_eq!(stored.len(), lz4.len());

    // Same comparison, far enough apart that zstd still wins once its prefix is counted.
    let zstd_framed = vec![0u8; 30 + ZSTD_LENGTH_PREFIX_BYTES];
    let (compression, stored) = pick_trailing(body.clone(), 60, vec![0u8; 47], zstd_framed.clone());
    assert_eq!(compression, Compression::Zstd1);
    assert_eq!(
        stored.len(),
        zstd_framed.len(),
        "the stored form is exactly the already-framed candidate, unchanged"
    );

    // A candidate clearing the bar only on its bare body must not be kept: 58 + 4 does not beat a threshold of 60.
    let zstd_framed = vec![0u8; 58 + ZSTD_LENGTH_PREFIX_BYTES];
    let (compression, stored) = pick_trailing(body.clone(), 60, vec![0u8; 61], zstd_framed);
    assert_eq!(
        compression,
        Compression::None,
        "a stage that only clears the savings bar by ignoring its own framing must be dropped"
    );
    assert_eq!(stored, body);
}

/// `compress_zstd_framed` sizes the stored block in one allocation rather than growing it as it frames — it must
/// still produce exactly the four-byte little-endian length prefix followed by the bare frame, and the framed output
/// must decompress back to the original bytes through the real (non-test) decompressor.
#[test]
fn compress_zstd_framed_matches_compress_then_frame_and_round_trips() {
    use super::decompressor::{Decompressor, SoftwareDecompressor};

    let corpus: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"tiny".to_vec(),
        vec![7u8; 5_000],
        (0..20_000u32).map(|i| (i % 251) as u8).collect(),
    ];
    for bytes in corpus {
        for level in [1, 3] {
            let framed = compress_zstd_framed(&bytes, level);

            let bare = compress_zstd(&bytes, level);
            let mut expected = Vec::with_capacity(ZSTD_LENGTH_PREFIX_BYTES + bare.len());
            expected.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            expected.extend_from_slice(&bare);
            assert_eq!(framed, expected, "level {level}, {} input bytes", bytes.len());

            let decompressed = SoftwareDecompressor.decompress(Compression::Zstd1, &framed).unwrap();
            assert_eq!(decompressed, bytes, "level {level} round trip");
        }
    }
}

/// The vectorized ALP probe must accept and reject exactly the values the scalar reference (`alp_try`) does, and
/// produce the same zigzag codes for the accepted ones — the probe decides the stored bytes, so any divergence would
/// break byte-identical encodes. The corpus leans on the traps: exact halves (where round-half-even and
/// round-half-away differ), signed zeros, non-finite values, subnormals, and magnitudes at the overflow gate.
#[test]
fn alp_probe_kernel_matches_the_scalar_reference() {
    let mut corpus: Vec<f64> = vec![
        0.0,
        -0.0,
        0.25,
        -0.25,
        0.5,
        -0.5,
        1.5,
        2.5,
        -2.5,
        0.049999999999999996,
        0.15,
        -0.15,
        f64::from_bits(0x3FDF_FFFF_FFFF_FFFF), // just under 0.5
        1.0 / 3.0,
        6.02214076e23,
        -6.62607015e-34,
        123.456,
        -9_876.543_21,
        8.9e18,
        -8.9e18,
        9.1e18,
        4.5e15,
        f64::MAX,
        f64::MIN,
        f64::MIN_POSITIVE,
        5e-324,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        (1u64 << 51) as f64,
        ((1u64 << 51) as f64) + 0.5,
        i64::MAX as f64,
    ];
    // A deterministic sweep of mixed-magnitude bit patterns rounds out the hand-picked traps.
    let mut state: u64 = 0x0DDB_1A5E_5BAD_5EED;
    for _ in 0..4096 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        corpus.push(f64::from_bits(state));
        corpus.push((state >> 40) as f64 / 100.0);
    }
    let mut zigzags = vec![0u64; corpus.len()];
    let mut encodable = vec![false; corpus.len()];
    for power in ALP_POWERS {
        alp_probe_chunk(&corpus, power, &mut zigzags, &mut encodable);
        for ((value, zig), ok) in corpus.iter().zip(&zigzags).zip(&encodable) {
            match alp_try(*value, power) {
                Some(int) => {
                    assert!(*ok, "kernel rejected {value:e} at power {power:e}, reference accepts");
                    assert_eq!(
                        *zig,
                        zigzag(int),
                        "kernel code diverges for {value:e} at power {power:e}"
                    );
                }
                None => {
                    assert!(!*ok, "kernel accepted {value:e} at power {power:e}, reference rejects");
                    assert_eq!(*zig, 0, "a rejected value must leave the masked zero code");
                }
            }
        }
    }
}

/// The branch-free integer→float conversion the narrow ALP reconstruction uses must equal the scalar `as f64`
/// conversion for every zigzag code a width-52 stream can hold — the widths above the bound take the scalar path, so
/// together the two paths cover every stream.
#[test]
fn alp_narrow_conversion_is_exact_across_the_width_bound() {
    let limit = mask(ALP_EXACT_CONVERT_MAX_WIDTH);
    let mut probes: Vec<u64> = vec![0, 1, 2, 3, limit, limit - 1, limit - 2, limit / 2, limit / 2 + 1];
    let mut state: u64 = 0xFEED_FACE_CAFE_BEEF;
    for _ in 0..4096 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        probes.push(state & limit);
    }
    for packed in probes {
        assert_eq!(
            alp_narrow_int_to_f64(packed),
            unzigzag(packed) as f64,
            "conversion diverges for packed {packed:#x} (int {})",
            unzigzag(packed)
        );
    }
}

/// An ALP block whose zigzag integers need more than the exact-conversion width must still decode through the scalar
/// conversion path bit for bit — huge scaled magnitudes are legal as long as they round-trip.
#[test]
fn alp_blocks_wider_than_the_exact_conversion_bound_still_round_trip() {
    // Multiples of 2^30 near 8e18 scale exactly at power 1e0 and need ~63 zigzag bits.
    let values: Vec<f64> = (0..1500u64)
        .map(|i| ((i << 30) as i64 - (750u64 << 30) as i64) as f64 * 8_192.0)
        .collect();
    let mut out = Writer::new();
    let side = encode_alp(&values, CascadeStrategy::DecodeOptimized, &mut out).expect("exact multiples encode");
    let bytes = out.into_bytes();
    let mut reader = Reader::new(&bytes);
    let decoded = decode_alp(&mut reader, side).unwrap();
    assert_eq!(decoded, values);
}

/// The ALP-RD split kernel's padded compare-select lookup must agree with a plain first-match linear search over the
/// real dictionary, including the miss sentinel, for every dictionary length up to the maximum.
#[test]
fn alp_rd_split_kernel_matches_a_linear_dictionary_search() {
    let corpus: Vec<f64> = (0..800)
        .map(|i| f64::from_bits(0x4037_0000_0000_0000u64 | (i as u64 * 0x0000_0421_8461_1077)))
        .collect();
    for right_width in [ALP_RD_MIN_RIGHT_WIDTH, 52, ALP_RD_MAX_RIGHT_WIDTH] {
        for dict_len in 0..=ALP_RD_MAX_DICT_LEN {
            // Draw the dictionary from the corpus's own left parts so hits and misses both occur.
            let mut dict: Vec<u16> = corpus
                .iter()
                .step_by(90)
                .take(dict_len)
                .map(|value| (value.to_bits() >> right_width) as u16)
                .collect();
            dict.dedup();
            let mut padded = [0u16; ALP_RD_MAX_DICT_LEN];
            for (slot, entry) in padded.iter_mut().zip(&dict) {
                *slot = *entry;
            }
            let mut codes = vec![0u64; corpus.len()];
            let mut rights = vec![0u64; corpus.len()];
            alp_rd_split_chunk(&corpus, right_width, &padded, dict.len(), &mut codes, &mut rights);
            for ((value, code), right) in corpus.iter().zip(&codes).zip(&rights) {
                let bits = value.to_bits();
                assert_eq!(*right, bits & mask(right_width));
                let left = (bits >> right_width) as u16;
                match dict.iter().position(|entry| *entry == left) {
                    Some(expected) => assert_eq!(*code, expected as u64),
                    None => assert_eq!(*code, ALP_RD_CODE_MISS),
                }
            }
        }
    }
}

#[test]
fn presence_rank_and_present_position_agree_with_a_prefix_count() {
    // Every third row present, over enough rows to cross several bitmap bytes.
    let rows: usize = 200;
    let mut bitmap = vec![0u8; rows.div_ceil(8)];
    for row in (0..rows).step_by(3) {
        bitmap[row / 8] |= 1 << (row % 8);
    }
    let rank = PresenceRank::new(&bitmap);
    for row in 0..rows {
        let expected = (row % 3 == 0).then(|| count_present_before(&bitmap, row));
        assert_eq!(rank.position(&bitmap, row), expected, "row {row}");
        assert_eq!(present_position(&bitmap, row), expected, "row {row}");
    }
    assert_eq!(rank.position(&bitmap, rows), None, "row past the bitmap");
    assert_eq!(present_position(&bitmap, rows), None, "row past the bitmap");
}
