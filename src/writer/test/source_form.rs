use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::matrix::tests::{federate, message_event, server_key};
use crate::events::matrix::{self, MatrixProvenance};
use crate::events::transcode::{SourceFormat, transcode};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::layout::reader::{HefFile, PayloadRead};
use crate::security::FooterEncryption;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, SharedStrings, build_hef_file};
use std::collections::BTreeMap;

/// A build config whose granules hold `rows_per_granule` rows, so a test spreads a few rows over several granules.
pub(crate) fn source_form_config(rows_per_granule: usize) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: FooterEncryption::Plaintext,
        freetext: FreetextDeclaration { fields: Vec::new() },
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan { columns: Vec::new() },
        targets: LayoutTargets {
            index_granularity: rows_per_granule,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 4096,
        },
        tenant_id: TenantId::new_test_id(7),
    }
}

/// Row `i` as the writer takes it, with `payload` as its canonical payload and no original form yet.
pub(crate) fn source_form_row(i: u64, payload: VariantValue) -> BuildRow {
    SharedStrings::default().build_row(HefRow {
        epoch: 1,
        event: EventInput {
            connector_delivery_hash_high: 0,
            connector_delivery_hash_low: 0,
            envelope: EventEnvelope {
                account_id: None,
                account_id_hash_low: 0,
                actor_id: None,
                actor_id_hash_low: 0,
                dedupe_hash_high: 0,
                dedupe_hash_low: i,
                entity_id: None,
                entity_id_hash_high: 0,
                entity_id_hash_low: i,
                entity_type: "room".to_owned(),
                event_id: EventId::new_test_id(0xA000 + u128::from(i)),
                event_type: "m.room.message".to_owned(),
                flags: EventFlags(0),
                ingested_at: TimestampValue::from_physical_nanos(2_000 + i as i64),
                occurred_at: TimestampValue::from_physical_nanos(1_000 + i as i64),
                schema_version: 1,
                source: "matrix".to_owned(),
                stream_id: StreamId(1),
                stream_sequence: i,
                tenant_id: TenantId::new_test_id(7),
                trace_id_hash_low: 0,
            },
            payload: PayloadInput::Variant(payload),
            provenance: None,
            relationships: None,
            source_delivery: None,
            source_schema: None,
        },
        sequence: i + 1,
    })
}

/// Row `i` carrying `raw` both as its original bytes and, transcoded, as its canonical payload.
pub(crate) fn json_row(i: u64, raw: &[u8]) -> BuildRow {
    let mut row = source_form_row(i, transcode(SourceFormat::Json, raw).unwrap());
    row.raw_payload = Some(raw.to_vec());
    row
}

/// A plain row whose payload is one small object.
pub(crate) fn plain_row(i: u64) -> BuildRow {
    source_form_row(
        i,
        VariantValue::Object(BTreeMap::from([("n".to_owned(), VariantValue::Int(i as i64))])),
    )
}

#[test]
fn canonical_json_pdus_round_trip_byte_for_byte() {
    let origin = server_key(11);
    let pdus: Vec<Vec<u8>> = vec![
        {
            let (raw, _) = federate("10", message_event(), &[("origin.example.org", "ed25519:a1", &origin)]);
            matrix::canonical_json(&raw).unwrap()
        },
        br#"{"content":{"body":"caf\u00e9 \ud83d\ude00 tab\tquote\"","big":9007199254740991,"small":-9007199254740991},"type":"m.room.message"}"#.to_vec(),
        // Not canonical: whitespace and key order are kept exactly as they arrived.
        b"{ \"type\" : \"m.room.message\",\n  \"content\" : { \"n\" : 9007199254740990 } }".to_vec(),
    ];
    let rows: Vec<BuildRow> = pdus
        .iter()
        .enumerate()
        .map(|(i, raw)| json_row(i as u64, raw))
        .collect();
    let built = build_hef_file(rows, &source_form_config(2)).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    for (row, raw) in pdus.iter().enumerate() {
        let stored = file.raw_payload(row as u64).unwrap().expect("the raw payload was kept");
        assert_eq!(&*stored, raw.as_slice(), "row {row} comes back byte for byte");
        // The canonical payload is still the shredded variant, built from the same bytes.
        assert!(matches!(
            file.payload(row as u64).unwrap(),
            PayloadRead::Value(VariantValue::Object(_))
        ));
    }
}

