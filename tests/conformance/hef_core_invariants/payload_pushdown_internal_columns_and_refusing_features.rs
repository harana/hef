//! Checks a group of core safety rules: a file that declares a required capability this reader does not understand is
//! refused outright rather than read partially, and a single field can be pulled out of an event body without decoding
//! the other fields beside it.
use crate::support;
use hef::compat::check_features;
use hef::error::FormatError;
use hef::events::variant::VariantValue;
use hef::layout::reader::HefFile;
use hef::layout::required_features;

/// conformance:
/// hef-core-invariants/payload-pushdown-internal-columns-and-refusing-features/unknown-required-feature-refuses
#[test]
fn unknown_required_feature_refuses() {
    // A required feature flag this reader does not understand refuses the file rather than serving partial or incorrect
    // data.
    let unknown = required_features::ALL | (1 << 59);
    assert!(matches!(
        check_features(unknown, 0),
        Err(FormatError::UnknownRequiredFeature { .. })
    ));
}

/// conformance:
/// hef-core-invariants/payload-pushdown-internal-columns-and-refusing-features/
/// single-payload-path-extracted-without-sibling-decode
#[test]
fn single_payload_path_extracted_without_sibling_decode() {
    // One unshredded payload path is read as the deterministic merge of shredded typed values and the residual value,
    // via offset-based navigation against the governing dictionary.
    let built = support::built_file(24);
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    // "rare" appears on every third row only: unshredded, residual-only.
    assert_eq!(file.payload_path(3, "rare").unwrap(), Some(VariantValue::Bool(true)));
    assert_eq!(file.payload_path(4, "rare").unwrap(), None);
    // "amount" passed the shred threshold: answered from the typed column.
    assert!(file.footer().shredded.iter().any(|entry| entry.path == "amount"));
    assert_eq!(file.payload_path(5, "amount").unwrap(), Some(VariantValue::Int(1_005)));
}
