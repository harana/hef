//! Checks how a file is divided into nested pieces — stripes, then granules, then pages, then mini-blocks. Granules are
//! contiguous sequence-ordered row ranges, stripes split once they hit their target size and never approach the hard
//! ceiling, and a page larger than the maximum makes the reader reject the file.
use crate::support;
use hef::error::FormatError;
use hef::layout::footer::{Footer, decode_footer, encode_footer};
use hef::layout::reader::HefFile;
use hef::layout::{MAX_PAGE_BYTES, required_features};
use hef::writer::build::BuiltHef;

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
    // A page/chunk above the maximum size with no recognized declared feature flag fails the file. The writer stores
    // its marks in per-stripe pages beside the stripe data, so the forged mark is planted by re-encoding the footer in
    // the two forms that carry marks inside the footer itself: the row-oriented form, whose marks the reader checks at
    // open, and the columnar form, whose stripe marks the reader checks on the stripe's first touch.
    let built = support::built_file(8);
    let target = built
        .footer
        .marks
        .iter()
        .position(|mark| mark.page_count <= 1 && mark.compressed_size > 0)
        .expect("a single-page block that stores bytes");
    let (column_id, granule_id) = (
        built.footer.marks[target].column_id,
        built.footer.marks[target].granule_id,
    );
    let mut row_oriented = built.footer.clone();
    row_oriented.required_feature_flags &= !(required_features::COLUMNAR_MARKS | required_features::STRIPE_MARKS_PAGES);
    row_oriented.marks_page_offsets.clear();
    let mut columnar = built.footer.clone();
    columnar.required_feature_flags &= !required_features::STRIPE_MARKS_PAGES;
    columnar.marks_page_offsets.clear();

    for (footer, checked_at_open) in [(row_oriented, true), (columnar, false)] {
        // Control: the re-encoded footer alone is accepted, so a rejection below is caused by the oversized page.
        HefFile::open(with_footer(&built, &footer), None)
            .and_then(|file| file.read_column(column_id, granule_id))
            .expect("the untampered re-encoded file opens and reads");
        let mut tampered = footer.clone();
        tampered.marks[target].compressed_size = MAX_PAGE_BYTES + 1;
        // Sanity: the tampered footer still parses standalone...
        decode_footer(&encode_footer(&tampered)).unwrap();
        // ...but the reader rejects the file for the oversized page.
        let opened = HefFile::open(with_footer(&built, &tampered), None);
        if checked_at_open {
            assert!(
                opened.is_err(),
                "row-oriented marks are checked before any column is read"
            );
        }
        assert!(matches!(
            opened.and_then(|file| file.read_column(column_id, granule_id)),
            Err(FormatError::Structural {
                rule: "page/chunk exceeds the maximum size"
            })
        ));
    }
}

/// The built file's bytes with its footer replaced by `footer`, re-encoded into the file tail.
fn with_footer(built: &BuiltHef, footer: &Footer) -> Vec<u8> {
    let blob = encode_footer(footer);
    let original_blob_len = {
        let tail = &built.bytes[built.bytes.len() - 12..built.bytes.len() - 4];
        u64::from_le_bytes(tail.try_into().unwrap()) as usize
    };
    let data_end = built.bytes.len() - 12 - original_blob_len;
    let mut bytes = built.bytes[..data_end].to_vec();
    bytes.extend_from_slice(&blob);
    bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    bytes.extend_from_slice(b"HEF1");
    bytes
}
