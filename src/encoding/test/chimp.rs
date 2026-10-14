use super::*;
use crate::encoding::retain_range;

fn round_trip(values: &[f64]) -> Vec<u8> {
    let mut out = Writer::new();
    encode_chimp(values, &mut out);
    let bytes = out.into_bytes();
    let mut reader = Reader::new(&bytes);
    let decoded = decode_chimp(&mut reader).unwrap();
    assert_eq!(reader.remaining(), 0);
    let expected: Vec<u64> = values.iter().map(|value| value.to_bits()).collect();
    let actual: Vec<u64> = decoded.iter().map(|value| value.to_bits()).collect();
    assert_eq!(actual, expected);
    bytes
}

/// Deterministic pseudo-random bits.
struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Signed zeros, infinities, NaN payloads and the extremes, with a held reading among them.
fn special_values() -> Vec<f64> {
    let quiet_nan_with_payload = f64::from_bits(0x7FF8_0000_DEAD_BEEF);
    let signalling_nan = f64::from_bits(0x7FF0_0000_0000_0001);
    let negative_nan = f64::from_bits(0xFFF8_0000_0000_0042);
    vec![
        0.0,
        -0.0,
        0.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        quiet_nan_with_payload,
        signalling_nan,
        negative_nan,
        negative_nan,
        f64::MIN_POSITIVE,
        f64::from_bits(1),
        f64::MAX,
        f64::MIN,
        f64::EPSILON,
        -0.0,
        1.0,
    ]
}

/// `count` floats with uniformly random bit patterns, the same ones on every run.
fn random_values(count: usize) -> Vec<f64> {
    let mut rng = XorShift(0x5EED_CAFE_F00D_0001);
    (0..count).map(|_| f64::from_bits(rng.next())).collect()
}

fn bits_of(values: &[f64]) -> Vec<u64> {
    values.iter().map(|value| value.to_bits()).collect()
}

/// FNV-1a over `bytes`: a compact fingerprint to pin a long encoding against.
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xCBF2_9CE4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01B3)
    })
}

/// Packs fields one bit at a time: the plainest statement of most-significant-first packing with a zero-padded last
/// byte.
fn pack_bit_by_bit(fields: &[(u64, u32)]) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut used = 0usize;
    for &(value, width) in fields {
        for bit in (0..width).rev() {
            if used.is_multiple_of(8) {
                bytes.push(0);
            }
            if (value >> bit) & 1 == 1 {
                *bytes.last_mut().unwrap() |= 0x80 >> (used % 8);
            }
            used += 1;
        }
    }
    bytes
}

