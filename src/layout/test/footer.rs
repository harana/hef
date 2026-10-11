use super::*;
use crate::benchmarks::{ProfileOverheadMeasurement, profile_overhead_gate_passes};
use crate::events::{RelationshipKind, RelationshipRef};
use crate::layout::HEADER_BLOCK_LEN;
use crate::writer::build::{BuiltHef, build_hef_file};
use crate::writer::source_form::tests::{plain_row, source_form_config};

fn minimal_footer() -> Footer {
    Footer {
        audit: None,
        clustering: Vec::new(),
        columns: vec![ColumnDescriptor {
            column_id: 0,
            internal_only: false,
            kind: ColumnKind::U64,
            name: "epoch".to_owned(),
            nullable: false,
        }],
        dictionaries: FileDictionaries::default(),
        embedding_row_offsets: Vec::new(),
        entity_hash_filters: Vec::new(),
        escape_hatches: Vec::new(),
        exact_counts: ExactCounts {
            row_count: 1,
            ..ExactCounts::default()
        },
        external_ids: Vec::new(),
        format_version: (1, 0),
        freetext: Vec::new(),
        freetext_row_offsets: Vec::new(),
        granules: vec![GranuleEntry {
            compressed_bytes_estimate: 0,
            first_epoch: 1,
            first_row_ordinal: 0,
            first_sequence: 1,
            granule_id: 0,
            last_epoch: 1,
            last_sequence: 100,
            max_ingested_at_physical: 0,
            max_occurred_at_physical: 0,
            min_ingested_at_physical: 0,
            min_occurred_at_physical: 0,
            row_count: 1,
            stripe_id: 0,
        }],
        graph: None,
        inbox_recipients: Vec::new(),
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
        profile: crate::layout::Profile::Event,
        reference_filters: None,
        required_feature_flags: 0,
        schema_fingerprint: [0u8; 32],
        shared_dictionaries: Vec::new(),
        shredded: Vec::new(),
        snapshot_keys: None,
        sparse_keys: Vec::new(),
        state_roots: Vec::new(),
        stripe_checksums: Vec::new(),
        stripe_proofs: Vec::new(),
        stripe_ndv: Vec::new(),
        stripes: Vec::new(),
        text_token_indexes: Vec::new(),
        text_token_offsets: Vec::new(),
        time_series_rollups: None,
    }
}

#[test]
fn clustering_entries_round_trip_through_encode_decode() {
    let mut footer = minimal_footer();
    footer.clustering = vec![
        ClusteringEntry {
            clustering_quality: 0.9,
            granule_id: 0,
            projection_id: 1,
            sortedness_proof: Some(SortednessProof {
                column_names: vec!["entity_id_hash".to_owned(), "occurred_at".to_owned()],
                direction: SortDirection::Ascending,
            }),
        },
        ClusteringEntry {
            clustering_quality: 0.2,
            granule_id: 1,
            projection_id: 1,
            sortedness_proof: None,
        },
    ];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert_eq!(decoded.clustering, footer.clustering);
}

/// A shared alphabet round-trips only in strictly ascending order: every shared-scope evaluator binary-searches the
/// alphabet, so an unsorted or duplicated one is refused at decode as structural corruption rather than silently
/// corrupting predicate answers.
#[test]
fn a_shared_dictionary_alphabet_must_be_strictly_ascending_to_decode() {
    let mut footer = minimal_footer();
    footer.shared_dictionaries = vec![SharedDictionaryEntry {
        column_id: 7,
        values: vec!["active".to_owned(), "closed".to_owned(), "open".to_owned()],
    }];
    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("a sorted alphabet decodes");
    assert_eq!(decoded.shared_dictionaries, footer.shared_dictionaries);

    for values in [
        vec!["closed".to_owned(), "active".to_owned()],
        vec!["active".to_owned(), "active".to_owned()],
    ] {
        footer.shared_dictionaries = vec![SharedDictionaryEntry { column_id: 7, values }];
        let bytes = encode_footer(&footer);
        assert!(
            decode_footer(&bytes).is_err(),
            "an out-of-order or duplicated alphabet must refuse to decode"
        );
    }
}

#[test]
fn text_token_entries_round_trip_through_encode_decode() {
    use crate::indexes::text_token::TextTokenIndex;

    let mut footer = minimal_footer();
    footer.text_token_indexes = vec![
        TextTokenEntry {
            column_id: 7,
            granule_id: 0,
            index_bytes: TextTokenIndex::build_from_values(&["alpha beta", "gamma"], false).encode(),
            page_index: 0,
        },
        TextTokenEntry {
            column_id: 7,
            granule_id: 0,
            index_bytes: TextTokenIndex::build_from_values(&["delta"], false).encode(),
            page_index: 1,
        },
    ];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert_eq!(decoded.text_token_indexes, footer.text_token_indexes);

    let index = TextTokenIndex::decode(&decoded.text_token_indexes[0].index_bytes).expect("stored index decodes");
    assert!(index.might_contain_token("beta"));
    assert!(!index.might_contain_token("delta"));
}

#[test]
fn entity_hash_filter_ranges_round_trip_through_encode_decode() {
    let mut footer = minimal_footer();
    footer.entity_hash_filters = vec![
        EntityHashFilterEntry {
            granule_id: 0,
            index_len: 296,
            index_offset: 4096,
        },
        EntityHashFilterEntry {
            granule_id: 1,
            index_len: 40,
            index_offset: 8192,
        },
    ];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert_eq!(decoded.entity_hash_filters, footer.entity_hash_filters);
}

#[test]
fn absent_entity_hash_filter_section_decodes_to_empty() {
    let footer = minimal_footer();
    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert!(decoded.entity_hash_filters.is_empty());
}

#[test]
fn external_id_index_and_reference_filters_round_trip_through_encode_decode() {
    let mut footer = minimal_footer();
    footer.external_ids = vec![
        ExternalIdEntry {
            id_hash: 3,
            row_ordinal: 0,
        },
        ExternalIdEntry {
            id_hash: 9,
            row_ordinal: 4,
        },
    ];
    footer.reference_filters = Some(vec![ReferenceFilterEntry {
        column_id: 7005,
        filter: vec![1, 2, 3],
        granule_id: 0,
    }]);
    let decoded = decode_footer(&encode_footer(&footer)).expect("footer decodes without error");
    assert_eq!(decoded.external_ids, footer.external_ids);
    assert_eq!(decoded.reference_filters, footer.reference_filters);

    // An empty filter list still says "this file has filters", unlike an absent section.
    footer.reference_filters = Some(Vec::new());
    let decoded = decode_footer(&encode_footer(&footer)).expect("footer decodes without error");
    assert_eq!(decoded.reference_filters, Some(Vec::new()));
    let decoded = decode_footer(&encode_footer(&minimal_footer())).expect("footer decodes without error");
    assert_eq!(decoded.reference_filters, None);
    assert!(decoded.external_ids.is_empty());
}

