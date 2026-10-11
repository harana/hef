use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::artifacts::state_chunk::build_state_tree;
use crate::columns::{FreetextDeclaration, PromotedColumn, PromotionPlan, column_ids};
use crate::compat::tests::{PRE_GRAPH_KNOWN, PRE_TIME_SERIES_REQUIRED_KNOWN};
use crate::events::variant::VariantValue;
use crate::events::{
    EventEnvelope, EventFlags, EventId, RelationshipKind, RelationshipRef, StreamId, TenantId, TimestampValue,
};
use crate::layout::footer::ColumnKind;
use crate::layout::graph::{ChainCoverInput, ChainLabel, DepthRange, EdgeKind, GraphInput, RowEdges, UnresolvedEdge};
use crate::layout::{LayoutTargets, Profile, required_features};
use crate::security::{StateChunkKey, TenantDek};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};
use std::collections::BTreeMap;

fn row(i: u64) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    payload.insert("amount".to_owned(), VariantValue::Int(1_000 + i as i64));
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            connector_delivery_hash_high: 0,
            connector_delivery_hash_low: i,
            envelope: EventEnvelope {
                account_id: None,
                account_id_hash_low: 4,
                actor_id: None,
                actor_id_hash_low: 3,
                dedupe_hash_high: 6,
                dedupe_hash_low: 100 + i,
                entity_id: Some(format!("opp-{i}")),
                entity_id_hash_high: 1,
                entity_id_hash_low: i,
                entity_type: "opportunity".to_owned(),
                event_id: EventId::new_test_id(0xF1F0_0000 + u128::from(i)),
                event_type: "deal.updated".to_owned(),
                flags: EventFlags(0),
                ingested_at: TimestampValue::from_physical_nanos(2_000_000 + i as i64 * 1000),
                occurred_at: TimestampValue::from_physical_nanos(1_000_000 + i as i64 * 1000),
                schema_version: 2,
                source: "crm".to_owned(),
                stream_id: StreamId(2),
                stream_sequence: i,
                tenant_id: TenantId::new_test_id(7),
                trace_id_hash_low: 5,
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            provenance: None,
            relationships: None,
            source_delivery: None,
            source_schema: None,
        },
    }
}

fn config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        audit: None,
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        graph: None,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile: crate::layout::Profile::Event,
        promotion: PromotionPlan { columns: Vec::new() },
        targets: LayoutTargets {
            index_granularity: 8,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 1 << 20,
        },
        tenant_id: TenantId::new_test_id(7),
        time_series_bucket_nanos: None,
    }
}

fn built() -> BuiltHef {
    build_hef_file((0..8).map(row).collect(), &config()).unwrap()
}

/// A graph file of 32 events whose rows carry protocol ids, so it holds an event-id index, with edges, one unresolved
/// parent, a chain cover and state roots when `everything`; with only its edges otherwise, as the first graph files
/// were written.
fn graph_file(everything: bool) -> BuiltHef {
    let key = StateChunkKey::derive(&TenantDek::new([4; 32]));
    let root = build_state_tree(&key, &BTreeMap::new()).unwrap().root;
    let mut config = source_form_config(8);
    config.profile = Profile::Graph;
    config.graph = Some(GraphInput {
        chain_cover: everything.then(|| ChainCoverInput {
            labels: (0..32).map(|seq| ChainLabel { chain_id: 1, seq }).collect(),
            links: Vec::new(),
        }),
        depth: everything.then_some(DepthRange { max: 33, min: 2 }),
        first_sn: 0,
        inbound: Vec::new(),
        rows: (0..32u64)
            .map(|sn| RowEdges {
                auth: (0..sn.min(1)).collect(),
                prev: sn.checked_sub(1).into_iter().collect(),
            })
            .collect(),
        state_roots: if everything {
            BTreeMap::from([(0, root)])
        } else {
            BTreeMap::new()
        },
        subject: b"!fixture:example.org".to_vec(),
        unresolved: if everything {
            vec![UnresolvedEdge {
                child: 3,
                parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$late:example.org").unwrap(),
            }]
        } else {
            Vec::new()
        },
    });
    let rows: Vec<_> = (0..32u64)
        .map(|i| {
            let mut row = plain_row(i);
            if everything {
                row.external_id = Some(format!("$fixture{i}:example.org").into_bytes());
            }
            row
        })
        .collect();
    build_hef_file(rows, &config).unwrap()
}

