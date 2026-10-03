use super::*;

fn object(fields: &[(&str, VariantValue)]) -> VariantValue {
    VariantValue::Object(
        fields
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect(),
    )
}

#[test]
fn observe_accumulates_counts_for_an_already_tracked_path() {
    let mut stats = PathStatistics::new();
    stats.observe(Some(&object(&[
        ("amount", VariantValue::Int(1)),
        ("note", VariantValue::String("first".to_owned())),
    ])));
    stats.observe(Some(&object(&[("amount", VariantValue::Double(0.5))])));
    stats.observe(Some(&object(&[("amount", VariantValue::Int(2))])));

    assert_eq!(stats.rows, 3);
    let amount = &stats.paths["amount"];
    assert_eq!(amount.present, 3);
    assert_eq!(amount.by_kind[ColumnKind::I64 as usize], 2);
    assert_eq!(amount.by_kind[ColumnKind::F64 as usize], 1);
    let note = &stats.paths["note"];
    assert_eq!(note.present, 1);
    assert_eq!(note.by_kind[ColumnKind::String as usize], 1);
}

#[test]
fn shred_candidates_selects_a_consistently_typed_frequent_path() {
    let mut stats = PathStatistics::new();
    for row in 0..4 {
        stats.observe(Some(&object(&[("amount", VariantValue::Int(row))])));
    }
    assert_eq!(
        stats.shred_candidates(&[]),
        vec![("amount".to_owned(), ColumnKind::I64)]
    );
}

/// Counting the rows in pieces and merging must select exactly what counting them in one pass selects — the property
/// the build's parallel statistics pass rests on. Split at an uneven boundary, so no piece sees a whole path.
#[test]
fn merging_partial_statistics_selects_what_one_pass_selects() {
    let rows: Vec<VariantValue> = (0..40)
        .map(|row| {
            object(&[
                ("amount", VariantValue::Int(row)),
                // Present on well under half the rows: a sparse candidate, not a dense one.
                ("region", VariantValue::String(format!("r{}", row % 3))),
                (
                    "mixed",
                    if row % 2 == 0 {
                        VariantValue::Int(row)
                    } else {
                        VariantValue::String("no consensus".to_owned())
                    },
                ),
            ])
        })
        .collect();

    let mut whole = PathStatistics::new();
    for row in &rows {
        whole.observe(Some(row));
    }

    let mut merged = PathStatistics::new();
    for chunk in rows.chunks(7) {
        let mut partial = PathStatistics::new();
        for row in chunk {
            partial.observe(Some(row));
        }
        merged.merge(partial);
    }

    assert_eq!(merged.rows, whole.rows);
    assert_eq!(merged.shred_candidates(&[]), whole.shred_candidates(&[]));
    assert_eq!(merged.sparse_candidates(&[]), whole.sparse_candidates(&[]));
}

#[test]
fn field_id_statistics_match_string_keyed_selection_for_missing_and_changed_types() {
    let rows = [
        object(&[
            ("amount", VariantValue::Int(1)),
            ("note", VariantValue::String("a".into())),
        ]),
        object(&[("amount", VariantValue::Int(2))]),
        object(&[("amount", VariantValue::Double(3.0))]),
        VariantValue::Null,
    ];
    let names = vec!["amount".to_owned(), "note".to_owned()];
    let mut named = PathStatistics::new();
    let mut identified = PathStatistics::for_field_ids(names.len());
    for value in &rows {
        named.observe(Some(value));
        match value {
            VariantValue::Object(fields) => {
                let ids: Vec<u32> = fields
                    .keys()
                    .map(|path| names.iter().position(|name| name == path).unwrap() as u32)
                    .collect();
                let values: Vec<VariantValue> = fields.values().cloned().collect();
                identified.observe_field_ids(&ids, &values);
            }
            _ => identified.observe_field_ids(&[], std::slice::from_ref(value)),
        }
    }

    assert_eq!(
        identified.shred_candidates_by_id(&names, &HashSet::new()),
        named.shred_candidates(&[])
    );
    assert_eq!(
        identified.sparse_candidates_by_id(&names, &HashSet::new()),
        named.sparse_candidates(&[])
    );
}

#[test]
fn very_wide_field_id_statistics_use_the_sparse_fallback() {
    let field_count = DENSE_FIELD_STATISTICS_LIMIT + 1;
    let mut names = vec![String::new(); field_count];
    names[field_count - 1] = "wide_tail".to_owned();
    let value = object(&[("wide_tail", VariantValue::Int(7))]);
    let mut statistics = PathStatistics::for_field_ids(field_count);
    assert!(statistics.dense_fields.is_empty());
    assert!(statistics.sparse_fields.is_some());
    for _ in 0..20 {
        let VariantValue::Object(fields) = &value else {
            unreachable!("the fixture is an object")
        };
        let values: Vec<VariantValue> = fields.values().cloned().collect();
        statistics.observe_field_ids(&[(field_count - 1) as u32], &values);
    }
    assert_eq!(
        statistics.shred_candidates_by_id(&names, &HashSet::new()),
        vec![("wide_tail".to_owned(), ColumnKind::I64)]
    );
}
