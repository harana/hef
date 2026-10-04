use super::*;
use crate::layout::footer::{ColumnDescriptor, ColumnKind, ExactCounts, FileDictionaries, Footer, GranuleEntry};

fn granule(id: u32, first_seq: u64, last_seq: u64, min_occ: i64, max_occ: i64) -> GranuleEntry {
    GranuleEntry {
        compressed_bytes_estimate: 0,
        first_epoch: 1,
        first_row_ordinal: 0,
        first_sequence: first_seq,
        granule_id: id,
        last_epoch: 1,
        last_sequence: last_seq,
        max_ingested_at_physical: max_occ,
        max_occurred_at_physical: max_occ,
        min_ingested_at_physical: min_occ,
        min_occurred_at_physical: min_occ,
        row_count: 4,
        stripe_id: 0,
    }
}

fn footer_with(granules: Vec<GranuleEntry>, row_count: u64) -> Footer {
    Footer {
        clustering: Vec::new(),
        columns: vec![ColumnDescriptor {
            column_id: 0,
            internal_only: false,
            kind: ColumnKind::U64,
            name: "c".to_owned(),
            nullable: false,
        }],
        dictionaries: FileDictionaries::default(),
        embedding_row_offsets: Vec::new(),
        entity_hash_filters: Vec::new(),
        escape_hatches: Vec::new(),
        exact_counts: ExactCounts {
            row_count,
            ..ExactCounts::default()
        },
        external_ids: Vec::new(),
        format_version: (1, 0),
        freetext: Vec::new(),
        freetext_row_offsets: Vec::new(),
        granules,
        integrity_gaps: Vec::new(),
        io_alignment_bytes: 0,
        marks: Vec::new(),
        marks_directory: Vec::new(),
        marks_page_offsets: Vec::new(),
        marks_pages: Vec::new(),
        optional_feature_flags: 0,
        page_directory: Vec::new(),
        page_minmax: Vec::new(),
        page_stats: Vec::new(),
        payload_granules: Vec::new(),
        presence: Vec::new(),
        reference_filters: None,
        required_feature_flags: 0,
        schema_fingerprint: [7u8; 32],
        shared_dictionaries: Vec::new(),
        shredded: Vec::new(),
        sparse_keys: Vec::new(),
        stripe_checksums: Vec::new(),
        stripe_proofs: Vec::new(),
        stripe_ndv: Vec::new(),
        stripes: Vec::new(),
        text_token_indexes: Vec::new(),
        text_token_offsets: Vec::new(),
    }
}

#[test]
fn summary_encode_decode_round_trip() {
    let summary = ManifestSummary {
        granule_count: 3,
        has_deletion_vectors: true,
        has_late_events: false,
        max_occurred_at_physical: 500,
        max_sequence: 99,
        min_occurred_at_physical: 100,
        min_sequence: 1,
        row_count: 42,
        schema_fingerprint: [9u8; 32],
    };
    let bytes = summary.encode();
    let decoded = ManifestSummary::decode(&bytes).unwrap();
    assert_eq!(decoded, summary);
}

#[test]
fn summary_decode_rejects_truncated() {
    assert!(ManifestSummary::decode(&[0u8; 10]).is_err());
}

#[test]
fn summary_decode_rejects_inverted_coverage_with_non_zero_counts() {
    // A flipped byte can invert a summary's coverage (min > max). can_prune_file reads inverted coverage as "empty
    // file" and prunes unconditionally, so a summary that still claims rows or granules must be rejected — otherwise
    // one corrupted byte would hide an entire non-empty file from every query.
    let forged = ManifestSummary {
        granule_count: 2,
        has_deletion_vectors: false,
        has_late_events: false,
        max_occurred_at_physical: 100,
        max_sequence: 20,
        min_occurred_at_physical: 500, // min > max: inverted occurred-at coverage
        min_sequence: 1,
        row_count: 8,
        schema_fingerprint: [0u8; 32],
    };
    assert!(ManifestSummary::decode(&forged.encode()).is_err());

    // Inverted only on the sequence axis is rejected too.
    let forged_sequence = ManifestSummary {
        max_sequence: 5,
        min_occurred_at_physical: 100,
        min_sequence: 30, // min > max: inverted sequence coverage
        ..forged
    };
    assert!(ManifestSummary::decode(&forged_sequence.encode()).is_err());
}

#[test]
fn summary_decode_allows_inverted_coverage_when_empty() {
    // The empty-file sentinel is inverted coverage with zero counts; it must still round-trip so an empty file's
    // summary decodes and prunes as intended.
    let empty = summary_from_footer(&footer_with(Vec::new(), 0));
    assert_eq!(empty.granule_count, 0);
    assert_eq!(empty.row_count, 0);
    assert!(empty.min_occurred_at_physical > empty.max_occurred_at_physical);
    assert_eq!(ManifestSummary::decode(&empty.encode()).unwrap(), empty);
}

