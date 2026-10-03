use super::*;
use crate::columns::PromotedColumn;
use crate::encoding::{Compression, Transform, encode_block};
use crate::events::provenance::{SignatureScheme, SignedEventProvenance, hex_bytes};
use crate::events::sim::{SimulatedEventAuthor, signed_event_payload};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::invariants::io::ThreadPoolEncodeExecutor;
use crate::layout::reader::{HefFile, HefFooter, PayloadRead, SpeculativeTail, tail_range};
use crate::typed_id::TypedIdTestExt;

/// The field list the writer takes, from the map a test spells its payload as.
fn fields_of(fields: &BTreeMap<String, VariantValue>) -> Vec<(FieldName, VariantValue)> {
    fields
        .iter()
        .map(|(name, value)| (FieldName::from(name.as_str()), value.clone()))
        .collect()
}

fn row(i: u64) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    payload.insert("kind".to_owned(), VariantValue::String(format!("k{}", i % 4)));
    payload.insert("amount".to_owned(), VariantValue::Int(1_000 + i as i64));
    payload.insert(
        "note".to_owned(),
        VariantValue::String(format!("free text body number {i}")),
    );
    if i.is_multiple_of(3) {
        payload.insert("rare".to_owned(), VariantValue::Bool(true));
    }
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xBEEF_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(7),
                stream_id: StreamId(2),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64 * 1000),
                ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64 * 1000),
                source: ["crm", "billing"][i as usize % 2].to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i,
                entity_id_hash_high: 1,
                entity_id: Some(format!("opp-{i}")),
                actor_id_hash_low: 3,
                actor_id: i.is_multiple_of(2).then(|| "actor-1".to_owned()),
                account_id_hash_low: 4,
                account_id: None,
                trace_id_hash_low: 5,
                dedupe_hash_low: 100 + i,
                dedupe_hash_high: 6,
                schema_version: 2,
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

#[test]
fn normalized_field_identities_reuse_shapes_without_interning_unused_nested_keys() {
    let nested = VariantValue::Object(BTreeMap::from([("nested-only".to_owned(), VariantValue::Int(1))]));
    let first = BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(1)),
        ("details".to_owned(), nested.clone()),
    ]);
    let same_shape = BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(2)),
        ("details".to_owned(), nested),
    ]);
    let evolved = BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(3)),
        ("region".to_owned(), VariantValue::String("au".to_owned())),
    ]);

    let mut identities = FieldIdentityCatalog::default();
    let first_shape = identities.intern_object(&fields_of(&first)).unwrap();
    let repeated_shape = identities.intern_object(&fields_of(&same_shape)).unwrap();
    let evolved_shape = identities.intern_object(&fields_of(&evolved)).unwrap();

    assert_eq!(repeated_shape, first_shape);
    assert_ne!(evolved_shape, first_shape);
    assert_eq!(identities.shapes.len(), 2);
    assert_eq!(identities.field_id("amount"), Some(0));
    assert!(identities.field_id("details").is_some());
    assert!(identities.field_id("region").is_some());
    assert_eq!(identities.field_id("nested-only"), None);
}

/// Interning refills one shared buffer with the field ids of the row it is working on. Rows that alternate between two
/// shapes both find their shape among the recent ones, and the ids each of them yields must be its own shape's, not
/// what the row before left in the buffer.
#[test]
fn alternating_payload_shapes_keep_their_own_field_ids() {
    let wide = BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(1)),
        ("kind".to_owned(), VariantValue::String("a".to_owned())),
        ("region".to_owned(), VariantValue::String("au".to_owned())),
    ]);
    let narrow = BTreeMap::from([("amount".to_owned(), VariantValue::Int(2))]);

    let mut identities = FieldIdentityCatalog::default();
    let mut seen = Vec::new();
    for round in 0..4 {
        let fields = if round % 2 == 0 { &wide } else { &narrow };
        let shape_id = identities.intern_object(&fields_of(fields)).unwrap();
        seen.push(shape_id);
    }

    assert_eq!(seen[0], seen[2]);
    assert_eq!(seen[1], seen[3]);
    assert_ne!(seen[0], seen[1]);
    assert_eq!(identities.shapes.len(), 2);
    let named = |shape_id: u32| -> Vec<&str> {
        identities
            .shape(shape_id)
            .iter()
            .map(|&field_id| &*identities.field_names()[field_id as usize])
            .collect()
    };
    assert_eq!(named(seen[0]), ["amount", "kind", "region"]);
    assert_eq!(named(seen[1]), ["amount"]);
}

/// Rows cycling through up to four shapes all find theirs among the recent shapes and intern nothing new; a fifth
/// shape evicts the least recently seen one, which is then interned afresh (finding the same id) when it comes back.
#[test]
fn recent_shapes_are_kept_most_recently_used_first() {
    let shapes: Vec<BTreeMap<String, VariantValue>> = (0..5)
        .map(|width| {
            (0..=width)
                .map(|field| (format!("f{field}"), VariantValue::Int(field as i64)))
                .collect()
        })
        .collect();
    let mut identities = FieldIdentityCatalog::default();
    let ids: Vec<u32> = shapes[..4]
        .iter()
        .map(|fields| identities.intern_object(&fields_of(fields)).unwrap())
        .collect();
    assert_eq!(ids, [0, 1, 2, 3]);
    assert_eq!(identities.recent_shapes, [3, 2, 1, 0]);

    for (round, fields) in shapes[..4].iter().enumerate().cycle().take(12) {
        assert_eq!(identities.intern_object(&fields_of(fields)).unwrap(), round as u32);
    }
    assert_eq!(identities.shapes.len(), 4);
    assert_eq!(identities.recent_shapes, [3, 2, 1, 0]);

    // The widest shape is new: it takes the front and pushes out shape 0, the least recently seen.
    assert_eq!(identities.intern_object(&fields_of(&shapes[4])).unwrap(), 4);
    assert_eq!(identities.recent_shapes, [4, 3, 2, 1]);
    // Shape 0 comes back through the catalogue rather than the recent list, and lands at the front again.
    assert_eq!(identities.intern_object(&fields_of(&shapes[0])).unwrap(), 0);
    assert_eq!(identities.recent_shapes, [0, 4, 3, 2]);
    assert_eq!(identities.shapes.len(), 5);
}

/// A route is read off the shape by position, so the same path has to sit at a different place in a shape that
/// inserts a field ahead of it, and a field no plan names has to be marked as staying in the residual.
#[test]
fn shape_routes_sit_at_each_shapes_own_field_positions() {
    let narrow = BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(1)),
        ("kind".to_owned(), VariantValue::String("renewal".to_owned())),
        ("note".to_owned(), VariantValue::String("body".to_owned())),
    ]);
    // `escalations` sorts between `amount` and `kind`, so it shifts every route after it one place right.
    let wide = BTreeMap::from([
        ("amount".to_owned(), VariantValue::Int(2)),
        ("escalations".to_owned(), VariantValue::Int(3)),
        ("kind".to_owned(), VariantValue::String("expansion".to_owned())),
        ("note".to_owned(), VariantValue::String("body".to_owned())),
    ]);
    let unrouted = BTreeMap::from([("escalations".to_owned(), VariantValue::Int(4))]);

    let mut identities = FieldIdentityCatalog::default();
    let narrow_shape = identities.intern_object(&fields_of(&narrow)).unwrap();
    let wide_shape = identities.intern_object(&fields_of(&wide)).unwrap();
    let unrouted_shape = identities.intern_object(&fields_of(&unrouted)).unwrap();

    let promotion = PromotionPlan {
        columns: vec![PromotedColumn {
            kind: ColumnKind::String,
            name: "kind".to_owned(),
            path: "kind".to_owned(),
            since_schema_version: 1,
            substring_searchable: false,
        }],
    };
    let shred_plan = vec![ShredEntry {
        column_id: 100,
        path: "amount".to_owned(),
    }];
    let freetext = vec![FreetextEntry {
        column_id: 200,
        declared_field: "note".to_owned(),
    }];
    let field_routes = build_field_routes(&identities, &promotion, &shred_plan, &freetext);
    let shape_routes = build_shape_routes(&identities, &field_routes);

    let field_id = |path: &str| identities.field_id(path).expect("interned");
    assert_eq!(
        shape_routes.field_routes(narrow_shape),
        [field_id("amount"), field_id("kind"), field_id("note")],
        "amount, kind and note all route"
    );
    assert_eq!(
        shape_routes.field_routes(wide_shape),
        [
            field_id("amount"),
            FIELD_STAYS_RESIDUAL,
            field_id("kind"),
            field_id("note")
        ],
        "the field inserted in the middle stays, and shifts the routes after it"
    );
    assert_eq!(shape_routes.field_routes(unrouted_shape), [FIELD_STAYS_RESIDUAL]);
    assert!(shape_routes.field_routes(NON_OBJECT_FIELD_SHAPE).is_empty());
    // The sentinel is what makes the row loop's single lookup answer "nothing to do" without a route to inspect.
    assert!(field_routes.get(FIELD_STAYS_RESIDUAL as usize).is_none());
}

/// A payload whose shape changes from one row to the next — an optional field coming and going, another sorting into
/// the middle of the field order — must route exactly as a single-shape payload does. Routes apply by position inside
/// a shape, so a shape resolved against the wrong positions would quietly copy a neighbouring field into a typed
/// column, or leave a declared one behind in the residual, while the file still opened and read back.
#[test]
fn payloads_that_change_shape_row_to_row_route_by_their_own_shape() {
    let rows: Vec<HefRow> = (0..64)
        .map(|i| {
            let mut shifting = row(i);
            // `middle` sorts between `kind` and `note`; `rare`, which `row` adds every third row, sorts after both.
            if i.is_multiple_of(5)
                && let PayloadInput::Variant(VariantValue::Object(fields)) = &mut shifting.event.payload
            {
                fields.insert("middle".to_owned(), VariantValue::Int(500 + i as i64));
            }
            shifting
        })
        .collect();

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    let amount = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("the frequent integer path shreds");
    let note = built.footer.freetext.first().expect("note moves by declaration");
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    // The typed columns carry every row's own value in row order: each route fired on the field it was declared for,
    // not on the one standing next to it.
    let mut first_row = 0u64;
    for granule in &file.footer().granules {
        let ColumnData::I64(amounts) = file.read_column(amount.column_id, granule.granule_id).unwrap().data else {
            panic!("amount shreds into an integer column");
        };
        let ColumnData::Strings(kinds) = file
            .read_column(column_ids::PROMOTED_BASE, granule.granule_id)
            .unwrap()
            .data
        else {
            panic!("kind is promoted into a string column");
        };
        let ColumnData::Strings(notes) = file.read_column(note.column_id, granule.granule_id).unwrap().data else {
            panic!("note is a free-text string column");
        };
        for offset in 0..amounts.len() {
            let i = first_row + offset as u64;
            assert_eq!(amounts.get(offset).copied(), Some(1_000 + i as i64), "row {i} amount");
            assert_eq!(
                kinds.get(offset),
                Some(Some(format!("k{}", i % 4).as_str())),
                "row {i} kind"
            );
            assert_eq!(
                notes.get(offset),
                Some(Some(format!("free text body number {i}").as_str())),
                "row {i} note"
            );
        }
        first_row += amounts.len() as u64;
    }
    assert_eq!(first_row, 64, "every row reached the typed columns");

    // Nothing that should have stayed moved: every payload still rebuilds to exactly what went in, including the two
    // fields that only some rows carry.
    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal}");
    }
    assert_eq!(file.payload_path(5, "middle").unwrap(), Some(VariantValue::Int(505)));
    assert_eq!(file.payload_path(6, "middle").unwrap(), None);
    assert_eq!(file.payload_path(6, "rare").unwrap(), Some(VariantValue::Bool(true)));
}

#[test]
#[ignore = "release-mode IMP-003 phase benchmark"]
fn benchmark_string_paths_against_stable_field_ids() {
    let payloads: Vec<VariantValue> = (0..100_000)
        .map(|row| {
            VariantValue::Object(BTreeMap::from([
                ("amount".to_owned(), VariantValue::Int(row)),
                ("kind".to_owned(), VariantValue::String(format!("k{}", row % 4))),
                ("note".to_owned(), VariantValue::String(format!("event {row}"))),
            ]))
        })
        .collect();
    let field_ids = [0, 1, 2];
    let names = vec!["amount".to_owned(), "kind".to_owned(), "note".to_owned()];
    // The id-addressed path reads the normalized values the build lays out once before any statistics run, so the
    // normalization is outside the timed region here exactly as it is in the writer.
    let normalized: Vec<Vec<VariantValue>> = payloads
        .iter()
        .map(|payload| match payload {
            VariantValue::Object(fields) => fields.values().cloned().collect(),
            other => vec![other.clone()],
        })
        .collect();
    let mut legacy_samples = Vec::new();
    let mut identified_samples = Vec::new();

    for iteration in 0..7 {
        let run_legacy = || {
            let started = Instant::now();
            let mut statistics = PathStatistics::new();
            for payload in &payloads {
                statistics.observe(Some(payload));
            }
            std::hint::black_box(statistics.shred_candidates(&[]));
            started.elapsed().as_nanos()
        };
        let run_identified = || {
            let started = Instant::now();
            let mut statistics = PathStatistics::for_field_ids(names.len());
            for values in &normalized {
                statistics.observe_field_ids(&field_ids, values);
            }
            std::hint::black_box(statistics.shred_candidates_by_id(&names, &HashSet::new()));
            started.elapsed().as_nanos()
        };
        if iteration % 2 == 0 {
            legacy_samples.push(run_legacy());
            identified_samples.push(run_identified());
        } else {
            identified_samples.push(run_identified());
            legacy_samples.push(run_legacy());
        }
    }
    legacy_samples.sort_unstable();
    identified_samples.sort_unstable();
    let legacy_median = legacy_samples.get(3).copied().unwrap_or_default();
    let identified_median = identified_samples.get(3).copied().unwrap_or_default();
    eprintln!(
        "IMP-003 path-statistics: legacy={legacy_samples:?} identified={identified_samples:?}; median legacy={}ns \
         identified={}ns reduction={:.1}%",
        legacy_median,
        identified_median,
        100.0 * (1.0 - identified_median as f64 / legacy_median as f64)
    );
}

fn config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
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
        promotion: PromotionPlan {
            columns: vec![PromotedColumn {
                name: "kind".to_owned(),
                path: "kind".to_owned(),
                kind: ColumnKind::String,
                since_schema_version: 1,
                substring_searchable: false,
            }],
        },
        targets: LayoutTargets {
            index_granularity: 16,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(7),
    }
}

/// A cold open reads the footer from the file's tail alone. Sizing the exact last-`footer_len` bytes (as a remote
/// reader would from the manifest entry) and calling `HefFooter::open` on them yields the same footer as a whole-file
/// open — so the footer-first path is a faithful, header-free open of the same closed file shape.
#[test]
fn footer_first_open_matches_whole_file_open() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();

    // The build path attaches no outboard tree, so the exact tail is the last `footer_len` bytes.
    let range = tail_range(built.bytes.len() as u64, Some(built.footer_len), built.tree_len);
    assert!(range.exact);
    assert_eq!(range.len, built.footer_len);
    assert_eq!(range.start, built.bytes.len() as u64 - built.footer_len);

    let tail = &built.bytes[range.start as usize..];
    let footer_first = HefFooter::open(tail).unwrap();
    let whole_file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    assert_eq!(footer_first.footer(), whole_file.footer());
}

/// `GranuleEntry.first_row_ordinal` is decoded straight off the wire and is never bounded, so `granule_for_ordinal`'s
/// row-range check must use a checked add and return `None` on overflow instead of panicking — the same fix applied
/// to every other `first_row_ordinal + row_count` site in the reader.
#[test]
fn granule_for_ordinal_returns_none_instead_of_panicking_on_an_overflowing_row_range() {
    let rows: Vec<HefRow> = (0..4).map(row).collect();
    let built = build_hef_file(rows, &config()).unwrap();
    let mut footer = built.footer.clone();
    footer.granules[0].first_row_ordinal = u64::MAX;

    assert_eq!(granule_for_ordinal(&footer, u64::MAX), None);
}

#[test]
fn update_reuses_unchanged_blocks_and_matches_a_full_rewrite() {
    let before: Vec<HefRow> = (0..64).map(row).collect();
    let source = build_hef_file(before.clone(), &config()).unwrap();
    let mut after = before.clone();
    let ordinals = [2usize, 19, 47];
    for ordinal in ordinals {
        let row = &mut after[ordinal];
        row.event.envelope.dedupe_hash_low += 10_000;
        let PayloadInput::Variant(VariantValue::Object(payload)) = &mut row.event.payload else {
            panic!("test rows carry object payloads");
        };
        payload.insert("amount".to_owned(), VariantValue::Int(90_000 + ordinal as i64));
        payload.insert(
            "note".to_owned(),
            VariantValue::String(format!("corrected note {ordinal}")),
        );
    }

    let full = build_hef_file(after.clone(), &config()).unwrap();
    assert_eq!(source.footer.columns, full.footer.columns);
    assert_eq!(source.footer.dictionaries, full.footer.dictionaries);
    assert_eq!(source.footer.freetext, full.footer.freetext);
    assert_eq!(source.footer.presence, full.footer.presence);
    assert_eq!(source.footer.shredded, full.footer.shredded);
    assert_eq!(source.footer.shared_dictionaries, full.footer.shared_dictionaries);
    assert!(!source.footer.marks.is_empty());
    assert!(
        source
            .footer
            .granules
            .iter()
            .zip(&full.footer.granules)
            .all(|(source, candidate)| {
                source.granule_id == candidate.granule_id
                    && source.first_row_ordinal == candidate.first_row_ordinal
                    && source.row_count == candidate.row_count
                    && source.stripe_id == candidate.stripe_id
            })
    );
    let changes: Vec<HefRowChange> = ordinals
        .into_iter()
        .map(|ordinal| HefRowChange {
            after: after[ordinal].clone(),
            before: before[ordinal].clone(),
            row_ordinal: ordinal as u64,
        })
        .collect();
    let updated =
        update_hef_file_from_changes_with_executor(&source, after, &changes, &config(), &SerialEncodeExecutor).unwrap();
    assert!(updated.reused_source_blocks > 0);
    assert!(updated.reused_source_block_bytes > 0);
    assert!(updated.encoded_blocks < full.encoded_blocks);
    assert!(updated.bytes.len() <= full.bytes.len() + full.bytes.len() / 100);

    let expected = HefFile::open(full.bytes, Some(&full.file_seal)).unwrap();
    let actual = HefFile::open(updated.bytes, Some(&updated.file_seal)).unwrap();
    assert_eq!(actual.footer().columns, expected.footer().columns);
    assert_eq!(actual.footer().granules, expected.footer().granules);
    for granule in &expected.footer().granules {
        for column in &expected.footer().columns {
            assert_eq!(
                actual.read_column(column.column_id, granule.granule_id).unwrap().data,
                expected.read_column(column.column_id, granule.granule_id).unwrap().data,
                "column {} granule {}",
                column.column_id,
                granule.granule_id
            );
        }
        for ordinal in granule.first_row_ordinal..granule.first_row_ordinal + u64::from(granule.row_count) {
            assert_eq!(
                actual.payload(ordinal).unwrap(),
                expected.payload(ordinal).unwrap(),
                "row {ordinal}"
            );
        }
    }
}

