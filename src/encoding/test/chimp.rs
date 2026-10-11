use super::*;

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

#[test]
fn special_values_round_trip_bit_for_bit() {
    let quiet_nan_with_payload = f64::from_bits(0x7FF8_0000_DEAD_BEEF);
    let signalling_nan = f64::from_bits(0x7FF0_0000_0000_0001);
    let negative_nan = f64::from_bits(0xFFF8_0000_0000_0042);
    round_trip(&[
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
    ]);
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
    let mut writer = BitWriter::default();
    writer.put(1.0f64.to_bits(), 64);
    writer.put(0b10, 2);
    writer.put(0, 62);
    let stream = writer.finish();
    let mut reuse = Writer::new();
    reuse.put_u32(2);
    reuse.put_u32(stream.len() as u32);
    reuse.put_slice(&stream);
    assert_eq!(
        decode_chimp(&mut Reader::new(reuse.bytes())),
        Err(FormatError::Structural {
            rule: "chimp reuses a leading-zero count no value has set"
        })
    );
}
