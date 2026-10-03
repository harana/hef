use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotionPlan, column_ids};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::indexes::learned_position::LearnedPositionIndex;
use crate::indexes::path_presence::PathPresenceIndex;
use crate::indexes::pruning::StatVerdict;
use crate::layout::LayoutTargets;
use crate::layout::reader::HefFile;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};

fn artifact() -> IndexArtifact {
    let g0 = BinaryFuseFilter::build(&[10, 20, 30]).unwrap().encode();
    let g1 = BinaryFuseFilter::build(&[40, 50]).unwrap().encode();
    IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation: 7,
            file_id: 0xABCD,
            granule_ranges: vec![(0, 1)],
            schema_fingerprint: [9u8; 32],
        },
        ArtifactHeader {
            column_id: 42,
            exact: false,
            false_positive_ppm: 4_000,
            kind: ArtifactKind::BinaryFuse,
            projection_id: 0,
        },
        vec![
            ArtifactPage {
                bytes: g0,
                first_granule: 0,
                last_granule: 0,
            },
            ArtifactPage {
                bytes: g1,
                first_granule: 1,
                last_granule: 1,
            },
        ],
    )
}

fn equality(column_id: u32, value: i128) -> FilterClause {
    FilterClause {
        column_id,
        predicate: PredicateKind::Equality,
        value_hi: value,
        value_lo: value,
    }
}

/// The sealed object round-trips exactly, and one flipped byte anywhere fails the seal instead of decoding garbage.
#[test]
fn an_artifact_round_trips_and_refuses_a_broken_seal() {
    let original = artifact();
    let bytes = original.encode();
    assert_eq!(IndexArtifact::decode(&bytes).unwrap(), original);

    let mut tampered = bytes.clone();
    let middle = tampered.len() / 2;
    tampered[middle] ^= 0x40;
    assert!(IndexArtifact::decode(&tampered).is_err());
}

/// The directory decodes from a prefix that carries no page bytes at all — the progressive-load contract: fetch the
/// head, plan which pages a query touches, fetch only those.
#[test]
fn the_directory_decodes_without_any_page_bytes() {
    let original = artifact();
    let bytes = original.encode();
    let page_bytes: usize = original.pages.iter().map(|page| page.bytes.len()).sum();
    let head = &bytes[..bytes.len() - 32 - page_bytes];
    let (header, coverage, directory) = decode_artifact_directory(head).unwrap();
    assert_eq!(header, original.header);
    assert_eq!(coverage, original.coverage);
    assert_eq!(directory.len(), 2);
    assert_eq!(directory[1].first_granule, 1);
    assert_eq!(directory[1].len, original.pages[1].bytes.len() as u64);
}

/// The staleness matrix: a fingerprint or file mismatch is never usable; an exact index dies with a superseded
/// deletion-vector generation while an inexact no-false-negative one survives it.
#[test]
fn staleness_rules_gate_which_scans_may_use_an_artifact() {
    let inexact = artifact();
    assert!(inexact.usable_for(0xABCD, &[9u8; 32], 7));
    assert!(
        inexact.usable_for(0xABCD, &[9u8; 32], 8),
        "an inexact index survives a newer deletion-vector generation"
    );
    assert!(
        !inexact.usable_for(0xABCD, &[9u8; 32], 6),
        "never a generation it postdates"
    );
    assert!(!inexact.usable_for(0xFFFF, &[9u8; 32], 7), "never another file");
    assert!(
        !inexact.usable_for(0xABCD, &[1u8; 32], 7),
        "never a fingerprint mismatch"
    );

    let mut exact = artifact();
    exact.header.exact = true;
    assert!(exact.usable_for(0xABCD, &[9u8; 32], 7));
    assert!(
        !exact.usable_for(0xABCD, &[9u8; 32], 8),
        "an exact index dies with a superseded deletion-vector generation"
    );
}

/// Covered granules gain artifact terms that prune absent values; uncovered granules, other columns, and the
/// seek-only learned-position kind contribute nothing — those granules simply keep their footer-tier terms.
#[test]
fn terms_prune_covered_granules_and_leave_the_rest_to_the_footer_tier() {
    let index = artifact();

    let hit = index
        .term_for(0, &equality(42, 20))
        .expect("covered granule, probing kind");
    assert_eq!(hit.verdict(), StatVerdict::CouldMatch);
    assert_eq!(hit.exactness, Exactness::InexactNoFalseNegative);

    let miss = index.term_for(0, &equality(42, 60)).expect("covered granule");
    assert_eq!(
        miss.verdict(),
        StatVerdict::ProvenAbsent,
        "an absent value prunes the granule"
    );

    let other_page = index.term_for(1, &equality(42, 40)).expect("second page");
    assert_eq!(other_page.verdict(), StatVerdict::CouldMatch);

    assert!(index.term_for(5, &equality(42, 20)).is_none(), "uncovered granule");
    assert!(index.term_for(0, &equality(7, 20)).is_none(), "another column");

    let mut seek_only = artifact();
    seek_only.header.kind = ArtifactKind::LearnedPosition;
    assert!(
        seek_only.term_for(0, &equality(42, 20)).is_none(),
        "a seek accelerator can never prove a value absent"
    );
}