#[test]
fn incompatible_schema_and_row_geometry_fall_back_to_a_full_rewrite() {
    let before: Vec<HefRow> = (0..64).map(row).collect();
    let source = build_hef_file(before.clone(), &config()).unwrap();

    let mut changed_schema = config();
    changed_schema.promotion.columns.push(PromotedColumn {
        name: "amount_copy".to_owned(),
        path: "amount".to_owned(),
        kind: ColumnKind::I64,
        since_schema_version: 1,
        substring_searchable: false,
    });
    let full_schema = build_hef_file(before.clone(), &changed_schema).unwrap();
    let updated_schema =
        update_hef_file_with_executor(&source, &before, before.clone(), &changed_schema, &SerialEncodeExecutor)
            .unwrap();
    assert_eq!(updated_schema.reused_source_blocks, 0);
    assert_eq!(updated_schema.bytes, full_schema.bytes);

    let after_delete = before[..63].to_vec();
    let full_delete = build_hef_file(after_delete.clone(), &config()).unwrap();
    let updated_delete =
        update_hef_file_with_executor(&source, &before, after_delete, &config(), &SerialEncodeExecutor).unwrap();
    assert_eq!(updated_delete.reused_source_blocks, 0);
    assert_eq!(updated_delete.bytes, full_delete.bytes);

    let dense_changes: Vec<HefRowChange> = (0..16)
        .map(|ordinal| HefRowChange {
            after: before[ordinal].clone(),
            before: before[ordinal].clone(),
            row_ordinal: ordinal as u64,
        })
        .collect();
    let dense = update_hef_file_from_changes_with_executor(
        &source,
        before.clone(),
        &dense_changes,
        &config(),
        &SerialEncodeExecutor,
    )
    .unwrap();
    assert_eq!(dense.reused_source_blocks, 0);
    assert_eq!(dense.bytes, source.bytes);
}

struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }
}

/// The update configuration the sparse-update tests share: one stripe for the whole file, so nothing but the
/// granule cut can move a boundary.
fn sparse_config() -> HefBuildConfig {
    let mut cfg = config();
    cfg.targets.stripe_target_bytes = 1 << 20;
    cfg
}

fn payload_of(row: &mut HefRow) -> &mut BTreeMap<String, VariantValue> {
    let PayloadInput::Variant(VariantValue::Object(payload)) = &mut row.event.payload else {
        panic!("test rows carry object payloads");
    };
    payload
}

/// One random edit of a row. The first three kinds change only a payload value or the dedupe hash, which a sparse
/// update applies; the `wide` kinds also change a promoted column, a path's presence or the path set, which sends
/// the update down the ordinary path.
fn mutate(row: &mut HefRow, rng: &mut Lcg, wide: bool, ordinal: usize) {
    match rng.below(if wide { 6 } else { 3 }) {
        0 => {
            payload_of(row).insert("amount".to_owned(), VariantValue::Int(rng.below(1_000_000) as i64));
        }
        1 => {
            let note = format!("rewritten note {ordinal} {}", rng.next());
            payload_of(row).insert("note".to_owned(), VariantValue::String(note));
        }
        2 => row.event.envelope.dedupe_hash_low = rng.next(),
        3 => {
            let kind = format!("k{}", rng.below(4));
            payload_of(row).insert("kind".to_owned(), VariantValue::String(kind));
        }
        4 => {
            let payload = payload_of(row);
            if payload.remove("rare").is_none() {
                payload.insert("rare".to_owned(), VariantValue::Bool(true));
            }
        }
        _ => {
            payload_of(row).insert("extra".to_owned(), VariantValue::Int(ordinal as i64));
        }
    }
}

fn changes_for(before: &[HefRow], after: &[HefRow], ordinals: &[usize]) -> Vec<HefRowChange> {
    ordinals
        .iter()
        .map(|&ordinal| HefRowChange {
            after: after[ordinal].clone(),
            before: before[ordinal].clone(),
            row_ordinal: ordinal as u64,
        })
        .collect()
}

/// The reuse plan a change set over event rows yields, once both are in the writer's shape.
fn plan_reuse<'a>(
    source: &'a BuiltHef,
    after: &[HefRow],
    changes: &[HefRowChange],
    cfg: &HefBuildConfig,
) -> Option<UpdateReuse<'a>> {
    plan_change_set_reuse(
        source,
        &HefRow::into_build_rows(after.to_vec()),
        &HefRow::build_changes(changes),
        cfg,
    )
}

/// The update built the way an update without a sparse plan builds it: every row normalized, every granule built,
/// unchanged blocks borrowed from the source.
fn ordinary_update(source: &BuiltHef, after: &[HefRow], changes: &[HefRowChange], cfg: &HefBuildConfig) -> BuiltHef {
    let reuse = plan_reuse(source, after, changes, cfg).map(|reuse| UpdateReuse { sparse: None, ..reuse });
    build_hef_file_sealed(
        HefRow::into_build_rows(after.to_vec()),
        cfg,
        &SerialEncodeExecutor,
        None,
        None,
        reuse.as_ref(),
    )
    .unwrap()
}

fn assert_same_content(expected: &BuiltHef, actual: &BuiltHef) {
    let expected = HefFile::open(expected.bytes.clone(), Some(&expected.file_seal)).unwrap();
    let actual = HefFile::open(actual.bytes.clone(), Some(&actual.file_seal)).unwrap();
    assert_eq!(actual.footer().columns, expected.footer().columns);
    assert_eq!(actual.footer().granules, expected.footer().granules);
    assert_eq!(actual.footer().page_stats, expected.footer().page_stats);
    for granule in &expected.footer().granules {
        for column in &expected.footer().columns {
            assert_eq!(
                actual.read_column(column.column_id, granule.granule_id).unwrap().data,
                expected.read_column(column.column_id, granule.granule_id).unwrap().data,
                "column {} granule {}",
                column.column_id,
                granule.granule_id
            );
        }
        for ordinal in granule.first_row_ordinal..granule.first_row_ordinal + u64::from(granule.row_count) {
            assert_eq!(
                actual.payload(ordinal).unwrap(),
                expected.payload(ordinal).unwrap(),
                "row {ordinal}"
            );
        }
    }
}

/// Random mutation sets over a 25-granule file (two replay segments): the update from the full replacement rows,
/// which builds only the touched granules when it can, is byte-for-byte what the ordinary update builds from every
/// row; the changes-only update, which reads the touched granules back from the source, matches too wherever the
/// change set allows it and refuses otherwise; and the decoded content is a full build's.
#[test]
fn sparse_update_matches_the_ordinary_update_byte_for_byte_over_random_mutations() {
    let cfg = sparse_config();
    let before: Vec<HefRow> = (0..400).map(row).collect();
    let source = build_hef_file(before.clone(), &cfg).unwrap();
    let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);
    let mut sparse_rounds = 0;
    for round in 0..24 {
        let mut after = before.clone();
        let count = 1 + rng.below(40);
        let mut ordinals: Vec<usize> = (0..count).map(|_| rng.below(before.len())).collect();
        ordinals.sort_unstable();
        ordinals.dedup();
        let wide = round % 4 == 3;
        for &ordinal in &ordinals {
            mutate(&mut after[ordinal], &mut rng, wide, ordinal);
        }
        let changes = changes_for(&before, &after, &ordinals);
        let sparse = plan_reuse(&source, &after, &changes, &cfg)
            .and_then(|reuse| reuse.sparse)
            .is_some();

        let ordinary = ordinary_update(&source, &after, &changes, &cfg);
        let updated =
            update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
                .unwrap();
        assert_eq!(updated.bytes, ordinary.bytes, "round {round}");
        assert_eq!(
            updated.reused_source_blocks, ordinary.reused_source_blocks,
            "round {round}"
        );

        let from_source =
            update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor);
        if sparse {
            sparse_rounds += 1;
            assert_eq!(from_source.unwrap().bytes, ordinary.bytes, "round {round}");
        } else {
            assert!(from_source.is_err(), "round {round}");
        }
        if round % 6 == 0 {
            assert_same_content(&build_hef_file(after.clone(), &cfg).unwrap(), &updated);
        }
    }
    assert!(sparse_rounds >= 12, "{sparse_rounds} sparse rounds");
}

/// A change confined to one tail granule of the second replay segment builds that granule and the segment's head —
/// whose values the tail's encode replays — and no other, and still reproduces the ordinary update.
#[test]
fn a_tail_granule_change_builds_only_itself_and_its_segment_head() {
    let cfg = sparse_config();
    let before: Vec<HefRow> = (0..400).map(row).collect();
    let source = build_hef_file(before.clone(), &cfg).unwrap();
    let mut after = before.clone();
    let ordinals: Vec<usize> = (320..336).step_by(3).collect();
    for &ordinal in &ordinals {
        payload_of(&mut after[ordinal]).insert("amount".to_owned(), VariantValue::Int(-(ordinal as i64)));
    }
    let changes = changes_for(&before, &after, &ordinals);
    let plan = plan_reuse(&source, &after, &changes, &cfg)
        .and_then(|reuse| reuse.sparse)
        .expect("a payload-only change set plans a sparse update");
    let built: Vec<usize> = (0..plan.build.len())
        .filter(|&index| plan.builds(index).is_some())
        .collect();
    assert_eq!(built, [16, 20]);
    assert!(plan.builds(16).unwrap().is_subset(plan.builds(20).unwrap()));

    let ordinary = ordinary_update(&source, &after, &changes, &cfg);
    let updated =
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
            .unwrap();
    assert_eq!(updated.bytes, ordinary.bytes);
    let from_source =
        update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor).unwrap();
    assert_eq!(from_source.bytes, ordinary.bytes);
    assert!(updated.reused_source_blocks > 0);
}

/// The sparse update's bytes do not depend on how many threads normalize its granules or encode its blocks.
#[test]
fn sparse_update_is_deterministic_across_thread_counts() {
    let cfg = sparse_config();
    let before: Vec<HefRow> = (0..400).map(row).collect();
    let source = build_hef_file(before.clone(), &cfg).unwrap();
    let mut after = before.clone();
    let mut rng = Lcg(7);
    let ordinals: Vec<usize> = (0..400).step_by(13).collect();
    for &ordinal in &ordinals {
        mutate(&mut after[ordinal], &mut rng, false, ordinal);
    }
    let changes = changes_for(&before, &after, &ordinals);
    assert!(
        plan_reuse(&source, &after, &changes, &cfg)
            .and_then(|reuse| reuse.sparse)
            .is_some()
    );

    let single_thread = rayon::ThreadPoolBuilder::new().num_threads(1).build().unwrap();
    let serial = single_thread.install(|| {
        (
            update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
                .unwrap(),
            update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor).unwrap(),
        )
    });
    let pooled =
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &ThreadPoolEncodeExecutor)
            .unwrap();
    let pooled_from_source =
        update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &ThreadPoolEncodeExecutor).unwrap();
    assert_eq!(serial.0.bytes, pooled.bytes);
    assert_eq!(serial.1.bytes, pooled.bytes);
    assert_eq!(pooled_from_source.bytes, pooled.bytes);
}

/// The changes-only update refuses what it cannot apply from the source alone — an envelope change, a change to a
/// promoted column, a new payload path — rather than building a wrong file.
#[test]
fn changes_only_update_refuses_what_needs_the_full_rows() {
    let cfg = sparse_config();
    let before: Vec<HefRow> = (0..64).map(row).collect();
    let source = build_hef_file(before.clone(), &cfg).unwrap();
    let refused = |edit: fn(&mut HefRow)| {
        let mut after = before.clone();
        edit(&mut after[5]);
        let changes = changes_for(&before, &after, &[5]);
        update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor)
            .err()
            .map(|error| error.to_string())
    };
    assert!(refused(|row| row.event.envelope.actor_id = Some("actor-9".to_owned())).is_some());
    assert!(
        refused(|row| {
            payload_of(row).insert("kind".to_owned(), VariantValue::String("k9".to_owned()));
        })
        .is_some()
    );
    assert!(
        refused(|row| {
            payload_of(row).insert("extra".to_owned(), VariantValue::Int(1));
        })
        .is_some()
    );
    // The same edits update fine from the full replacement rows.
    let mut after = before.clone();
    after[5].event.envelope.actor_id = Some("actor-9".to_owned());
    let changes = changes_for(&before, &after, &[5]);
    let updated =
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
            .unwrap();
    assert_eq!(updated.bytes, ordinary_update(&source, &after, &changes, &cfg).bytes);
}

/// Rows with a residual-only binary field, whose value can grow without changing the path statistics.
fn row_with_blob(i: u64) -> HefRow {
    let mut built = row(i);
    payload_of(&mut built).insert("blob".to_owned(), VariantValue::Binary(vec![i as u8; 8]));
    built
}

fn granule_bytes(built: &BuiltHef, granule: &GranuleEntry) -> usize {
    built
        .footer
        .page_stats
        .iter()
        .filter(|stats| stats.granule_id == granule.granule_id)
        .map(|stats| stats.row_count as usize * 8)
        .sum::<usize>()
        + granule.compressed_bytes_estimate as usize
}

/// A rebuilt residual arena that grows enough to push the next granule into another stripe changes the file's
/// geometry, which is more than a sparse update can borrow around: the update from the full rows starts over the
/// ordinary way — with its rows restored exactly, so the result is a full build's — and the changes-only update
/// refuses.
#[test]
fn a_residual_that_moves_a_stripe_boundary_falls_back_to_the_ordinary_update() {
    let before: Vec<HefRow> = (0..96).map(row_with_blob).collect();
    let probe = build_hef_file(before.clone(), &sparse_config()).unwrap();
    assert!(probe.footer.shredded.iter().all(|entry| entry.path != "blob"));
    let mut cfg = config();
    cfg.targets.stripe_target_bytes =
        granule_bytes(&probe, &probe.footer.granules[0]) + granule_bytes(&probe, &probe.footer.granules[1]) + 16;
    let source = build_hef_file(before.clone(), &cfg).unwrap();
    assert_eq!(source.footer.granules[0].stripe_id, source.footer.granules[1].stripe_id);
    assert_ne!(source.footer.granules[1].stripe_id, source.footer.granules[2].stripe_id);

    let mut after = before.clone();
    payload_of(&mut after[3]).insert("blob".to_owned(), VariantValue::Binary(vec![7; 200]));
    let changes = changes_for(&before, &after, &[3]);
    assert!(
        plan_reuse(&source, &after, &changes, &cfg)
            .and_then(|reuse| reuse.sparse)
            .is_some()
    );
    let updated =
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
            .unwrap();
    let full = build_hef_file(after.clone(), &cfg).unwrap();
    assert_eq!(updated.reused_source_blocks, 0);
    assert_eq!(updated.bytes, full.bytes);
    assert_ne!(full.footer.granules[1].stripe_id, full.footer.granules[0].stripe_id);
    assert!(update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor).is_err());
}

/// The same for a granule the source cut by payload bytes: a residual that grows past the byte target moves the cut,
/// so the update from the full rows starts over and the changes-only update refuses.
#[test]
fn a_residual_that_moves_a_byte_cut_falls_back_to_the_ordinary_update() {
    let mut cfg = sparse_config();
    cfg.targets.index_granularity = 1_000;
    cfg.targets.index_granularity_bytes = 2_000;
    let before: Vec<HefRow> = (0..96).map(row_with_blob).collect();
    let source = build_hef_file(before.clone(), &cfg).unwrap();
    assert!(source.footer.granules.len() > 2);
    assert!(source.footer.granules.iter().all(|granule| granule.row_count < 1_000));

    let mut after = before.clone();
    payload_of(&mut after[1]).insert("blob".to_owned(), VariantValue::Binary(vec![7; 1_500]));
    let changes = changes_for(&before, &after, &[1]);
    assert!(
        plan_reuse(&source, &after, &changes, &cfg)
            .and_then(|reuse| reuse.sparse)
            .is_some()
    );
    let updated =
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
            .unwrap();
    let full = build_hef_file(after.clone(), &cfg).unwrap();
    assert_eq!(updated.bytes, full.bytes);
    assert_ne!(full.footer.granules[0].row_count, source.footer.granules[0].row_count);
    assert!(update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor).is_err());

    // A residual change that stays under the target keeps the cut and the sparse update.
    let mut after = before.clone();
    payload_of(&mut after[1]).insert("blob".to_owned(), VariantValue::Binary(vec![7; 9]));
    let changes = changes_for(&before, &after, &[1]);
    let updated =
        update_hef_file_from_changes_with_executor(&source, after.clone(), &changes, &cfg, &SerialEncodeExecutor)
            .unwrap();
    assert!(updated.reused_source_blocks > 0);
    assert_eq!(updated.bytes, ordinary_update(&source, &after, &changes, &cfg).bytes);
    let from_source =
        update_hef_file_from_source_changes_with_executor(&source, &changes, &cfg, &SerialEncodeExecutor).unwrap();
    assert_eq!(from_source.bytes, updated.bytes);
}

/// A remote reader authenticates the fixed header and exact tail first, then fetches only the proof-leaf-expanded
/// bytes for one stripe range. The stripe checksum is both the footer leaf and the Bao-style proof root, so corrupt
/// range bytes are rejected without downloading the rest of the stripe or maintaining a second whole-file hash.
#[test]
fn authenticated_footer_proves_a_remote_stripe_range() {
    let rows: Vec<HefRow> = (0..64)
        .map(|i| {
            let mut built = row(i);
            let mut state = i.wrapping_add(1);
            let note: String = (0..32 * 1024)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    char::from(33 + ((state >> 32) % 94) as u8)
                })
                .collect();
            let PayloadInput::Variant(VariantValue::Object(ref mut payload)) = built.event.payload else {
                unreachable!()
            };
            payload.insert("note".to_owned(), VariantValue::String(note));
            built
        })
        .collect();
    let mut cfg = config();
    cfg.page_size_rows = 16;
    cfg.targets.index_granularity = 64;
    cfg.targets.index_granularity_bytes = 64 * 1024 * 1024;
    cfg.targets.stripe_target_bytes = 64 * 1024 * 1024;
    let built = build_hef_file(rows, &cfg).unwrap();
    assert!(built.tree_len.is_some(), "a multi-megabyte stripe emits proof nodes");
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).expect("the eager reader accepts generated proof trees");

    let tail_range = tail_range(built.total_len, Some(built.footer_len), built.tree_len);
    let tail = &built.bytes[tail_range.start as usize..];
    let authenticated = HefFooter::open_authenticated(
        &built.bytes[..HEADER_BLOCK_LEN],
        tail,
        built.total_len,
        &built.file_seal,
        None,
    )
    .unwrap();
    assert!(authenticated.is_authenticated());
    let mut wrong_seal = built.file_seal;
    wrong_seal[0] ^= 1;
    assert!(
        HefFooter::open_authenticated(
            &built.bytes[..HEADER_BLOCK_LEN],
            tail,
            built.total_len,
            &wrong_seal,
            None,
        )
        .is_err(),
        "proof roots must be bound to the manifest's segment seal"
    );

    let stripe = built
        .footer
        .stripes
        .iter()
        .find(|stripe| stripe.byte_len > crate::file::constant::CHUNK_GROUP_BYTES as u64)
        .expect("at least one stripe spans multiple proof groups");
    let requested_offset = crate::file::constant::CHUNK_GROUP_BYTES as u64 - 31;
    let request = authenticated
        .plan_verified_stripe_read(stripe.stripe_id, requested_offset, 97)
        .unwrap();
    assert!(
        request.length < stripe.byte_len,
        "the GET is range-native, not a whole-stripe fallback"
    );
    let fetched = &built.bytes[request.file_offset as usize..(request.file_offset + request.length) as usize];
    let verified = authenticated.verify_stripe_range(&request, fetched).unwrap();
    assert_eq!(
        verified,
        &built.bytes
            [(stripe.file_offset + requested_offset) as usize..(stripe.file_offset + requested_offset + 97) as usize]
    );

    let mut corrupt = fetched.to_vec();
    corrupt[crate::file::constant::CHUNK_GROUP_BYTES - 1] ^= 1;
    assert!(authenticated.verify_stripe_range(&request, &corrupt).is_err());

    let planning_only = HefFooter::open(tail).unwrap();
    assert!(
        planning_only
            .plan_verified_stripe_read(stripe.stripe_id, requested_offset, 97)
            .is_err(),
        "an unauthenticated footer can plan metadata but must never vouch for content bytes"
    );
}

