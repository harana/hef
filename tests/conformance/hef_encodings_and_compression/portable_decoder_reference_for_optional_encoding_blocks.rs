//! Checks that a new encoding for an *optional* block can ship before every reader has native code for it: the writer
//! may advertise a minimum reader version and a fleet-resolvable portable decoder alongside the block, an older reader
//! resolves that decoder (or safely skips the block), and the advertisement never weakens the rule that a software
//! path must produce identical results.

use hef::compat::{
    EscapeHatch, OptionalBlockOutcome, OptionalBlockPlan, SimulatedDecoder, SimulatedDecoderFleet, encode_demo_block,
    native_reference_decode, plan_optional_block, read_optional_block_via_escape_hatch,
};

/// An optional feature bit no reader in this build knows natively — the stand-in for a new shredded encoding shipping
/// ahead of the fleet's native readers.
const FUTURE_OPTIONAL: u64 = 1 << 60;

/// conformance: hef-encodings-and-compression/portable-decoder-reference-for-optional-encoding-blocks/new-shredded-encoding-ships-ahead-of-native-readers
#[test]
fn new_shredded_encoding_ships_ahead_of_native_readers() {
    // The writer records a min_reader_version and a portable_decoder_ref for the optional block it wrote in the new
    // encoding.
    let block = encode_demo_block(&[7, 11, 13]);
    let checksum = *blake3::hash(&block).as_bytes();
    let hatch = EscapeHatch {
        min_reader_version: 2,
        optional_feature_bit: FUTURE_OPTIONAL,
        portable_decoder_ref: "shredded-codec-v2".to_owned(),
    };

    // An older reader without native code resolves the advertised decoder and reads the block through it.
    let fleet = SimulatedDecoderFleet::new().with_decoder("shredded-codec-v2", SimulatedDecoder::conformant(2));
    assert_eq!(
        plan_optional_block(FUTURE_OPTIONAL, 3, Some(&hatch), &fleet),
        OptionalBlockPlan::PortableDecode
    );
    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, FUTURE_OPTIONAL, 3, Some(&hatch), &fleet).unwrap();
    assert_eq!(
        outcome,
        OptionalBlockOutcome::Decoded(native_reference_decode(&block).unwrap())
    );

    // A reader that resolves neither the native encoding nor the decoder still skips the block to a correct scan —
    // the advertisement is advice, never a requirement.
    let empty_fleet = SimulatedDecoderFleet::new();
    assert_eq!(
        plan_optional_block(FUTURE_OPTIONAL, 3, Some(&hatch), &empty_fleet),
        OptionalBlockPlan::Skip
    );
    let skipped =
        read_optional_block_via_escape_hatch(&block, &checksum, FUTURE_OPTIONAL, 3, Some(&hatch), &empty_fleet)
            .unwrap();
    assert_eq!(skipped, OptionalBlockOutcome::Skip);
}

/// conformance: hef-encodings-and-compression/portable-decoder-reference-for-optional-encoding-blocks/portable-decoder-reference-does-not-weaken-software-parity
#[test]
fn portable_decoder_reference_does_not_weaken_software_parity() {
    let block = encode_demo_block(&[100, 200, 300, 400]);
    let checksum = *blake3::hash(&block).as_bytes();
    let hatch = EscapeHatch {
        min_reader_version: 2,
        optional_feature_bit: FUTURE_OPTIONAL,
        portable_decoder_ref: "shredded-codec-v2".to_owned(),
    };

    // The portable decoder's rows are byte-for-byte what the native software decode produces — advertising the
    // reference changes nothing about the decoded bytes.
    let fleet = SimulatedDecoderFleet::new().with_decoder("shredded-codec-v2", SimulatedDecoder::conformant(2));
    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, FUTURE_OPTIONAL, 3, Some(&hatch), &fleet).unwrap();
    let native = native_reference_decode(&block).unwrap();
    assert_eq!(outcome, OptionalBlockOutcome::Decoded(native));

    // A decoder the fleet has not certified against the conformance and software-parity gate is never trusted; the
    // reader skips to a scan rather than surface unverified rows.
    let uncertified = SimulatedDecoderFleet::new().with_decoder("shredded-codec-v2", SimulatedDecoder::uncertified(2));
    assert_eq!(
        plan_optional_block(FUTURE_OPTIONAL, 3, Some(&hatch), &uncertified),
        OptionalBlockPlan::Skip
    );

    // Required encodings never lean on the reference: a feature bit this reader knows natively is read natively, with
    // or without a hatch.
    let known_bit = 1; // bit 0 is inside the layout's KNOWN optional-feature set
    assert_eq!(
        plan_optional_block(known_bit, 3, Some(&hatch), &fleet),
        OptionalBlockPlan::Native
    );
    assert_eq!(
        plan_optional_block(known_bit, 3, None, &SimulatedDecoderFleet::new()),
        OptionalBlockPlan::Native
    );
}