/// A time-series file of 2,048 readings of `reading(i)` in one promoted float column.
fn time_series_file(reading: impl Fn(u64) -> f64) -> BuiltHef {
    let mut config = config();
    config.profile = Profile::TimeSeries;
    config.promotion = PromotionPlan {
        columns: vec![PromotedColumn {
            kind: ColumnKind::F64,
            name: "used".to_owned(),
            path: "used".to_owned(),
            since_schema_version: 1,
            substring_searchable: false,
        }],
    };
    let rows = (0..2_048)
        .map(|i| {
            let mut row = row(i);
            let payload = BTreeMap::from([("used".to_owned(), VariantValue::Double(reading(i)))]);
            row.event.payload = PayloadInput::Variant(VariantValue::Object(payload));
            row
        })
        .collect();
    build_hef_file(rows, &config).unwrap()
}

/// Used memory over a 16 GB total, holding still on every other reading: full double precision, which Chimp stores
/// smaller than any earlier float encoding.
fn memory_reading(i: u64) -> f64 {
    (6_000_000_000 + (i / 2) * 40_960) as f64 / 16_000_000_000.0
}

/// Two-decimal readings, which the writer keeps in its earlier float encodings.
fn decimal_reading(i: u64) -> f64 {
    (2_000 + (i * 7) % 50) as f64 / 100.0
}

/// What a reader that implements only the required features in `known` does on opening `bytes`: this reader's
/// checks, then a refusal of every required bit outside `known`, the rule `check_features` applies to its own set.
fn open_as_reader_knowing(bytes: &[u8], known: u64) -> Result<HefFile, FormatError> {
    let file = HefFile::open(bytes.to_vec(), None)?;
    let unknown = file.footer().required_feature_flags & !known;
    if unknown != 0 {
        return Err(FormatError::UnknownRequiredFeature { bits: unknown });
    }
    Ok(file)
}

/// Fixtures of the files that declare the newest required bits, a graph file with every graph block and a time-series
/// file with Chimp float blocks, sweep clean with this reader and are refused, naming those bits, by a reader from
/// before them.
#[test]
fn graph_and_xor_float_fixtures_open_here_and_are_refused_by_older_readers() {
    let dir = tempfile::tempdir().unwrap();
    let graph = graph_file(true);
    let time_series = time_series_file(memory_reading);
    generate_fixture(dir.path(), "graph", &graph).unwrap();
    generate_fixture(dir.path(), "time-series-xor-floats", &time_series).unwrap();
    let sweep = read_fixture_corpus(dir.path()).unwrap();
    assert!(sweep.failures.is_empty(), "{:?}", sweep.failures);
    assert_eq!(
        sweep.opened,
        ["hef-v1.0-graph.hef", "hef-v1.0-time-series-xor-floats.hef"]
    );

    assert_eq!(
        open_as_reader_knowing(&graph.bytes, PRE_GRAPH_KNOWN).unwrap_err(),
        FormatError::UnknownRequiredFeature {
            bits: required_features::GRAPH
        }
    );
    assert!(open_as_reader_knowing(&graph.bytes, required_features::KNOWN).is_ok());
    assert_eq!(
        open_as_reader_knowing(&time_series.bytes, PRE_TIME_SERIES_REQUIRED_KNOWN).unwrap_err(),
        FormatError::UnknownRequiredFeature {
            bits: required_features::XOR_FLOAT_BLOCKS
        }
    );
    assert!(open_as_reader_knowing(&time_series.bytes, required_features::KNOWN).is_ok());
}