/// A speculative fetch that covers the whole footer opens directly; one that underflows reports the exact byte count to
/// re-fetch, and that exact count is the recorded `footer_len` — the single exact retry.
#[test]
fn speculative_open_underflows_then_hits_exact_length() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();

    // The whole file is a large-enough speculative tail: the footer opens directly.
    match HefFooter::open_speculative(&built.bytes).unwrap() {
        SpeculativeTail::Opened(footer) => assert_eq!(footer.footer().granules.len(), built.footer.granules.len()),
        SpeculativeTail::NeedsExactTail { tail_len } => panic!("whole file should not underflow, asked for {tail_len}"),
    }

    // A tail a few bytes short of the footer region underflows and names exactly `footer_len` bytes to re-fetch.
    let short_start = built.bytes.len() - (built.footer_len as usize - 5);
    match HefFooter::open_speculative(&built.bytes[short_start..]).unwrap() {
        SpeculativeTail::NeedsExactTail { tail_len } => {
            assert_eq!(tail_len, built.footer_len);
            let exact = &built.bytes[built.bytes.len() - tail_len as usize..];
            assert!(HefFooter::open(exact).is_ok(), "exact retry opens the footer");
        }
        SpeculativeTail::Opened(_) => panic!("a short tail must underflow"),
    }
}

#[test]
fn build_and_read_round_trip() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    // Granules respect the row target; the shred plan picked the frequent typed paths; free text moved by declaration.
    assert!(built.footer.granules.len() >= 4);
    assert!(built.footer.shredded.iter().any(|entry| entry.path == "amount"));
    assert!(!built.footer.shredded.iter().any(|entry| entry.path == "note"));
    assert_eq!(built.footer.freetext.len(), 1);

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    assert_eq!(file.header().row_count, 64);
    // Sequence-ordered granules with authoritative marks: every required column decodes per granule.
    for granule in &file.footer().granules {
        let sequences = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        let ColumnData::U64(values) = sequences.data else {
            panic!("sequence is u64");
        };
        assert_eq!(values.first().copied().unwrap(), granule.first_sequence);
        assert_eq!(values.last().copied().unwrap(), granule.last_sequence);
    }
    // Payload reconstruction: the deterministic merge restores the original canonical value for every row
    // (payload-complete file).
    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal}");
    }
    // Single-path extraction without sibling decode: shredded and residual paths both answer.
    assert_eq!(file.payload_path(5, "amount").unwrap(), Some(VariantValue::Int(1005)));
    assert_eq!(file.payload_path(6, "rare").unwrap(), Some(VariantValue::Bool(true)));
    assert_eq!(file.payload_path(5, "missing").unwrap(), None);
    // Pruning through the sequence and time skip indexes.
    let hits = file.granules_for_sequence(1, 1, 16);
    assert!(!hits.is_empty() && hits.len() < file.footer().granules.len());
    let none = file.granules_for_sequence(2, 1, 16);
    assert!(none.is_empty());
}

/// The benchmark's no-trailing lifecycle builds a file whose every column block stores its body uncompressed and whose
/// content reads back exactly as the fresh-publication file's does, so the two can be timed against each other over
/// identical rows.
#[test]
fn a_build_without_trailing_stages_reads_back_the_same_rows() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let published = build_hef_file(rows.clone(), &config()).unwrap();
    let control = build_hef_file(
        rows,
        &HefBuildConfig {
            lifecycle: BuildLifecycle::FreshPublicationWithoutTrailing,
            ..config()
        },
    )
    .unwrap();
    let published = HefFile::open(published.bytes, None).unwrap();
    let control = HefFile::open(control.bytes, None).unwrap();
    assert_eq!(published.header().row_count, control.header().row_count);
    for granule in &control.footer().granules {
        for column in &control.footer().columns {
            let mark = control.mark(column.column_id, 0, granule.granule_id).unwrap().unwrap();
            assert_eq!(
                mark.codec_pipeline_id.compression().unwrap(),
                Compression::None,
                "column {} granule {}",
                column.column_id,
                granule.granule_id
            );
            assert_eq!(
                control.read_column(column.column_id, granule.granule_id).unwrap().data,
                published
                    .read_column(column.column_id, granule.granule_id)
                    .unwrap()
                    .data,
                "column {} granule {}",
                column.column_id,
                granule.granule_id
            );
        }
    }
    for ordinal in 0..64 {
        assert_eq!(control.payload(ordinal).unwrap(), published.payload(ordinal).unwrap());
    }
}

/// The per-row byte-offset index for declared free-text columns is opt-in: a default build stores no second copy of
/// the free text and declares no `typed_column_row_offsets`, and a build that asks for the index gets one entry per
/// granule per declared free-text column. Implements `hef-column-design` — "Per-row byte-offset index makes wide typed
/// columns point-accessible".
#[test]
fn the_freetext_row_offset_index_is_opt_in() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();

    let default_build = build_hef_file(rows.clone(), &config()).unwrap();
    assert_eq!(
        default_build.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a default build declares no per-row offset index"
    );
    assert!(
        default_build.footer.freetext_row_offsets.is_empty(),
        "a default build stores no per-row offset index"
    );

    let mut opted_in = config();
    opted_in.freetext_row_offset_index = true;
    let indexed_build = build_hef_file(rows, &opted_in).unwrap();
    assert_ne!(
        indexed_build.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0,
        "a build that opts in declares the per-row offset index"
    );
    assert_eq!(
        indexed_build.footer.freetext_row_offsets.len(),
        indexed_build.footer.granules.len(),
        "one row-offset index entry per granule for the one declared free-text column"
    );
    assert!(
        indexed_build.bytes.len() > default_build.bytes.len(),
        "the index costs bytes the default build does not spend"
    );
}

/// Every declared searchable string column — a promoted string column or a declared free-text field — carries an
/// encoded token-membership filter, keyed by column, granule, and page, so a scan can prove a queried string absent
/// and skip the block without reading its bytes. The filter bytes live in the data area: the footer records only
/// their byte ranges, so a cold open — which fetches the footer alone — never pays for them, and the reader resolves
/// a filter lazily on first demand. Undeclared string columns (identity columns, paths the statistics shredded)
/// carry none: no declaration marks them searchable, and their filters would prune nothing.
#[test]
fn string_column_blocks_carry_a_text_token_filter() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();

    // The filter bytes are out of the footer: only offsets remain inline.
    assert!(
        built.footer.text_token_indexes.is_empty(),
        "no filter bytes ride the footer"
    );
    assert!(
        built.footer.optional_feature_flags & crate::layout::optional_features::TEXT_TOKEN_FILTER_OFFSETS != 0,
        "the relocation is declared as an optional feature"
    );

    // The promoted "kind" column is a string column; every granule block of it must carry a filter range.
    let kind_id = built
        .footer
        .columns
        .iter()
        .find(|descriptor| descriptor.name == "kind")
        .expect("promoted kind column present")
        .column_id;
    let kind_entries: Vec<_> = built
        .footer
        .text_token_offsets
        .iter()
        .filter(|entry| entry.column_id == kind_id)
        .collect();
    assert_eq!(
        kind_entries.len(),
        built.footer.granules.len(),
        "one unpaged filter entry per granule for the kind column"
    );

    // The lazily resolved filter answers membership: a token every granule stores is present, a foreign token is
    // provably absent. Values are k0..k3, so every 16-row granule holds all four.
    let file = HefFile::open(built.bytes, None).unwrap();
    for granule in &file.footer().granules {
        let index = file
            .text_token_filter(kind_id, granule.granule_id, 0)
            .unwrap()
            .expect("the kind column's filter resolves from the data area");
        assert!(index.might_contain_token("k1"));
        assert!(!index.might_contain_token("no-such-kind"));

        // Non-string columns never carry a filter, and undeclared string columns (entity_id) carry none either.
        assert!(
            file.text_token_filter(column_ids::SEQUENCE, granule.granule_id, 0)
                .unwrap()
                .is_none()
        );
        assert!(
            file.text_token_filter(column_ids::ENTITY_ID, granule.granule_id, 0)
                .unwrap()
                .is_none()
        );
    }
}

/// Trigrams are a per-column declaration, not a property of the free-text family. A promoted or context-projection
/// string column declared `substring_searchable` carries them too, so a `CONTAINS` against it proves a needle absent
/// and skips the granule. Left undeclared, the same column's filter still answers whole-value equality, and a
/// substring probe conservatively keeps every block — correct, but no pruning at all.
#[test]
fn a_column_declared_substring_searchable_carries_trigrams() {
    let rows: Vec<HefRow> = (0..64)
        .map(|i| {
            let mut r = row(i);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            map.insert(
                "title".to_owned(),
                VariantValue::String(format!("quarterly revenue report {i}")),
            );
            r
        })
        .collect();

    // "title" is the second promoted column (index 1), after "kind" from the base config; "region" is the first
    // context projection.
    let title_id = column_ids::PROMOTED_BASE + 1;
    let region_id = column_ids::CONTEXT_BASE;
    let build_with = |substring_searchable: bool| {
        let mut cfg = config();
        cfg.promotion.columns.push(PromotedColumn {
            kind: ColumnKind::String,
            name: "title".to_owned(),
            path: "title".to_owned(),
            since_schema_version: 1,
            substring_searchable,
        });
        cfg.analytical_columns = vec![AnalyticalColumn {
            column_id: region_id,
            data: ColumnData::Strings(
                (0..rows.len())
                    .map(|i| Some(format!("northwest district {i}")))
                    .collect(),
            ),
            internal_only: false,
            kind: ColumnKind::String,
            name: "region".to_owned(),
            substring_searchable,
        }];
        let built = build_hef_file(rows.clone(), &cfg).unwrap();
        HefFile::open(built.bytes, None).unwrap()
    };

    // A token every row of the column holds, so its presence is not a granule accident.
    let columns = [(title_id, "revenue"), (region_id, "district")];

    let undeclared = build_with(false);
    for granule in &undeclared.footer().granules {
        for (column_id, present) in columns {
            let index = undeclared
                .text_token_filter(column_id, granule.granule_id, 0)
                .unwrap()
                .expect("a searchable string column carries a token filter either way");
            assert!(!index.has_ngrams(), "no trigrams without the declaration");
            assert!(
                index.might_contain_token(present),
                "whole-value tokens are still answered"
            );
            assert!(
                index.might_contain_substring("zeppelin"),
                "without trigrams a substring probe cannot prune, so the granule is kept",
            );
        }
    }

    let declared = build_with(true);
    for granule in &declared.footer().granules {
        for (column_id, present) in columns {
            let index = declared
                .text_token_filter(column_id, granule.granule_id, 0)
                .unwrap()
                .expect("a searchable string column carries a token filter either way");
            assert!(index.has_ngrams(), "the declaration puts trigrams on the filter");
            assert!(
                index.might_contain_token(present),
                "whole-value tokens are still answered"
            );
            assert!(index.might_contain_substring(present));
            assert!(
                !index.might_contain_substring("zeppelin"),
                "a needle whose trigrams the block never saw is proved absent",
            );
        }
    }
}

/// The substring declaration only reaches a column that carries a text-token filter at all: a string column, and for an
/// analytical column a public one. Anywhere else it would be silently ignored — bytes paid, nothing pruned, or nothing
/// paid and nothing pruned — so the build refuses the mismatch instead of dropping it.
#[test]
fn substring_search_declared_where_no_filter_carries_it_is_rejected() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();

    let mut promoted = config();
    promoted.promotion.columns.push(PromotedColumn {
        kind: ColumnKind::I64,
        name: "amount_promoted".to_owned(),
        path: "amount".to_owned(),
        since_schema_version: 1,
        substring_searchable: true,
    });
    let err = build_hef_file(rows.clone(), &promoted).unwrap_err();
    assert!(
        matches!(
            err,
            FormatError::Structural { rule } if rule == "only a string promoted column may be declared substring-searchable"
        ),
        "{err:?}",
    );

    let analytical = |kind, data, internal_only| AnalyticalColumn {
        column_id: column_ids::CONTEXT_BASE,
        data,
        internal_only,
        kind,
        name: "ctx".to_owned(),
        substring_searchable: true,
    };
    let text = || ColumnData::Strings((0..rows.len()).map(|i| Some(format!("value {i}"))).collect());
    for column in [
        analytical(ColumnKind::I64, ColumnData::I64(vec![0i64; rows.len()]), false),
        analytical(ColumnKind::String, text(), true),
    ] {
        let mut cfg = config();
        cfg.analytical_columns = vec![column];
        let err = build_hef_file(rows.clone(), &cfg).unwrap_err();
        assert!(
            matches!(
                err,
                FormatError::Structural { rule }
                    if rule == "only a public string analytical column may be declared substring-searchable"
            ),
            "{err:?}",
        );
    }
}

/// Each stripe's columnar marks pages are placed in the data area immediately after that stripe's filter bytes: the
/// filters and the marks pages tile one contiguous tail extent inside the stripe (gaps only from block-alignment
/// padding), ending exactly at the stripe's end, so a single ranged IO fetches both. The pages decode back to exactly
/// the stripe's slice of the authoritative row-oriented marks and per-page directory — the co-location the marks
/// requirement permits ("co-located with the stripe or in the footer region"), governed by the `STRIPE_MARKS_PAGES`
/// required feature. A reader resolves every mark and per-page entry through those co-located pages, byte-identical
/// to the directory the writer laid out.
#[test]
fn stripe_marks_pages_are_colocated_with_the_stripes_filter_bytes() {
    // Paged blocks too, so the co-located pages carry a per-page directory alongside the granule-level marks.
    let mut cfg = config();
    cfg.page_size_rows = 8;
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows, &cfg).unwrap();

    assert!(
        built.footer.required_feature_flags & crate::layout::required_features::STRIPE_MARKS_PAGES != 0,
        "the placement is declared as a required feature"
    );
    assert!(built.footer.stripes.len() > 1, "the fixture spans several stripes");

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for stripe in &built.footer.stripes {
        let granules_of_stripe: hashbrown::HashSet<u32> = built
            .footer
            .granules
            .iter()
            .filter(|granule| granule.stripe_id == stripe.stripe_id)
            .map(|granule| granule.granule_id)
            .collect();

        // Decode identity: the reader's marks — which resolve only through the co-located data-area pages under
        // `STRIPE_MARKS_PAGES` — equal the directory the writer laid out, mark for mark and page for page.
        for mark in built
            .footer
            .marks
            .iter()
            .filter(|mark| granules_of_stripe.contains(&mark.granule_id))
        {
            assert_eq!(
                file.mark(mark.column_id, mark.projection_id, mark.granule_id).unwrap(),
                Some(*mark),
                "mark for column {} granule {}",
                mark.column_id,
                mark.granule_id
            );
        }
        let mut by_block: std::collections::BTreeMap<(u32, u32, u32), Vec<_>> = std::collections::BTreeMap::new();
        for page in built
            .footer
            .page_directory
            .iter()
            .filter(|page| granules_of_stripe.contains(&page.granule_id))
        {
            by_block
                .entry((page.column_id, page.projection_id, page.granule_id))
                .or_default()
                .push(*page);
        }
        for ((column_id, projection_id, granule_id), expected_pages) in by_block {
            assert_eq!(
                file.page_directory_for(column_id, projection_id, granule_id).unwrap(),
                expected_pages,
                "page directory for column {column_id} granule {granule_id}"
            );
        }

        // Placement: the stripe's filter ranges and marks pages, in file order, form one contiguous tail — filters
        // first, every marks page after them, gaps bounded by the 64-byte block alignment, ending at the stripe end.
        let mut extents: Vec<(u64, u64, bool)> = Vec::new();
        for entry in built
            .footer
            .text_token_offsets
            .iter()
            .filter(|entry| granules_of_stripe.contains(&entry.granule_id))
        {
            extents.push((stripe.file_offset + entry.index_offset, entry.index_len, false));
        }
        for entry in built
            .footer
            .entity_hash_filters
            .iter()
            .filter(|entry| granules_of_stripe.contains(&entry.granule_id))
        {
            extents.push((stripe.file_offset + entry.index_offset, entry.index_len, false));
        }
        for entry in built
            .footer
            .marks_page_offsets
            .iter()
            .filter(|entry| entry.stripe_id == stripe.stripe_id)
        {
            extents.push((stripe.file_offset + entry.page_offset, entry.page_len, true));
        }
        extents.sort_by_key(|&(start, ..)| start);
        let first_marks_page = extents
            .iter()
            .position(|&(_, _, is_marks_page)| is_marks_page)
            .expect("the stripe has marks pages");
        assert!(first_marks_page > 0, "the stripe has filter bytes ahead of its pages");
        assert!(
            extents
                .get(first_marks_page..)
                .unwrap_or_default()
                .iter()
                .all(|&(_, _, is_marks_page)| is_marks_page),
            "every marks page follows the filters"
        );
        for pair in extents.windows(2) {
            let [(a_start, a_len, _), (b_start, ..)] = pair else {
                continue;
            };
            let gap = b_start - (a_start + a_len);
            assert!(gap < 64, "gap of {gap} bytes exceeds block-alignment padding");
        }
        let (last_start, last_len, _) = *extents.last().unwrap();
        assert_eq!(
            last_start + last_len,
            stripe.file_offset + stripe.byte_len,
            "the co-located tail ends exactly at the stripe end, inside its checksummed range"
        );
    }
}

#[test]
fn a_file_with_an_out_of_order_occurred_at_is_flagged_as_carrying_late_events() {
    // Rows whose occurred_at rises with ingest (epoch, sequence) order carry no late event.
    let ordered: Vec<HefRow> = (0..32).map(row).collect();
    let built = build_hef_file(ordered.clone(), &config()).unwrap();
    assert_eq!(
        built.footer.optional_feature_flags & optional_features::HEF_LATE_EVENTS,
        0,
        "a file with occurred_at in ingest order must not declare late events"
    );

    // Give the last-ingested row an occurred_at older than every earlier row: a late event.
    let mut late = ordered;
    if let Some(last) = late.last_mut() {
        last.event.envelope.occurred_at = TimestampValue::from_physical_nanos(1);
    }
    let built = build_hef_file(late, &config()).unwrap();
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::HEF_LATE_EVENTS,
        0,
        "a row whose occurred_at predates an earlier-ingested row must declare late events"
    );
}