#[test]
fn inbox_recipient_directory_round_trips_and_rejects_a_forged_count() {
    let mut footer = minimal_footer();
    footer.inbox_recipients = vec![
        InboxRecipientEntry {
            entity_id_hash_high: 2,
            entity_id_hash_low: 1,
            first_row_ordinal: 0,
            last_epoch: 1,
            last_sequence: 9,
            row_count: 3,
        },
        InboxRecipientEntry {
            entity_id_hash_high: 2,
            entity_id_hash_low: 5,
            first_row_ordinal: 3,
            last_epoch: 2,
            last_sequence: 4,
            row_count: 1,
        },
    ];
    let mut bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert_eq!(decoded.inbox_recipients, footer.inbox_recipients);
    let decoded = decode_footer(&encode_footer(&minimal_footer())).expect("footer decodes without error");
    assert!(decoded.inbox_recipients.is_empty());

    let (entry_start, section_offset, section_len) = find_directory_entry(&bytes, sections::INBOX_RECIPIENTS);
    bytes[section_offset..section_offset + 4].copy_from_slice(&3u32.to_le_bytes());
    let section_bytes = bytes[section_offset..section_offset + section_len].to_vec();
    write_checksum_at(&mut bytes, entry_start, &section_bytes);
    assert_eq!(
        decode_footer(&bytes),
        Err(FormatError::Structural {
            rule: "inbox recipient count exceeds input"
        })
    );
}

#[test]
fn absent_text_token_section_decodes_to_empty() {
    let footer = minimal_footer();
    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert!(decoded.text_token_indexes.is_empty());
}

#[test]
fn embedding_row_offsets_round_trip_through_encode_decode() {
    let mut footer = minimal_footer();
    footer.embedding_row_offsets = vec![
        EmbeddingRowOffsets {
            bytes_len: 128,
            bytes_offset: 64,
            column_id: 5000,
            granule_id: 0,
            offsets_len: 16,
            offsets_offset: 48,
        },
        EmbeddingRowOffsets {
            bytes_len: 256,
            bytes_offset: 200,
            column_id: 5000,
            granule_id: 1,
            offsets_len: 16,
            offsets_offset: 184,
        },
    ];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert_eq!(decoded.embedding_row_offsets, footer.embedding_row_offsets);
}

#[test]
fn page_minmax_round_trips_through_encode_decode() {
    let mut footer = minimal_footer();
    footer.page_minmax = vec![
        PageMinMax {
            column_id: 7,
            granule_id: 0,
            max_i128: Some(9_000),
            min_i128: Some(-42),
            null_count: 2,
            page_index: 0,
            row_count: 100,
        },
        PageMinMax {
            column_id: 7,
            granule_id: 1,
            max_i128: None,
            min_i128: None,
            null_count: 0,
            page_index: 3,
            row_count: 64,
        },
    ];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes without error");
    assert_eq!(decoded.page_minmax, footer.page_minmax);
}

#[test]
fn footer_without_embedding_row_offsets_decodes_to_empty_vec() {
    let footer = minimal_footer();
    assert!(footer.embedding_row_offsets.is_empty());
    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes");
    assert!(decoded.embedding_row_offsets.is_empty());
}

#[test]
fn footer_without_clustering_decodes_to_empty_vec() {
    let footer = minimal_footer();
    assert!(footer.clustering.is_empty());
    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes");
    assert!(decoded.clustering.is_empty());
}

/// Scenario: planner skips a sort it can prove unnecessary.
///
/// When a query requires output ordered by `(entity_id_hash, occurred_at)` and every granule of the chosen projection
/// carries a sortedness proof for that ordering, `all_granules_sorted_for` returns `true` — the planner may set
/// `ordering_proven` and drop the sort stage.
#[test]
fn all_granules_sorted_for_returns_true_when_every_entry_has_matching_proof() {
    let ordering = ["entity_id_hash", "occurred_at"];
    let mut footer = minimal_footer();
    footer.clustering = vec![
        ClusteringEntry {
            clustering_quality: 0.9,
            granule_id: 0,
            projection_id: 2,
            sortedness_proof: Some(SortednessProof {
                column_names: ordering.iter().map(|s| s.to_string()).collect(),
                direction: SortDirection::Ascending,
            }),
        },
        ClusteringEntry {
            clustering_quality: 0.85,
            granule_id: 1,
            projection_id: 2,
            sortedness_proof: Some(SortednessProof {
                column_names: ordering.iter().map(|s| s.to_string()).collect(),
                direction: SortDirection::Ascending,
            }),
        },
    ];

    assert!(footer.all_granules_sorted_for(2, &ordering, SortDirection::Ascending));
}

#[test]
fn all_granules_sorted_for_returns_false_when_any_entry_lacks_proof() {
    let ordering = ["entity_id_hash", "occurred_at"];
    let mut footer = minimal_footer();
    footer.granules.push(second_granule());
    footer.clustering = vec![
        ClusteringEntry {
            clustering_quality: 0.9,
            granule_id: 0,
            projection_id: 2,
            sortedness_proof: Some(SortednessProof {
                column_names: ordering.iter().map(|s| s.to_string()).collect(),
                direction: SortDirection::Ascending,
            }),
        },
        ClusteringEntry {
            clustering_quality: 0.3,
            granule_id: 1,
            projection_id: 2,
            sortedness_proof: None,
        },
    ];

    assert!(!footer.all_granules_sorted_for(2, &ordering, SortDirection::Ascending));
}

/// A granule with no clustering entry at all has no sortedness proof, so the projection is not proven ordered —
/// eliding the sort would reorder rows nothing ever proved were in order.
#[test]
fn all_granules_sorted_for_returns_false_when_a_granule_has_no_clustering_entry() {
    let ordering = ["entity_id_hash", "occurred_at"];
    let mut footer = minimal_footer();
    footer.granules.push(second_granule());
    // Only granule 0 is proven; granule 1 is named by the file but has no entry for the projection.
    footer.clustering = vec![ClusteringEntry {
        clustering_quality: 0.9,
        granule_id: 0,
        projection_id: 2,
        sortedness_proof: Some(SortednessProof {
            column_names: ordering.iter().map(|s| s.to_string()).collect(),
            direction: SortDirection::Ascending,
        }),
    }];

    assert!(!footer.all_granules_sorted_for(2, &ordering, SortDirection::Ascending));
}

/// A second physical granule, so a test can hold one proven granule and one that is not.
fn second_granule() -> GranuleEntry {
    GranuleEntry {
        granule_id: 1,
        first_row_ordinal: 1,
        first_sequence: 101,
        last_sequence: 200,
        ..minimal_footer().granules[0]
    }
}

#[test]
fn all_granules_sorted_for_returns_false_for_unknown_projection() {
    let footer = minimal_footer();
    assert!(!footer.all_granules_sorted_for(99, &["epoch"], SortDirection::Ascending));
}

/// Byte offset of the section directory's first entry: `format_version(4) + feature_flags(16) + fingerprint(32) +
/// section_count(4)`.
const DIRECTORY_START: usize = 56;
/// One section-directory entry: `id(4) + offset(8) + len(8) + checksum(32)`.
const DIRECTORY_ENTRY_LEN: usize = 52;

fn section_count(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[52..56].try_into().expect("4 bytes"))
}

