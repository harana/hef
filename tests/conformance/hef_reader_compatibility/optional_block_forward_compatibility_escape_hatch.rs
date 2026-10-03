//! Checks the one narrow path that lets a reader read an optional block whose encoding it predates: a footer note
//! (`min_reader_version` plus a `portable_decoder_ref`) that points at a portable decoder the fleet trusts. When the
//! reader can satisfy the note it validates the block checksum and decodes through that decoder, admitting rows only
//! because they are byte-for-byte what a native decoder would produce; otherwise it skips the block and answers from a
//! plain scan. The path never applies to required features, and an uncertified decoder is never trusted.
use hef::compat::{
    EscapeHatch, OptionalBlockOutcome, OptionalBlockPlan, SimulatedDecoder, SimulatedDecoderFleet, encode_demo_block,
    native_reference_decode, read_optional_block_via_escape_hatch,
};
use hef::layout::footer::encode_footer;
use hef::layout::reader::HefFile;

use crate::support;
use hef::columns::REQUIRED_COLUMNS;
/// An optional feature bit no reader in this build knows natively.
const FUTURE_OPTIONAL: u64 = 1 << 60;

/// Swaps a file's footer for `blob`, fixing up the trailing footer length and magic — the same retailing helper the
/// other reader-compatibility tests use.
fn retail(bytes: &[u8], blob: &[u8]) -> Vec<u8> {
    let original_blob_len = {
        let tail = &bytes[bytes.len() - 12..bytes.len() - 4];
        u64::from_le_bytes(tail.try_into().unwrap()) as usize
    };
    let data_end = bytes.len() - 12 - original_blob_len;
    let mut out = bytes[..data_end].to_vec();
    out.extend_from_slice(blob);
    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    out.extend_from_slice(b"HEF1");
    out
}

/// conformance:
/// hef-reader-compatibility/optional-block-forward-compatibility-escape-hatch/
/// old-reader-reads-a-new-optional-encoding-via-a-portable-decoder
#[test]
fn old_reader_reads_a_new_optional_encoding_via_a_portable_decoder() {
    // The reader lacks native code for this optional encoding, but the footer names a min_reader_version it meets and a
    // portable_decoder_ref the fleet resolves to a conformance-passing decoder. It validates the block checksum,
    // decodes through that decoder, and admits the rows only because they are byte-for-byte identical to a native
    // decode.
    let block = encode_demo_block(&[100, 200, 300]);
    let checksum = *blake3::hash(&block).as_bytes();
    let hatch = EscapeHatch {
        min_reader_version: 2,
        optional_feature_bit: FUTURE_OPTIONAL,
        portable_decoder_ref: "fancy-codec-v2".to_owned(),
    };
    let fleet = SimulatedDecoderFleet::new().with_decoder("fancy-codec-v2", SimulatedDecoder::conformant(2));

    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, FUTURE_OPTIONAL, 2, Some(&hatch), &fleet).unwrap();
    assert_eq!(
        outcome,
        OptionalBlockOutcome::Decoded(native_reference_decode(&block).unwrap()),
        "decoded rows are byte-for-byte identical to a native decode"
    );

    // The checksum is validated before anything decodes: a single flipped byte is refused, never surfaced as misdecoded
    // output.
    let mut corrupted = block.clone();
    corrupted[0] ^= 0x01;
    assert!(
        read_optional_block_via_escape_hatch(&corrupted, &checksum, FUTURE_OPTIONAL, 2, Some(&hatch), &fleet).is_err()
    );
}