#[test]
fn paged_columns_round_trip_with_non_byte_aligned_pages() {
    let rows: Vec<HefRow> = (0..17).map(row).collect();
    let mut cfg = config();
    cfg.page_size_rows = 5;
    cfg.targets.index_granularity = 64;
    cfg.targets.index_granularity_bytes = 1 << 30;

    let built = build_hef_file(rows.clone(), &cfg).unwrap();
    assert_eq!(built.footer.granules.len(), 1, "test expects one paged granule");
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::PER_PAGE_MARKS,
        0,
        "paged files must declare the page directory feature",
    );

    let granule_id = built.footer.granules[0].granule_id;
    let sequence_mark = built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == column_ids::SEQUENCE && mark.granule_id == granule_id)
        .expect("sequence mark");
    assert_eq!(sequence_mark.page_count, 4);
    let mut sequence_pages: Vec<_> = built
        .footer
        .page_directory
        .iter()
        .filter(|entry| entry.column_id == column_ids::SEQUENCE && entry.granule_id == granule_id)
        .collect();
    sequence_pages.sort_by_key(|entry| entry.page_index);
    assert_eq!(
        sequence_pages
            .iter()
            .map(|entry| (entry.page_index, entry.first_row_ordinal, entry.row_count))
            .collect::<Vec<_>>(),
        vec![(0, 0, 5), (1, 5, 5), (2, 10, 5), (3, 15, 2)]
    );
    assert!(
        sequence_pages
            .windows(2)
            .all(|window| { window[0].compressed_offset + window[0].compressed_len == window[1].compressed_offset }),
        "page directory entries must point at each contiguous encoded page"
    );

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for page_index in 0..sequence_pages.len() {
        let page = file
            .read_page(column_ids::SEQUENCE, granule_id, page_index as u32)
            .unwrap();
        assert_eq!(
            page.data,
            ColumnData::U64(
                (sequence_pages[page_index].first_row_ordinal + 1
                    ..=sequence_pages[page_index].first_row_ordinal + u64::from(sequence_pages[page_index].row_count))
                    .collect()
            ),
            "plain page {page_index} must read its own [start, end) range"
        );
    }

    let sequences = file.read_column(column_ids::SEQUENCE, granule_id).unwrap();
    assert_eq!(
        sequences.data,
        ColumnData::U64((1..=17).collect()),
        "plain paged columns must concatenate every page range"
    );

    let promoted = file.read_column(column_ids::PROMOTED_BASE, granule_id).unwrap();
    let ColumnData::Strings(promoted_values) = &promoted.data else {
        panic!("promoted kind is string data");
    };
    assert_eq!(promoted_values.len(), rows.len());

    let second_promoted_page = file.read_page(column_ids::PROMOTED_BASE, granule_id, 1).unwrap();
    let ColumnData::Strings(second_promoted_values) = &second_promoted_page.data else {
        panic!("promoted kind is string data");
    };
    assert_eq!(
        second_promoted_values.iter().collect::<Vec<_>>(),
        vec![Some("k1"), Some("k2"), Some("k3"), Some("k0"), Some("k1")],
        "presence-gated page 1 must slice dense values by value offset plus present count"
    );
    assert_eq!(
        second_promoted_page.presence[0] & 0b0001_1111,
        0b0001_1111,
        "each page presence bitmap is repacked from bit 0"
    );

    for row_index in 0..rows.len() {
        assert_ne!(
            promoted.presence[row_index / 8] & (1 << (row_index % 8)),
            0,
            "presence bit {row_index} must survive bit-level page append"
        );
    }

    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present for row {ordinal}");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal}");
    }
}

/// A column the promotion plan justified earns per-page min/max, one entry per physical page with bounds scoped to
/// that page's own rows; a column the plan does not mention (however queryable) defaults to granule-level stats and
/// carries no page-level entries at all.
#[test]
fn page_minmax_emitted_for_hot_column_and_empty_for_cold_column() {
    let rows: Vec<HefRow> = (0..17).map(row).collect();
    let mut cfg = config();
    cfg.promotion.columns.push(PromotedColumn {
        name: "amount_promoted".to_owned(),
        path: "amount".to_owned(),
        kind: ColumnKind::I64,
        since_schema_version: 1,
        substring_searchable: false,
    });
    cfg.page_size_rows = 5;
    cfg.targets.index_granularity = 64;
    cfg.targets.index_granularity_bytes = 1 << 30;

    let built = build_hef_file(rows.clone(), &cfg).unwrap();
    let granule_id = built.footer.granules[0].granule_id;
    let amount_column_id = column_ids::PROMOTED_BASE + 1;

    let mut amount_pages: Vec<_> = built
        .footer
        .page_minmax
        .iter()
        .filter(|entry| entry.column_id == amount_column_id && entry.granule_id == granule_id)
        .collect();
    amount_pages.sort_by_key(|entry| entry.page_index);
    assert_eq!(
        amount_pages
            .iter()
            .map(|entry| (entry.page_index, entry.min_i128, entry.max_i128, entry.row_count))
            .collect::<Vec<_>>(),
        vec![
            (0, Some(1000), Some(1004), 5),
            (1, Some(1005), Some(1009), 5),
            (2, Some(1010), Some(1014), 5),
            (3, Some(1015), Some(1016), 2),
        ],
        "each hot page's bounds must match its own slice of values, not the granule-wide range"
    );

    assert!(
        built
            .footer
            .page_minmax
            .iter()
            .all(|entry| entry.column_id != column_ids::SEQUENCE),
        "a required column the promotion plan does not mention must carry no page-level minmax entries, even though \
         it is split into the same physical pages as the hot column"
    );
}

/// A single-page column has only page 0, so asking for any higher page index is out of range. The fallback read path
/// (no per-page directory entry, `page_count <= 1`) must reject that index rather than silently return page 0 — which
/// would hand the caller the whole column while claiming it was a page that does not exist.
#[test]
fn read_page_rejects_out_of_range_index_on_single_page_column() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let granule_id = built.footer.granules[0].granule_id;

    assert!(
        file.read_page(column_ids::SEQUENCE, granule_id, 0).is_ok(),
        "page 0 of a single-page column reads as the whole column",
    );
    assert!(
        matches!(
            file.read_page(column_ids::SEQUENCE, granule_id, 7),
            Err(FormatError::RefOutOfRange { .. }),
        ),
        "an index past the only page must be rejected, not read as page 0",
    );
}

/// Sequence numbers reset at each epoch, so a granule may cover at most one epoch — otherwise its
/// `(first_sequence, last_sequence)` range would splice two independent sequence spaces and sequence pruning would read
/// them as one interval. Rows spanning two epochs must land in two single-epoch granules, and pruning for one epoch must
/// return only that epoch's granule.
#[test]
fn granules_never_span_more_than_one_epoch() {
    let mut rows: Vec<HefRow> = Vec::new();
    for epoch in 1..=2u64 {
        for sequence in 1..=6u64 {
            let mut r = row(sequence);
            r.epoch = epoch;
            r.sequence = sequence;
            rows.push(r);
        }
    }
    let mut cfg = config();
    cfg.targets.index_granularity = 64;
    cfg.targets.index_granularity_bytes = 1 << 30;

    let built = build_hef_file(rows.clone(), &cfg).unwrap();
    assert_eq!(
        built.footer.granules.len(),
        2,
        "the epoch change forces a granule boundary even though one granule would otherwise fit every row",
    );
    for granule in &built.footer.granules {
        assert_eq!(
            granule.first_epoch, granule.last_epoch,
            "each granule covers exactly one epoch, so its sequence range is epoch-scoped",
        );
    }

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let epoch_one = file.granules_for_sequence(1, 1, 6);
    assert_eq!(epoch_one.len(), 1);
    assert_eq!(epoch_one[0].first_epoch, 1);
    let epoch_two = file.granules_for_sequence(2, 1, 6);
    assert_eq!(epoch_two.len(), 1);
    assert_eq!(epoch_two[0].first_epoch, 2);
}

/// A decimal column stores one fixed scale, but values may arrive at different scales. The writer must lift every value
/// to the largest scale present, so `1.5` (scale 1) and `1.50` (scale 2) both read back as their true magnitude instead
/// of the second inheriting the first's scale and decoding as `15.0`.
#[test]
fn mixed_scale_decimals_normalize_to_the_largest_scale() {
    let prices = [(15i128, 1u8), (150, 2), (2567, 3)];
    let rows: Vec<HefRow> = prices
        .iter()
        .enumerate()
        .map(|(i, &(unscaled, scale))| {
            let mut r = row(i as u64);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            map.insert("price".to_owned(), VariantValue::Decimal { scale, unscaled });
            r
        })
        .collect();
    let mut cfg = config();
    cfg.promotion.columns.push(PromotedColumn {
        kind: ColumnKind::Decimal,
        name: "price".to_owned(),
        path: "price".to_owned(),
        since_schema_version: 1,
        substring_searchable: false,
    });

    let built = build_hef_file(rows.clone(), &cfg).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let granule_id = built.footer.granules[0].granule_id;
    // "price" is the second promoted column (index 1), after "kind" from the base config.
    let column = file.read_column(column_ids::PROMOTED_BASE + 1, granule_id).unwrap();
    assert_eq!(
        column.data,
        ColumnData::Decimal {
            scale: 3,
            values: vec![1500, 1500, 2567],
        },
        "each value is lifted to scale 3: 1.5 and 1.50 become 1500, 2.567 stays 2567",
    );
}

/// Under `STRIPE_RELATIVE_MARKS` a mark's `compressed_offset` — and every payload-arena offset (dictionary / offsets /
/// residual) — is measured from its stripe's base (`StripeEntry.file_offset`), not the start of the file. So relocating
/// a stripe rewrites only that one base-offset entry: every mark and every payload offset already sits within
/// `[0, stripe.byte_len)`, and the stripe's bytes — and its BLAKE3 — move unchanged. This proves the offset domain and
/// its stripe bound without rewriting a single mark or payload entry, and that reads still resolve through
/// `stripe_base + relative`.
#[test]
fn marks_are_stripe_relative_and_bounded_by_their_stripe() {
    use crate::layout::{HEADER_BLOCK_LEN, required_features};
    use hashbrown::HashMap;

    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();

    // The refusing feature is declared in both the footer directory and the header mirror.
    assert_ne!(
        built.footer.required_feature_flags & required_features::STRIPE_RELATIVE_MARKS,
        0,
        "footer must declare STRIPE_RELATIVE_MARKS",
    );
    assert_ne!(
        built.header.feature_flags & required_features::STRIPE_RELATIVE_MARKS,
        0,
        "header must mirror the required feature",
    );
    // Multiple stripes, so at least one stripe base sits well past the header — where relative and absolute diverge.
    assert!(
        built.footer.stripes.len() >= 2,
        "test needs multiple stripes to be meaningful"
    );

    let stripe: HashMap<u32, (u64, u64)> = built
        .footer
        .stripes
        .iter()
        .map(|s| (s.stripe_id, (s.file_offset, s.byte_len)))
        .collect();
    let stripe_of_granule: HashMap<u32, u32> = built
        .footer
        .granules
        .iter()
        .map(|g| (g.granule_id, g.stripe_id))
        .collect();

    let mut min_offset_in_stripe: HashMap<u32, u64> = HashMap::new();
    for mark in &built.footer.marks {
        let stripe_id = stripe_of_granule[&mark.granule_id];
        let (base, byte_len) = stripe[&stripe_id];
        // Every mark resolves inside its own stripe's byte range: relocating the stripe rewrites one base entry, never
        // this mark.
        assert!(
            mark.compressed_offset + mark.compressed_size <= byte_len,
            "mark ({}, {}) offset {} + size {} escapes stripe {} (len {})",
            mark.column_id,
            mark.granule_id,
            mark.compressed_offset,
            mark.compressed_size,
            stripe_id,
            byte_len,
        );
        // The resolved file position stays within the stripe's absolute span.
        let absolute = base + mark.compressed_offset;
        assert!(absolute >= base && absolute + mark.compressed_size <= base + byte_len);
        let entry = min_offset_in_stripe.entry(stripe_id).or_insert(u64::MAX);
        *entry = (*entry).min(mark.compressed_offset);
    }
    // The first block of every stripe sits exactly at the stripe base (relative offset 0), and at least one stripe base
    // is past the header — the non-trivial case a single-stripe file would not exercise.
    assert!(
        min_offset_in_stripe.values().all(|&m| m == 0),
        "each stripe's first block is at its base (relative offset 0)",
    );
    assert!(
        stripe.values().any(|&(base, _)| base > HEADER_BLOCK_LEN as u64),
        "a later stripe must start past the header",
    );

    // The payload arena (dictionary / offsets / residual per granule) is stripe-relative too: every arena range sits
    // within its granule's stripe, so relocating the stripe rewrites one base entry and touches no payload offset.
    assert!(
        !built.footer.payload_granules.is_empty(),
        "test needs payload granules to be meaningful",
    );
    for payload in &built.footer.payload_granules {
        let stripe_id = stripe_of_granule[&payload.granule_id];
        let (base, byte_len) = stripe[&stripe_id];
        for (what, offset, len) in [
            ("dictionary", payload.dictionary_offset, payload.dictionary_len),
            ("offsets", payload.offsets_offset, payload.offsets_len),
            ("residual", payload.residual_offset, payload.residual_len),
        ] {
            assert!(
                offset + len <= byte_len,
                "payload {what} for granule {} offset {offset} + len {len} escapes stripe {stripe_id} (len {byte_len})",
                payload.granule_id,
            );
            // The resolved file position stays within the stripe's absolute span.
            let absolute = base + offset;
            assert!(absolute >= base && absolute + len <= base + byte_len);
        }
    }

    // Reads still resolve correctly through stripe_base + relative for every granule.
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for granule in &file.footer().granules {
        let read = file.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        let ColumnData::U64(values) = read.data else {
            panic!("sequence is u64");
        };
        assert_eq!(values.len(), granule.row_count as usize);
        assert_eq!(values[0], granule.first_sequence);
    }
    // Payload reads resolve through stripe_base + relative on the arena paths (residual slot, residual bytes, and the
    // granule dictionary) too.
    let mut saw_value = false;
    for row in 0..rows.len() as u64 {
        if let PayloadRead::Value(_) = file.payload(row).expect("payload read resolves") {
            saw_value = true;
        }
    }
    assert!(
        saw_value,
        "at least one row's payload resolves through the stripe-relative arena",
    );
}

/// A granule's residual arena is sized from an estimate of what the rows leave behind, not a bound on it. A payload
/// that keeps a long string and a nested object in its residual costs more than the estimate's flat per-row header
/// allowance, so this is the case where the arena has to grow past its reservation — and every row must still land
/// intact when it does.
#[test]
fn residuals_larger_than_the_arena_estimate_still_round_trip() {
    let rows: Vec<HefRow> = (0..48)
        .map(|index| {
            let mut built = row(index);
            let PayloadInput::Variant(VariantValue::Object(fields)) = &mut built.event.payload else {
                panic!("the generated payload is an object");
            };
            fields.insert("essay".to_owned(), VariantValue::String("x".repeat(4096)));
            fields.insert(
                "details".to_owned(),
                VariantValue::Object(BTreeMap::from([
                    ("depth".to_owned(), VariantValue::Int(index as i64)),
                    (
                        "tags".to_owned(),
                        VariantValue::Array(vec![VariantValue::String("t".repeat(64))]),
                    ),
                ])),
            );
            built
        })
        .collect();

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    for (ordinal, input) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadInput::Variant(expected) = &input.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal}");
    }
}

#[test]
fn identical_input_builds_identical_files() {
    let rows: Vec<HefRow> = (0..32).map(row).collect();
    let first = build_hef_file(rows.clone(), &config()).unwrap();
    let second = build_hef_file(rows.clone(), &config()).unwrap();
    assert_eq!(first.bytes, second.bytes);
    assert_eq!(first.file_id, second.file_id);
    assert_eq!(first.file_seal, second.file_seal);
}

/// An FSST string column spanning more granules than one replay segment: every granule's block is FSST, each reads
/// back its rows exactly (later blocks compressed with their segment head's captured table), and a second build is
/// byte-identical.
#[test]
fn an_fsst_column_replays_its_heads_table_across_granules_and_round_trips() {
    let values: Vec<Option<String>> = (0..5_000u64)
        .map(|i| Some(format!("user-{}@example-{}.test", i * 7919 % 100_003, i % 13)))
        .collect();
    let mut cfg = config();
    cfg.targets.index_granularity = 256;
    cfg.analytical_columns = vec![AnalyticalColumn {
        column_id: column_ids::CONTEXT_BASE,
        internal_only: false,
        kind: ColumnKind::String,
        name: "ctx".to_owned(),
        data: ColumnData::Strings(values.clone().into()),
        substring_searchable: false,
    }];
    let rows: Vec<HefRow> = (0..5_000).map(row).collect();
    let built = build_hef_file(rows.clone(), &cfg).unwrap();
    assert!(built.footer.granules.len() > REPLAY_SEGMENT_GRANULES);
    let marks: Vec<_> = built
        .footer
        .marks
        .iter()
        .filter(|mark| mark.column_id == column_ids::CONTEXT_BASE)
        .collect();
    assert!(!marks.is_empty());
    assert!(
        marks.iter().all(|mark| mark
            .codec_pipeline_id
            .transform()
            .is_ok_and(|transform| transform == Transform::FsstString)),
        "every granule of the column stores an FSST block"
    );

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let mut first_row = 0usize;
    for granule in &file.footer().granules {
        let read = file.read_column(column_ids::CONTEXT_BASE, granule.granule_id).unwrap();
        let expected = values
            .get(first_row..first_row + granule.row_count as usize)
            .unwrap()
            .to_vec();
        assert_eq!(
            read.data,
            ColumnData::Strings(expected.into()),
            "granule {}",
            granule.granule_id
        );
        first_row += granule.row_count as usize;
    }
    assert_eq!(first_row, values.len());
    assert_eq!(build_hef_file(rows, &cfg).unwrap().bytes, built.bytes);
}

/// The file dictionaries are the sorted distinct values of each envelope string column whatever the cardinality:
/// one value, a few dozen, thousands, or one per row (where the sort sees every row again and must still agree).
#[test]
fn file_dictionaries_are_the_sorted_distinct_values_at_every_cardinality() {
    let row_count = 3_000u64;
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    for cardinality in [1, 2, 37, 500, 2_999, row_count] {
        let mut draw = |i: u64| if cardinality == row_count { i } else { next(cardinality) };
        let rows: Vec<HefRow> = (0..row_count)
            .map(|i| {
                let mut row = row(i);
                let envelope = &mut row.event.envelope;
                envelope.source = format!("source-{}", draw(i));
                envelope.event_type = format!("type-{}", draw(i));
                envelope.entity_type = format!("entity-{}", draw(i));
                row
            })
            .collect();
        let expected = |value: fn(&EventEnvelope) -> &str| -> Vec<String> {
            let distinct: std::collections::BTreeSet<&str> = rows.iter().map(|r| value(&r.event.envelope)).collect();
            distinct.into_iter().map(str::to_owned).collect()
        };
        let expected_source = expected(|e| &e.source);
        let expected_event_type = expected(|e| &e.event_type);
        let expected_entity_type = expected(|e| &e.entity_type);
        let built = build_hef_file(rows, &config()).unwrap();
        assert_eq!(
            built.footer.dictionaries.source, expected_source,
            "cardinality {cardinality}"
        );
        assert_eq!(
            built.footer.dictionaries.event_type, expected_event_type,
            "cardinality {cardinality}"
        );
        assert_eq!(
            built.footer.dictionaries.entity_type, expected_entity_type,
            "cardinality {cardinality}"
        );
        if cardinality == row_count {
            assert_eq!(built.footer.dictionaries.source.len(), row_count as usize);
        }
    }
}