/// A page is decoded once however many probes land on it: pruning asks an artifact for a term per (granule, clause),
/// and re-decoding the same filter page for each of those probes was pure repeated work.
#[test]
fn a_page_is_decoded_once_however_many_probes_land_on_it() {
    let index = artifact();
    assert_eq!(index.decoded_page_count(), 0, "an unprobed artifact decodes nothing");

    for value in [10, 20, 30, 60] {
        assert!(index.term_for(0, &equality(42, value)).is_some());
    }
    assert_eq!(
        index.decoded_page_count(),
        1,
        "every clause probing one granule shares that granule's page decode"
    );

    assert!(index.term_for(1, &equality(42, 40)).is_some());
    assert_eq!(
        index.decoded_page_count(),
        2,
        "a second page is decoded only when a probe reaches it"
    );

    // The held decodes never change a verdict: the same probes answer as they did on the first, cold pass.
    assert_eq!(
        index.term_for(0, &equality(42, 20)).unwrap().verdict(),
        StatVerdict::CouldMatch
    );
    assert_eq!(
        index.term_for(0, &equality(42, 60)).unwrap().verdict(),
        StatVerdict::ProvenAbsent
    );
}

fn built_file_row(i: u64) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    payload.insert("kind".to_owned(), VariantValue::String(format!("k{}", i % 3)));
    // Jittered, so a learned-position build over this shredded column is refused as unsorted.
    payload.insert("amount".to_owned(), VariantValue::Int(1_000 + (i as i64 * 37) % 91));
    if i % 2 == 0 {
        payload.insert(
            "attributes".to_owned(),
            VariantValue::Object(std::collections::BTreeMap::from([(
                "revenue".to_owned(),
                VariantValue::Int(i as i64),
            )])),
        );
    }
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xFACE_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(3),
                stream_id: StreamId(1),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000 + i as i64 * 10),
                ingested_at: TimestampValue::from_physical_nanos(2_000 + i as i64),
                source: ["crm", "billing"][i as usize % 2].to_owned(),
                event_type: "deal.updated".to_owned(),
                entity_type: "opportunity".to_owned(),
                entity_id_hash_low: i,
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

/// A small sealed file spanning several granules, the substrate every builder below indexes.
fn built_file() -> BuiltHef {
    let rows: Vec<HefRow> = (0..40).map(built_file_row).collect();
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration { fields: Vec::new() },
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 8,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    build_hef_file(rows, &config).unwrap()
}

/// The path-presence builder writes one filter page per granule over the payload paths its rows actually carry —
/// dotted nested paths included — and a path no row carries is provably absent.
#[test]
fn path_presence_builder_covers_every_granule_with_the_real_paths() {
    let built = built_file();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    let artifact = build_path_presence_artifact(&file, 7).unwrap();

    assert_eq!(artifact.header.kind, ArtifactKind::PathPresence);
    assert_eq!(artifact.header.column_id, column_ids::PAYLOAD_REF);
    assert_eq!(artifact.pages.len(), file.footer().granules.len());
    for granule in &file.footer().granules {
        assert!(artifact.coverage.covers(granule.granule_id));
        let page = artifact.page_for(granule.granule_id).expect("page per granule");
        let index = PathPresenceIndex::decode(&page.bytes).unwrap();
        assert!(index.might_contain_path("kind"));
        assert!(index.might_contain_path("attributes.revenue"));
        assert!(!index.might_contain_path("no.such.path"));
    }
}

/// The range-filter builder proves an empty key range absent per granule and keeps every real key's range non-empty.
#[test]
fn range_filter_builder_answers_range_emptiness_per_granule() {
    let built = built_file();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    // occurred_at is dense i64 with values 1_000 + 10 * i: gaps of 10 between neighbours.
    let artifact = build_range_filter_artifact(&file, column_ids::OCCURRED_AT, 7).unwrap();

    assert_eq!(artifact.header.kind, ArtifactKind::RangeFilter);
    let granule = &file.footer().granules[0];
    let page = artifact.page_for(granule.granule_id).unwrap();
    let filter = RangeFilter::decode(&page.bytes).unwrap();
    assert!(filter.range_nonempty(1_000, 1_010), "a stored key's range is non-empty");
    assert!(
        !filter.range_nonempty(u64::MAX / 2, u64::MAX / 2 + 1),
        "a far-away empty range is provably empty"
    );
}