/// conformance:
/// hef-reader-compatibility/optional-block-forward-compatibility-escape-hatch/
/// old-reader-skips-the-optional-block-to-a-correct-scan
#[test]
fn old_reader_skips_the_optional_block_to_a_correct_scan() {
    // The file declares an optional block this reader cannot decode and a hatch it cannot satisfy: the reader version
    // is below min_reader_version and the fleet resolves nothing. The reader skips the block as if absent and still
    // answers from a plain scan, returning correct, complete results.
    let built = support::built_file(8);
    let mut footer = built.footer.clone();
    footer.optional_feature_flags |= FUTURE_OPTIONAL;
    footer.escape_hatches.push(EscapeHatch {
        min_reader_version: 9,
        optional_feature_bit: FUTURE_OPTIONAL,
        portable_decoder_ref: "future-codec".to_owned(),
    });
    let blob = encode_footer(&footer);
    let file = HefFile::open(retail(&built.bytes, &blob), None).unwrap();

    let fleet = SimulatedDecoderFleet::new();
    assert_eq!(
        file.optional_feature_plan(FUTURE_OPTIONAL, 1, &fleet),
        OptionalBlockPlan::Skip
    );
    // The unknown optional bit is not usable, and the scan path is intact: skipping an acceleration costs performance,
    // never correctness.
    assert_eq!(file.usable_optional_features() & FUTURE_OPTIONAL, 0);
    assert_eq!(file.header().row_count, 8);
    let granule = file.footer().granules[0].granule_id;
    for spec in REQUIRED_COLUMNS {
        file.read_column(spec.column_id, granule).unwrap();
    }
}

/// conformance:
/// hef-reader-compatibility/optional-block-forward-compatibility-escape-hatch/
/// escape-hatch-never-touches-required-features
#[test]
fn escape_hatch_never_touches_required_features() {
    // An unknown REQUIRED feature fails the file whether or not a portable_decoder_ref is present in the footer: the
    // escape hatch is for optional blocks only, and required features stay refuse.
    let built = support::built_file(8);

    // Unknown required feature, no escape hatch: fails.
    let mut footer = built.footer.clone();
    footer.required_feature_flags |= 1 << 62;
    let blob = encode_footer(&footer);
    assert!(HefFile::open(retail(&built.bytes, &blob), None).is_err());

    // Unknown required feature with an escape hatch present anywhere in the footer: still fails — the hatch cannot
    // rescue a required feature.
    let mut footer = built.footer.clone();
    footer.required_feature_flags |= 1 << 62;
    footer.escape_hatches.push(EscapeHatch {
        min_reader_version: 1,
        optional_feature_bit: FUTURE_OPTIONAL,
        portable_decoder_ref: "irrelevant".to_owned(),
    });
    let blob = encode_footer(&footer);
    assert!(HefFile::open(retail(&built.bytes, &blob), None).is_err());
}

/// conformance:
/// hef-reader-compatibility/optional-block-forward-compatibility-escape-hatch/
/// unconformant-portable-decoder-is-not-trusted
#[test]
fn unconformant_portable_decoder_is_not_trusted() {
    // The fleet resolves the ref, but to a decoder that has not passed the conformance suite and software-parity check.
    // The reader declines it and falls back to skipping the block — its output is never surfaced, even though the ref
    // resolves and the block checksum is valid.
    let block = encode_demo_block(&[1, 2, 3]);
    let checksum = *blake3::hash(&block).as_bytes();
    let hatch = EscapeHatch {
        min_reader_version: 1,
        optional_feature_bit: FUTURE_OPTIONAL,
        portable_decoder_ref: "unvetted".to_owned(),
    };
    let fleet = SimulatedDecoderFleet::new().with_decoder("unvetted", SimulatedDecoder::uncertified(1));

    let outcome =
        read_optional_block_via_escape_hatch(&block, &checksum, FUTURE_OPTIONAL, 1, Some(&hatch), &fleet).unwrap();
    assert_eq!(
        outcome,
        OptionalBlockOutcome::Skip,
        "an uncertified decoder is declined, not used"
    );

    // A file declaring that same hatch plans to skip the block, not decode it.
    let built = support::built_file(8);
    let mut footer = built.footer.clone();
    footer.optional_feature_flags |= FUTURE_OPTIONAL;
    footer.escape_hatches.push(hatch);
    let blob = encode_footer(&footer);
    let file = HefFile::open(retail(&built.bytes, &blob), None).unwrap();
    assert_eq!(
        file.optional_feature_plan(FUTURE_OPTIONAL, 1, &fleet),
        OptionalBlockPlan::Skip
    );
}