/// Files written with feature sets from before the newest bits, an event file, a graph file with only its edges and
/// a time-series file without Chimp blocks, declare nothing an older reader lacks and still open and read here.
#[test]
fn files_with_earlier_feature_sets_still_open_and_read() {
    let dir = tempfile::tempdir().unwrap();
    let event = built();
    let edges_only = graph_file(false);
    let decimal = time_series_file(decimal_reading);
    for (label, built, known) in [
        ("event", &event, PRE_GRAPH_KNOWN),
        ("graph-edges-only", &edges_only, PRE_TIME_SERIES_REQUIRED_KNOWN),
        ("time-series-decimal", &decimal, PRE_TIME_SERIES_REQUIRED_KNOWN),
    ] {
        assert!(open_as_reader_knowing(&built.bytes, known).is_ok(), "{label}");
        generate_fixture(dir.path(), label, built).unwrap();
    }
    assert_eq!(
        edges_only.footer.required_feature_flags & required_features::GRAPH,
        required_features::GRAPH_DENSE_IDS
            | required_features::GRAPH_CSR_FORWARD
            | required_features::GRAPH_CSR_REVERSE
    );
    let sweep = read_fixture_corpus(dir.path()).unwrap();
    assert!(sweep.failures.is_empty(), "{:?}", sweep.failures);
    assert_eq!(sweep.opened.len(), 3);

    let file = HefFile::open(edges_only.bytes.clone(), Some(&edges_only.file_seal)).unwrap();
    assert_eq!(file.granule_edges(1).unwrap().parents(9, EdgeKind::Prev), [8]);
    let file = HefFile::open(decimal.bytes.clone(), Some(&decimal.file_seal)).unwrap();
    let aggregate = file
        .time_series_aggregate(column_ids::PROMOTED_BASE, 0, 60_000_000_000, 60_000_000_000)
        .unwrap();
    assert_eq!(aggregate.count, 2_048);
}

/// The committed corpus sweeps clean today (pre-freeze it is empty), and an absent directory is the same empty corpus,
/// not an error.
#[test]
fn the_committed_corpus_and_an_absent_directory_both_sweep_clean() {
    let committed = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_CORPUS_DIR);
    let sweep = read_fixture_corpus(&committed).unwrap();
    assert!(
        sweep.failures.is_empty(),
        "a committed fixture no longer opens: {:?}",
        sweep.failures
    );

    let absent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("test_data/no-such-corpus");
    let sweep = read_fixture_corpus(&absent).unwrap();
    assert!(sweep.opened.is_empty() && sweep.failures.is_empty());
}

/// A generated fixture lands under the version-stamped name and the sweep opens it; corrupting it turns up as a named
/// failure instead of a panic, which is what CI reports after format freeze.
#[test]
fn generated_fixtures_are_swept_and_a_broken_one_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let built = built();
    let path = generate_fixture(dir.path(), "minimal", &built).unwrap();
    assert_eq!(
        path.file_name().unwrap().to_string_lossy(),
        "hef-v1.0-minimal.hef",
        "fixture names carry the frozen writer version"
    );
    let sweep = read_fixture_corpus(dir.path()).unwrap();
    assert_eq!(sweep.opened, vec!["hef-v1.0-minimal.hef".to_owned()]);
    assert!(sweep.failures.is_empty());

    let mut corrupted = built.bytes.clone();
    let middle = corrupted.len() / 2;
    corrupted[middle] ^= 0xFF;
    std::fs::write(dir.path().join("hef-v1.0-corrupted.hef"), &corrupted).unwrap();
    let sweep = read_fixture_corpus(dir.path()).unwrap();
    assert_eq!(sweep.opened, vec!["hef-v1.0-minimal.hef".to_owned()]);
    assert_eq!(sweep.failures.len(), 1);
    assert_eq!(sweep.failures[0].0, "hef-v1.0-corrupted.hef");
}
