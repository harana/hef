use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};

/// One row with a free-text note on every row but the fifth, an integer, and a low-cardinality string, so the file
/// carries string columns of both the view-decodable and the materializing kind beside the numeric envelope.
fn row(i: u64) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    if !i.is_multiple_of(5) {
        payload.insert(
            "note".to_owned(),
            VariantValue::String(format!(
                "row {i} carries a note of its own, {}",
                "x".repeat((i % 7) as usize)
            )),
        );
    }
    payload.insert("amount".to_owned(), VariantValue::Int(i as i64 * 37 - 500));
    payload.insert(
        "region".to_owned(),
        VariantValue::String(["north", "south", "east", "west"][(i % 4) as usize].to_owned()),
    );
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0x5CA7_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(3),
                stream_id: StreamId(1),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000 + i as i64 * 3),
                ingested_at: TimestampValue::from_physical_nanos(2_000 + i as i64 * 3),
                source: "crm".to_owned(),
                event_type: if i % 3 == 0 { "deal.updated" } else { "deal.created" }.to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i * 7919,
                entity_id_hash_high: 1,
                entity_id: None,
                actor_id_hash_low: 3,
                actor_id: None,
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: i,
                dedupe_hash_high: 6,
                schema_version: 1,
                flags: EventFlags(0),
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            source_schema: None,
            source_delivery: None,
            connector_delivery_hash_low: i,
            connector_delivery_hash_high: 0,
            provenance: None,
            relationships: None,
        },
    }
}

/// A file of `rows` rows cut into granules of `rows_per_granule`, opened for reading.
fn file(rows: u64, rows_per_granule: usize) -> HefFile {
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration {
            fields: vec!["note".to_owned()],
        },
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: rows_per_granule,
            index_granularity_bytes: 64 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    let built = build_hef_file((0..rows).map(row).collect(), &config).unwrap();
    HefFile::open(built.bytes, Some(&built.file_seal)).unwrap()
}

fn all_columns(file: &HefFile) -> Vec<u32> {
    file.footer().columns.iter().map(|column| column.column_id).collect()
}

fn all_granules(file: &HefFile) -> Vec<u32> {
    file.footer()
        .granules
        .iter()
        .map(|granule| granule.granule_id)
        .collect()
}

/// Every batch of a scan, collected in delivery order.
fn collect(
    file: &HefFile,
    column_ids: &[u32],
    granule_ids: &[u32],
    fan_out: bool,
    window_bytes: u64,
) -> Result<Vec<ScanBatch>, FormatError> {
    let mut batches = Vec::new();
    file.scan_projected_inner(column_ids, granule_ids, fan_out, window_bytes, |batch| {
        batches.push(batch);
        Ok(())
    })?;
    Ok(batches)
}

/// A scanned block in a form the per-column reads can be compared against: the presence bitmap and the values.
fn flatten(column: &ScanColumn) -> (Vec<u8>, Vec<Option<String>>) {
    match column {
        ScanColumn::Materialized(read) => {
            let values = match &read.data {
                ColumnData::Strings(values) => values.iter().map(|value| value.map(str::to_owned)).collect(),
                other => (0..other.row_count()).map(|_| None).collect(),
            };
            (read.presence.clone(), values)
        }
        ScanColumn::Views { presence, views } => (
            presence.clone(),
            views.iter().map(|value| value.map(str::to_owned)).collect(),
        ),
    }
}

/// The scan hands back, block for block, exactly what the per-column reads decode — the same presence bitmap and the
/// same values through both the view and the materializing form — with the granules in the order asked for and each
/// batch's columns in the order asked for. Implements `hef-apis` — "Zero-copy string column scans" for the bulk path.
#[test]
fn projected_scan_agrees_with_per_column_reads_in_order() {
    let file = file(40, 6);
    let columns = all_columns(&file);
    let granules = all_granules(&file);
    assert!(granules.len() >= 6, "{} granules", granules.len());
    let mut asked_columns = columns.clone();
    asked_columns.reverse();
    let mut asked_granules = granules.clone();
    asked_granules.reverse();

    let batches = collect(&file, &asked_columns, &asked_granules, false, SCAN_WINDOW_BYTES).unwrap();

    assert_eq!(
        batches.iter().map(|batch| batch.granule_id).collect::<Vec<_>>(),
        asked_granules
    );
    let mut string_blocks_as_views = 0;
    for batch in &batches {
        let granule = file
            .footer()
            .granules
            .iter()
            .find(|granule| granule.granule_id == batch.granule_id)
            .unwrap();
        assert_eq!(batch.row_count, granule.row_count);
        assert_eq!(batch.columns.len(), asked_columns.len());
        let mut expected_bytes = 0;
        for (column_id, column) in asked_columns.iter().zip(&batch.columns) {
            let mark = file.mark(*column_id, 0, batch.granule_id).unwrap().unwrap();
            expected_bytes += mark.compressed_size;
            let read = file.read_column(*column_id, batch.granule_id).unwrap();
            let (presence, values) = flatten(column);
            assert_eq!(
                presence, read.presence,
                "column {column_id} granule {}",
                batch.granule_id
            );
            match column {
                ScanColumn::Materialized(scanned) => assert_eq!(scanned.data, read.data),
                ScanColumn::Views { .. } => {
                    string_blocks_as_views += 1;
                    let ColumnData::Strings(expected) = &read.data else {
                        panic!("views for a non-string column {column_id}");
                    };
                    let expected: Vec<Option<String>> = expected.iter().map(|value| value.map(str::to_owned)).collect();
                    assert_eq!(values, expected, "column {column_id} granule {}", batch.granule_id);
                    let (view_presence, views) = file
                        .read_column_string_views(*column_id, batch.granule_id)
                        .unwrap()
                        .unwrap();
                    assert_eq!(view_presence, presence);
                    assert_eq!(
                        views.iter().map(|value| value.map(str::to_owned)).collect::<Vec<_>>(),
                        values
                    );
                }
            }
        }
        assert_eq!(batch.bytes, expected_bytes);
    }
    assert!(string_blocks_as_views > 0, "no string block took the view path");
}