/// The executor is scheduling only: granule assembly, block encoding, footer metadata, and stripe integrity all
/// reassemble their products in index order, so serial execution and differently sized production pools must emit the
/// same file.
#[test]
fn thread_pool_encode_builds_the_same_bytes_as_the_serial_executor() {
    use crate::invariants::io::ThreadPoolEncodeExecutor;
    use crate::invariants::sim::SerialEncodeExecutor;
    let rows: Vec<HefRow> = (0..256).map(row).collect();
    let serial = build_hef_file_with_executor(rows.clone(), &config(), &SerialEncodeExecutor).unwrap();
    for thread_count in [1, 2, 4] {
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(thread_count)
            .build()
            .expect("test pool");
        let pooled =
            pool.install(|| build_hef_file_with_executor(rows.clone(), &config(), &ThreadPoolEncodeExecutor).unwrap());
        assert_eq!(serial.bytes, pooled.bytes, "{thread_count} workers");
        assert_eq!(serial.file_id, pooled.file_id, "{thread_count} workers");
        assert_eq!(serial.file_seal, pooled.file_seal, "{thread_count} workers");
    }
}

#[test]
fn footer_metadata_groups_tiny_granule_jobs_per_worker() {
    assert_eq!(metadata_job_count(0, 8), 0);
    assert_eq!(metadata_job_count(3, 8), 1);
    assert_eq!(metadata_job_count(13, 8), 4);
    assert_eq!(metadata_job_count(100, 6), 6);
    assert_eq!(metadata_job_count(100, 1), 1);
}

#[test]
fn profiled_build_preserves_bytes_and_reports_each_phase_once() {
    use crate::invariants::sim::SerialEncodeExecutor;
    let rows: Vec<HefRow> = (0..256).map(row).collect();
    let ordinary = build_hef_file_with_executor(rows.clone(), &config(), &SerialEncodeExecutor).unwrap();
    let (profiled, profile) = build_hef_file_profiled_with_executor(rows, &config(), &SerialEncodeExecutor).unwrap();
    assert_eq!(profiled.bytes, ordinary.bytes);
    assert_eq!(profile.phases.len(), 10);
    for phase in [
        BuildPhase::Normalization,
        BuildPhase::DictionaryConstruction,
        BuildPhase::PathStatistics,
        BuildPhase::GranuleConstruction,
        BuildPhase::BlockEncoding,
        BuildPhase::Layout,
        BuildPhase::FooterConstruction,
        BuildPhase::Integrity,
        BuildPhase::FileAssembly,
        BuildPhase::RowDestruction,
    ] {
        assert_eq!(
            profile.phases.iter().filter(|(reported, _)| *reported == phase).count(),
            1,
            "{phase:?}"
        );
    }
}

#[test]
fn corrupted_file_refuses() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    // Flip one byte inside a stripe: the checksum directory refuses it.
    let mut corrupted = built.bytes.clone();
    let stripe_offset = built.footer.stripes[0].file_offset as usize;
    corrupted[stripe_offset] ^= 0xFF;
    assert!(HefFile::open(corrupted, None).is_err());
    // Whole-file BLAKE3 against the manifest entry's hash.
    let mut tampered = built.bytes.clone();
    let last = tampered.len() - 20;
    tampered[last] ^= 0x01;
    assert!(HefFile::open(tampered, Some(&built.file_seal)).is_err());
}

#[test]
fn unknown_required_feature_refuses_unknown_optional_ignored() {
    use crate::compat::check_features;
    use crate::error::FormatError;
    // A future required bit beyond this reader's knowledge: refuse.
    let unknown_required = required_features::ALL | (1 << 60);
    assert!(matches!(
        check_features(unknown_required, 0),
        Err(FormatError::UnknownRequiredFeature { .. })
    ));
    // Unknown optional bits are ignored; known ones survive.
    let usable = check_features(
        required_features::ALL,
        optional_features::VARIANT_SHREDDED_FIELD_BLOCKS | (1 << 61),
    )
    .unwrap();
    assert_eq!(usable, optional_features::VARIANT_SHREDDED_FIELD_BLOCKS);
}

#[test]
fn promoted_column_presence_map_gates_by_schema_version() {
    let mut rows: Vec<HefRow> = (0..8).map(row).collect();
    // Half the rows predate the promotion schema version.
    for row in rows.iter_mut().take(4) {
        row.event.envelope.schema_version = 0;
    }
    let mut gated = config();
    gated.promotion.columns[0].since_schema_version = 1;
    let built = build_hef_file(rows.clone(), &gated).unwrap();
    let file = HefFile::open(built.bytes, None).unwrap();
    let promoted = file
        .read_column(column_ids::PROMOTED_BASE, file.footer().granules[0].granule_id)
        .unwrap();
    // Rows 0..4 predate the promotion: absent from the typed column (readers fall back to the payload, never NULL);
    // rows 4..8 present.
    for index in 0..4usize {
        assert_eq!(promoted.presence[index / 8] & (1 << (index % 8)), 0);
    }
    for index in 4..8usize {
        assert_ne!(promoted.presence[index / 8] & (1 << (index % 8)), 0);
    }
    // The payload still answers for the old rows.
    assert!(matches!(file.payload(0).unwrap(), PayloadRead::Value(_)));
}

#[test]
fn encoded_block_uncompressed_len_is_the_decoded_length() {
    // A long run of identical strings dictionary-encodes to a small code stream plus one long dictionary entry, and the
    // trailing compression stage then shrinks that entry — so the block's decoded length is strictly larger than the
    // bytes it stores. This is the value the ColumnMark must carry, not a copy of the compressed length.
    let data = ColumnData::Strings(vec![Some("compressible payload value ".repeat(20)); 2_000].into());
    let encoded = encode_block(&data, false);
    assert!(
        encoded.uncompressed_len >= encoded.bytes.len() as u64,
        "decoded length is never smaller than the stored length",
    );
    assert!(
        encoded.uncompressed_len > encoded.bytes.len() as u64,
        "a compressible block decodes to strictly more bytes than it stores (uncompressed_len={}, bytes={})",
        encoded.uncompressed_len,
        encoded.bytes.len(),
    );
}

#[test]
fn column_marks_record_the_uncompressed_not_the_compressed_size() {
    // One granule holding every row, plus a highly compressible analytical column, guarantees at least one block whose
    // trailing compression stage removes bytes — so its mark's uncompressed_size strictly exceeds its compressed_size,
    // proving the field is the true decoded length rather than a copy of the compressed length.
    let mut cfg = config();
    cfg.targets.index_granularity = 4_096;
    cfg.analytical_columns = vec![AnalyticalColumn {
        column_id: column_ids::CONTEXT_BASE,
        internal_only: false,
        kind: ColumnKind::String,
        name: "ctx".to_owned(),
        data: ColumnData::Strings(vec![Some("compressible payload value ".repeat(20)); 2_000].into()),
        substring_searchable: false,
    }];
    let rows: Vec<HefRow> = (0..2_000).map(row).collect();
    let built = build_hef_file(rows.clone(), &cfg).unwrap();

    assert!(!built.footer.marks.is_empty());
    for mark in &built.footer.marks {
        assert!(
            mark.uncompressed_size >= mark.compressed_size,
            "decoded size must be at least the stored size (column {}, granule {})",
            mark.column_id,
            mark.granule_id,
        );
    }
    assert!(
        built
            .footer
            .marks
            .iter()
            .any(|mark| mark.uncompressed_size > mark.compressed_size),
        "at least one compressed block must decode to more bytes than it stores",
    );
}

#[test]
fn analytical_column_with_wrong_value_count_is_rejected() {
    // The AnalyticalColumn contract is one value per row. A column carrying fewer values than there are rows must fail
    // the build rather than silently slicing into empty granule blocks that still advertise the full granule row count.
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let mut cfg = config();
    cfg.analytical_columns = vec![AnalyticalColumn {
        column_id: column_ids::CONTEXT_BASE,
        internal_only: false,
        kind: ColumnKind::I64,
        name: "short".to_owned(),
        data: ColumnData::I64(vec![0i64; rows.len() - 1]),
        substring_searchable: false,
    }];
    assert!(build_hef_file(rows.clone(), &cfg).is_err());
}

#[test]
fn an_analytical_column_id_that_collides_with_another_column_is_rejected() {
    // Column blocks are addressed by `(column_id, granule_id)` and the reader collects marks into a map on that key,
    // so a duplicate id silently replaces a block rather than conflicting loudly. A caller-supplied analytical column
    // that reuses a core column's id would take `payload_flags`' place and corrupt every read of it (issue #7489).
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let analytical = |column_id| AnalyticalColumn {
        column_id,
        internal_only: false,
        kind: ColumnKind::I64,
        name: format!("analytical-{column_id}"),
        data: ColumnData::I64(vec![0i64; rows.len()]),
        substring_searchable: false,
    };

    // A required column's id.
    let mut cfg = config();
    cfg.analytical_columns = vec![analytical(column_ids::PAYLOAD_FLAGS)];
    assert!(build_hef_file(rows.clone(), &cfg).is_err());

    // A generated free-text column's id.
    let mut cfg = config();
    cfg.freetext = FreetextDeclaration {
        fields: vec!["note".to_owned()],
    };
    cfg.analytical_columns = vec![analytical(column_ids::FREETEXT_BASE)];
    assert!(build_hef_file(rows.clone(), &cfg).is_err());

    // Two analytical columns sharing one id.
    let mut cfg = config();
    cfg.analytical_columns = vec![
        analytical(column_ids::CONTEXT_BASE),
        analytical(column_ids::CONTEXT_BASE),
    ];
    assert!(build_hef_file(rows.clone(), &cfg).is_err());

    // Distinct ids in the analytical ranges still build.
    let mut cfg = config();
    cfg.analytical_columns = vec![
        analytical(column_ids::CONTEXT_BASE),
        analytical(column_ids::EMBEDDING_BASE),
    ];
    assert!(build_hef_file(rows.clone(), &cfg).is_ok());
}

#[test]
fn io_alignment_is_recorded_and_padded_when_configured() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();

    // Zero alignment (the default): the footer records 0, so a reader takes the buffered path.
    let unaligned = build_hef_file(rows.clone(), &config()).unwrap();
    assert_eq!(unaligned.footer.io_alignment_bytes, 0);

    // A configured IO granularity: the footer records exactly that value (never a placeholder zero), and every column
    // mark's compressed_offset is congruent to it, so the aligned direct-read path is usable.
    let mut aligned_config = config();
    aligned_config.io_alignment_bytes = 4_096;
    let aligned = build_hef_file(rows.clone(), &aligned_config).unwrap();
    assert_eq!(aligned.footer.io_alignment_bytes, 4_096);
    for mark in &aligned.footer.marks {
        assert_eq!(
            mark.compressed_offset % 4_096,
            0,
            "block offset must be congruent to the recorded IO alignment (column {}, granule {})",
            mark.column_id,
            mark.granule_id,
        );
    }

    // Alignment changes only the byte layout: the decoded data is identical either way.
    let buffered = HefFile::open(unaligned.bytes.clone(), Some(&unaligned.file_seal)).unwrap();
    let direct = HefFile::open(aligned.bytes.clone(), Some(&aligned.file_seal)).unwrap();
    assert_eq!(buffered.header().row_count, direct.header().row_count);
    for granule in &direct.footer().granules {
        let a = direct.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        let b = buffered.read_column(column_ids::SEQUENCE, granule.granule_id).unwrap();
        assert_eq!(a.data, b.data, "decoded column identical regardless of alignment");
    }
}

/// A recorded IO alignment must be a power of two no larger than the 4 KiB header block. A non-power-of-two value, or one
/// larger than the header block, would leave padded block offsets incongruent with the alignment the footer advertises,
/// so an `O_DIRECT` reader would slice at the wrong boundary. The writer rejects such a value before building.
#[test]
fn a_non_power_of_two_or_oversized_io_alignment_is_rejected() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();

    for bad in [3_u32, 100, 6_000, 8_192] {
        let mut cfg = config();
        cfg.io_alignment_bytes = bad;
        assert!(
            matches!(
                build_hef_file(rows.clone(), &cfg),
                Err(FormatError::Structural { rule })
                    if rule == "io_alignment_bytes must be a power of two no larger than the header block"
            ),
            "io_alignment_bytes = {bad} must be rejected (non-power-of-two or larger than the header block)"
        );
    }

    // A power-of-two alignment no larger than the header block still builds.
    let mut ok = config();
    ok.io_alignment_bytes = 512;
    assert!(build_hef_file(rows.clone(), &ok).is_ok());
}

/// Every column block the writer emits must stay under the reader's `MAX_PAGE_BYTES` (1 MiB) single-read bound: a block
/// larger than that trips the reader's own guard and is unreadable. Guards that the build path never emits one.
#[test]
fn built_column_blocks_stay_under_the_reader_max_page_bound() {
    let rows: Vec<HefRow> = (0..512).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();

    for mark in &built.footer.marks {
        if mark.page_count <= 1 {
            assert!(
                mark.compressed_size <= crate::layout::MAX_PAGE_BYTES,
                "single-page block exceeds MAX_PAGE_BYTES (column {}, granule {}, {} bytes)",
                mark.column_id,
                mark.granule_id,
                mark.compressed_size,
            );
        }
    }
    for page in &built.footer.page_directory {
        assert!(
            page.compressed_len <= crate::layout::MAX_PAGE_BYTES,
            "page block exceeds MAX_PAGE_BYTES (column {}, granule {}, page {}, {} bytes)",
            page.column_id,
            page.granule_id,
            page.page_index,
            page.compressed_len,
        );
    }
    // The reader opens the built file without tripping either bound.
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
}

/// Reconstructing a granule that holds many shredded rows must stay correct — and, because the shredded column is now
/// decoded once and its presence bitmap indexed for rank/select, the work is linear in rows rather than quadratic.
/// Guards the former per-row re-decode + popcount rescan (issue #1368): under the old path this loop re-decoded each
/// shredded block once per row and rescanned its presence bitmap, so it grew as the square of the granule's row count.
#[test]
fn payload_reconstruction_over_many_shredded_rows_stays_correct() {
    let rows: Vec<HefRow> = (0..2_048).map(row).collect();
    let mut config = config();
    // One big granule so a single shredded column block carries every row whose rank is looked up per row.
    config.targets.index_granularity = 4_096;
    config.targets.index_granularity_bytes = 1 << 30;
    let built = build_hef_file(rows.clone(), &config).unwrap();
    assert_eq!(
        built.footer.granules.len(),
        1,
        "the whole batch must land in one granule"
    );
    assert!(built.footer.shredded.iter().any(|entry| entry.path == "amount"));

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present for row {ordinal}");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal}");
    }
    // Single-path extraction over the shredded column answers at every rank, including a late row and an absent value.
    assert_eq!(
        file.payload_path(1_000, "amount").unwrap(),
        Some(VariantValue::Int(2_000))
    );
    assert_eq!(
        file.payload_path(2_047, "amount").unwrap(),
        Some(VariantValue::Int(1_000 + 2_047))
    );
    assert_eq!(file.payload_path(1_000, "missing").unwrap(), None);
}

#[test]
fn rewrite_planner_reuses_real_stripes_and_preserves_file_seal() {
    use crate::layout::HEADER_BLOCK_LEN;
    use crate::writer::compaction::{StripeChange, StripeDisposition, plan_stripe_reuse};

    // A file large enough to span several stripes (the small stripe target splits 2 048 rows apart).
    let rows: Vec<HefRow> = (0..2_048).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert!(
        built.footer.stripes.len() >= 2,
        "need multiple stripes to exercise reuse"
    );

    // Nothing changed: every stripe and its checksum leaf are reused. The authoritative seal belongs to the finished
    // replacement and does not require a raw-byte prefix re-hash.
    let unchanged: Vec<StripeChange> = built
        .footer
        .stripes
        .iter()
        .map(|&stripe| StripeChange {
            changed: false,
            stripe,
            uncertain: false,
        })
        .collect();
    let plan = plan_stripe_reuse(&unchanged, HEADER_BLOCK_LEN as u64);
    assert!(plan.reuses_every_stripe());
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    // Marking the last stripe changed rebuilds only it; earlier leaves remain independently reusable.
    let mut with_change = unchanged.clone();
    let last = with_change.len() - 1;
    with_change[last].changed = true;
    let plan = plan_stripe_reuse(&with_change, HEADER_BLOCK_LEN as u64);
    assert!(matches!(plan.dispositions[last], StripeDisposition::Rebuild { .. }));
}

/// The provider upload checksum absorbs data ranges as they are assembled and the small tail as it is emitted. It is
/// exactly the platform's CRC-64/NVME over the stored object, so a CRC-64/NVME-capable provider (S3) can reject a
/// corrupted commit without a finished-file checksum pass. Identical input yields the same segment seal and checksum,
/// so the HEF identity is independent of the provider it lands on.
#[test]
fn provider_checksum_is_computed_once_alongside_the_segment_seal() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert_eq!(built.file_crc64_nvme, crate::file::integrity::crc64_nvme(&built.bytes));

    let rebuilt = build_hef_file(rows.clone(), &config()).unwrap();
    assert_eq!(built.file_seal, rebuilt.file_seal);
    assert_eq!(built.file_crc64_nvme, rebuilt.file_crc64_nvme);
}

/// A shredded decimal column stores one fixed scale — the first value's — and admits only decimals already at that
/// scale; decimals at any other scale stay in the residual. Every value must read back at its exact `(unscaled, scale)`,
/// never rescaled into the column's scale. Reproduces #2540/#2747: the old max-scale rescale changed a value's scale on
/// read-back.
#[test]
fn mixed_scale_shredded_decimals_round_trip_exactly() {
    let prices = [(15i128, 1u8), (150, 2), (2567, 3), (7, 1), (99, 2), (1, 0)];
    let rows: Vec<HefRow> = prices
        .iter()
        .enumerate()
        .map(|(i, &(unscaled, scale))| {
            let mut r = row(i as u64);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            map.insert("price".to_owned(), VariantValue::Decimal { scale, unscaled });
            r
        })
        .collect();

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert!(
        built.footer.shredded.iter().any(|entry| entry.path == "price"),
        "the fully-present decimal path is shredded",
    );

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal} round-trips exactly");
    }
    // A column-scale value (row 0) and a residual value (row 1) each keep their own scale on a point read.
    assert_eq!(
        file.payload_path(0, "price").unwrap(),
        Some(VariantValue::Decimal { unscaled: 15, scale: 1 }),
    );
    assert_eq!(
        file.payload_path(1, "price").unwrap(),
        Some(VariantValue::Decimal {
            unscaled: 150,
            scale: 2
        }),
    );
}