/// The learned-position builder models a sorted column across the whole file — the decoded window always contains the
/// true row — and refuses an unsorted column outright.
#[test]
fn learned_position_builder_models_sorted_columns_and_refuses_unsorted() {
    let built = built_file();
    let amount_column = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount auto-shreds")
        .column_id;
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    let artifact = build_learned_position_artifact(&file, column_ids::OCCURRED_AT, 7, 8).unwrap();
    assert_eq!(artifact.pages.len(), 1, "one model page covers the whole file");
    let model = LearnedPositionIndex::decode(&artifact.pages[0].bytes).unwrap();
    for row in [0u64, 13, 39] {
        let key = 1_000 + row as i64 * 10;
        let window = model.estimate_position(key);
        assert!(
            (window.search_from..=window.search_to).contains(&row),
            "window [{}, {}] must contain row {row}",
            window.search_from,
            window.search_to
        );
    }

    assert!(
        build_learned_position_artifact(&file, amount_column, 7, 8).is_err(),
        "an unsorted column refuses instead of modelling garbage"
    );
}

/// The bitmap builder records, per granule, exactly which rows carry each value — exact, so its pruning term proves
/// both presence and absence.
#[test]
fn bitmap_builder_maps_values_to_their_granule_rows_exactly() {
    let built = built_file();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    // source_id is dense u64 with two values alternating row by row within every granule.
    let artifact = build_bitmap_artifact(&file, column_ids::SOURCE_ID, 7).unwrap();

    assert!(artifact.header.exact);
    let granule = &file.footer().granules[0];
    let page = artifact.page_for(granule.granule_id).unwrap();
    let index = BitmapIndex::decode(&page.bytes).unwrap();
    let read = file.read_column(column_ids::SOURCE_ID, granule.granule_id).unwrap();
    let crate::encoding::ColumnData::U64(values) = &read.data else {
        panic!("source_id is a u64 column");
    };
    for (row, value) in values.iter().enumerate() {
        let rows = index.bitmap(*value).expect("every stored value is mapped");
        assert!(rows.contains(row as u64), "value {value} must map row {row}");
    }
}

#[test]
fn page_for_finds_the_covering_page_among_many_pages() {
    let pages: Vec<ArtifactPage> = (0..64u32)
        .map(|granule| ArtifactPage {
            bytes: BinaryFuseFilter::build(&[u64::from(granule)]).unwrap().encode(),
            first_granule: granule,
            last_granule: granule,
        })
        .collect();
    let index = IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation: 0,
            file_id: 1,
            granule_ranges: vec![(0, 63)],
            schema_fingerprint: [0u8; 32],
        },
        ArtifactHeader {
            column_id: 42,
            exact: false,
            false_positive_ppm: 4_000,
            kind: ArtifactKind::BinaryFuse,
            projection_id: 0,
        },
        pages,
    );
    for granule in 0..64u32 {
        assert_eq!(index.page_for(granule).map(|page| page.first_granule), Some(granule));
    }
    assert!(index.page_for(64).is_none(), "past the last page");
}

#[test]
fn page_for_reports_no_page_for_a_granule_in_a_gap_between_pages() {
    let index = IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation: 0,
            file_id: 1,
            granule_ranges: vec![(0, 1), (4, 5)],
            schema_fingerprint: [0u8; 32],
        },
        ArtifactHeader {
            column_id: 42,
            exact: false,
            false_positive_ppm: 4_000,
            kind: ArtifactKind::BinaryFuse,
            projection_id: 0,
        },
        vec![
            ArtifactPage {
                bytes: BinaryFuseFilter::build(&[10]).unwrap().encode(),
                first_granule: 0,
                last_granule: 1,
            },
            ArtifactPage {
                bytes: BinaryFuseFilter::build(&[20]).unwrap().encode(),
                first_granule: 4,
                last_granule: 5,
            },
        ],
    );
    assert_eq!(index.page_for(1).map(|page| page.first_granule), Some(0));
    assert!(index.page_for(2).is_none(), "granule between two pages");
    assert_eq!(index.page_for(4).map(|page| page.first_granule), Some(4));
}
