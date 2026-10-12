use super::*;
use crate::layout::LayoutTargets;
use crate::writer::build::{BuildRow, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};

fn record(i: u64) -> EventRecord<'static> {
    EventRecord {
        account_id: None,
        account_id_hash_low: 0,
        actor_id: Some("@alice:example.org"),
        actor_id_hash_low: 0xDEAD_BEEF,
        dedupe_hash_high: u64::MAX,
        dedupe_hash_low: i,
        entity_id: Some(""),
        entity_id_hash_high: 0,
        entity_id_hash_low: 7,
        entity_type_id: 2,
        event_id: u128::MAX - u128::from(i),
        event_type_id: 300,
        external_id: Some(b"$event:example.org"),
        flags: u32::MAX,
        ingested_at: 2_000 - i as i64,
        occurred_at: i64::MIN + i as i64,
        schema_version: 1,
        source_id: 0,
        stream_id: u64::MAX,
        stream_sequence: i,
        trace_id_hash_low: 0,
    }
}

fn decode_row(block: &[u8], row: u32) -> EventRecord<'_> {
    let (bases, bytes) = record_bytes(block, row).unwrap();
    decode_record(bytes, bases).unwrap()
}

#[test]
fn a_record_block_round_trips_every_field() {
    let records = [
        record(0),
        EventRecord {
            account_id: Some("acct"),
            external_id: None,
            occurred_at: i64::MAX,
            ..record(1)
        },
        record(2),
    ];
    let mut block = EventRecordBlock::default();
    for record in &records {
        block.push(record);
    }
    let block = block.finish();
    for (row, expected) in records.iter().enumerate() {
        assert_eq!(&decode_row(&block, row as u32), expected, "row {row}");
    }
    assert!(record_bytes(&block, 3).is_err(), "a row past the block is refused");
}

#[test]
fn offsets_that_run_backwards_are_refused() {
    let mut block = EventRecordBlock::default();
    block.push(&record(0));
    block.push(&record(1));
    let mut block = block.finish();
    // Row 1's start offset, after the header and row 0's offset, made larger than its end.
    block[BLOCK_HEADER_LEN + 4..BLOCK_HEADER_LEN + 8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(record_bytes(&block, 1).is_err());
}

#[test]
fn rewriting_dedupe_hashes_changes_nothing_else() {
    let mut block = EventRecordBlock::default();
    block.push(&record(0));
    block.push(&record(1));
    let block = block.finish();
    let rewritten = with_dedupe_hashes(&block, &[(1, 2), (3, 4)]).unwrap();
    assert_eq!(
        decode_row(&rewritten, 1),
        EventRecord {
            dedupe_hash_high: 3,
            dedupe_hash_low: 4,
            ..record(1)
        }
    );
    assert!(
        with_dedupe_hashes(&block, &[(1, 2)]).is_err(),
        "the row count must match"
    );
}

/// Row `i` with an external id and, on every third row, no actor and an account instead.
fn row_with_id(i: u64) -> BuildRow {
    let mut row = plain_row(i);
    row.external_id = Some(format!("$event-{i}:example.org").into_bytes());
    row.envelope.entity_id = Some(format!("!room{}:example.org", i % 4));
    if i % 3 == 0 {
        row.envelope.account_id = Some(String::new());
        row.envelope.account_id_hash_low = i;
    } else {
        row.envelope.actor_id = Some(format!("@user{i}:example.org"));
        row.envelope.actor_id_hash_low = u64::MAX - i;
    }
    row
}

#[test]
fn a_file_with_external_ids_reads_each_event_from_its_record_alone() {
    let rows: Vec<BuildRow> = (0..50).map(row_with_id).collect();
    let mut config = source_form_config(8);
    config.targets = LayoutTargets {
        index_granularity: 8,
        ..LayoutTargets::default()
    };
    let built = build_hef_file(rows, &config).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_ne!(file.usable_optional_features() & optional_features::EVENT_RECORDS, 0);
    assert_eq!(file.footer().event_records.len(), file.footer().granules.len());

    let before = file.column_block_reads();
    for i in 0..50u64 {
        let id = format!("$event-{i}:example.org").into_bytes();
        assert_eq!(file.row_by_external_id(&id).unwrap(), Some(i));
        assert_eq!(file.event_record(i).unwrap().unwrap().external_id, Some(id.as_slice()));
    }
    assert_eq!(file.row_by_external_id(b"$missing").unwrap(), None);
    let envelopes: Vec<EventEnvelope> = (0..50).map(|i| file.envelope(i).unwrap()).collect();
    assert_eq!(
        file.column_block_reads(),
        before,
        "lookups and envelopes read no column block"
    );

    // The envelope columns, read granule by granule, hold exactly what the records do.
    let from_columns: Vec<EventEnvelope> = file
        .footer()
        .granules
        .iter()
        .flat_map(|granule| file.granule_envelopes(granule.granule_id).unwrap())
        .collect();
    assert_eq!(envelopes, from_columns);
}

#[test]
fn a_file_without_external_ids_stores_no_records() {
    let built = build_hef_file((0..20).map(plain_row).collect::<Vec<_>>(), &source_form_config(8)).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_eq!(
        file.footer().optional_feature_flags & optional_features::EVENT_RECORDS,
        0
    );
    assert!(file.footer().event_records.is_empty());
    assert_eq!(file.event_record(3).unwrap(), None);
    assert_eq!(file.envelope(3).unwrap().dedupe_hash_low, 3);
}