#[test]
fn a_federation_signed_pdu_re_verifies_from_the_sealed_file() {
    let origin = server_key(12);
    let relay = server_key(13);
    let mut rows = Vec::new();
    let mut ids = Vec::new();
    for i in 0..3u64 {
        let mut event = message_event();
        event["depth"] = serde_json::json!(100 + i);
        let (raw, signatures) = federate(
            "10",
            event,
            &[
                ("origin.example.org", "ed25519:a1", &origin),
                ("relay.example.net", "ed25519:b2", &relay),
            ],
        );
        let id = matrix::event_id("10", &raw).unwrap();
        let mut row = json_row(i, &raw);
        row.external_id = Some(id.as_bytes().to_vec());
        row.matrix_provenance = Some(Box::new(MatrixProvenance::new("10", signatures).unwrap()));
        rows.push(row);
        ids.push(id);
    }
    let built = build_hef_file(rows, &source_form_config(2)).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    for (row, id) in ids.iter().enumerate() {
        let row = row as u64;
        let raw = file.raw_payload(row).unwrap().expect("raw payload");
        let stored_id = file.external_id(row).unwrap().expect("external id");
        assert_eq!(stored_id, id.as_bytes());
        // Recomputing the reference hash from the stored bytes yields the stored event id.
        assert_eq!(matrix::event_id("10", &raw).unwrap().as_bytes(), stored_id.as_slice());
        let provenance = file.matrix_provenance(row).unwrap().expect("matrix provenance");
        assert_eq!(provenance.signatures().len(), 2);
        provenance.verify(&raw, &stored_id).unwrap();
    }
}

#[test]
fn a_stream_without_original_forms_pays_nothing() {
    let built = build_hef_file((0..4).map(plain_row).collect::<Vec<_>>(), &source_form_config(2)).unwrap();
    assert!(
        !built.footer.columns.iter().any(|column| SOURCE_FORM_COLUMNS
            .iter()
            .any(|spec| spec.column_id == column.column_id)),
        "no source-form column is declared"
    );
    assert!(built.footer.external_ids.is_empty());
    assert_eq!(built.footer.reference_filters, None);
    let file = HefFile::open(built.bytes, None).unwrap();
    assert_eq!(file.raw_payload(0).unwrap(), None);
    assert_eq!(file.row_by_external_id(b"$anything").unwrap(), None);
}

#[test]
fn malformed_original_forms_are_refused_at_build() {
    let mut binary = plain_row(0);
    binary.raw_payload = Some(vec![0xff, 0xfe]);
    assert_eq!(
        build_hef_file(vec![binary], &source_form_config(2)).err(),
        Some(FormatError::InvalidUtf8 { what: "raw payload" })
    );
    let mut too_long = plain_row(0);
    too_long.external_id = Some(vec![b'x'; 256]);
    assert!(build_hef_file(vec![too_long], &source_form_config(2)).is_err());
}

#[test]
fn the_external_id_index_is_sorted_and_points_at_rows() {
    let mut rows: Vec<BuildRow> = (0..6).map(plain_row).collect();
    for (i, row) in rows.iter_mut().enumerate() {
        if i != 3 {
            row.external_id = Some(format!("$event-{i}").into_bytes());
        }
    }
    let built = build_hef_file(rows, &source_form_config(2)).unwrap();
    let index = &built.footer.external_ids;
    assert_eq!(index.len(), 5);
    assert!(
        index
            .windows(2)
            .all(|pair| (pair[0].id_hash, pair[0].row_ordinal) <= (pair[1].id_hash, pair[1].row_ordinal))
    );
    assert!(index.iter().all(|entry| entry.row_ordinal != 3));
}