/// Locates the `(directory_entry_start, section_offset, section_len)` for `id`, where `section_offset` is absolute
/// within `bytes` (directory-relative offset plus the start of the section area).
fn find_directory_entry(bytes: &[u8], id: u32) -> (usize, usize, usize) {
    let count = section_count(bytes) as usize;
    let section_area_start = DIRECTORY_START + count * DIRECTORY_ENTRY_LEN;
    for index in 0..count {
        let entry_start = DIRECTORY_START + index * DIRECTORY_ENTRY_LEN;
        let entry_id = u32::from_le_bytes(bytes[entry_start..entry_start + 4].try_into().expect("4 bytes"));
        if entry_id == id {
            let rel_offset = u64::from_le_bytes(bytes[entry_start + 4..entry_start + 12].try_into().expect("8 bytes"));
            let len = u64::from_le_bytes(bytes[entry_start + 12..entry_start + 20].try_into().expect("8 bytes"));
            return (entry_start, section_area_start + rel_offset as usize, len as usize);
        }
    }
    panic!("section {id} not found in directory");
}

fn write_u64_at(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

fn write_checksum_at(bytes: &mut [u8], entry_start: usize, section_bytes: &[u8]) {
    let checksum = blake3::hash(section_bytes);
    bytes[entry_start + 20..entry_start + 52].copy_from_slice(checksum.as_bytes());
}

/// Scenario: bounded decode of a hostile footer — a section's recorded element count outruns the bytes actually
/// present in that section. The decoder must reject the footer via `bounded_count` rather than let a forged count
/// drive an unbounded `Vec::with_capacity`.
#[test]
fn inconsistent_recorded_count_is_rejected_without_unbounded_allocation() {
    let footer = minimal_footer();
    let mut bytes = encode_footer(&footer);

    let (entry_start, section_offset, section_len) = find_directory_entry(&bytes, sections::COLUMNS);
    // The column count is the section's first u32. Forge it to claim far more columns than the section holds.
    bytes[section_offset..section_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    let section_bytes = bytes[section_offset..section_offset + section_len].to_vec();
    write_checksum_at(&mut bytes, entry_start, &section_bytes);

    let err = decode_footer(&bytes).expect_err("forged column count must be rejected");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "column count exceeds input"
        }
    );
}

/// Scenario: bounded decode of a hostile footer — a directory entry claims a byte extent larger than the section
/// area actually holds. The decoder must reject the footer (via the bounds-checked `slice` helper) rather than read
/// or allocate past the end of the input.
#[test]
fn oversized_byte_extent_is_rejected() {
    let footer = minimal_footer();
    let mut bytes = encode_footer(&footer);

    let (entry_start, _section_offset, section_len) = find_directory_entry(&bytes, sections::MARKS);
    // Claim a length far beyond both the section area and the whole blob.
    write_u64_at(&mut bytes, entry_start + 12, section_len as u64 + 1_000_000_000);

    let err = decode_footer(&bytes).expect_err("oversized extent must be rejected");
    assert_eq!(err, FormatError::Truncated { what: "section" });
}

/// Scenario: unknown section skipped — a directory entry for a section id the decoder does not know carries a
/// wildly oversized (and otherwise invalid) offset/length. Decoding still succeeds because an unknown id is never
/// looked up, let alone read: it is skipped entirely by its directory-recorded length.
#[test]
fn unknown_section_id_is_skipped_without_being_read() {
    let footer = minimal_footer();
    let bytes = encode_footer(&footer);

    let count = section_count(&bytes);
    let insert_at = DIRECTORY_START + count as usize * DIRECTORY_ENTRY_LEN;

    let mut forged_entry = Vec::with_capacity(DIRECTORY_ENTRY_LEN);
    forged_entry.extend_from_slice(&9_999u32.to_le_bytes()); // unknown section id
    forged_entry.extend_from_slice(&0u64.to_le_bytes()); // offset
    forged_entry.extend_from_slice(&u64::MAX.to_le_bytes()); // absurd length
    forged_entry.extend_from_slice(&[0u8; 32]); // checksum, never verified since never read

    let mut mutated = bytes.clone();
    mutated.splice(insert_at..insert_at, forged_entry);
    mutated[52..56].copy_from_slice(&(count + 1).to_le_bytes());

    let decoded = decode_footer(&mutated).expect("unknown section is skipped, not read");
    assert_eq!(decoded.columns, footer.columns);
}

/// Scenario: checksum failure rejected — a section's bytes are altered without updating its recorded BLAKE3
/// checksum. The decoder must reject the section rather than decode the tampered bytes.
#[test]
fn checksum_mismatch_is_rejected_rather_than_decoded() {
    let footer = minimal_footer();
    let mut bytes = encode_footer(&footer);

    let (_entry_start, section_offset, _section_len) = find_directory_entry(&bytes, sections::COLUMNS);
    // Flip a byte inside the column name without touching the recorded checksum.
    bytes[section_offset] ^= 0xFF;

    let err = decode_footer(&bytes).expect_err("tampered section must fail checksum verification");
    assert_eq!(
        err,
        FormatError::Blake3Mismatch {
            scope: "footer section"
        }
    );
}

fn sample_mark(projection_id: u32, column_id: u32, granule_id: u32, first_value_offset: Option<u64>) -> ColumnMark {
    ColumnMark {
        codec_pipeline_id: PipelineId(7),
        column_id,
        compressed_offset: u64::from(granule_id) * 1000,
        compressed_size: 100 + u64::from(granule_id),
        first_value_offset,
        granule_id,
        page_count: 1,
        projection_id,
        row_count: 8192,
        uncompressed_offset: u64::from(granule_id) * 2000,
        uncompressed_size: 200 + u64::from(granule_id),
    }
}

fn sample_page(projection_id: u32, column_id: u32, granule_id: u32, page_index: u32) -> PageDirectoryEntry {
    let base = u64::from(granule_id) * 10 + u64::from(page_index);
    PageDirectoryEntry {
        column_id,
        compressed_len: 50 + base,
        compressed_offset: base * 1000,
        first_row_ordinal: base * 8192,
        granule_id,
        max_f64: Some(base as f64 + 0.5),
        max_i128: Some(base as i128 + 100),
        max_occurred_at_physical: base as i64 + 5,
        max_sequence: base + 9,
        min_f64: Some(base as f64),
        min_i128: Some(base as i128),
        min_occurred_at_physical: -(base as i64) - 5,
        min_sequence: base,
        null_count: page_index,
        page_index,
        projection_id,
        row_count: 4096,
    }
}

