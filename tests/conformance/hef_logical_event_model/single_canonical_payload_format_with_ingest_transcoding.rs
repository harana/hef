//! Checks that incoming event bodies in various formats (such as JSON) are converted on the way in to one canonical
//! internal format, and the original bytes are discarded — only a short note recording the source format is kept.
//! Later, queries can be answered from the promoted columns, reading the body only for the rows that actually match.
use crate::support;
use hef::artifacts::batch::{PayloadInput, build_batch, decode_batch};
use hef::columns::column_ids;
use hef::encoding::ColumnData;
use hef::events::transcode::{SourceFormat, transcode};
use hef::events::variant::{VariantRef, VariantValue};
use hef::layout::reader::{HefFile, PayloadRead};

/// conformance:
/// hef-logical-event-model/single-canonical-payload-format-with-ingest-transcoding/
/// source-body-transcoded-source-bytes-discarded
#[test]
fn source_body_transcoded_source_bytes_discarded() {
    // A JSON body is transcoded into one canonical harana_variant_v1 value before HEJ append; the stored frame holds
    // only the variant value, and lineage is the internal source_schema_ref string.
    let body = br#"{"amount": 12, "kind": "won", "nested": {"x": [1, 2]}}"#;
    let value = transcode(SourceFormat::Json, body).unwrap();
    let mut input = support::event(0);
    input.payload = PayloadInput::Variant(value.clone());
    input.source_schema = Some("json:sha256:abc".to_owned());
    let payload = build_batch(std::slice::from_ref(&input), 1, 0).unwrap();
    // The source bytes do not survive ingest anywhere in the batch.
    assert!(
        !payload.windows(body.len()).any(|window| window == body),
        "source bytes must not be stored"
    );
    let decoded = decode_batch(&payload, 1).unwrap();
    let stored = decoded.events[0].payload.unwrap();
    let round = VariantRef::new(stored).decode(&decoded.dictionary).unwrap();
    assert_eq!(round, value);
    assert_eq!(decoded.events[0].source_schema, Some("json:sha256:abc"));
    // Raw/undecodable bodies become a single variant binary scalar.
    let raw = transcode(SourceFormat::RawBytes, &[0xFF, 0x00, 0x01]).unwrap();
    assert_eq!(raw, VariantValue::Binary(vec![0xFF, 0x00, 0x01]));
}

/// conformance:
/// hef-logical-event-model/single-canonical-payload-format-with-ingest-transcoding/
/// query-satisfiable-from-promoted-columns
#[test]
fn query_satisfiable_from_promoted_columns() {
    // Build a file where "amount" is automatically shredded — it appears in every event row, exceeding the presence
    // threshold.
    let built = support::built_file(32);
    assert!(
        built.footer.shredded.iter().any(|e| e.path == "amount"),
        "amount must be shredded — it is present in every event"
    );

    let file = HefFile::open(built.bytes, None).unwrap();

    // payload_path returns "amount" from the shredded typed column directly, taking the early-return path before any
    // payload arena bytes are read. The query is therefore answered from the promoted column alone.
    let val0 = file.payload_path(0, "amount").unwrap();
    assert_eq!(
        val0,
        Some(VariantValue::Int(1_000)),
        "row 0 amount from shredded column"
    );
    let val10 = file.payload_path(10, "amount").unwrap();
    assert_eq!(
        val10,
        Some(VariantValue::Int(1_010)),
        "row 10 amount from shredded column"
    );

    // A field that was not shredded ("rare" appears only on every third row) falls through to the payload arena —
    // proving the two paths are distinct.
    assert!(!built.footer.shredded.iter().any(|e| e.path == "rare"));
}

/// conformance:
/// hef-logical-event-model/single-canonical-payload-format-with-ingest-transcoding/
/// payload-read-only-for-final-matching-rows
#[test]
fn payload_read_only_for_final_matching_rows() {
    // Build a file with 32 events assigned sequences 1–32.
    let built = support::built_file(32);
    let file = HefFile::open(built.bytes, None).unwrap();

    // Step 1: coarse prune — skip granules whose sequence range cannot contain sequence 5. Payload is not touched here.
    let surviving = file.granules_for_sequence(1, 5, 5);
    assert!(!surviving.is_empty(), "sequence 5 must survive granule pruning");

    // Step 2: read the sequence column for each surviving granule to find the exact matching row ordinal. Still no
    // payload access.
    let mut matching_row: Option<u64> = None;
    for granule in &surviving {
        let col = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        let ColumnData::U64(sequences) = &col.data else {
            continue;
        };
        for (offset, &seq) in sequences.iter().enumerate() {
            if seq == 5 {
                matching_row = Some(granule.first_row_ordinal + offset as u64);
                break;
            }
        }
    }
    let row = matching_row.expect("sequence 5 must be found within a surviving granule");

    // Step 3: payload is read only for this one final matching row. Every other row in the granule is skipped entirely
    // — demonstrating late materialization: prune → filter → payload only for matches.
    let PayloadRead::Value(value) = file.payload(row).unwrap() else {
        panic!("payload must be present for the matching row");
    };
    let VariantValue::Object(fields) = value else {
        panic!("event payload must be an object");
    };
    // Sequence 5 is event(4): amount = 1_000 + 4 = 1_004.
    assert_eq!(
        fields.get("amount"),
        Some(&VariantValue::Int(1_004)),
        "payload read for the matching row must carry the correct amount"
    );
}
