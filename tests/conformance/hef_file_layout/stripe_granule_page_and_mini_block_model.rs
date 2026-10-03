//! Checks how a file is divided into nested pieces — stripes, then granules, then pages, then mini-blocks. Granules are
//! contiguous sequence-ordered row ranges, stripes split once they hit their target size and never approach the hard
//! ceiling, and a page larger than the maximum makes the reader reject the file.
use crate::support;
use hef::layout::MAX_PAGE_BYTES;
use hef::layout::footer::{decode_footer, encode_footer};
use hef::layout::reader::HefFile;

/// conformance: hef-file-layout/stripe-granule-page-and-mini-block-model/stripe-exceeds-maximum
#[test]
fn stripe_exceeds_maximum() {
    // Stripe formation honours the targets: granules are contiguous sequence-ordered row ranges, stripes split at the
    // target, and no stripe approaches the 512 MiB clamp (the writer would close the file first; the clamp is a
    // backstop, not the roll trigger).
    let built = support::built_file(64);
    assert!(built.footer.stripes.len() > 1, "stripe target splits stripes");
    for stripe in &built.footer.stripes {
        assert!(stripe.byte_len < 512 * 1024 * 1024);
    }
    let mut previous_end: Option<u64> = None;
    for granule in &built.footer.granules {
        if let Some(end) = previous_end {
            assert_eq!(granule.first_sequence, end + 1, "contiguous sequence order");
        }
        previous_end = Some(granule.last_sequence);
    }
}

/// conformance: hef-file-layout/stripe-granule-page-and-mini-block-model/oversized-page-on-read
#[test]
fn oversized_page_on_read() {
    // A page/chunk above the maximum size with no recognized declared feature flag fails the file on open.
    let built = support::built_file(8);
    let mut footer = built.footer.clone();
    footer.marks[0].compressed_size = MAX_PAGE_BYTES + 1;
    // Re-encode the tampered footer into the file tail.
    let blob = encode_footer(&footer);
    let original_blob_len = {
        let tail = &built.bytes[built.bytes.len() - 12..built.bytes.len() - 4];
        u64::from_le_bytes(tail.try_into().unwrap()) as usize
    };
    let data_end = built.bytes.len() - 12 - original_blob_len;
    let mut tampered = built.bytes[..data_end].to_vec();
    tampered.extend_from_slice(&blob);
    tampered.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    tampered.extend_from_slice(b"HEF1");
    // Sanity: the tampered footer still parses standalone…
    decode_footer(&blob).unwrap();
    // …but the reader rejects the file for the oversized page.
    assert!(HefFile::open(tampered, None).is_err());
}
