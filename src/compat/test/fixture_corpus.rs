use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::LayoutTargets;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};

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

fn built() -> crate::writer::build::BuiltHef {
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 42,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan { columns: Vec::new() },
        targets: LayoutTargets {
            index_granularity: 8,
            index_granularity_bytes: 1 << 20,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
            stripe_target_bytes: 1 << 20,
        },
        tenant_id: TenantId::new_test_id(7),
    };
    build_hef_file((0..8).map(row).collect(), &config).unwrap()
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