/// A near-maximum `i128` mantissa at a non-column scale would, under the old max-scale rescale, be multiplied and clamp
/// to `i128::MAX`. It now stays in the residual and round-trips exactly. Reproduces the clamping half of #2747.
#[test]
fn large_mantissa_decimal_round_trips_without_saturating() {
    let huge = i128::MAX - 1;
    let mut rows: Vec<HefRow> = Vec::new();
    // Row 0 fixes the column scale at 3; row 1 carries the huge mantissa at scale 1 (lifting it to scale 3 overflows).
    for (i, (unscaled, scale)) in [(2567i128, 3u8), (huge, 1)].into_iter().enumerate() {
        let mut r = row(i as u64);
        let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
            unreachable!("row payload is a variant object");
        };
        map.insert("price".to_owned(), VariantValue::Decimal { scale, unscaled });
        rows.push(r);
    }
    // Enough further scale-3 rows that the path clears the presence bar and shreds.
    for i in 2..8u64 {
        let mut r = row(i);
        let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
            unreachable!("row payload is a variant object");
        };
        map.insert(
            "price".to_owned(),
            VariantValue::Decimal {
                scale: 3,
                unscaled: i128::from(i),
            },
        );
        rows.push(r);
    }

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    assert_eq!(
        file.payload_path(1, "price").unwrap(),
        Some(VariantValue::Decimal {
            unscaled: huge,
            scale: 1
        }),
        "the large mantissa survives intact instead of clamping to i128::MAX",
    );
    assert_eq!(
        file.payload_path(0, "price").unwrap(),
        Some(VariantValue::Decimal {
            unscaled: 2567,
            scale: 3
        }),
    );
}

/// A `Timestamp` value must read back as a `Timestamp`, never an `Int`. A path that is `Int` on most rows shreds as
/// I64 on that consensus, but its `Timestamp` rows must stay in the residual so their logical type survives. Reproduces
/// the timestamp half of #2539.
#[test]
fn timestamp_field_round_trips_as_timestamp_including_mixed_int_path() {
    let rows: Vec<HefRow> = (0..16)
        .map(|i| {
            let mut r = row(i);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            let value = if i.is_multiple_of(8) {
                VariantValue::Timestamp(TimestampValue::from_physical_nanos(9_000 + i as i64))
            } else {
                VariantValue::Int(500 + i as i64)
            };
            map.insert("when".to_owned(), value);
            r
        })
        .collect();

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert!(
        built.footer.shredded.iter().any(|entry| entry.path == "when"),
        "the Int-consensus path shreds as I64",
    );

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal} round-trips exactly");
    }
    // A Timestamp row reads back as Timestamp (from the residual); an Int row as Int (from the column).
    assert_eq!(
        file.payload_path(0, "when").unwrap(),
        Some(VariantValue::Timestamp(TimestampValue::from_physical_nanos(9_000))),
    );
    assert_eq!(file.payload_path(1, "when").unwrap(), Some(VariantValue::Int(501)));
}

/// A `Float` value must read back as a `Float`, never a `Double`. A pure-`Float` path is never a shred candidate, so it
/// stays in the residual; a `Double` path shreds as F64 and reads back as `Double`; an `Int` path shreds as I64 and
/// reads back as `Int`. Reproduces the float half of #2539.
#[test]
fn float_stays_residual_while_double_and_int_shred() {
    let rows: Vec<HefRow> = (0..16)
        .map(|i| {
            let mut r = row(i);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            map.insert("ratio".to_owned(), VariantValue::Float(1.5 + i as f32));
            map.insert("score".to_owned(), VariantValue::Double(2.5 + i as f64));
            r
        })
        .collect();

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert!(
        !built.footer.shredded.iter().any(|entry| entry.path == "ratio"),
        "a pure-Float path is never shredded",
    );
    assert!(built.footer.shredded.iter().any(|entry| entry.path == "score"));
    assert!(built.footer.shredded.iter().any(|entry| entry.path == "amount"));

    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadRead::Value(value) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadInput::Variant(expected) = &input_row.event.payload else {
            panic!("variant input");
        };
        assert_eq!(&value, expected, "row {ordinal} round-trips exactly");
    }
    assert_eq!(file.payload_path(3, "ratio").unwrap(), Some(VariantValue::Float(4.5)));
    assert_eq!(file.payload_path(3, "score").unwrap(), Some(VariantValue::Double(5.5)));
    assert_eq!(file.payload_path(3, "amount").unwrap(), Some(VariantValue::Int(1003)));
}

/// A promoted decimal column lifts every value to the largest scale present. A value whose magnitude cannot survive
/// that lift must not be silently clamped to `i128::MAX/MIN` — that would corrupt the acceleration copy. The build now
/// fails instead of clamping. Reproduces the overflow half of #2747 on the promoted path.
#[test]
fn promoted_decimal_that_overflows_on_rescale_is_rejected() {
    let huge = i128::MAX - 1;
    // Scale 0 near the i128 ceiling, then a scale-2 value: lifting the first to scale 2 multiplies by 100 and overflows.
    let prices = [(huge, 0u8), (150, 2)];
    let rows: Vec<HefRow> = prices
        .iter()
        .enumerate()
        .map(|(i, &(unscaled, scale))| {
            let mut r = row(i as u64);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            map.insert("price".to_owned(), VariantValue::Decimal { scale, unscaled });
            r
        })
        .collect();
    let mut cfg = config();
    cfg.promotion.columns.push(PromotedColumn {
        kind: ColumnKind::Decimal,
        name: "price".to_owned(),
        path: "price".to_owned(),
        since_schema_version: 1,
        substring_searchable: false,
    });

    let err = build_hef_file(rows.clone(), &cfg).unwrap_err();
    assert!(
        matches!(
            err,
            FormatError::Structural { rule } if rule == "decimal value overflows i128 when rescaled to the column scale"
        ),
        "overflow on promoted rescale must error, not clamp: {err:?}",
    );
}

/// Rows carrying large string bodies must cut granules on `index_granularity_bytes`: an estimate counting only
/// variant keys never reached the byte target, so the granule grew to the row limit, its real residual arena tripped
/// the stripe clamp, and every rebuild of the same range failed identically — a permanently unpublishable range.
#[test]
fn payload_heavy_rows_cut_granules_on_bytes_instead_of_tripping_the_stripe_clamp() {
    let body = "x".repeat(32 * 1024);
    let rows: Vec<HefRow> = (0..32)
        .map(|i| {
            let mut r = row(i);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            // A unique key per row keeps the heavy strings in the residual (no path clears the shred presence bar).
            map.insert(format!("body_{i}"), VariantValue::String(body.clone()));
            r
        })
        .collect();
    let mut cfg = config();
    cfg.targets = LayoutTargets {
        index_granularity: 1024,
        index_granularity_bytes: 64 * 1024,
        max_stripe_bytes: 256 * 1024,
        min_bytes_for_wide: 10 * 1024 * 1024,
        stripe_target_bytes: 128 * 1024,
    };

    let built = build_hef_file(rows.clone(), &cfg).expect("the byte cut must fire before the stripe clamp");
    assert!(
        built.footer.granules.len() > 1,
        "the payload value bytes force byte-target granule cuts"
    );
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    assert_eq!(file.header().row_count, 32);
}

/// The free-text arenas are written inside the stripe, so the stripe clamp must count them: a granule whose declared
/// free-text bodies alone exceed `max_stripe_bytes` must fail the build rather than grow unchecked toward the point
/// where the u32 per-row offset index wraps and corrupts point reads.
#[test]
fn oversized_freetext_arena_is_caught_by_the_stripe_clamp() {
    let rows: Vec<HefRow> = (0..8)
        .map(|i| {
            let mut r = row(i);
            let PayloadInput::Variant(VariantValue::Object(ref mut map)) = r.event.payload else {
                unreachable!("row payload is a variant object");
            };
            // "note" is the declared free-text field, so these bodies move out of the residual into the arena.
            map.insert("note".to_owned(), VariantValue::String("y".repeat(64 * 1024)));
            r
        })
        .collect();
    let mut cfg = config();
    // The arena is what the clamp must see here, so this build opts into it; a default build stores no such arena.
    cfg.freetext_row_offset_index = true;
    // No byte cut and no row cut: the whole batch lands in one granule whose free-text arena alone is oversized.
    cfg.targets = LayoutTargets {
        index_granularity: 1024,
        index_granularity_bytes: 1 << 30,
        max_stripe_bytes: 256 * 1024,
        min_bytes_for_wide: 10 * 1024 * 1024,
        stripe_target_bytes: 128 * 1024,
    };

    let err = build_hef_file(rows.clone(), &cfg).unwrap_err();
    assert!(
        matches!(
            err,
            FormatError::Structural { rule } if rule == "stripe clamp: a single granule exceeds the maximum stripe size"
        ),
        "the clamp must see the free-text arena: {err:?}",
    );
}

/// A default build leaves the footer plaintext: no `FOOTER_ENCRYPTED` header bit, and the footer region is exactly the
/// plaintext `encode_footer` output, so existing readers open it unchanged.
#[test]
fn default_build_leaves_the_footer_plaintext() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert_eq!(built.header.feature_flags & required_features::FOOTER_ENCRYPTED, 0);
    let end = built.bytes.len() - 12;
    let start = end - (built.footer_len as usize - 12);
    assert_eq!(
        &built.bytes[start..end],
        crate::layout::footer::encode_footer(&built.footer).as_slice(),
        "a plaintext build writes the encode_footer output verbatim",
    );
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).expect("plaintext file opens via plain open");
}

/// Sealing the footer requires the caller's file DEK: `Encrypted` with no `footer_dek` is rejected rather than writing
/// an unsealed footer under a "should be encrypted" schema. Implements `hef-security-and-isolation` — "Footer
/// encryption for sensitive schemas".
#[test]
fn encrypted_footer_without_a_key_is_rejected() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let mut cfg = config();
    cfg.footer_encryption = crate::security::FooterEncryption::Encrypted;
    cfg.footer_dek = None;
    assert!(
        build_hef_file(rows.clone(), &cfg).is_err(),
        "an encrypted footer with no key must fail rather than write plaintext",
    );
}

/// The two-GET authenticated cold open works for encrypted metadata too: the stored ciphertext remains part of the
/// segment seal, while the caller's DEK reveals the stripe roots and proof geometry only after that binding succeeds.
#[test]
fn authenticated_footer_open_decrypts_after_binding_the_stored_tail() {
    let dek = [0x5au8; 32];
    let mut cfg = config();
    cfg.footer_encryption = crate::security::FooterEncryption::Encrypted;
    cfg.footer_dek = Some(dek);
    let built = build_hef_file((0..8).map(row).collect(), &cfg).unwrap();
    let range = tail_range(built.total_len, Some(built.footer_len), built.tree_len);
    let tail = &built.bytes[range.start as usize..];

    let opened = HefFooter::open_authenticated(
        &built.bytes[..HEADER_BLOCK_LEN],
        tail,
        built.total_len,
        &built.file_seal,
        Some(&dek),
    )
    .unwrap();
    assert!(opened.is_authenticated());
    assert_eq!(opened.footer().stripe_checksums, built.footer.stripe_checksums);
    assert_eq!(opened.footer().stripe_proofs, built.footer.stripe_proofs);
    assert_eq!(opened.footer().integrity_gaps, built.footer.integrity_gaps);
    assert!(
        HefFooter::open_authenticated(
            &built.bytes[..HEADER_BLOCK_LEN],
            tail,
            built.total_len,
            &built.file_seal,
            None,
        )
        .is_err()
    );
    let wrong_dek = [0xa5u8; 32];
    assert!(
        HefFooter::open_authenticated(
            &built.bytes[..HEADER_BLOCK_LEN],
            tail,
            built.total_len,
            &built.file_seal,
            Some(&wrong_dek),
        )
        .is_err()
    );
}

/// A signed row whose payload is the content and tags the author actually hashed.
fn signed_row(author: &SimulatedEventAuthor, i: u64) -> HefRow {
    let payload = signed_event_payload(&format!("message {i}"), &[&["e", "root"], &["p", "peer"]]);
    let provenance = author
        .sign(1, 1_700_000_000 + i as i64, &payload)
        .expect("the payload is well-shaped");
    let mut row = row(i);
    row.event.payload = PayloadInput::Variant(payload);
    row.event.provenance = Some(provenance);
    row
}

#[test]
fn an_archived_signed_event_re_verifies_offline_from_the_file_alone() {
    // Write signed events, seal the file, then read it back knowing nothing but its bytes: rebuild the canonical
    // serialization, recompute the protocol id, and check the signature. No wire bytes, no trust in the store.
    let author = SimulatedEventAuthor::from_seed(6);
    let rows: Vec<HefRow> = (0..8).map(|i| signed_row(&author, i)).collect();
    let built = build_hef_file(rows.clone(), &config()).expect("a HEF of signed rows builds");
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).expect("the sealed file opens");

    let granule_id = file.footer().granules.first().expect("one granule").granule_id;
    let pubkeys = file
        .read_column(column_ids::AUTHOR_PUBKEY, granule_id)
        .expect("author_pubkey is materialized");
    let signatures = file
        .read_column(column_ids::SIGNATURE, granule_id)
        .expect("signature is materialized");
    let ids = file
        .read_column(column_ids::PROTOCOL_EVENT_ID, granule_id)
        .expect("protocol_event_id is materialized");
    let schemes = file
        .read_column(column_ids::SIGNATURE_SCHEME, granule_id)
        .expect("signature_scheme is materialized");
    let kinds = file
        .read_column(column_ids::PROTOCOL_KIND, granule_id)
        .expect("protocol_kind is materialized");
    let claimed = file
        .read_column(column_ids::CLAIMED_AT, granule_id)
        .expect("claimed_at is materialized");

    let (
        ColumnData::Strings(pubkeys),
        ColumnData::Strings(signatures),
        ColumnData::Strings(ids),
        ColumnData::Strings(schemes),
        ColumnData::I64(kinds),
        ColumnData::I64(claimed),
    ) = (
        pubkeys.data,
        signatures.data,
        ids.data,
        schemes.data,
        kinds.data,
        claimed.data,
    )
    else {
        panic!("provenance columns keep their declared types");
    };

    for (index, row) in rows.iter().enumerate() {
        let scheme = SignatureScheme::from_str(schemes.get(index).flatten().expect("a stored scheme tag"))
            .expect("the tag is in the registry");
        let stored = SignedEventProvenance {
            author_pubkey: hex_bytes(pubkeys.get(index).flatten().expect("a pubkey")).expect("lowercase hex"),
            claimed_at: TimestampValue::from_physical_nanos(*claimed.get(index).expect("a claimed_at")),
            protocol_event_id: hex_bytes(ids.get(index).flatten().expect("an id")).expect("lowercase hex"),
            protocol_kind: u32::try_from(*kinds.get(index).expect("a kind")).expect("kinds are small"),
            scheme,
            signature: hex_bytes(signatures.get(index).flatten().expect("a signature")).expect("lowercase hex"),
        };
        assert_eq!(Some(&stored), row.event.provenance.as_ref(), "row {index} round-trips");

        let PayloadRead::Value(payload) = file.payload(index as u64).expect("the payload reads back") else {
            panic!("a signed event carries an inline payload");
        };
        stored
            .verify(&payload)
            .unwrap_or_else(|error| panic!("archived row {index} must re-verify offline: {error}"));
    }
}

#[test]
fn a_file_of_unsigned_events_materializes_no_provenance_columns() {
    let built = build_hef_file((0..4).map(row).collect::<Vec<_>>(), &config()).expect("a HEF builds");
    assert!(
        !built
            .footer
            .columns
            .iter()
            .any(|column| column.column_id >= column_ids::PROVENANCE_BASE),
        "a stream without signatures pays nothing for the provenance family"
    );
}

/// A point read into a rewritten granule whose residual arena spans several frames inflates only the frame the value
/// falls in — the whole-arena inflation the format used to pay is what the seekable frames exist to avoid.
#[test]
fn a_point_read_into_a_seekable_residual_arena_inflates_one_frame_not_the_whole_arena() {
    // A nested object stays in the residual (only scalars shred), and one granule holds every row, so the arena
    // spans several 64 KiB frames while each row's own span stays small.
    let rows: Vec<HefRow> = (0..2048)
        .map(|index| {
            let mut row = row(index);
            if let PayloadInput::Variant(VariantValue::Object(payload)) = &mut row.event.payload {
                payload.insert(
                    "detail".to_owned(),
                    VariantValue::Object(
                        (0..4)
                            .map(|field| {
                                (
                                    format!("field{field}"),
                                    VariantValue::String(format!("row {index} field {field} of the payload")),
                                )
                            })
                            .collect(),
                    ),
                );
            }
            row
        })
        .collect();
    let one_granule = HefBuildConfig {
        targets: LayoutTargets {
            index_granularity: 2048,
            ..config().targets
        },
        ..config()
    };
    let fresh = build_hef_file(rows.clone(), &one_granule).unwrap();
    let rewritten = build_hef_file(
        rows,
        &HefBuildConfig {
            lifecycle: BuildLifecycle::RewriteOrCompaction,
            ..one_granule
        },
    )
    .unwrap();

    let arena_bytes = fresh.footer.payload_granules.first().unwrap().residual_len;
    assert!(
        arena_bytes > 2 * u64::from(seekable_zstd::FRAME_BYTES),
        "the arena must span several frames for this test to mean anything: {arena_bytes} bytes"
    );
    assert_eq!(
        rewritten.footer.payload_granules.first().unwrap().residual_compression,
        crate::layout::footer::ResidualCompression::ZstdSeekable
    );

    let fresh_file = HefFile::open(fresh.bytes, None).unwrap();
    let rewritten_file = HefFile::open(rewritten.bytes.clone(), None).unwrap();
    for ordinal in [0u64, 1, 1000, 2047] {
        assert_eq!(
            fresh_file.payload(ordinal).unwrap(),
            rewritten_file.payload(ordinal).unwrap(),
            "row {ordinal} must reconstruct identically from the seekable arena"
        );
    }

    // A single point read on a freshly opened reader holds one frame, not the arena it came from.
    let point_reader = HefFile::open(rewritten.bytes, None).unwrap();
    assert_eq!(point_reader.payload(1000).unwrap(), fresh_file.payload(1000).unwrap());
    let held = point_reader.residual_cache_bytes();
    assert!(held > 0, "the read must have inflated the frame it needed");
    assert!(
        held <= u64::from(seekable_zstd::FRAME_BYTES),
        "a point read held {held} bytes of a {arena_bytes}-byte arena; it must hold at most one frame"
    );
}