/// Cutting a scan into windows — one granule per window at the smallest bound — changes when batches are delivered,
/// never what they hold or their order, on both the serial and the fanned-out path. The file is sized so its granules
/// clear the fan-out rule, so the parallel path runs.
#[test]
fn projected_scan_windows_and_fan_out_do_not_change_the_batches() {
    let file = file(4 * 8192, 8192);
    let columns = all_columns(&file);
    let granules = all_granules(&file);
    assert_eq!(granules.len(), 4);
    let inflated: u64 = granules
        .iter()
        .flat_map(|granule_id| columns.iter().map(move |column_id| (*column_id, *granule_id)))
        .map(|(column_id, granule_id)| file.mark(column_id, 0, granule_id).unwrap().unwrap().uncompressed_size)
        .sum();
    assert!(
        scan_in_parallel(granules.len(), inflated / granules.len() as u64),
        "{inflated} inflated bytes"
    );
    let reference: Vec<_> = collect(&file, &columns, &granules, false, SCAN_WINDOW_BYTES)
        .unwrap()
        .iter()
        .map(|batch| {
            (
                batch.granule_id,
                batch.row_count,
                batch.bytes,
                batch.columns.iter().map(flatten).collect::<Vec<_>>(),
            )
        })
        .collect();
    for (fan_out, window_bytes) in [(false, 1), (true, 1), (true, SCAN_WINDOW_BYTES)] {
        let batches = collect(&file, &columns, &granules, fan_out, window_bytes).unwrap();
        let shaped: Vec<_> = batches
            .iter()
            .map(|batch| {
                (
                    batch.granule_id,
                    batch.row_count,
                    batch.bytes,
                    batch.columns.iter().map(flatten).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert!(shaped == reference, "fan_out {fan_out} window {window_bytes}");
    }
}

/// A projection the footer does not list, a granule the directory does not hold, and a sink that gives up each end
/// the scan with an error rather than a panic or a silent gap.
#[test]
fn projected_scan_refuses_unknown_columns_and_granules_and_stops_on_sink_error() {
    let file = file(20, 6);
    let columns = all_columns(&file);
    let granules = all_granules(&file);

    assert!(matches!(
        collect(&file, &[u32::MAX], &granules, false, SCAN_WINDOW_BYTES),
        Err(FormatError::RefOutOfRange { .. })
    ));
    assert!(matches!(
        collect(&file, &columns, &[u32::MAX], false, SCAN_WINDOW_BYTES),
        Err(FormatError::RefOutOfRange { .. })
    ));

    let mut delivered = 0;
    let stopped = file.scan_projected_inner(&columns, &granules, false, 1, |_| {
        delivered += 1;
        Err(FormatError::Structural { rule: "sink gave up" })
    });
    assert!(matches!(stopped, Err(FormatError::Structural { rule: "sink gave up" })));
    assert_eq!(delivered, 1);
}

/// An empty projection still yields one batch per granule — the row counts a `count(*)` needs — and an empty granule
/// list yields nothing.
#[test]
fn projected_scan_of_nothing_is_empty_but_well_formed() {
    let file = file(20, 6);
    let granules = all_granules(&file);
    let batches = file.scan_projected(&[], &granules, |_| Ok(())).map(|()| ());
    assert!(batches.is_ok());
    let mut rows = 0;
    file.scan_projected(&[], &granules, |batch| {
        assert!(batch.columns.is_empty());
        assert_eq!(batch.bytes, 0);
        rows += u64::from(batch.row_count);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, 20);
    let mut delivered = 0;
    file.scan_projected_serial(&all_columns(&file), &[], |_| {
        delivered += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(delivered, 0);
}
