use super::*;
use crate::encoding::StringColumn;
use crate::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};
use crate::events::variant::VariantValue;
use crate::layout::LayoutTargets;
use crate::writer::build::{BuildLifecycle, BuildRow, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config, source_form_row};
use std::collections::BTreeMap;
use std::time::Instant;

fn with_id(i: u64, id: &[u8]) -> BuildRow {
    let mut row = plain_row(i);
    row.external_id = Some(id.to_vec());
    row
}

#[test]
fn an_external_id_finds_its_row_through_the_index() {
    let long_id = vec![b'L'; 40];
    let rows = vec![
        with_id(0, b"$first"),
        plain_row(1),
        with_id(2, &long_id),
        with_id(3, b"$dup"),
        with_id(4, b"$last"),
        with_id(5, b"$dup"),
    ];
    let built = build_hef_file(rows, &source_form_config(2)).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    let before = file.column_block_reads();
    assert_eq!(file.row_by_external_id(b"$missing").unwrap(), None);
    assert_eq!(
        file.column_block_reads(),
        before,
        "a miss is answered from the footer index alone"
    );
    assert_eq!(file.row_by_external_id(b"$last").unwrap(), Some(4));
    assert!(
        file.column_block_reads() - before <= 1,
        "a hit reads only the granule that holds it"
    );

    assert_eq!(file.row_by_external_id(b"$first").unwrap(), Some(0));
    assert_eq!(
        file.row_by_external_id(&long_id).unwrap(),
        Some(2),
        "ids longer than 32 bytes index like any other"
    );
    assert_eq!(file.rows_with_external_id(b"$dup").unwrap(), vec![3, 5]);
    assert_eq!(
        file.row_by_external_id(b"$dup").unwrap(),
        Some(5),
        "the newest row answers"
    );
    assert_eq!(file.external_id(1).unwrap(), None);
    assert_eq!(file.external_ids().unwrap().len(), 5);
}

#[test]
fn prev_and_auth_references_round_trip_and_reverse_lookups_skip_granules() {
    let target: &[u8] = b"$abc:example.org";
    let rows: Vec<BuildRow> = (0..8u64)
        .map(|i| {
            let mut row = with_id(i, format!("$event-{i}").as_bytes());
            let mut refs: Vec<RelationshipRef> = (0..20)
                .map(|p| {
                    RelationshipRef::to_external(RelationshipKind::Prev, format!("$prev-{i}-{p}").as_bytes()).unwrap()
                })
                .collect();
            // Only row 5 names the target among its auth events.
            refs.extend((0..10).map(|a| {
                let id = if i == 5 && a == 0 {
                    target.to_vec()
                } else {
                    format!("$auth-{i}-{a}").into_bytes()
                };
                RelationshipRef::to_external(RelationshipKind::Auth, &id).unwrap()
            }));
            row.relationships = Some(EventRelationships::new(refs).unwrap());
            row
        })
        .collect();
    let built = build_hef_file(rows.clone(), &source_form_config(2)).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    for granule in &file.footer().granules {
        for kind in [RelationshipKind::Prev, RelationshipKind::Auth] {
            let read = file.read_column(kind.column_id(), granule.granule_id).unwrap();
            for row in 0..granule.row_count as usize {
                let ordinal = granule.first_row_ordinal as usize + row;
                let text = string_at(&read, granule.row_count as usize, row).expect("every row declares both kinds");
                let declared: Vec<RelationshipRef> = rows[ordinal]
                    .relationships
                    .as_ref()
                    .unwrap()
                    .refs()
                    .iter()
                    .filter(|reference| reference.kind == kind)
                    .cloned()
                    .collect();
                assert_eq!(EventRelationships::parse_column_text(kind, text).unwrap(), declared);
            }
        }
    }

    let lookup = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    let granules = lookup.footer().granules.len() as u64;
    assert_eq!(granules, 4);
    let before = lookup.column_block_reads();
    assert_eq!(
        lookup
            .rows_referencing(RelationshipKind::Auth, TargetIdSpace::ExternalId, target)
            .unwrap(),
        vec![5]
    );
    let reads = lookup.column_block_reads() - before;
    assert!(
        (1..granules).contains(&reads),
        "the reference filters skip granules that cannot hold the target: {reads} of {granules} read"
    );

    // A reference in the external-id space resolves through the file's external-id index.
    let reference = RelationshipRef::to_external(RelationshipKind::Prev, b"$event-2").unwrap();
    assert_eq!(lookup.resolve_reference(&reference).unwrap(), vec![2]);
}

