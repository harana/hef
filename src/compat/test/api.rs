use super::*;
use crate::compat::sim::{SimulatedDecoder, SimulatedDecoderFleet, encode_demo_block, native_reference_decode};
use crate::error::FormatError;
use crate::layout::optional_features;

/// An optional feature bit this build does not know natively.
const UNKNOWN_OPTIONAL: u64 = 1 << 60;

fn hatch() -> EscapeHatch {
    EscapeHatch {
        min_reader_version: 3,
        optional_feature_bit: UNKNOWN_OPTIONAL,
        portable_decoder_ref: "demo".to_owned(),
    }
}

fn trusting_fleet() -> SimulatedDecoderFleet {
    SimulatedDecoderFleet::new().with_decoder("demo", SimulatedDecoder::conformant(3))
}

#[test]
fn native_feature_is_planned_native() {
    let fleet = SimulatedDecoderFleet::new();
    let plan = plan_optional_block(optional_features::VARIANT_SHREDDED_FIELD_BLOCKS, 3, None, &fleet);
    assert_eq!(plan, OptionalBlockPlan::Native);
}

#[test]
fn unknown_optional_without_hatch_is_skipped() {
    let fleet = trusting_fleet();
    let plan = plan_optional_block(UNKNOWN_OPTIONAL, 3, None, &fleet);
    assert_eq!(plan, OptionalBlockPlan::Skip);
}

#[test]
fn unknown_optional_with_usable_hatch_is_portable_decode() {
    let plan = plan_optional_block(UNKNOWN_OPTIONAL, 3, Some(&hatch()), &trusting_fleet());
    assert_eq!(plan, OptionalBlockPlan::PortableDecode);
}

#[test]
fn reader_below_min_version_skips() {
    let plan = plan_optional_block(UNKNOWN_OPTIONAL, 2, Some(&hatch()), &trusting_fleet());
    assert_eq!(plan, OptionalBlockPlan::Skip);
}

#[test]
fn unresolvable_decoder_skips() {
    let empty = SimulatedDecoderFleet::new();
    let plan = plan_optional_block(UNKNOWN_OPTIONAL, 3, Some(&hatch()), &empty);
    assert_eq!(plan, OptionalBlockPlan::Skip);
}

#[test]
fn uncertified_decoder_skips() {
    let fleet = SimulatedDecoderFleet::new().with_decoder("demo", SimulatedDecoder::uncertified(3));
    let plan = plan_optional_block(UNKNOWN_OPTIONAL, 3, Some(&hatch()), &fleet);
    assert_eq!(plan, OptionalBlockPlan::Skip);
}

#[test]
fn read_decodes_byte_for_byte_with_native() {
    let block = encode_demo_block(&[10, 20, 30]);
    let checksum = *blake3::hash(&block).as_bytes();
    let outcome = read_optional_block_via_escape_hatch(
        &block,
        &checksum,
        UNKNOWN_OPTIONAL,
        3,
        Some(&hatch()),
        &trusting_fleet(),
    )
    .unwrap();
    assert_eq!(
        outcome,
        OptionalBlockOutcome::Decoded(native_reference_decode(&block).unwrap())
    );
}

#[test]
fn read_without_usable_decoder_skips() {
    let block = encode_demo_block(&[1]);
    let checksum = *blake3::hash(&block).as_bytes();
    let empty = SimulatedDecoderFleet::new();
    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, UNKNOWN_OPTIONAL, 3, Some(&hatch()), &empty).unwrap();
    assert_eq!(outcome, OptionalBlockOutcome::Skip);
}

#[test]
fn read_rejects_a_corrupted_block_before_decoding() {
    let mut block = encode_demo_block(&[1, 2]);
    let checksum = *blake3::hash(&block).as_bytes();
    block[0] ^= 0x01;
    let error = read_optional_block_via_escape_hatch(
        &block,
        &checksum,
        UNKNOWN_OPTIONAL,
        3,
        Some(&hatch()),
        &trusting_fleet(),
    )
    .unwrap_err();
    assert_eq!(
        error,
        FormatError::Blake3Mismatch {
            scope: "optional block"
        }
    );
}

#[test]
fn uncertified_decoder_is_never_invoked_even_with_valid_checksum() {
    // A resolvable but uncertified decoder must not be trusted: the outcome is a clean skip, never its (here,
    // identical) bytes.
    let block = encode_demo_block(&[7]);
    let checksum = *blake3::hash(&block).as_bytes();
    let fleet = SimulatedDecoderFleet::new().with_decoder("demo", SimulatedDecoder::uncertified(3));
    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, UNKNOWN_OPTIONAL, 3, Some(&hatch()), &fleet).unwrap();
    assert_eq!(outcome, OptionalBlockOutcome::Skip);
}

#[test]
fn a_hatch_for_another_block_is_refused() {
    // The hatch describes feature bit UNKNOWN_OPTIONAL, but this block declares a different optional feature bit. The
    // hatch must not authorize decoding a block it was not written for: both entry points fall back to a skip.
    let other_bit = 1 << 59;
    let block = encode_demo_block(&[4, 5]);
    let checksum = *blake3::hash(&block).as_bytes();

    assert_eq!(
        plan_optional_block(other_bit, 3, Some(&hatch()), &trusting_fleet()),
        OptionalBlockPlan::Skip,
        "a hatch bound to a different feature bit must not plan a portable decode"
    );
    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, other_bit, 3, Some(&hatch()), &trusting_fleet())
            .unwrap();
    assert_eq!(
        outcome,
        OptionalBlockOutcome::Skip,
        "reading a block through a hatch written for a different block must be refused"
    );
}

#[test]
fn a_matching_hatch_is_honored() {
    // The hatch's optional_feature_bit equals the block's feature bit, so the matching hatch is honored end to end.
    let block = encode_demo_block(&[6, 7]);
    let checksum = *blake3::hash(&block).as_bytes();

    assert_eq!(
        plan_optional_block(UNKNOWN_OPTIONAL, 3, Some(&hatch()), &trusting_fleet()),
        OptionalBlockPlan::PortableDecode
    );
    let outcome = read_optional_block_via_escape_hatch(
        &block,
        &checksum,
        UNKNOWN_OPTIONAL,
        3,
        Some(&hatch()),
        &trusting_fleet(),
    )
    .unwrap();
    assert_eq!(
        outcome,
        OptionalBlockOutcome::Decoded(native_reference_decode(&block).unwrap())
    );
}