/// Scenario: marks and the per-page directory both decode from columnar arrays riding the same per-stripe page — the
/// columnar and row-oriented forms decode to a byte-identical logical directory, spanning multiple projections,
/// columns, stripes, and multi-page groups.
#[test]
fn columnar_marks_round_trip_to_byte_identical_logical_directory() {
    let marks = vec![
        sample_mark(0, 1, 0, None),
        sample_mark(0, 1, 1, Some(42)),
        sample_mark(0, 1, 2, None),
        sample_mark(0, 2, 0, None),
        sample_mark(1, 1, 0, None),
    ];
    let page_directory = vec![
        sample_page(0, 1, 0, 0),
        sample_page(0, 1, 0, 1),
        sample_page(0, 1, 1, 0),
        sample_page(0, 1, 2, 0),
        sample_page(0, 2, 0, 0),
        sample_page(1, 1, 0, 0),
    ];
    // Granules 0 and 1 sit in stripe 0; granule 2 rolls into stripe 1.
    let granule_stripe_ids = BTreeMap::from([(0, 0), (1, 0), (2, 1)]);

    let bytes = encode_columnar_marks(&marks, &page_directory, &granule_stripe_ids).expect("encode succeeds");
    let (mut decoded_marks, mut decoded_pages) = decode_columnar_marks(&bytes).expect("decode succeeds");

    let mark_sort_key = |mark: &ColumnMark| (mark.projection_id, mark.column_id, mark.granule_id);
    let mut expected_marks = marks;
    decoded_marks.sort_by_key(mark_sort_key);
    expected_marks.sort_by_key(mark_sort_key);
    assert_eq!(decoded_marks, expected_marks);

    let page_sort_key =
        |page: &PageDirectoryEntry| (page.projection_id, page.column_id, page.granule_id, page.page_index);
    let mut expected_pages = page_directory;
    decoded_pages.sort_by_key(page_sort_key);
    expected_pages.sort_by_key(page_sort_key);
    assert_eq!(decoded_pages, expected_pages);
}

/// Scenario: a file with no per-page directory at all (the `PER_PAGE_MARKS` optional feature absent) still round-trips
/// its marks — an empty per-group page array is a valid, additive absence, not an encode/decode error.
#[test]
fn columnar_marks_round_trip_with_no_page_directory() {
    let marks = vec![sample_mark(0, 1, 0, None), sample_mark(0, 1, 1, None)];
    let granule_stripe_ids = BTreeMap::from([(0, 0), (1, 0)]);

    let bytes = encode_columnar_marks(&marks, &[], &granule_stripe_ids).expect("encode succeeds");
    let (mut decoded_marks, decoded_pages) = decode_columnar_marks(&bytes).expect("decode succeeds");

    let sort_key = |mark: &ColumnMark| (mark.projection_id, mark.column_id, mark.granule_id);
    let mut expected_marks = marks;
    decoded_marks.sort_by_key(sort_key);
    expected_marks.sort_by_key(sort_key);
    assert_eq!(decoded_marks, expected_marks);
    assert!(decoded_pages.is_empty(), "no page directory was supplied to encode");
}

/// Scenario: a hostile columnar marks page declares a page-directory count far beyond what its remaining bytes could
/// encode. Every entry carries at least four option-tag bytes read straight from the page, so the count must be
/// rejected up front rather than sizing any allocation (issue #4000). The directory decodes on its own, apart from
/// the page's marks, so that is where the bound is enforced — before any caller can be handed an entry.
#[test]
fn columnar_page_directory_count_beyond_remaining_bytes_is_rejected() {
    let mut out = Writer::new();
    // Marks half of the page: zero marks, eight empty field arrays, no option tags.
    out.put_u32(0);
    for _ in 0..8 {
        encode_u64_column(&mut out, std::iter::empty());
    }
    // Page-directory half: declare 200 entries backed by only 400 trailing bytes — enough to pass a one-byte-per-entry
    // bound, but 200 entries need at least 800 option-tag bytes.
    out.put_u32(200);
    out.put_slice(&[0u8; 400]);
    let err =
        decode_marks_page_directory(1, 0, &out.into_bytes()).expect_err("forged page directory count must be rejected");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "columnar page directory count exceeds input"
        }
    );
}

/// Scenario: a hostile columnar marks page declares more marks than its field arrays actually decode to. The count is
/// re-validated against the decoded arrays before the marks vector is built, so the forged count dies as a length
/// mismatch instead of driving an oversized allocation (issue #4000).
#[test]
fn columnar_marks_count_beyond_decoded_arrays_is_rejected() {
    let mut out = Writer::new();
    out.put_u32(64);
    for _ in 0..8 {
        encode_u64_column(&mut out, std::iter::empty());
    }
    // Padding keeps the declared count within the page's byte budget, so the array-length check is what rejects it.
    out.put_slice(&[0u8; 64]);
    let err = decode_marks_page(1, 0, &out.into_bytes()).expect_err("forged mark count must be rejected");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "columnar marks column length mismatch"
        }
    );
}

/// Scenario: a mark whose granule has no entry in the writer-supplied stripe map is a caller error, not a silently
/// dropped mark — the authoritative directory must never lose an entry.
#[test]
fn columnar_marks_encode_rejects_a_granule_missing_from_the_stripe_map() {
    let marks = vec![sample_mark(0, 1, 0, None)];
    let err = encode_columnar_marks(&marks, &[], &BTreeMap::new()).expect_err("missing stripe mapping must error");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "columnar marks encode missing granule stripe mapping"
        }
    );
}

/// Scenario: a page-directory entry whose granule has no entry in the writer-supplied stripe map is a caller error
/// too — the per-page directory must never silently lose an entry either. The marks' own granule is mapped, so this
/// exercises the page-directory branch specifically rather than the mark-loop check above.
#[test]
fn columnar_marks_encode_rejects_a_page_directory_entry_missing_from_the_stripe_map() {
    let marks = vec![sample_mark(0, 1, 0, None)];
    let page_directory = vec![sample_page(0, 1, 5, 0)];
    let granule_stripe_ids = BTreeMap::from([(0, 0)]);
    let err = encode_columnar_marks(&marks, &page_directory, &granule_stripe_ids)
        .expect_err("missing stripe mapping must error");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "columnar marks encode missing granule stripe mapping"
        }
    );
}

/// Scenario: `columnar_marks` is a known required feature and sits in `ALL`: the writer emits the columnar marks
/// section in every new file, so every new file declares the bit and old readers refuse it.
#[test]
fn columnar_marks_is_a_known_required_feature_every_new_file_declares() {
    use crate::layout::required_features;

    assert_eq!(required_features::COLUMNAR_MARKS, 1 << 15);
    assert_eq!(
        required_features::KNOWN & required_features::COLUMNAR_MARKS,
        required_features::COLUMNAR_MARKS
    );
    assert_eq!(
        required_features::ALL & required_features::COLUMNAR_MARKS,
        required_features::COLUMNAR_MARKS
    );
}

fn columnar_granule(granule_id: u32, stripe_id: u32) -> GranuleEntry {
    GranuleEntry {
        compressed_bytes_estimate: 0,
        first_epoch: 1,
        first_row_ordinal: 0,
        first_sequence: 1,
        granule_id,
        last_epoch: 1,
        last_sequence: 100,
        max_ingested_at_physical: 0,
        max_occurred_at_physical: 0,
        min_ingested_at_physical: 0,
        min_occurred_at_physical: 0,
        row_count: 1,
        stripe_id,
    }
}