/// A rewrite/compaction build compresses each granule's residual arena into seekable Zstd-3 frames when that shrinks
/// it, records the choice per granule, and every payload still reads back exactly as from the uncompressed fresh
/// publication — the reader inflates the frames a value falls in and serves point reads from the cached inflation.
#[test]
fn rewrite_lifecycle_compresses_residual_arenas_and_payloads_read_back_identically() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let fresh = build_hef_file(
        rows.clone(),
        &HefBuildConfig {
            lifecycle: BuildLifecycle::FreshPublication,
            ..config()
        },
    )
    .unwrap();
    let rewritten = build_hef_file(
        rows,
        &HefBuildConfig {
            lifecycle: BuildLifecycle::RewriteOrCompaction,
            ..config()
        },
    )
    .unwrap();

    assert!(
        fresh
            .footer
            .payload_granules
            .iter()
            .all(|payload| payload.residual_compression == crate::layout::footer::ResidualCompression::None),
        "fresh publications keep residual arenas uncompressed"
    );
    // The repetitive test payloads compress well, so the rewrite stores at least one compressed arena and fewer
    // residual bytes overall.
    assert!(
        rewritten
            .footer
            .payload_granules
            .iter()
            .any(|payload| payload.residual_compression == crate::layout::footer::ResidualCompression::ZstdSeekable),
        "the rewrite lifecycle compresses residual arenas that shrink"
    );
    let fresh_residual: u64 = fresh.footer.payload_granules.iter().map(|p| p.residual_len).sum();
    let rewritten_residual: u64 = rewritten.footer.payload_granules.iter().map(|p| p.residual_len).sum();
    assert!(rewritten_residual < fresh_residual);

    let fresh_file = HefFile::open(fresh.bytes, None).unwrap();
    let rewritten_file = HefFile::open(rewritten.bytes, None).unwrap();
    for ordinal in 0..64u64 {
        assert_eq!(
            fresh_file.payload(ordinal).unwrap(),
            rewritten_file.payload(ordinal).unwrap(),
            "row {ordinal} must reconstruct identically from the compressed arena"
        );
    }
    // A single-path point read exercises the per-granule inflation cache directly.
    assert_eq!(
        fresh_file.payload_path(3, "amount").unwrap(),
        rewritten_file.payload_path(3, "amount").unwrap()
    );
}

/// Two byte-identical column blocks in one stripe are stored once: the second mark aliases the first extent, both
/// columns decode to their original values, and the build stays deterministic. Implements `hef-file-layout` — "Marks
/// may alias identical extents".
#[test]
fn byte_identical_blocks_within_a_stripe_share_one_extent() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let mut config = config();
    config.analytical_columns = vec![
        AnalyticalColumn {
            column_id: crate::columns::column_ids::CONTEXT_BASE,
            internal_only: false,
            kind: ColumnKind::I64,
            name: "left".to_owned(),
            data: ColumnData::I64((0..8).map(|i| 5_000 + i).collect()),
            substring_searchable: false,
        },
        AnalyticalColumn {
            column_id: crate::columns::column_ids::CONTEXT_BASE + 1,
            internal_only: false,
            kind: ColumnKind::I64,
            name: "right".to_owned(),
            data: ColumnData::I64((0..8).map(|i| 5_000 + i).collect()),
            substring_searchable: false,
        },
    ];
    let built = build_hef_file(rows.clone(), &config).unwrap();
    assert!(built.aliased_block_bytes > 0, "identical blocks must be aliased");

    let left_mark = built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == crate::columns::column_ids::CONTEXT_BASE)
        .unwrap();
    let right_mark = built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == crate::columns::column_ids::CONTEXT_BASE + 1)
        .unwrap();
    assert_eq!(left_mark.compressed_offset, right_mark.compressed_offset);
    assert_eq!(left_mark.compressed_size, right_mark.compressed_size);

    let file = crate::layout::reader::HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for column in [
        crate::columns::column_ids::CONTEXT_BASE,
        crate::columns::column_ids::CONTEXT_BASE + 1,
    ] {
        let read = file.read_column(column, 0).unwrap();
        assert_eq!(read.data, ColumnData::I64((0..8).map(|i| 5_000 + i).collect()));
    }
    // Reader-side read-region dedup: the second aliasing column shares the first one's fetch and decode.
    assert_eq!(
        file.column_block_reads(),
        1,
        "an aliased extent must be fetched and decoded once per scan"
    );

    // Aliasing is deterministic: the same rows build byte-identical files.
    let again = build_hef_file(rows, &config).unwrap();
    assert_eq!(built.bytes, again.bytes);
}

/// Two constant string columns with different values produce byte-identical file-scope code streams: the writer
/// aliases the shared bytes, but each column still decodes through its own footer alphabet — the aliased-extent
/// cache must never serve one column's values to the other.
#[test]
fn aliased_file_scope_dictionary_blocks_decode_each_columns_own_values() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let mut config = config();
    config.analytical_columns = vec![
        AnalyticalColumn {
            column_id: crate::columns::column_ids::CONTEXT_BASE,
            internal_only: false,
            kind: ColumnKind::String,
            name: "left".to_owned(),
            data: ColumnData::Strings((0..64).map(|_| Some("aaaa")).collect()),
            substring_searchable: false,
        },
        AnalyticalColumn {
            column_id: crate::columns::column_ids::CONTEXT_BASE + 1,
            internal_only: false,
            kind: ColumnKind::String,
            name: "right".to_owned(),
            data: ColumnData::Strings((0..64).map(|_| Some("bbbb")).collect()),
            substring_searchable: false,
        },
    ];
    let built = build_hef_file(rows, &config).unwrap();
    let left_col = crate::columns::column_ids::CONTEXT_BASE;
    let right_col = crate::columns::column_ids::CONTEXT_BASE + 1;
    for (column, mark_value) in [(left_col, "aaaa"), (right_col, "bbbb")] {
        assert!(
            built
                .footer
                .shared_dictionaries
                .iter()
                .any(|entry| { entry.column_id == column && entry.values == vec![mark_value.to_owned()] }),
            "column {column} must carry its own one-value file-scope alphabet"
        );
    }
    let left_mark = built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == left_col)
        .unwrap();
    let right_mark = built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == right_col)
        .unwrap();
    assert_eq!(
        left_mark.codec_pipeline_id.side_stream(),
        Ok(crate::encoding::SideStream::FileScopeDictionary),
        "the test must exercise file-scope code streams"
    );
    assert_eq!(left_mark.compressed_offset, right_mark.compressed_offset);
    assert_eq!(left_mark.compressed_size, right_mark.compressed_size);

    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    for (column, expected) in [(left_col, "aaaa"), (right_col, "bbbb")] {
        let read = file.read_column(column, 0).unwrap();
        assert_eq!(
            read.data,
            ColumnData::Strings((0..16).map(|_| Some(expected)).collect()),
            "column {column} must decode through its own alphabet, not a cached alias"
        );
    }
}

/// The build records one distinct-count entry per (promoted column, stripe), exact for a small value set, and the
/// entry round-trips through the real footer bytes. Implements `hef-aggregation-metadata` — "Per-stripe
/// distinct-count estimates for the planner".
#[test]
fn per_stripe_distinct_counts_are_recorded_for_promoted_columns() {
    let rows: Vec<HefRow> = (0..24).map(row).collect();
    let built = build_hef_file(rows, &config()).unwrap();
    assert!(
        !built.footer.stripe_ndv.is_empty(),
        "the promoted column must carry NDV entries"
    );
    for entry in &built.footer.stripe_ndv {
        assert!(entry.exact, "four distinct kinds count exactly");
        assert!(
            (1..=4).contains(&entry.distinct_count),
            "kind takes four values, stripe {} claims {}",
            entry.stripe_id,
            entry.distinct_count
        );
    }
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_eq!(
        file.footer().stripe_ndv,
        built.footer.stripe_ndv,
        "footer bytes round-trip the entries"
    );
}

/// A dense single-page integer block whose exact stats prove one value stores no bytes: its mark is a zero-length
/// extent governed by the `elided_constant_blocks` required bit, reads rebuild it from the stats without a block
/// fetch, and the raw path refuses. Implements `hef-encodings-and-compression` — "Constant and all-null blocks store
/// no data bytes".
#[test]
fn constant_integer_blocks_store_no_bytes_and_rebuild_from_stats() {
    let constant_col = crate::columns::column_ids::CONTEXT_BASE;
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let mut config = config();
    config.analytical_columns = vec![AnalyticalColumn {
        column_id: constant_col,
        internal_only: false,
        kind: ColumnKind::I64,
        name: "constant".to_owned(),
        data: ColumnData::I64(vec![777; 8]),
        substring_searchable: false,
    }];
    let built = build_hef_file(rows, &config).unwrap();
    assert_ne!(
        built.footer.required_feature_flags & crate::layout::required_features::ELIDED_CONSTANT_BLOCKS,
        0,
        "elision is governed by its required bit"
    );
    let mark = *built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == constant_col && mark.granule_id == 0)
        .expect("the constant column has a mark");
    assert_eq!(mark.compressed_size, 0, "a provable constant block must elide its body");

    let file = crate::layout::reader::HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    let expected = ColumnData::I64(vec![777; mark.row_count as usize]);
    let read = file.read_column(constant_col, 0).unwrap();
    assert_eq!(read.data, expected);
    assert!(read.presence.is_empty());
    assert_eq!(
        file.column_block_reads(),
        0,
        "rebuilding from stats must not count as a block fetch"
    );
    assert!(
        file.read_column_raw(constant_col, 0).is_err(),
        "an elided block has no raw bytes"
    );
    assert_eq!(file.read_page(constant_col, 0, 0).unwrap().data, expected);
}

/// A promoted column present on every row records the zero-byte all-present presence form in the real file — the
/// kilobyte-of-set-bits case the encoded side stream exists to remove — governed by the `compressed_presence`
/// required bit, and reads rebuild the same all-set bitmap. Implements `hef-encodings-and-compression` — "Presence
/// and null bitmaps are encoded side streams".
#[test]
fn a_fully_present_promoted_column_stores_the_zero_byte_presence_form() {
    let rows: Vec<HefRow> = (0..24).map(row).collect();
    let built = build_hef_file(rows, &config()).unwrap();
    assert_ne!(
        built.footer.required_feature_flags & crate::layout::required_features::COMPRESSED_PRESENCE,
        0,
        "the encoded presence frame is governed by its required bit"
    );
    let kind_col = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount auto-shreds")
        .column_id;
    let mark = *built
        .footer
        .marks
        .iter()
        .find(|mark| mark.column_id == kind_col && mark.granule_id == 0)
        .expect("kind has a mark");
    assert!(mark.page_count <= 1 && mark.compressed_size > 0);
    let stripe_id = built.footer.granules[0].stripe_id;
    let stripe_base = built
        .footer
        .stripes
        .iter()
        .find(|stripe| stripe.stripe_id == stripe_id)
        .unwrap()
        .file_offset;
    let frame_start = (stripe_base + mark.compressed_offset) as usize;
    assert_eq!(
        built.bytes[frame_start],
        crate::layout::presence_forms::ALL_PRESENT,
        "a fully present block stores the zero-byte form"
    );

    let file = crate::layout::reader::HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    let read = file.read_column(kind_col, 0).unwrap();
    assert_eq!(read.data.row_count() as u32, mark.row_count);
    let all_set = (0..mark.row_count as usize).all(|row| read.presence[row / 8] & (1 << (row % 8)) != 0);
    assert!(all_set, "the rebuilt bitmap marks every row present");
}

/// A type-consistent path present on a quarter of rows — under the dense threshold, above the sparse floor — stores
/// as a sparse shredded column: declared in the footer's sparse key set under its required bit, riding the shredded
/// machinery, and the payload merge reconstructs every row identically. Implements `hef-column-design` — "Sparse
/// shredded columns below the promotion threshold".
#[test]
fn hot_but_sparse_paths_store_as_sparse_shredded_columns() {
    let rows: Vec<HefRow> = (0..64)
        .map(|i| {
            let mut sparse_row = row(i);
            if i % 4 == 0
                && let PayloadInput::Variant(VariantValue::Object(fields)) = &mut sparse_row.event.payload
            {
                fields.insert("burst".to_owned(), VariantValue::Int(9_000 + i as i64));
            }
            sparse_row
        })
        .collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert_ne!(
        built.footer.required_feature_flags & crate::layout::required_features::SPARSE_SHREDDED_COLUMNS,
        0,
        "the sparse tier is governed by its required bit"
    );
    let sparse = built
        .footer
        .sparse_keys
        .iter()
        .find(|entry| entry.path == "burst")
        .expect("burst qualifies for the sparse tier");
    assert!(
        built
            .footer
            .shredded
            .iter()
            .any(|entry| entry.column_id == sparse.column_id && entry.path == "burst"),
        "sparse keys ride the shredded machinery"
    );
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for i in 0..64u64 {
        assert_eq!(
            file.payload_path(i, "burst").unwrap(),
            (i % 4 == 0).then(|| VariantValue::Int(9_000 + i as i64)),
            "row {i}"
        );
    }
    // Tier selection is deterministic: the same rows build byte-identical files.
    let again = build_hef_file(rows, &config()).unwrap();
    assert_eq!(built.bytes, again.bytes);
}

/// More qualifying paths than the pinned budget: exactly the budget store sparse (densest first, ties by path), the
/// rest stay residual, and every value still reads back — the file remains fully correct.
#[test]
fn the_sparse_key_budget_leaves_overflow_paths_residual() {
    let rows: Vec<HefRow> = (0..64)
        .map(|i| {
            let mut sparse_row = row(i);
            if let PayloadInput::Variant(VariantValue::Object(fields)) = &mut sparse_row.event.payload {
                for key in 0..20u64 {
                    if i % 4 == key % 4 {
                        fields.insert(format!("sparse-{key:02}"), VariantValue::Int((i * 100 + key) as i64));
                    }
                }
            }
            sparse_row
        })
        .collect();
    let built = build_hef_file(rows, &config()).unwrap();
    assert_eq!(
        built.footer.sparse_keys.len(),
        crate::columns::SPARSE_KEYS_PER_FILE_MAX,
        "the pinned budget bounds the sparse key set"
    );
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    for i in 0..64u64 {
        for key in 0..20u64 {
            assert_eq!(
                file.payload_path(i, &format!("sparse-{key:02}")).unwrap(),
                (i % 4 == key % 4).then(|| VariantValue::Int((i * 100 + key) as i64)),
                "row {i} key {key}"
            );
        }
    }
}

/// A status-like promoted column repeating the same small value set in every granule stores one file-scope alphabet:
/// the footer declares it under the shared_dictionaries required bit, its blocks record the file scope, and reads
/// resolve codes through the alphabet. Implements `hef-encodings-and-compression` — "Dictionary alphabets may be
/// shared at file scope".
#[test]
fn a_status_like_column_shares_one_file_scope_alphabet() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows.clone(), &config()).unwrap();
    assert_ne!(
        built.footer.required_feature_flags & crate::layout::required_features::SHARED_DICTIONARIES,
        0,
        "file-scope alphabets are governed by their required bit"
    );
    let kind_col = crate::columns::column_ids::PROMOTED_BASE;
    let entry = built
        .footer
        .shared_dictionaries
        .iter()
        .find(|entry| entry.column_id == kind_col)
        .expect("the promoted kind column repeats one value set per granule, so it shares");
    assert_eq!(entry.values, vec!["k0", "k1", "k2", "k3"]);
    assert!(
        built.footer.marks.iter().any(|mark| {
            mark.column_id == kind_col
                && mark.codec_pipeline_id.side_stream() == Ok(crate::encoding::SideStream::FileScopeDictionary)
        }),
        "kind blocks record the file dictionary scope"
    );
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    for (ordinal, input_row) in rows.iter().enumerate() {
        let expected = match &input_row.event.payload {
            PayloadInput::Variant(VariantValue::Object(fields)) => fields.get("kind").cloned(),
            _ => None,
        };
        assert_eq!(
            file.payload_path(ordinal as u64, "kind").unwrap(),
            expected,
            "row {ordinal}"
        );
    }
}

/// A sealed file gains a membership index artifact without a single byte of it changing: the heat-gated plan targets
/// the promoted columns, the built artifact seals and round-trips, its terms prune values the column never holds and
/// keep the rest — and granules the artifact does not cover simply stay on the footer tier. Implements
/// `hef-query-metadata-and-indexes` — "Heavy skip indexes publish as index artifacts outside the file".
#[test]
fn a_sealed_file_gains_a_membership_artifact_without_changing_a_byte() {
    use crate::indexes::artifact::{ArtifactKind, IndexArtifact, build_membership_artifact, plan_artifact_builds};
    use crate::indexes::pruning::{FilterClause, PredicateKind, StatVerdict};
    use crate::indexes::stable_hash;

    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let built = build_hef_file(rows, &config()).unwrap();
    let before = built.bytes.clone();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    let plans = plan_artifact_builds(&file);
    assert!(
        !plans.is_empty(),
        "promoted columns are the heat signal that gates building"
    );
    let (kind, column_id) = plans[0];
    assert_eq!(kind, ArtifactKind::BinaryFuse);

    let artifact = build_membership_artifact(&file, column_id, 3).unwrap();
    assert_eq!(
        built.bytes, before,
        "building an artifact reads the file, never rewrites it"
    );
    let encoded = artifact.encode();
    assert_eq!(IndexArtifact::decode(&encoded).unwrap(), artifact);
    assert!(artifact.usable_for(file.header().file_id, &file.footer().schema_fingerprint, 3));
    assert!(
        artifact.usable_for(file.header().file_id, &file.footer().schema_fingerprint, 9),
        "an inexact membership filter survives newer deletion-vector generations"
    );

    let clause = |value: u64| FilterClause {
        column_id,
        predicate: PredicateKind::Equality,
        value_hi: value as i128,
        value_lo: value as i128,
    };
    for granule in &file.footer().granules {
        let present = artifact
            .term_for(granule.granule_id, &clause(stable_hash(b"k1")))
            .expect("covered granule");
        assert_eq!(
            present.verdict(),
            StatVerdict::CouldMatch,
            "granule {}",
            granule.granule_id
        );
        let absent = artifact
            .term_for(granule.granule_id, &clause(stable_hash(b"never-a-kind")))
            .expect("covered granule");
        assert_eq!(
            absent.verdict(),
            StatVerdict::ProvenAbsent,
            "granule {}",
            granule.granule_id
        );
    }
    let past_the_file = file.footer().granules.last().unwrap().granule_id + 100;
    assert!(
        artifact.term_for(past_the_file, &clause(stable_hash(b"k1"))).is_none(),
        "an uncovered granule keeps its footer-tier terms"
    );
}

