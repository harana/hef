//! Checks the batched payload read: one result per reference in the caller's order, identical to the per-row read;
//! the granule dictionary decoded once per granule per call rather than once per reference; and an absent payload
//! reported in its own position without failing the batch.

use crate::support;
use hef::artifacts::batch::PayloadInput;
use hef::layout::reader::{HefFile, PayloadRead, PayloadRef};
use hef::writer::build::{HefRow, build_hef_file};

/// A file spanning several granules (index granularity 16), with row 10 storing no payload at all.
fn file() -> HefFile {
    let mut rows: Vec<HefRow> = (0..48)
        .map(|i| HefRow {
            epoch: 1,
            sequence: i + 1,
            event: support::event(i),
        })
        .collect();
    rows[10].event.payload = PayloadInput::None;
    let built = build_hef_file(rows, &support::build_config()).unwrap();
    HefFile::open(built.bytes, None).unwrap()
}

/// conformance: hef-apis/batched-payload-read-amortizes-per-granule-work/batched-and-per-row-reads-agree-on-values-and-order
#[test]
fn batched_and_per_row_reads_agree_on_values_and_order() {
    let file = file();
    // Scrambled across granules, with a duplicate, so caller order and row order differ.
    let refs: Vec<PayloadRef> = [40, 3, 17, 47, 3, 33, 0, 25]
        .into_iter()
        .map(|row_ordinal| PayloadRef { row_ordinal })
        .collect();

    let batch = file.read_payloads(&refs).unwrap();
    assert_eq!(batch.payloads.len(), refs.len(), "one result per reference");
    for (reference, read) in refs.iter().zip(&batch.payloads) {
        assert_eq!(
            *read,
            file.payload(reference.row_ordinal).unwrap(),
            "row {} must match the per-row oracle in the caller's position",
            reference.row_ordinal
        );
        assert!(
            matches!(read, PayloadRead::Value(_)),
            "every referenced row here stores a payload"
        );
    }
}

/// conformance: hef-apis/batched-payload-read-amortizes-per-granule-work/one-granules-dictionary-is-decoded-once-per-call
#[test]
fn one_granules_dictionary_is_decoded_once_per_call() {
    let file = file();
    // Eight references confined to the first granule (rows 0..16 under index granularity 16).
    let refs: Vec<PayloadRef> = [1, 5, 3, 12, 7, 2, 15, 9]
        .into_iter()
        .map(|row_ordinal| PayloadRef { row_ordinal })
        .collect();

    let before = file.granule_dictionary_decodes();
    let batch = file.read_payloads(&refs).unwrap();
    assert_eq!(batch.payloads.len(), refs.len());
    assert_eq!(
        file.granule_dictionary_decodes() - before,
        1,
        "a batch confined to one granule decodes that granule's dictionary once for the call, not once per reference"
    );
}

/// conformance: hef-apis/batched-payload-read-amortizes-per-granule-work/an-absent-payload-reports-absent-in-place
#[test]
fn an_absent_payload_reports_absent_in_place() {
    let file = file();
    let refs = [
        PayloadRef { row_ordinal: 4 },
        PayloadRef { row_ordinal: 10 },
        PayloadRef { row_ordinal: 20 },
    ];

    let batch = file.read_payloads(&refs).unwrap();
    assert!(matches!(batch.payloads[0], PayloadRead::Value(_)));
    assert_eq!(
        batch.payloads[1],
        PayloadRead::None,
        "the row that stores no payload reports absent in its own position"
    );
    assert!(
        matches!(batch.payloads[2], PayloadRead::Value(_)),
        "an absent payload does not fail the rest of the batch"
    );
    // Absent-in-place is the same outcome the per-row read gives that row.
    assert_eq!(file.payload(10).unwrap(), PayloadRead::None);
}