/// Scenario: a footer that declares `columnar_marks` decodes only the two-level marks directory at open — the
/// row-oriented `marks` and `page_directory` both stay empty, and the page bytes are held raw for a reader to decode
/// per surviving stripe, one page per `(projection, column, stripe)` group rather than one entry per granule. That
/// page carries both the group's marks and its per-page directory entries — task 4.3's addition. Implements
/// `hef-file-layout` — "Pages are independently addressable within a granule".
#[test]
fn columnar_marks_footer_decodes_directory_only_leaving_row_oriented_marks_and_page_directory_empty() {
    let mut footer = minimal_footer();
    footer.required_feature_flags = required_features::COLUMNAR_MARKS;
    // Granules 0 and 1 sit in stripe 0; granule 2 rolls into stripe 1.
    footer.granules = vec![columnar_granule(0, 0), columnar_granule(1, 0), columnar_granule(2, 1)];
    footer.marks = vec![
        sample_mark(0, 1, 0, None),
        sample_mark(0, 1, 1, None),
        sample_mark(0, 1, 2, None),
    ];
    footer.page_directory = vec![
        sample_page(0, 1, 0, 0),
        sample_page(0, 1, 1, 0),
        sample_page(0, 1, 2, 0),
    ];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes");

    assert!(
        decoded.marks.is_empty(),
        "row-oriented marks must stay empty for a columnar file"
    );
    assert!(
        decoded.page_directory.is_empty(),
        "row-oriented page directory must stay empty for a columnar file"
    );
    assert_eq!(
        decoded.marks_directory.len(),
        2,
        "one page per (projection, column, stripe) group"
    );

    let mut rebuilt_marks = Vec::new();
    let mut rebuilt_pages = Vec::new();
    for entry in &decoded.marks_directory {
        let (marks, pages) = decode_columnar_marks_page(&decoded.marks_pages, entry).expect("every page decodes");
        rebuilt_marks.extend(marks);
        rebuilt_pages.extend(pages);
    }

    let mark_sort_key = |mark: &ColumnMark| (mark.projection_id, mark.column_id, mark.granule_id);
    rebuilt_marks.sort_by_key(mark_sort_key);
    let mut expected_marks = footer.marks.clone();
    expected_marks.sort_by_key(mark_sort_key);
    assert_eq!(rebuilt_marks, expected_marks);

    let page_sort_key =
        |page: &PageDirectoryEntry| (page.projection_id, page.column_id, page.granule_id, page.page_index);
    rebuilt_pages.sort_by_key(page_sort_key);
    let mut expected_pages = footer.page_directory.clone();
    expected_pages.sort_by_key(page_sort_key);
    assert_eq!(rebuilt_pages, expected_pages);
}

/// Scenario: a footer that does not declare `columnar_marks` decodes the row-oriented `marks` exactly as before, and
/// the columnar directory/pages stay empty.
#[test]
fn row_oriented_marks_footer_leaves_columnar_directory_empty() {
    let mut footer = minimal_footer();
    footer.marks = vec![sample_mark(0, 1, 0, None)];

    let bytes = encode_footer(&footer);
    let decoded = decode_footer(&bytes).expect("footer decodes");

    assert_eq!(decoded.marks, footer.marks);
    assert!(decoded.marks_directory.is_empty());
    assert!(decoded.marks_pages.is_empty());
}

/// Task 7.1 (issue #2051): round-trip parity and dual-emit. The identical logical marks and per-page directory,
/// encoded once as the row-oriented form and once columnar under `columnar_marks`, decode to the *same*
/// `(projection, column, granule)` directory — matching mark `compressed_offset`, `compressed_size`, and `row_count`
/// (and every other field) and matching per-page entries. A migration writer may therefore dual-emit both forms and a
/// reader gets byte-identical results whether it takes the row-oriented or the columnar decode path. Implements
/// `hef-file-layout` — "Granule directory and authoritative marks": "Marks decode from columnar arrays".
#[test]
fn columnar_and_row_oriented_forms_decode_to_the_same_logical_directory() {
    let marks = vec![
        sample_mark(0, 1, 0, None),
        sample_mark(0, 1, 1, Some(42)),
        sample_mark(0, 1, 2, None),
        sample_mark(0, 2, 0, None),
        sample_mark(1, 1, 0, None),
    ];
    let page_directory = vec![
        sample_page(0, 1, 0, 0),
        sample_page(0, 1, 0, 1),
        sample_page(0, 1, 1, 0),
        sample_page(0, 1, 2, 0),
        sample_page(0, 2, 0, 0),
        sample_page(1, 1, 0, 0),
    ];
    // Granules 0 and 1 sit in stripe 0; granule 2 rolls into stripe 1.
    let granules = vec![columnar_granule(0, 0), columnar_granule(1, 0), columnar_granule(2, 1)];

    // The row-oriented form: no `columnar_marks`, marks and the per-page directory in their own footer sections.
    let mut row_oriented = minimal_footer();
    row_oriented.granules = granules.clone();
    row_oriented.marks = marks.clone();
    row_oriented.page_directory = page_directory.clone();
    let row_decoded = decode_footer(&encode_footer(&row_oriented)).expect("row-oriented footer decodes");

    // The columnar form: the same logical directory, `columnar_marks` declared, two-level per-stripe marks pages.
    let mut columnar = minimal_footer();
    columnar.required_feature_flags = required_features::COLUMNAR_MARKS;
    columnar.granules = granules;
    columnar.marks = marks;
    columnar.page_directory = page_directory;
    let col_decoded = decode_footer(&encode_footer(&columnar)).expect("columnar footer decodes");

    assert!(
        col_decoded.marks.is_empty() && col_decoded.page_directory.is_empty(),
        "a columnar footer carries no row-oriented marks or page directory"
    );
    assert!(
        !row_decoded.marks.is_empty() && !row_decoded.page_directory.is_empty(),
        "the row-oriented footer carries both sections"
    );

    // The columnar decode path: reconstruct the directory by decoding every per-stripe page.
    let mut col_marks = Vec::new();
    let mut col_pages = Vec::new();
    for entry in &col_decoded.marks_directory {
        let (page_marks, page_pages) =
            decode_columnar_marks_page(&col_decoded.marks_pages, entry).expect("page decodes");
        col_marks.extend(page_marks);
        col_pages.extend(page_pages);
    }

    let mark_key = |m: &ColumnMark| (m.projection_id, m.column_id, m.granule_id);
    let page_key = |p: &PageDirectoryEntry| (p.projection_id, p.column_id, p.granule_id, p.page_index);

    // Parity of the whole logical directory against the row-oriented decode, field for field.
    let mut expected_marks = row_decoded.marks.clone();
    expected_marks.sort_by_key(mark_key);
    col_marks.sort_by_key(mark_key);
    assert_eq!(
        col_marks, expected_marks,
        "columnar marks must equal the row-oriented directory field for field"
    );

    let mut expected_pages = row_decoded.page_directory.clone();
    expected_pages.sort_by_key(page_key);
    col_pages.sort_by_key(page_key);
    assert_eq!(
        col_pages, expected_pages,
        "columnar per-page directory must equal the row-oriented one"
    );

    // The three fields the acceptance criterion names, resolved by `(projection, column, granule)`.
    for expected in &expected_marks {
        let got = col_marks
            .iter()
            .find(|m| mark_key(m) == mark_key(expected))
            .expect("every (projection, column, granule) resolves in the columnar directory");
        assert_eq!(
            (got.compressed_offset, got.compressed_size, got.row_count),
            (expected.compressed_offset, expected.compressed_size, expected.row_count),
        );
    }
}