/// The streamed build hands the file's byte ranges to the sink as they become final and never materializes the
/// concatenated file, yet its identity and seal are byte-for-byte the materialized build's: same bytes, file_id,
/// stripe checksums, domain-separated segment seal, and CRC — with no extra pass over the large stripe bytes. Every
/// stripe leaves before the footer, and the header — which carries the footer pointer and file id — leaves last.
/// Implements `hef-write-path` — "The builder streams at stripe scope with bounded memory".
#[test]
fn a_streamed_build_is_byte_identical_to_the_materialized_one() {
    let rows: Vec<HefRow> = (0..64).map(row).collect();
    let materialized = build_hef_file(rows.clone(), &config()).unwrap();

    let mut streamed_bytes = vec![0u8; materialized.bytes.len()];
    let mut ranges: Vec<(u64, usize)> = Vec::new();
    let streamed = build_hef_file_streamed(rows, &config(), &SerialEncodeExecutor, &mut |offset, bytes| {
        ranges.push((offset, bytes.len()));
        let at = offset as usize;
        streamed_bytes[at..at + bytes.len()].copy_from_slice(&bytes);
    })
    .unwrap();

    assert!(
        ranges.len() >= 4,
        "stripes, footer region, trailer, and header arrive as separate ranges"
    );
    assert_eq!(
        ranges.last(),
        Some(&(0, HEADER_BLOCK_LEN)),
        "the header is the last range out"
    );
    let stripe_ranges: Vec<(u64, usize)> = materialized
        .footer
        .stripes
        .iter()
        .map(|stripe| (stripe.file_offset, stripe.byte_len as usize))
        .collect();
    assert!(
        stripe_ranges
            .iter()
            .all(|(offset, len)| ranges.iter().any(|(at, span)| at == offset && span >= len)),
        "every stripe arrives as its own range"
    );
    let mut sorted = ranges.clone();
    sorted.sort_unstable();
    let mut expected_start = 0u64;
    for (offset, len) in sorted {
        assert_eq!(
            offset, expected_start,
            "the ranges partition the file without gaps or overlaps"
        );
        expected_start = offset + len as u64;
    }
    assert_eq!(expected_start, streamed.total_len);
    assert_eq!(streamed_bytes, materialized.bytes);
    assert_eq!(streamed.file_id, materialized.file_id);
    assert_eq!(streamed.file_seal, materialized.file_seal);
    assert_eq!(streamed.file_crc64_nvme, materialized.file_crc64_nvme);
    assert_eq!(streamed.total_len, materialized.bytes.len() as u64);
    assert_eq!(streamed.footer.stripe_checksums, materialized.footer.stripe_checksums);

    let reopened = HefFile::open(streamed_bytes, Some(&streamed.file_seal)).unwrap();
    assert_eq!(reopened.header().row_count, 64);
}

/// IMP-007: how long granule construction takes over a homogeneous payload whose fields mostly stay in the residual —
/// nine top-level fields of which three route (one promoted, one shredded, one free text) and six carry types no shred
/// plan lifts. That is the shape where settling routes once per payload shape can pay, since the six that stay never
/// look a route up. Release-mode only:
/// `cargo test -p storage --features write --release -- --ignored benchmark_granule_construction`.
#[test]
#[ignore = "release-mode IMP-007 phase benchmark"]
fn benchmark_granule_construction_over_mostly_residual_payloads() {
    use crate::invariants::sim::SerialEncodeExecutor;
    let rows: Vec<HefRow> = (0..50_000)
        .map(|i| {
            let mut wide = row(i);
            let PayloadInput::Variant(VariantValue::Object(fields)) = &mut wide.event.payload else {
                unreachable!("row payload is a variant object");
            };
            // `row` carries `rare` every third row; dropping it leaves every row on one shape.
            fields.remove("rare");
            fields.insert("ratio".to_owned(), VariantValue::Float(1.5 + i as f32));
            fields.insert("weight".to_owned(), VariantValue::Float(0.25 * i as f32));
            fields.insert(
                "detail".to_owned(),
                VariantValue::Object(BTreeMap::from([
                    ("channel".to_owned(), VariantValue::String("web".to_owned())),
                    ("retries".to_owned(), VariantValue::Int(i as i64 % 3)),
                ])),
            );
            fields.insert(
                "meta".to_owned(),
                VariantValue::Object(BTreeMap::from([(
                    "locale".to_owned(),
                    VariantValue::String("en-gb".to_owned()),
                )])),
            );
            fields.insert(
                "tags".to_owned(),
                VariantValue::Array(vec![
                    VariantValue::String("a".to_owned()),
                    VariantValue::String("b".to_owned()),
                ]),
            );
            fields.insert(
                "history".to_owned(),
                VariantValue::Array(vec![VariantValue::Int(i as i64)]),
            );
            wide
        })
        .collect();
    let mut cfg = config();
    cfg.targets.index_granularity = 8_192;
    cfg.targets.index_granularity_bytes = usize::MAX;
    cfg.targets.stripe_target_bytes = 8 * 1024 * 1024;

    let mut samples = Vec::new();
    for _ in 0..7 {
        let (built, profile) =
            build_hef_file_profiled_with_executor(rows.clone(), &cfg, &SerialEncodeExecutor).unwrap();
        assert_eq!(
            built.footer.shredded.len(),
            1,
            "only amount shreds; the rest of the payload stays residual: {:?}",
            built.footer.shredded,
        );
        samples.push(
            profile
                .phases
                .iter()
                .find(|(phase, _)| *phase == BuildPhase::GranuleConstruction)
                .map(|(_, nanos)| *nanos)
                .unwrap_or_default(),
        );
        std::hint::black_box(built.bytes.len());
    }
    samples.sort_unstable();
    eprintln!(
        "IMP-007 granule construction over 50,000 mostly-residual rows: {samples:?}; median={}ns",
        samples.get(3).copied().unwrap_or_default(),
    );
}

/// Normalization is where payloads stop being per-row trees: their values move into one arena in their shape's field
/// order, a payload that is not an object is held there whole, and the rows keep nothing but an external reference —
/// the one payload form the file writes straight out of the row.
#[test]
fn normalization_lays_payload_values_out_in_shape_order() {
    let mut rows = HefRow::into_build_rows(vec![row(0), row(1), row(2), row(3)]);
    rows[0].payload = BuildPayload::Object(fields_of(&BTreeMap::from([
        ("kind".to_owned(), VariantValue::String("renewal".to_owned())),
        ("amount".to_owned(), VariantValue::Int(1)),
    ])));
    rows[1].payload = BuildPayload::Whole(VariantValue::Int(9));
    rows[2].payload = BuildPayload::ExternalRef("blob://payload/2".to_owned());
    rows[3].payload = BuildPayload::None;

    let all_rows = [(0, rows.len())];
    let payloads = NormalizedPayloads::normalize(&mut rows, &all_rows).unwrap();

    let object = payloads.payload(0);
    assert_eq!(payloads.field_names(object).collect::<Vec<_>>(), ["amount", "kind"]);
    assert_eq!(
        payloads.values(object),
        [VariantValue::Int(1), VariantValue::String("renewal".to_owned())].as_slice()
    );
    assert_eq!(payloads.payload(1), NormalizedPayload::Whole { batch: 0, start: 2 });
    assert_eq!(payloads.values(payloads.payload(1)), [VariantValue::Int(9)].as_slice());
    assert_eq!(payloads.payload(2), NormalizedPayload::ExternalRef);
    assert!(payloads.values(payloads.payload(2)).is_empty());
    assert_eq!(payloads.payload(3), NormalizedPayload::None);
    assert!(payloads.values(payloads.payload(3)).is_empty());

    assert_eq!(rows[0].payload, BuildPayload::None);
    assert_eq!(rows[1].payload, BuildPayload::None);
    assert_eq!(
        rows[2].payload,
        BuildPayload::ExternalRef("blob://payload/2".to_owned())
    );
    assert_eq!(rows[3].payload, BuildPayload::None);
}

/// Event rows convert into the writer's shape with the strings they repeat shared rather than copied — the same
/// envelope string or field name in two rows is one allocation — and the file built from the converted rows is the
/// file built from the event rows, byte for byte.
#[test]
fn event_rows_convert_to_build_rows_sharing_their_strings() {
    let rows: Vec<HefRow> = (0..8).map(row).collect();
    let mut shared = SharedStrings::default();
    let first = shared.build_row(rows[0].clone());
    let third = shared.build_row(rows[2].clone());
    assert!(Arc::ptr_eq(&first.envelope.source, &third.envelope.source));
    assert!(Arc::ptr_eq(&first.envelope.event_type, &third.envelope.event_type));
    let BuildPayload::Object(first_fields) = &first.payload else {
        panic!("test rows carry object payloads");
    };
    let BuildPayload::Object(third_fields) = &third.payload else {
        panic!("test rows carry object payloads");
    };
    assert_eq!(
        first_fields.iter().map(|(name, _)| &**name).collect::<Vec<_>>(),
        ["amount", "kind", "note", "rare"]
    );
    assert!(Arc::ptr_eq(&first_fields[0].0, &third_fields[0].0));
    assert_eq!(first.envelope.entity_id.as_deref(), Some("opp-0"));

    let converted = HefRow::into_build_rows(rows.clone());
    assert_eq!(converted[0], first);
    assert_eq!(converted[2], third);
    let from_events = build_hef_file(rows, &config()).unwrap();
    let from_build_rows = build_hef_file(converted, &config()).unwrap();
    assert_eq!(from_build_rows.bytes, from_events.bytes);
}

/// A field list is the writer's own shape, so the writer checks what a map guaranteed: keys ascending and distinct.
/// Anything else is refused before a byte is encoded.
#[test]
fn a_payload_field_list_out_of_key_order_is_refused() {
    let mut rows = HefRow::into_build_rows(vec![row(0), row(1)]);
    rows[1].payload = BuildPayload::Object(vec![
        (FieldName::from("kind"), VariantValue::String("k1".to_owned())),
        (FieldName::from("amount"), VariantValue::Int(1)),
    ]);
    let unsorted = build_hef_file(rows.clone(), &config()).unwrap_err();
    assert!(matches!(unsorted, FormatError::Structural { rule } if rule.contains("ascending key order")));

    rows[1].payload = BuildPayload::Object(vec![
        (FieldName::from("amount"), VariantValue::Int(1)),
        (FieldName::from("amount"), VariantValue::Int(2)),
    ]);
    let repeated = build_hef_file(rows, &config()).unwrap_err();
    assert!(matches!(repeated, FormatError::Structural { rule } if rule.contains("ascending key order")));
}

/// Every `VariantValue` kind has to survive the build-only representation, whichever way it is carried: as a field of
/// a normalized object, nested inside one, whole in the arena because the payload was not an object at all, or not in
/// the arena at all because the row named an external body or carried nothing. The file has to give back exactly what
/// went in, and — the representation being internal — the same input has to keep producing the identical file.
#[test]
fn every_variant_kind_round_trips_through_the_normalized_representation() {
    let nested = VariantValue::Object(BTreeMap::from([
        ("depth".to_owned(), VariantValue::Int(2)),
        (
            "tags".to_owned(),
            VariantValue::Array(vec![VariantValue::String("a".to_owned()), VariantValue::Null]),
        ),
    ]));
    let every_kind = VariantValue::Object(BTreeMap::from([
        (
            "array".to_owned(),
            VariantValue::Array(vec![VariantValue::Int(1), VariantValue::Bool(false)]),
        ),
        ("binary".to_owned(), VariantValue::Binary(vec![0, 1, 254, 255])),
        ("bool".to_owned(), VariantValue::Bool(true)),
        (
            "decimal".to_owned(),
            VariantValue::Decimal {
                unscaled: -12_345,
                scale: 3,
            },
        ),
        ("double".to_owned(), VariantValue::Double(-1.5)),
        ("float".to_owned(), VariantValue::Float(0.25)),
        ("int".to_owned(), VariantValue::Int(-9_007_199_254_740_993)),
        ("null".to_owned(), VariantValue::Null),
        ("object".to_owned(), nested),
        (
            "string".to_owned(),
            VariantValue::String("a string long enough to leave the short-string form behind".to_owned()),
        ),
        (
            "timestamp".to_owned(),
            VariantValue::Timestamp(TimestampValue::from_physical_nanos(1_700_000_000_000_000_000)),
        ),
        (
            "uuid".to_owned(),
            VariantValue::Uuid(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
        ),
    ]));
    let payloads = [
        PayloadInput::Variant(every_kind),
        PayloadInput::Variant(VariantValue::Array(vec![VariantValue::Int(7)])),
        PayloadInput::Variant(VariantValue::String("bare".to_owned())),
        PayloadInput::ExternalRef("blob://payload/1".to_owned()),
        PayloadInput::None,
        // An object again, so a shape is interned after rows that had none.
        PayloadInput::Variant(VariantValue::Object(BTreeMap::from([(
            "int".to_owned(),
            VariantValue::Int(5),
        )]))),
    ];
    let rows: Vec<HefRow> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            let mut row = row(index as u64);
            row.event.payload = payload.clone();
            row
        })
        .collect();

    let built = build_hef_file(rows.clone(), &config()).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    for (ordinal, payload) in payloads.iter().enumerate() {
        let read = file.payload(ordinal as u64).unwrap();
        match payload {
            PayloadInput::Variant(expected) => {
                let PayloadRead::Value(value) = read else {
                    panic!("row {ordinal} reads back as a value");
                };
                assert_eq!(&value, expected, "row {ordinal}");
            }
            PayloadInput::ExternalRef(expected) => {
                assert_eq!(read, PayloadRead::External(expected.clone()), "row {ordinal}");
            }
            PayloadInput::None => assert_eq!(read, PayloadRead::None, "row {ordinal}"),
            PayloadInput::Encoded(_) => unreachable!("the fixture carries no encoded payloads"),
        }
    }

    let again = build_hef_file(rows, &config()).unwrap();
    assert_eq!(again.bytes, built.bytes);
    assert_eq!(again.file_id, built.file_id);
}

/// The granule that actually holds an entity id must never be ruled out by its filter — that is the one direction a
/// membership filter is not allowed to be wrong in, because a lookup that skipped the granule would report the id
/// absent from a file that holds it. Checked for every row of a multi-granule file, not a sampled few.
#[test]
fn entity_hash_filters_never_rule_out_the_granule_holding_an_id() {
    let rows: Vec<HefRow> = (0..128).map(row).collect();
    let mut cfg = config();
    cfg.targets.index_granularity = 16;
    let built = build_hef_file(rows.clone(), &cfg).unwrap();
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::ENTITY_HASH_POINT_FILTERS,
        0,
        "a file whose filters fit the budget declares the feature",
    );
    assert_eq!(
        built.footer.entity_hash_filters.len(),
        built.footer.granules.len(),
        "one filter per granule",
    );
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    let mut candidates_seen = 0usize;
    for (ordinal, source) in rows.iter().enumerate() {
        let holder = file
            .footer()
            .granules
            .iter()
            .find(|granule| {
                let first = granule.first_row_ordinal;
                (first..first + u64::from(granule.row_count)).contains(&(ordinal as u64))
            })
            .expect("every row sits in a granule");
        let candidates = file.granules_for_entity_hash(source.event.envelope.entity_id_hash_low);
        assert!(
            candidates.iter().any(|granule| granule.granule_id == holder.granule_id),
            "row {ordinal} lives in granule {} but its hash was ruled out of it",
            holder.granule_id,
        );
        candidates_seen += candidates.len();
    }
    assert!(
        candidates_seen < rows.len() * 2,
        "a present hash should usually name its own granule and no other, not {candidates_seen} across {} lookups",
        rows.len(),
    );
}

/// The point of the filters: an id the file never held costs no column reads at all, because every granule's filter
/// rules it out. A split-block Bloom filter admits a small share of hashes it never saw, so this asserts the rate is
/// low rather than zero — an occasional false positive costs a wasted block read, never a wrong answer.
#[test]
fn entity_hash_filters_rule_out_granules_for_ids_the_file_does_not_hold() {
    let rows: Vec<HefRow> = (0..128).map(row).collect();
    let mut cfg = config();
    cfg.targets.index_granularity = 16;
    let built = build_hef_file(rows, &cfg).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    let absent: Vec<u64> = (1_000..1_512).collect();
    let fully_pruned = absent
        .iter()
        .filter(|hash| file.granules_for_entity_hash(**hash).is_empty())
        .count();
    assert!(
        fully_pruned * 100 >= absent.len() * 95,
        "only {fully_pruned} of {} absent hashes read nothing",
        absent.len(),
    );
}

/// Two entity ids can hash to the same 64-bit value. The filter answers about the hash, so both granules stay
/// candidates and the caller's confirmation against the real id is what separates them — exactly the reason the
/// lookup may not stop at a hash match.
#[test]
fn entity_hash_filters_keep_every_granule_sharing_a_colliding_hash() {
    const COLLIDING: u64 = 0x00c0_11ee;
    let mut rows: Vec<HefRow> = (0..64).map(row).collect();
    for (index, ordinal) in [3usize, 40].into_iter().enumerate() {
        let envelope = &mut rows[ordinal].event.envelope;
        envelope.entity_id_hash_low = COLLIDING;
        envelope.entity_id = Some(format!("collides-{index}"));
    }
    let mut cfg = config();
    cfg.targets.index_granularity = 16;
    let built = build_hef_file(rows, &cfg).unwrap();
    let file = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();

    let candidates = file.granules_for_entity_hash(COLLIDING);
    let ids: Vec<u32> = candidates.iter().map(|granule| granule.granule_id).collect();
    assert!(
        ids.contains(&0) && ids.contains(&2),
        "both granules carrying the colliding hash must survive pruning, got {ids:?}",
    );
}

/// A filter is sized by the entities a granule holds, not by its rows: a stream where one entity emits event after
/// event must not pay footer bytes per event. The repeated-id file's filters are a fraction of the all-distinct
/// file's over the same row count.
#[test]
fn repeated_entity_ids_size_the_filters_by_entity_not_by_row() {
    let mut cfg = config();
    cfg.targets.index_granularity = 256;
    let distinct: Vec<HefRow> = (0..1024).map(row).collect();
    let repeated: Vec<HefRow> = (0..1024)
        .map(|index| {
            let mut built = row(index);
            built.event.envelope.entity_id_hash_low = index % 4;
            built.event.envelope.entity_id = Some(format!("opp-{}", index % 4));
            built
        })
        .collect();

    let filter_bytes = |rows: Vec<HefRow>| -> usize {
        let built = build_hef_file(rows, &cfg).unwrap();
        built
            .footer
            .entity_hash_filters
            .iter()
            .map(|entry| entry.index_len as usize)
            .sum()
    };
    let distinct_bytes = filter_bytes(distinct);
    let repeated_bytes = filter_bytes(repeated);
    assert!(
        repeated_bytes * 4 < distinct_bytes,
        "repeated ids cost {repeated_bytes} bytes against {distinct_bytes} for all-distinct ids",
    );
}

/// The filters ride the footer, so sealing the footer hides them from anyone without the key and reveals them intact
/// to anyone with it — pruning works the same on an encrypted file as on a plaintext one.
#[test]
fn an_encrypted_footer_still_prunes_on_the_entity_hash_filters() {
    let dek = [0x27u8; 32];
    let mut cfg = config();
    cfg.targets.index_granularity = 16;
    cfg.footer_encryption = crate::security::FooterEncryption::Encrypted;
    cfg.footer_dek = Some(dek);
    let rows: Vec<HefRow> = (0..128).map(row).collect();
    let built = build_hef_file(rows, &cfg).unwrap();
    let file = HefFile::open_with_keys(built.bytes.clone(), Some(&built.file_seal), Some(&dek)).unwrap();

    assert_eq!(file.footer().entity_hash_filters.len(), file.footer().granules.len());
    let held = file.granules_for_entity_hash(70);
    assert_eq!(
        held.iter().map(|granule| granule.granule_id).collect::<Vec<_>>(),
        vec![4],
        "row 70 sits in granule 4 and no other granule's filter admits its hash",
    );
    assert!(file.granules_for_entity_hash(1_000).is_empty());
}