#[test]
fn summary_from_footer_aggregates_granule_ranges() {
    let footer = footer_with(
        vec![
            granule(0, 1, 10, 100, 200),
            granule(1, 11, 20, 50, 300),
            granule(2, 21, 30, 250, 250),
        ],
        12,
    );
    let summary = summary_from_footer(&footer);
    assert_eq!(summary.granule_count, 3);
    assert_eq!(summary.row_count, 12);
    assert_eq!(summary.min_sequence, 1);
    assert_eq!(summary.max_sequence, 30);
    assert_eq!(summary.min_occurred_at_physical, 50);
    assert_eq!(summary.max_occurred_at_physical, 300);
    assert_eq!(summary.schema_fingerprint, [7u8; 32]);
}

#[test]
fn summary_copies_the_deletion_vector_flag_from_the_footer() {
    // The summary is a faithful copy of footer metadata: a file declaring native deletion vectors must not get a
    // manifest summary that denies having them (issue #4010).
    let mut footer = footer_with(vec![granule(0, 1, 10, 100, 200)], 4);
    footer.optional_feature_flags = crate::layout::optional_features::HEF_NATIVE_DELETION_VECTORS;
    assert!(summary_from_footer(&footer).has_deletion_vectors);

    footer.optional_feature_flags = 0;
    assert!(!summary_from_footer(&footer).has_deletion_vectors);
}

#[test]
fn summary_copies_the_late_events_flag_from_the_footer() {
    // The summary is a faithful copy of footer metadata: a file declaring late events must not get a manifest summary
    // that denies having them (issue #4055).
    let mut footer = footer_with(vec![granule(0, 1, 10, 100, 200)], 4);
    footer.optional_feature_flags = crate::layout::optional_features::HEF_LATE_EVENTS;
    assert!(summary_from_footer(&footer).has_late_events);

    footer.optional_feature_flags = 0;
    assert!(!summary_from_footer(&footer).has_late_events);
}

#[test]
fn summary_from_empty_footer_prunes_everything() {
    let footer = footer_with(Vec::new(), 0);
    let summary = summary_from_footer(&footer);
    // Empty coverage (min > max) -> any predicate prunes.
    assert!(can_prune_file(&summary, &FilePredicate::default()));
}

#[test]
fn prune_when_predicate_range_is_outside_coverage() {
    let summary = summary_from_footer(&footer_with(vec![granule(0, 10, 20, 1_000, 2_000)], 4));

    // Occurred-at entirely above the file's coverage.
    assert!(can_prune_file(
        &summary,
        &FilePredicate {
            min_occurred_at_physical: Some(3_000),
            ..FilePredicate::default()
        }
    ));
    // Occurred-at entirely below.
    assert!(can_prune_file(
        &summary,
        &FilePredicate {
            max_occurred_at_physical: Some(500),
            ..FilePredicate::default()
        }
    ));
    // Sequence entirely above.
    assert!(can_prune_file(
        &summary,
        &FilePredicate {
            min_sequence: Some(100),
            ..FilePredicate::default()
        }
    ));
    // Sequence entirely below.
    assert!(can_prune_file(
        &summary,
        &FilePredicate {
            max_sequence: Some(5),
            ..FilePredicate::default()
        }
    ));
}

#[test]
fn never_prune_when_coverage_overlaps() {
    let summary = summary_from_footer(&footer_with(vec![granule(0, 10, 20, 1_000, 2_000)], 4));

    // Predicate range overlaps the occurred-at coverage -> must NOT prune.
    assert!(!can_prune_file(
        &summary,
        &FilePredicate {
            min_occurred_at_physical: Some(1_500),
            max_occurred_at_physical: Some(2_500),
            ..FilePredicate::default()
        }
    ));
    // Predicate touches the boundary (inclusive) -> overlap, must NOT prune.
    assert!(!can_prune_file(
        &summary,
        &FilePredicate {
            min_occurred_at_physical: Some(2_000),
            ..FilePredicate::default()
        }
    ));
    assert!(!can_prune_file(
        &summary,
        &FilePredicate {
            max_sequence: Some(10),
            ..FilePredicate::default()
        }
    ));
    // Fully unbounded predicate cannot rule the file out.
    assert!(!can_prune_file(&summary, &FilePredicate::default()));
    // One axis that rules the file out is enough, even when the other overlaps.
    assert!(can_prune_file(
        &summary,
        &FilePredicate {
            min_occurred_at_physical: Some(1_500), // overlaps
            min_sequence: Some(1_000),             // above -> prune
            ..FilePredicate::default()
        }
    ));
}

#[test]
fn planning_reads_granules_from_footer() {
    let footer = footer_with(vec![granule(3, 1, 5, 1, 2), granule(4, 6, 9, 3, 4)], 8);
    assert!(footer_is_authoritative_for_planning());
    assert_eq!(plan_granules_from_footer(&footer), vec![3, 4]);
}