#[test]
fn all_granules_sorted_for_returns_false_when_columns_differ() {
    let mut footer = minimal_footer();
    footer.clustering = vec![ClusteringEntry {
        clustering_quality: 0.9,
        granule_id: 0,
        projection_id: 3,
        sortedness_proof: Some(SortednessProof {
            column_names: vec!["epoch".to_owned()],
            direction: SortDirection::Ascending,
        }),
    }];

    // The planner asks about a different ordering — must not be proven.
    assert!(!footer.all_granules_sorted_for(3, &["occurred_at", "epoch"], SortDirection::Ascending));
}

/// Scenario: a checksummed footer whose page directory carries inverted bounds is rejected at decode.
///
/// The planner forwards these bounds as exact page statistics and drops a page when a searched value lies outside them,
/// so an inverted pair fails open: every matching row in the page is skipped. Both the row-oriented and the columnar
/// encoding of the directory must refuse it (issue #8927).
#[test]
fn page_directory_bounds_that_are_inverted_are_rejected_by_both_encodings() {
    let inverted = PageDirectoryEntry {
        max_i128: Some(10),
        min_i128: Some(100),
        ..sample_page(0, 1, 5, 0)
    };

    let mut row_oriented = minimal_footer();
    row_oriented.page_directory = vec![inverted.clone()];
    assert!(
        decode_footer(&encode_footer(&row_oriented)).is_err(),
        "an inverted integer bound must not decode from the row-oriented directory"
    );

    let marks = vec![sample_mark(0, 1, 5, None)];
    let granule_stripe_ids = std::collections::BTreeMap::from([(5u32, 0u32)]);
    let bytes = encode_columnar_marks(&marks, &[inverted.clone()], &granule_stripe_ids).expect("encode succeeds");
    assert!(
        decode_columnar_marks(&bytes).is_err(),
        "an inverted integer bound must not decode from the columnar directory"
    );

    // The other paired and ordered fields are checked the same way.
    for forged in [
        PageDirectoryEntry {
            max_i128: None,
            ..sample_page(0, 1, 5, 0)
        },
        PageDirectoryEntry {
            max_sequence: 1,
            min_sequence: 9,
            ..sample_page(0, 1, 5, 0)
        },
        PageDirectoryEntry {
            max_occurred_at_physical: -100,
            min_occurred_at_physical: 100,
            ..sample_page(0, 1, 5, 0)
        },
        PageDirectoryEntry {
            null_count: 5000,
            row_count: 4096,
            ..sample_page(0, 1, 5, 0)
        },
    ] {
        let mut footer = minimal_footer();
        footer.page_directory = vec![forged];
        assert!(decode_footer(&encode_footer(&footer)).is_err());
    }
}

#[test]
fn every_profile_round_trips_through_the_footer() {
    for profile in [
        Profile::Audit,
        Profile::Event,
        Profile::Graph,
        Profile::Inbox,
        Profile::Log,
        Profile::Snapshot,
        Profile::TimeSeries,
    ] {
        let mut footer = minimal_footer();
        footer.profile = profile;
        assert_eq!(decode_footer(&encode_footer(&footer)).unwrap().profile, profile);
    }
}

#[test]
fn an_event_footer_carries_no_profile_section() {
    let event = encode_footer(&minimal_footer());
    let mut graph = minimal_footer();
    graph.profile = Profile::Graph;
    let graph = encode_footer(&graph);
    // One directory entry (52 B) plus the one profile byte: the event footer is byte-identical to one written before
    // profiles existed.
    assert_eq!(graph.len(), event.len() + 4 + 8 + 8 + 32 + 1);
}

/// The section ids in a built file's stored footer directory.
fn stored_section_ids(built: &BuiltHef) -> Vec<u32> {
    let end = built.bytes.len() - built.tree_len.unwrap_or(0) as usize;
    let blob = &built.bytes[end - built.footer_len as usize..end - 8];
    (0..section_count(blob) as usize)
        .map(|index| {
            let at = DIRECTORY_START + index * DIRECTORY_ENTRY_LEN;
            u32::from_le_bytes(blob[at..at + 4].try_into().unwrap())
        })
        .collect()
}

/// The same rows built as an event file and as a graph file with no edges differ by one footer directory entry and
/// the one profile byte; the header block stays 4 KiB.
#[test]
fn a_profile_adds_one_footer_entry_and_one_byte_to_a_file() {
    let rows = || (0..20).map(plain_row).collect::<Vec<_>>();
    let event = build_hef_file(rows(), &source_form_config(4)).unwrap();
    let mut config = source_form_config(4);
    config.profile = Profile::Graph;
    let graph = build_hef_file(rows(), &config).unwrap();
    assert!(!stored_section_ids(&event).contains(&sections::PROFILE));
    assert!(stored_section_ids(&graph).contains(&sections::PROFILE));

    let measurement = ProfileOverheadMeasurement {
        event_file_bytes: event.bytes.len() as u64,
        event_header_block_bytes: event.footer.stripes[0].file_offset,
        profiled_file_bytes: graph.bytes.len() as u64,
        profiled_header_block_bytes: graph.footer.stripes[0].file_offset,
    };
    println!(
        "profile overhead: {} extra bytes, header block {} bytes",
        measurement.profiled_file_bytes - measurement.event_file_bytes,
        measurement.profiled_header_block_bytes
    );
    assert_eq!(measurement.profiled_file_bytes - measurement.event_file_bytes, 52 + 1);
    assert_eq!(measurement.profiled_header_block_bytes, HEADER_BLOCK_LEN as u64);
    assert!(profile_overhead_gate_passes(&measurement));
}

#[test]
fn an_unknown_footer_profile_code_is_a_structural_error() {
    let mut footer = minimal_footer();
    footer.profile = Profile::Graph;
    let mut blob = encode_footer(&footer);
    // The profile section is the only one-byte section, and the last byte of this footer's section area.
    let last = blob.len() - 1;
    assert_eq!(blob[last], Profile::Graph.code());
    blob[last] = 0xEE;
    // Re-seal the section checksum so the decoder reaches the profile code instead of failing on the hash.
    let section_count = u32::from_le_bytes(blob[52..56].try_into().unwrap()) as usize;
    let directory_end = 56 + section_count * 52;
    for entry in 0..section_count {
        let at = 56 + entry * 52;
        if u32::from_le_bytes(blob[at..at + 4].try_into().unwrap()) == sections::PROFILE {
            let offset = u64::from_le_bytes(blob[at + 4..at + 12].try_into().unwrap()) as usize;
            let hash = crate::file::integrity::hash_tree(&blob[directory_end + offset..directory_end + offset + 1]);
            blob[at + 20..at + 52].copy_from_slice(hash.as_bytes());
        }
    }
    assert_eq!(
        decode_footer(&blob).unwrap_err(),
        FormatError::Structural {
            rule: "unknown profile"
        }
    );
}

