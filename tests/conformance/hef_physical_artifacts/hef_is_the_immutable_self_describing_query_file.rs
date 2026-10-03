//! Checks that the on-disk query file describes itself: a reader can open it and list the features in its footer
//! without outside metadata, and there is no separate "lifecycle label" knob to get out of sync. (Choosing which file
//! to read for a query is covered by the query-execution work, so that part is only stubbed here.)

use crate::support;
use hef::layout::footer::encode_footer;
use hef::layout::optional_features;
use hef::layout::reader::HefFile;

/// Rewrites the trailing `[footer blob][footer_len u64]["HEF1"]` of a built file with a re-encoded footer, so a test
/// can open a file whose footer was mutated after the writer produced it.
fn with_footer(bytes: &[u8], blob: &[u8]) -> Vec<u8> {
    let original_len = {
        let tail = &bytes[bytes.len() - 12..bytes.len() - 4];
        u64::from_le_bytes(tail.try_into().unwrap()) as usize
    };
    let data_end = bytes.len() - 12 - original_len;
    let mut out = bytes[..data_end].to_vec();
    out.extend_from_slice(blob);
    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    out.extend_from_slice(b"HEF1");
    out
}

/// conformance:
/// hef-physical-artifacts/hef-is-the-immutable-self-describing-query-file/planner-inspects-feature-directory
#[test]
fn planner_inspects_feature_directory() {
    // stub (partial): the file is self-describing — a reader determines which optional blocks it may use purely from
    // the footer's declared feature flags, never from a separate lifecycle label — but "the planner selects an HEF
    // file" is query territory (D6).
    let built = support::built_file(20);

    let mut without_per_page = built.footer.clone();
    without_per_page.optional_feature_flags &= !optional_features::PER_PAGE_MARKS;
    let opened = HefFile::open(with_footer(&built.bytes, &encode_footer(&without_per_page)), None).unwrap();
    assert_eq!(opened.usable_optional_features() & optional_features::PER_PAGE_MARKS, 0);

    let mut with_per_page = built.footer.clone();
    with_per_page.optional_feature_flags |= optional_features::PER_PAGE_MARKS;
    let opened = HefFile::open(with_footer(&built.bytes, &encode_footer(&with_per_page)), None).unwrap();
    assert_ne!(opened.usable_optional_features() & optional_features::PER_PAGE_MARKS, 0);

    // `HefFile::open` takes only the file's bytes and an optional whole-file checksum — never a catalog lifecycle
    // label such as `hef::lifecycle::PartState` — so which optional blocks a reader may use cannot depend on
    // one; it is determined entirely by what the footer itself declares.
}