/// A Chimp block of `count` values around the stream `write` packs, its stream length filled in to match.
fn block(count: u32, write: impl FnOnce(&mut BitWriter<'_>)) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u32(count);
    out.put_u32(0);
    let mut bits = BitWriter::new(&mut out);
    write(&mut bits);
    bits.finish();
    let stream_len = out.len() - 8;
    out.patch_u32(4, stream_len as u32);
    out.into_bytes()
}

#[test]
fn special_values_round_trip_bit_for_bit() {
    round_trip(&special_values());
}

#[test]
fn empty_single_and_repeated_blocks_round_trip() {
    assert_eq!(round_trip(&[]).len(), 8);
    // One value: the counts, then its 64 bits.
    assert_eq!(round_trip(&[42.5]).len(), 8 + 8);
    // A held reading costs two bits per repeat.
    let held = round_trip(&[17.25; 1001]);
    assert_eq!(held.len(), 8 + 8 + 1000 * 2 / 8);
}

#[test]
fn arbitrary_bit_patterns_round_trip() {
    let mut rng = XorShift(0x5EED_CAFE_F00D_0001);
    let random: Vec<f64> = (0..5000).map(|_| f64::from_bits(rng.next())).collect();
    round_trip(&random);
    // Neighbours that share their top bits, their low bits, or both, so every control code is exercised.
    let mut walk = Vec::new();
    let mut bits = 0x4049_0000_0000_0000u64;
    for step in 0..5000u64 {
        bits ^= match step % 5 {
            0 => 0,
            1 => rng.next() & 0xFF,
            2 => (rng.next() & 0xFFFF) << 20,
            3 => rng.next() & 0x000F_FFFF_FFFF_FFFF,
            _ => rng.next() >> 1,
        };
        walk.push(f64::from_bits(bits));
    }
    round_trip(&walk);
}

#[test]
fn every_leading_zero_count_rounds_to_a_code_that_never_overstates_it() {
    for leading in 0..64 {
        let (code, rounded) = round_leading_zeros(leading);
        assert!(rounded <= leading);
        assert_eq!(leading_zeros_for(code), rounded);
        assert!(code < 8);
    }
}

#[test]
fn malformed_streams_are_refused() {
    let mut out = Writer::new();
    encode_chimp(&[1.0, 1.5, 1.5, 2.0, -3.25], &mut out);
    let bytes = out.into_bytes();
    // Every truncation fails.
    for cut in 0..bytes.len() {
        assert!(decode_chimp(&mut Reader::new(&bytes[..cut])).is_err());
    }
    // A count the stream cannot hold is refused before decoding.
    let mut forged = bytes.clone();
    forged[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        decode_chimp(&mut Reader::new(&forged)),
        Err(FormatError::Structural {
            rule: "chimp value count beyond what the stream can hold"
        })
    );
    // An extra byte after the last value is refused.
    let mut padded = bytes[..8].to_vec();
    let stream_len = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) + 1;
    padded[4..8].copy_from_slice(&stream_len.to_le_bytes());
    padded.extend_from_slice(&bytes[8..]);
    padded.push(0);
    assert!(decode_chimp(&mut Reader::new(&padded)).is_err());
    // A `10` control code with no leading-zero count to reuse is refused.
    let reuse = block(2, |bits| {
        bits.put(1.0f64.to_bits(), 64);
        bits.put(0b10, 2);
        bits.put(0, 62);
    });
    assert_eq!(
        decode_chimp(&mut Reader::new(&reuse)),
        Err(FormatError::Structural {
            rule: "chimp reuses a leading-zero count no value has set"
        })
    );
}

#[test]
fn bit_writer_packs_after_the_existing_bytes_exactly_as_a_bit_by_bit_packer() {
    let mut rng = XorShift(0x0BAD_5EED_0000_0007);
    // Unmasked values: bits above each width must be dropped. Widths 0 to 64 cross byte boundaries at every offset.
    let fields: Vec<(u64, u32)> = (0..2000).map(|_| (rng.next(), (rng.next() % 65) as u32)).collect();
    // Short prefixes of the fields end at every bit offset within the last byte.
    for take in (0..=40).chain([fields.len()]) {
        let fields = &fields[..take];
        let mut expected = b"prefix".to_vec();
        expected.extend(pack_bit_by_bit(fields));
        // Without enough room the destination grows; with plenty it is written in place.
        for capacity in [0, 1 << 16] {
            let mut out = Writer::with_capacity(capacity);
            out.put_slice(b"prefix");
            let mut bits = BitWriter::new(&mut out);
            for &(value, width) in fields {
                bits.put(value, width);
            }
            bits.finish();
            assert_eq!(out.bytes(), expected.as_slice());
        }
    }
}

#[test]
fn encoding_is_byte_for_byte_what_the_format_has_always_stored() {
    // Pinned from the encoder as it stood before it wrote straight into the destination.
    let encode = |values: &[f64]| {
        let mut out = Writer::new();
        encode_chimp(values, &mut out);
        out.into_bytes()
    };
    assert_eq!(encode(&[]), [0u8; 8]);
    assert_eq!(encode(&[42.5]), [1u8, 0, 0, 0, 8, 0, 0, 0, 64, 69, 64, 0, 0, 0, 0, 0]);
    assert_eq!(
        encode(&[1.0, 1.5, 1.5, 2.0, -3.25]),
        [
            5u8, 0, 0, 0, 16, 0, 0, 0, 63, 240, 0, 0, 0, 0, 0, 0, 80, 49, 6, 191, 253, 7, 192, 5
        ]
    );
    // Control codes `00`, `01` and `11`, and a partly filled last byte.
    assert_eq!(
        encode(&special_values()),
        [
            16u8, 0, 0, 0, 81, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 64, 52, 3, 65, 143, 254, 128, 120, 128, 8, 0, 0, 222,
            173, 190, 239, 212, 0, 0, 111, 86, 223, 119, 98, 0, 32, 0, 0, 0, 0, 1, 12, 199, 255, 64, 0, 0, 0, 0, 2, 22,
            68, 0, 0, 0, 0, 0, 0, 112, 255, 223, 255, 255, 255, 255, 255, 252, 128, 120, 195, 95, 255, 255, 255, 255,
            255, 255, 65, 151, 150, 131, 47, 252
        ]
    );
    // Control codes `01`, `10` and `11` over thousands of byte-boundary crossings.
    let random = encode(&random_values(5000));
    assert_eq!((random.len(), fingerprint(&random)), (41269, 0x7853_9933_984F_918E));
}