fn audit_footer(retention: Option<ObjectLockRetention>, legal_hold: bool) -> Footer {
    let mut footer = minimal_footer();
    footer.audit = Some(AuditTrail {
        chain_end: [7; 32],
        chain_start: [3; 32],
        legal_hold,
        retention,
    });
    footer.profile = Profile::Audit;
    footer.required_feature_flags |= required_features::AUDIT_HASH_CHAIN;
    footer
}

fn graph_footer() -> Footer {
    let mut footer = minimal_footer();
    footer.profile = Profile::Graph;
    footer.required_feature_flags |= required_features::GRAPH_DENSE_IDS
        | required_features::GRAPH_CSR_FORWARD
        | required_features::GRAPH_CSR_REVERSE;
    footer.graph = Some(GraphDirectory {
        chain_cover: None,
        blocks: vec![GraphBlockEntry {
            granule_id: 0,
            len: 300,
            offset: 4_096,
        }],
        depth: None,
        event_ids: None,
        first_sn: 77,
        subject: b"!room:example.org".to_vec(),
        unresolved: vec![UnresolvedEdge {
            child: 77,
            parent: RelationshipRef::to_external(RelationshipKind::Prev, b"$missing:example.org").unwrap(),
        }],
    });
    footer
}

#[test]
fn an_audit_trail_round_trips_through_the_footer() {
    for (retention, legal_hold) in [
        (None, false),
        (None, true),
        (
            Some(ObjectLockRetention {
                mode: RetentionMode::Governance,
                retain_until: crate::events::TimestampValue::from_physical_nanos(-5),
            }),
            false,
        ),
        (
            Some(ObjectLockRetention {
                mode: RetentionMode::Compliance,
                retain_until: crate::events::TimestampValue::from_physical_nanos(i64::MAX),
            }),
            true,
        ),
    ] {
        let footer = audit_footer(retention, legal_hold);
        assert_eq!(decode_footer(&encode_footer(&footer)).unwrap(), footer);
    }
    assert!(RetentionMode::from_u8(2).is_err());
    assert_eq!(decode_footer(&encode_footer(&minimal_footer())).unwrap().audit, None);
}

#[test]
fn an_audit_section_without_its_feature_or_profile_is_refused() {
    let refused = Err(FormatError::Structural {
        rule: "an audit section, the audit_hash_chain feature and the audit profile appear together",
    });
    let mut no_bit = audit_footer(None, false);
    no_bit.required_feature_flags &= !required_features::AUDIT_HASH_CHAIN;
    assert_eq!(decode_footer(&encode_footer(&no_bit)), refused);
    let mut wrong_profile = audit_footer(None, false);
    wrong_profile.profile = Profile::Log;
    assert_eq!(decode_footer(&encode_footer(&wrong_profile)), refused);
    let mut no_section = audit_footer(None, false);
    no_section.audit = None;
    assert_eq!(decode_footer(&encode_footer(&no_section)), refused);
}

#[test]
fn the_graph_edge_directory_round_trips_through_the_footer() {
    let footer = graph_footer();
    assert_eq!(decode_footer(&encode_footer(&footer)).unwrap().graph, footer.graph);
    assert_eq!(decode_footer(&encode_footer(&minimal_footer())).unwrap().graph, None);
}

#[test]
fn a_graph_depth_range_round_trips_and_a_reversed_one_is_refused() {
    let mut footer = graph_footer();
    if let Some(graph) = &mut footer.graph {
        graph.depth = Some(crate::layout::graph::DepthRange { max: 90, min: 12 });
    }
    assert_eq!(decode_footer(&encode_footer(&footer)).unwrap().graph, footer.graph);

    if let Some(graph) = &mut footer.graph {
        graph.depth = Some(crate::layout::graph::DepthRange { max: 11, min: 12 });
    }
    assert_eq!(
        decode_footer(&encode_footer(&footer)),
        Err(FormatError::Structural {
            rule: "graph depth section holds a minimum then a maximum it does not pass",
        })
    );
}

fn indexed_graph_footer() -> Footer {
    let mut footer = graph_footer();
    footer.required_feature_flags |= required_features::GRAPH_EVENT_ID_INDEX;
    if let Some(graph) = &mut footer.graph {
        graph.event_ids = crate::layout::graph::EventIdIndex::build([(&b"$a:example.org"[..], 77)]).unwrap();
    }
    footer
}

#[test]
fn a_graph_event_id_index_round_trips_only_with_its_feature_bit() {
    let footer = indexed_graph_footer();
    assert_eq!(decode_footer(&encode_footer(&footer)).unwrap().graph, footer.graph);

    let mut no_bit = indexed_graph_footer();
    no_bit.required_feature_flags &= !required_features::GRAPH_EVENT_ID_INDEX;
    let mut no_index = indexed_graph_footer();
    if let Some(graph) = &mut no_index.graph {
        graph.event_ids = None;
    }
    let mut outside = indexed_graph_footer();
    if let Some(index) = outside.graph.as_mut().and_then(|graph| graph.event_ids.as_mut()) {
        index.sns[0] = 78;
    }
    let mut repeated = indexed_graph_footer();
    if let Some(index) = repeated.graph.as_mut().and_then(|graph| graph.event_ids.as_mut()) {
        index.hashes.push(index.hashes[0]);
        index.sns.push(77);
    }
    let mut no_filter = indexed_graph_footer();
    if let Some(index) = no_filter.graph.as_mut().and_then(|graph| graph.event_ids.as_mut()) {
        index.filter.clear();
    }
    let mut no_graph = indexed_graph_footer();
    no_graph.graph = None;
    no_graph.required_feature_flags &= !required_features::GRAPH;
    no_graph.required_feature_flags |= required_features::GRAPH_EVENT_ID_INDEX;
    for footer in [no_bit, no_index, outside, repeated, no_filter, no_graph] {
        assert!(decode_footer(&encode_footer(&footer)).is_err());
    }
}

fn chain_cover_footer() -> Footer {
    let mut footer = graph_footer();
    footer.required_feature_flags |= required_features::GRAPH_CHAIN_COVER;
    if let Some(graph) = &mut footer.graph {
        graph.chain_cover = Some(crate::layout::graph::ChainCover::build(
            &crate::layout::graph::ChainCoverInput {
                labels: vec![crate::layout::graph::ChainLabel { chain_id: 4, seq: 9 }],
                links: vec![
                    crate::layout::graph::ChainLink {
                        from_chain: 4,
                        from_seq: 9,
                        to_chain: 2,
                        to_seq: 3,
                    },
                    crate::layout::graph::ChainLink {
                        from_chain: 1,
                        from_seq: 5,
                        to_chain: 0,
                        to_seq: 1,
                    },
                ],
            },
        ));
    }
    footer
}