#[test]
fn a_file_without_reference_filters_scans_every_granule() {
    let built = build_hef_file((0..4).map(plain_row).collect::<Vec<_>>(), &source_form_config(2)).unwrap();
    assert_eq!(
        reference_may_be_in(&built.footer, column_ids::AUTH_REFS, 0, "external_id:00"),
        Ok(true)
    );
}

#[test]
fn string_at_reads_dense_and_presence_gated_blocks() {
    let mut dense = StringColumn::new();
    dense.push(Some("a"));
    dense.push(None);
    dense.push(Some("c"));
    let read = ColumnRead {
        data: ColumnData::Strings(dense),
        presence: Vec::new(),
    };
    assert_eq!(string_at(&read, 3, 0), Some("a"));
    assert_eq!(string_at(&read, 3, 1), None);
    assert_eq!(string_at(&read, 3, 2), Some("c"));

    let mut sparse = StringColumn::new();
    sparse.push(Some("b"));
    sparse.push(Some("d"));
    let read = ColumnRead {
        data: ColumnData::Strings(sparse),
        presence: vec![0b1010],
    };
    assert_eq!(string_at(&read, 4, 0), None);
    assert_eq!(string_at(&read, 4, 1), Some("b"));
    assert_eq!(string_at(&read, 4, 3), Some("d"));
}

/// A Matrix-shaped event id for row `i`: `$`, 43 url-safe characters derived from `i`, and a server name.
fn matrix_event_id(i: u64) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut state = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut id = vec![b'$'];
    for _ in 0..43 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        id.push(ALPHABET[(state & 63) as usize]);
    }
    id.extend_from_slice(b":example.org");
    id
}

/// Row `i` of the lookup benchmark: a chat message with a sender, a room, an external id and a small body.
fn message_row(i: u64) -> BuildRow {
    let body = format!("message number {i} in a room with a few words of text");
    let mut row = source_form_row(
        i,
        VariantValue::Object(BTreeMap::from([
            ("body".to_owned(), VariantValue::String(body)),
            ("msgtype".to_owned(), VariantValue::String("m.text".to_owned())),
        ])),
    );
    row.envelope.actor_id = Some(format!("@user{}:example.org", i % 5_000));
    row.envelope.entity_id = Some(format!("!room{}:example.org", i % 2_000));
    row.external_id = Some(matrix_event_id(i));
    row
}

