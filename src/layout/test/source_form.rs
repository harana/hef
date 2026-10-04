use super::*;
use crate::encoding::StringColumn;
use crate::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};
use crate::writer::build::{BuildRow, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};

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
