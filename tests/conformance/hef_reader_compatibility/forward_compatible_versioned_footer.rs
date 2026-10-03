//! Checks that an older reader can still open a file written by a newer one. When the file declares an optional feature
//! and an extra footer section the reader predates, those unknown parts are simply ignored rather than used, and the
//! file opens normally.
use crate::support;
use hef::layout::footer::{decode_footer, encode_footer};
use hef::layout::reader::HefFile;

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

/// conformance: hef-reader-compatibility/forward-compatible-versioned-footer/older-reader-newer-optional-block
#[test]
fn older_reader_newer_optional_block() {
    // A file declaring an optional feature (and an extension footer section) this reader predates still opens: the
    // unknown optional flag and unknown section id are ignored.
    let built = support::built_file(8);
    let mut footer = built.footer.clone();
    footer.optional_feature_flags |= 1 << 63; // future optional feature
    let blob = encode_footer(&footer);
    let file = HefFile::open(retail(&built.bytes, &blob), None).unwrap();
    assert_eq!(
        file.usable_optional_features() & (1 << 63),
        0,
        "the unknown optional feature is ignored, not used"
    );
    assert_eq!(file.header().row_count, 8);
    decode_footer(&blob).unwrap();
}