/// Compares fetching one whole event by its external id in a 1,000,000-event file against SQLite holding the same
/// events in a table with a unique index on the external id, and prints time per lookup. Both answer random ids from
/// the whole file, one at a time: HEF from one open file whose bytes are in memory, SQLite from one open connection
/// over a database file in the OS page cache, through a prepared statement. Run by hand:
/// `cargo test --release --all-features --lib external_id_lookup_against_sqlite -- --ignored --nocapture`.
#[test]
#[ignore = "release-mode external-id lookup benchmark"]
fn external_id_lookup_against_sqlite() {
    let events = 1_000_000u64;
    let probes = 20_000usize;
    let rows: Vec<BuildRow> = (0..events).map(message_row).collect();
    let mut config = source_form_config(8192);
    config.targets = LayoutTargets::default();
    for lifecycle in [BuildLifecycle::FreshPublication, BuildLifecycle::RewriteOrCompaction] {
        config.lifecycle = lifecycle;
        let started = Instant::now();
        let built = build_hef_file(rows.clone(), &config).unwrap();
        let build_time = started.elapsed();
        let size = built.bytes.len();
        let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let ids: Vec<(u64, Vec<u8>)> = (0..probes)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let i = state % events;
                (i, matrix_event_id(i))
            })
            .collect();
        let started = Instant::now();
        for (i, id) in &ids {
            assert_eq!(file.row_by_external_id(id).unwrap(), Some(*i));
        }
        let probe = started.elapsed() / probes as u32;
        let started = Instant::now();
        for (i, _) in &ids {
            std::hint::black_box(file.envelope(*i).unwrap());
        }
        let envelope = started.elapsed() / probes as u32;
        let started = Instant::now();
        for (i, _) in &ids {
            std::hint::black_box(file.payload(*i).unwrap());
        }
        let payload = started.elapsed() / probes as u32;
        let started = Instant::now();
        for (_, id) in &ids {
            let row = file.row_by_external_id(id).unwrap().unwrap();
            std::hint::black_box(file.event(row).unwrap());
        }
        let whole = started.elapsed() / probes as u32;
        println!(
            "hef {lifecycle:?}: {size} B, built in {build_time:?}; per lookup: id probe {probe:?}, envelope \
             {envelope:?}, payload {payload:?}, whole event {whole:?}"
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.db");
    let mut db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE events (
             id INTEGER PRIMARY KEY, external_id BLOB NOT NULL, event_id BLOB NOT NULL, occurred_at INTEGER,
             ingested_at INTEGER, source TEXT, event_type TEXT, entity_type TEXT, entity_id TEXT, actor_id TEXT,
             account_id TEXT, stream_id INTEGER, stream_sequence INTEGER, flags INTEGER, schema_version INTEGER,
             entity_hash_low INTEGER, entity_hash_high INTEGER, actor_hash INTEGER, account_hash INTEGER,
             trace_hash INTEGER, dedupe_low INTEGER, dedupe_high INTEGER, payload BLOB);
         CREATE UNIQUE INDEX events_external_id ON events (external_id);",
    )
    .unwrap();
    let started = Instant::now();
    let transaction = db.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO events VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                 ?18, ?19, ?20, ?21, ?22, ?23)",
            )
            .unwrap();
        for (i, row) in rows.iter().enumerate() {
            let envelope = &row.envelope;
            let payload = format!(
                "{{\"body\":\"message number {i} in a room with a few words of text\",\"msgtype\":\"m.text\"}}"
            );
            insert
                .execute(rusqlite::params![
                    i as i64,
                    row.external_id.as_deref().unwrap(),
                    envelope.event_id.to_string(),
                    envelope.occurred_at.physical_nanos(),
                    envelope.ingested_at.physical_nanos(),
                    &*envelope.source,
                    &*envelope.event_type,
                    &*envelope.entity_type,
                    envelope.entity_id,
                    envelope.actor_id,
                    envelope.account_id,
                    envelope.stream_id.0 as i64,
                    envelope.stream_sequence as i64,
                    i64::from(envelope.flags.0),
                    envelope.schema_version,
                    envelope.entity_id_hash_low as i64,
                    envelope.entity_id_hash_high as i64,
                    envelope.actor_id_hash_low as i64,
                    envelope.account_id_hash_low as i64,
                    envelope.trace_id_hash_low as i64,
                    envelope.dedupe_hash_low as i64,
                    envelope.dedupe_hash_high as i64,
                    payload.as_bytes(),
                ])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    let build_time = started.elapsed();
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").unwrap();
    let size = std::fs::metadata(&path).unwrap().len();
    let mut select = db
        .prepare_cached("SELECT * FROM events WHERE external_id = ?1")
        .unwrap();
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let ids: Vec<Vec<u8>> = (0..probes)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            matrix_event_id(state % events)
        })
        .collect();
    let mut fetch = |id: &[u8]| {
        select
            .query_row([id], |row| {
                let mut owned = Vec::with_capacity(23);
                for column in 0..23 {
                    owned.push(row.get::<_, rusqlite::types::Value>(column)?);
                }
                Ok(owned)
            })
            .unwrap()
    };
    // One pass loads the database file into the OS page cache, as the in-memory HEF bytes are.
    for id in &ids {
        fetch(id);
    }
    let started = Instant::now();
    for id in &ids {
        std::hint::black_box(fetch(id));
    }
    let lookup = started.elapsed() / probes as u32;
    println!("sqlite: {size} B, built in {build_time:?}; per lookup: whole row {lookup:?}");
}
