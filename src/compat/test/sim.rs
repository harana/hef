use super::*;
use crate::compat::{PortableDecoder, PortableDecoderFleet};

#[test]
fn demo_block_round_trips_through_a_decoder() {
    let block = encode_demo_block(&[1, 2, 3, 4]);
    let decoder = SimulatedDecoder::conformant(1);
    let decoded = decoder.decode(&block).unwrap();
    assert_eq!(decoded, native_reference_decode(&block).unwrap());
    // The canonical form is little-endian, so decoding three known values lands exactly where a native reader would
    // read them.
    assert_eq!(&decoded[0..8], &1u64.to_le_bytes());
}

#[test]
fn empty_fleet_resolves_nothing() {
    let fleet = SimulatedDecoderFleet::new();
    assert!(fleet.resolve("demo").is_none());
}

#[test]
fn fleet_resolves_a_registered_decoder() {
    let fleet = SimulatedDecoderFleet::new().with_decoder("demo", SimulatedDecoder::conformant(2));
    let decoder = fleet.resolve("demo").unwrap();
    assert!(decoder.is_conformant_at(2));
    assert!(decoder.is_conformant_at(1));
}

#[test]
fn conformant_decoder_below_its_version_is_not_trusted() {
    let decoder = SimulatedDecoder::conformant(2);
    assert!(!decoder.is_conformant_at(3));
}

#[test]
fn uncertified_decoder_is_never_trusted() {
    let decoder = SimulatedDecoder::uncertified(9);
    assert!(!decoder.is_conformant_at(1));
}

#[test]
fn a_misaligned_block_refuses() {
    assert!(native_reference_decode(&[0, 1, 2]).is_err());
}