#[test]
fn encoding_after_existing_bytes_leaves_them_alone_and_fills_in_only_its_own_length() {
    let values = special_values();
    let mut alone = Writer::new();
    encode_chimp(&values, &mut alone);
    for capacity in [0, 4096] {
        let mut out = Writer::with_capacity(capacity);
        // Shaped like another block's header, so a length written to the wrong place would show.
        out.put_u32(u32::MAX);
        out.put_u32(7);
        encode_chimp(&values, &mut out);
        let (prefix, own) = out.bytes().split_at(8);
        assert_eq!(prefix, [255u8, 255, 255, 255, 7, 0, 0, 0]);
        assert_eq!(own, alone.bytes());
    }
}

#[test]
fn a_range_decode_keeps_exactly_the_full_decode_slice() {
    // Special values bring control codes `00`, `01` and `11`; random bits bring `10`.
    let mut values = special_values();
    values.extend(random_values(500));
    let mut out = Writer::new();
    encode_chimp(&values, &mut out);
    let bytes = out.into_bytes();
    let full = decode_chimp(&mut Reader::new(&bytes)).unwrap();
    let count = values.len();
    for (start, end) in [
        (0, 1),
        (count / 2, count / 2 + 1),
        (count - 1, count),
        (3, 20),
        (0, count),
        (count - 5, count + 100),
        (count, count + 1),
        (count + 10, usize::MAX),
        (7, 7),
        (9, 4),
    ] {
        let mut reader = Reader::new(&bytes);
        let range = decode_chimp_range(&mut reader, start, end).unwrap();
        assert_eq!(reader.remaining(), 0);
        let expected = retain_range(full.clone(), start, end);
        assert_eq!(bits_of(&range), bits_of(&expected), "rows {start}..{end}");
        // Room for the rows returned, not for the whole block.
        assert_eq!(range.capacity(), expected.len(), "rows {start}..{end}");
    }
}

#[test]
fn a_range_decode_refuses_every_malformed_stream_the_full_decode_refuses() {
    let mut out = Writer::new();
    encode_chimp(&[1.0, 1.5, 1.5, 2.0, -3.25], &mut out);
    let bytes = out.into_bytes();
    let mut malformed: Vec<Vec<u8>> = (0..bytes.len()).map(|cut| bytes[..cut].to_vec()).collect();
    let mut forged = bytes.clone();
    forged[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    malformed.push(forged);
    // Each fault below sits after two good values, so a range over those values still has to refuse it.
    let one = 1.0f64.to_bits();
    malformed.push(block(3, |bits| {
        bits.put(one, 64);
        bits.put(0b00, 2);
        bits.put(0b10, 2);
        bits.put(0, 62);
    }));
    malformed.push(block(3, |bits| {
        bits.put(one, 64);
        bits.put(0b00, 2);
        bits.put(0b01, 2);
        bits.put(0, 3);
        bits.put(0, 6);
    }));
    malformed.push(block(3, |bits| {
        bits.put(one, 64);
        bits.put(0b00, 2);
        bits.put(0b00, 2);
        bits.put(0, 8);
    }));
    let mut set_padding = block(2, |bits| {
        bits.put(one, 64);
        bits.put(0b00, 2);
    });
    *set_padding.last_mut().unwrap() |= 1;
    malformed.push(set_padding);
    for fixture in &malformed {
        let full = decode_chimp(&mut Reader::new(fixture));
        assert!(full.is_err());
        for (start, end) in [(0, 1), (0, 2), (1, 2), (2, 3), (4, 5), (10, 20), (0, usize::MAX)] {
            assert_eq!(decode_chimp_range(&mut Reader::new(fixture), start, end), full);
        }
    }
}
