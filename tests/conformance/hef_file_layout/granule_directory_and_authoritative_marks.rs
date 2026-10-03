//! Checks that a file has a directory of fixed offsets so any chunk of any column can be found directly. Looking up one
//! (column, chunk) entry is a constant-time jump to the right place, and it decodes exactly that chunk's rows.
use crate::support;
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::layout::reader::HefFile;

/// conformance: hef-file-layout/granule-directory-and-authoritative-marks/random-access-via-marks
#[test]
fn random_access_via_marks() {
    // A specific (column, projection, granule) resolves through the marks directory — a constant-time offset lookup —
    // and decodes exactly that granule's rows.
    let built = support::built_file(48);
    let file = HefFile::open(built.bytes, None).unwrap();
    let granule = file.footer().granules.last().copied().unwrap();
    let mark = file
        .footer()
        .marks
        .iter()
        .find(|mark| {
            mark.column_id == column_ids::SEQUENCE && mark.projection_id == 0 && mark.granule_id == granule.granule_id
        })
        .expect("mark exists for (column, projection, granule)");
    // The mark offset is stripe-relative: it is measured from the base offset of its own stripe
    // (`StripeEntry.file_offset`), not the file start. Resolving `stripe_base + relative` lands the block past the 4 KiB
    // header and inside its stripe's byte range — the same file position the reader slices at.
    let stripe = file
        .footer()
        .stripes
        .iter()
        .find(|s| s.stripe_id == granule.stripe_id)
        .expect("stripe exists for granule");
    let absolute = stripe.file_offset + mark.compressed_offset;
    assert!(absolute >= 4096, "resolved block starts past the header");
    assert!(
        mark.compressed_offset + mark.compressed_size <= stripe.byte_len,
        "a stripe-relative mark stays inside its stripe, so relocating the stripe rewrites one base-offset entry",
    );
    assert_eq!(mark.row_count, granule.row_count);
    let read = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
    let ColumnData::U64(values) = read.data else {
        panic!("u64 column")
    };
    assert_eq!(values.len(), granule.row_count as usize);
    assert_eq!(values[0], granule.first_sequence);
}