#[test]
fn a_graph_chain_cover_round_trips_only_with_its_feature_bit() {
    let footer = chain_cover_footer();
    assert_eq!(decode_footer(&encode_footer(&footer)).unwrap().graph, footer.graph);

    let mut no_bit = chain_cover_footer();
    no_bit.required_feature_flags &= !required_features::GRAPH_CHAIN_COVER;
    let mut no_cover = chain_cover_footer();
    if let Some(graph) = &mut no_cover.graph {
        graph.chain_cover = None;
    }
    let mut unsorted_links = chain_cover_footer();
    if let Some(cover) = unsorted_links
        .graph
        .as_mut()
        .and_then(|graph| graph.chain_cover.as_mut())
    {
        cover.links.swap(0, 1);
    }
    let mut inverted_range = chain_cover_footer();
    if let Some(cover) = inverted_range
        .graph
        .as_mut()
        .and_then(|graph| graph.chain_cover.as_mut())
    {
        cover.chains[0].min_seq = 10;
    }
    let mut no_graph = chain_cover_footer();
    no_graph.graph = None;
    no_graph.required_feature_flags &= !required_features::GRAPH;
    no_graph.required_feature_flags |= required_features::GRAPH_CHAIN_COVER;
    for footer in [no_bit, no_cover, unsorted_links, inverted_range, no_graph] {
        assert!(decode_footer(&encode_footer(&footer)).is_err());
    }
}

#[test]
fn graph_edges_and_their_feature_bits_come_together_on_a_graph_profile_file() {
    let mut event_profile = graph_footer();
    event_profile.profile = Profile::Event;
    let mut forward_only = graph_footer();
    forward_only.required_feature_flags &= !required_features::GRAPH_CSR_REVERSE;
    let mut sparse_ids = graph_footer();
    sparse_ids.required_feature_flags &= !required_features::GRAPH_DENSE_IDS;
    let mut bits_without_edges = graph_footer();
    bits_without_edges.graph = None;
    let mut overflowing = graph_footer();
    if let Some(graph) = &mut overflowing.graph {
        graph.first_sn = u64::MAX;
    }
    let mut unresolved_outside = graph_footer();
    if let Some(graph) = &mut unresolved_outside.graph {
        graph.unresolved[0].child = 78;
    }
    let mut unresolved_by_sequence = graph_footer();
    if let Some(graph) = &mut unresolved_by_sequence.graph {
        graph.unresolved[0].parent = RelationshipRef::to_subject_sequence(RelationshipKind::Prev, 3);
    }
    for footer in [
        event_profile,
        forward_only,
        sparse_ids,
        bits_without_edges,
        overflowing,
        unresolved_outside,
        unresolved_by_sequence,
    ] {
        assert!(matches!(
            decode_footer(&encode_footer(&footer)),
            Err(FormatError::Structural { .. })
        ));
    }
}

fn state_roots_footer() -> Footer {
    let mut footer = graph_footer();
    footer.exact_counts.row_count = 3;
    footer.required_feature_flags |= required_features::GRAPH_STATE_ROOTS;
    footer.state_roots = [0, 2]
        .into_iter()
        .map(|row_ordinal| StateRootEntry {
            root: StateRoot {
                address: [row_ordinal as u8 + 1; 32],
                entry_count: 40 + row_ordinal as u32,
            },
            row_ordinal,
        })
        .collect();
    footer
}

#[test]
fn state_roots_round_trip_through_the_footer() {
    let footer = state_roots_footer();
    assert_eq!(
        decode_footer(&encode_footer(&footer)).unwrap().state_roots,
        footer.state_roots
    );
    assert!(
        decode_footer(&encode_footer(&graph_footer()))
            .unwrap()
            .state_roots
            .is_empty()
    );
}

#[test]
fn state_roots_come_only_with_their_bit_on_a_graph_file_with_edges_and_name_its_rows_in_order() {
    let mut bit_without_roots = state_roots_footer();
    bit_without_roots.state_roots.clear();
    let mut roots_without_bit = state_roots_footer();
    roots_without_bit.required_feature_flags &= !required_features::GRAPH_STATE_ROOTS;
    let mut without_edges = state_roots_footer();
    without_edges.graph = None;
    without_edges.required_feature_flags &=
        !(required_features::GRAPH_CSR_FORWARD | required_features::GRAPH_CSR_REVERSE);
    let mut event_profile = state_roots_footer();
    event_profile.profile = Profile::Event;
    let mut out_of_order = state_roots_footer();
    out_of_order.state_roots.reverse();
    let mut past_the_file = state_roots_footer();
    past_the_file.state_roots[1].row_ordinal = 3;
    for footer in [
        bit_without_roots,
        roots_without_bit,
        without_edges,
        event_profile,
        out_of_order,
        past_the_file,
    ] {
        assert!(matches!(
            decode_footer(&encode_footer(&footer)),
            Err(FormatError::Structural { .. })
        ));
    }
}

#[test]
fn a_snapshot_key_index_round_trips_only_with_one_first_key_per_granule() {
    let mut footer = minimal_footer();
    assert_eq!(footer.granules.len(), 1);
    footer.snapshot_keys = Some(SnapshotKeyIndex::build(&["a", "b"], &[0]).unwrap());
    assert_eq!(
        decode_footer(&encode_footer(&footer)).unwrap().snapshot_keys,
        footer.snapshot_keys
    );

    footer.snapshot_keys = Some(SnapshotKeyIndex::build(&["a", "b"], &[0, 1]).unwrap());
    assert_eq!(
        decode_footer(&encode_footer(&footer)).unwrap_err(),
        FormatError::Structural {
            rule: "snapshot key index must hold one first key per granule"
        }
    );
}

/// A time-series footer with one float column and rollups covering its one row.
fn time_series_footer() -> Footer {
    let mut footer = minimal_footer();
    footer.profile = Profile::TimeSeries;
    footer.columns.push(ColumnDescriptor {
        column_id: 1000,
        internal_only: false,
        kind: ColumnKind::F64,
        name: "cpu".to_owned(),
        nullable: true,
    });
    let mut aggregate = crate::layout::time_series::FloatAggregate::default();
    aggregate.add(0.25);
    footer.time_series_rollups = Some(crate::layout::time_series::TimeSeriesRollups {
        bucket_nanos: 60_000_000_000,
        buckets: vec![crate::layout::time_series::RollupBucket {
            aggregates: vec![aggregate],
            bucket: -3,
            first_row_ordinal: 0,
            row_count: 1,
        }],
        column_ids: vec![1000],
    });
    footer
}

#[test]
fn time_series_rollups_round_trip_and_ride_only_a_time_series_footer_over_its_rows_and_float_columns() {
    let footer = time_series_footer();
    let decoded = decode_footer(&encode_footer(&footer)).unwrap();
    assert_eq!(decoded.time_series_rollups, footer.time_series_rollups);
    assert!(
        decode_footer(&encode_footer(&minimal_footer()))
            .unwrap()
            .time_series_rollups
            .is_none()
    );

    let refused = FormatError::Structural {
        rule: "time-series rollups cover a time-series file's rows and float columns",
    };
    let mut event = time_series_footer();
    event.profile = Profile::Event;
    assert_eq!(decode_footer(&encode_footer(&event)).unwrap_err(), refused);
    let mut more_rows = time_series_footer();
    more_rows.exact_counts.row_count = 2;
    assert_eq!(decode_footer(&encode_footer(&more_rows)).unwrap_err(), refused);
    let mut not_a_float = time_series_footer();
    if let Some(rollups) = &mut not_a_float.time_series_rollups {
        rollups.column_ids = vec![0];
    }
    assert_eq!(decode_footer(&encode_footer(&not_a_float)).unwrap_err(), refused);
}
