use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::columns::{FreetextDeclaration, PromotedColumn, PromotionPlan};
use crate::encoding::encode_block;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::file::bytes::Writer;
use crate::indexes::bitmap::RoaringRangeBitmap;
use crate::indexes::rank_select::RankSelect;
use crate::layout::footer::{ColumnKind, ColumnMark, EmbeddingRowOffsets, ExactCounts, Footer, GranuleEntry};
use crate::layout::{LayoutClass, LayoutTargets, optional_features};
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{AnalyticalColumn, BuildLifecycle, HefBuildConfig, HefRow, build_hef_file};

fn minimal_header() -> HefHeader {
    HefHeader {
        created_at_physical: 0,
        feature_flags: 0,
        file_id: 0,
        footer_pointer_hint: 0,
        generation_id: 0,
        layout_class: LayoutClass::Compact,
        max_epoch: 1,
        max_ingested_at_physical: 0,
        max_occurred_at_physical: 0,
        max_sequence: 1,
        min_epoch: 1,
        min_ingested_at_physical: 0,
        min_occurred_at_physical: 0,
        min_sequence: 1,
        projection_count: 1,
        row_count: 1,
        tenant_id: TenantId::new_test_id(1),
        version_major: 1,
        version_minor: 0,
    }
}

fn minimal_footer(mark: ColumnMark) -> Footer {
    Footer {
        clustering: Vec::new(),
        columns: Vec::new(),
        dictionaries: Default::default(),
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
            granule_id: mark.granule_id,
            last_epoch: 1,
            last_sequence: 1,
            max_ingested_at_physical: 0,
            max_occurred_at_physical: 0,
            min_ingested_at_physical: 0,
            min_occurred_at_physical: 0,
            row_count: mark.row_count,
            stripe_id: 0,
        }],
        integrity_gaps: Vec::new(),
        io_alignment_bytes: 0,
        marks: vec![mark],
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
        // The current writer's full feature set: these fixtures hand-craft current-format blocks, so the footer
        // must declare the framing the blocks actually use (compressed presence in particular).
        required_feature_flags: required_features::ALL,
        schema_fingerprint: [0u8; 32],
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

fn file_with_mark(mark: ColumnMark, bytes: Vec<u8>, usable_optional_features: u64) -> HefFile {
    let footer = minimal_footer(mark);
    HefFile {
        aliased_extent_cache: DashMap::default(),
        aliased_extent_cache_bytes: AtomicU64::new(0),
        aliased_extents: DashSet::default(),
        block_reads: AtomicU64::new(0),
        bytes: Arc::new(bytes),
        cache_access_counter: AtomicU64::new(0),
        column_cache: DashMap::default(),
        column_cache_bytes: AtomicU64::new(0),
        decoded_cache_budget: DECODED_CACHE_BUDGET_BYTES,
        dictionary_decodes: AtomicU64::new(0),
        embedding_row_offsets: HashMap::new(),
        entity_hash_filters_by_granule: HashMap::new(),
        footer,
        freetext_row_offsets: HashMap::new(),
        granule_dictionaries: DashMap::default(),
        granule_dictionaries_bytes: AtomicU64::new(0),
        granule_stripe_base: HashMap::new(),
        header: minimal_header(),
        inflated_residuals: DashMap::default(),
        inflated_residuals_bytes: AtomicU64::new(0),
        lazy_stripes: None,
        marks: Marks::Eager([((mark.column_id, mark.projection_id, mark.granule_id), mark)].into()),
        page_marks: BTreeMap::new(),
        page_stats_index: HashMap::new(),
        payload_granule_index: HashMap::new(),
        point_probe_counts: DashMap::default(),
        residual_seek_tables: DashMap::default(),
        shared_alphabets: HashMap::new(),
        shared_string_views: DashMap::new(),
        stripe_relative: false,
        text_token_cache: DashMap::default(),
        text_token_indexes_by_key: HashMap::new(),
        text_token_offsets: HashMap::new(),
        usable_optional_features,
    }
}

/// The rank index must answer "is this row present, and where does its value sit among the present rows" exactly as the
/// old per-row popcount over the presence bitmap did — for every row, over a bitmap with gaps and multi-byte runs. This
/// is the equivalence the O(n²)→O(n log) rewrite relies on (issue #1368).
#[test]
fn rank_matches_naive_presence_popcount() {
    let presence: Vec<u8> = vec![0b1011_0010, 0b0000_0000, 0b1111_0001, 0b0100_0000, 0b1010_1010];
    let rows = (presence.len() * 8) as u64;
    let bitmap = RoaringRangeBitmap::from_rows(presence_set_rows(&presence));
    let rank = RankSelect::from_bitmap(&bitmap);
    for row in 0..rows {
        let set = presence[row as usize / 8] & (1 << (row % 8)) != 0;
        let before = (0..row)
            .filter(|r| presence[*r as usize / 8] & (1 << (r % 8)) != 0)
            .count() as u64;
        assert_eq!(rank.rank(row), before, "dense position before row {row}");
        assert_eq!(rank.rank(row + 1) > rank.rank(row), set, "presence of row {row}");
    }
}

/// The compressed-form rank index the reader now builds (`RankSelect::from_packed_bits`, straight from the presence
/// bytes' run structure) must answer every rank and select query identically to the raw path that materializes one row
/// id per set bit — over the presence bitmap of a real built file's sparse column, not just hand-made patterns.
#[test]
fn cached_column_rank_from_packed_bits_matches_raw_path_on_a_real_file() {
    // "note" is a declared free-text field carried only by even rows, so its column block stores a sparse presence
    // bitmap with real gaps.
    let rows: Vec<_> = (0..40)
        .map(|i| freetext_row(i, (i % 2 == 0).then_some("free text body")))
        .collect();
    let built = build_hef_file(rows, &freetext_config()).unwrap();
    let note_column_id = built.footer.freetext.first().expect("note is declared").column_id;
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    for granule in &file.footer().granules {
        let read = file.read_column(note_column_id, granule.granule_id).unwrap();
        assert!(!read.presence.is_empty(), "a free-text column stores sparse presence");
        let compressed = RankSelect::from_packed_bits(&read.presence);
        let raw = RankSelect::from_bitmap(&RoaringRangeBitmap::from_rows(presence_set_rows(&read.presence)));
        assert_eq!(compressed.total(), raw.total());
        for row in 0..=(read.presence.len() as u64 * 8) {
            assert_eq!(compressed.rank(row), raw.rank(row), "rank({row})");
        }
        for k in 0..=raw.total() {
            assert_eq!(compressed.select(k), raw.select(k), "select({k})");
        }
    }
}

/// `presence_set_rows` lists exactly the set-bit indices, in ascending order, so it is a faithful input to the bitmap
/// builder — LSB-0 within each byte, bytes low to high.
#[test]
fn presence_set_rows_lists_set_bits_in_order() {
    let presence: Vec<u8> = vec![0b0000_0001, 0b1000_0000, 0b0000_0000, 0b0000_0010];
    let got: Vec<u64> = presence_set_rows(&presence).collect();
    assert_eq!(got, vec![0, 15, 25]);
}

/// An all-zero presence bitmap yields no present rows, so every row reports absent and the dense count stays zero.
#[test]
fn empty_presence_has_no_present_rows() {
    let presence = vec![0u8; 4];
    let bitmap = RoaringRangeBitmap::from_rows(presence_set_rows(&presence));
    let rank = RankSelect::from_bitmap(&bitmap);
    assert_eq!(rank.total(), 0);
    for row in 0..(presence.len() * 8) as u64 {
        assert_eq!(rank.rank(row + 1), rank.rank(row), "row {row} must be absent");
    }
}

/// A content tail like `[len word]["HEF1"]` builds an outboard object by appending `[tree][tree_len]["HEFT"]`, and
/// stripping the trailer returns exactly the content — so `HefFooter` reads the footer from the same bytes whether or
/// not the file carries an outboard tree.
#[test]
fn strip_outboard_tree_removes_the_trailer() {
    let content: Vec<u8> = [&1000u64.to_le_bytes()[..], b"HEF1"].concat();
    let tree = vec![0xABu8; 40];
    let mut object = content.clone();
    object.extend_from_slice(&tree);
    object.extend_from_slice(&(tree.len() as u64).to_le_bytes());
    object.extend_from_slice(&TREE_TRAILER_MAGIC);
    assert_eq!(strip_outboard_tree(&object).unwrap(), content.as_slice());
    // A tail that does not end in the trailer magic is already content.
    assert_eq!(strip_outboard_tree(&content).unwrap(), content.as_slice());
}

/// `footer_blob_len` reads the footer-length word after checking the trailing `"HEF1"` magic; a wrong magic is rejected
/// rather than mis-sized.
#[test]
fn footer_blob_len_reads_the_length_word() {
    let content: Vec<u8> = [&4096u64.to_le_bytes()[..], b"HEF1"].concat();
    assert_eq!(footer_blob_len(&content).unwrap(), 4096);
    let bad: Vec<u8> = [&4096u64.to_le_bytes()[..], b"XXXX"].concat();
    assert!(matches!(footer_blob_len(&bad), Err(FormatError::BadMagic { .. })));
}

/// With `footer_len` recorded, the tail range is exact — the last `footer_len + tree_len` bytes — so a cold open is one
/// request; without it the reader falls back to a speculative last-`SPECULATIVE_TAIL_BYTES` fetch.
#[test]
fn tail_range_sizes_exact_and_speculative_fetches() {
    let exact = tail_range(10_000, Some(300), Some(64));
    assert_eq!((exact.exact, exact.len, exact.start), (true, 364, 9_636));

    let no_tree = tail_range(10_000, Some(300), None);
    assert_eq!((no_tree.exact, no_tree.len, no_tree.start), (true, 300, 9_700));

    let big = tail_range(10 * SPECULATIVE_TAIL_BYTES, None, None);
    assert_eq!(big.exact, false);
    assert_eq!(big.len, SPECULATIVE_TAIL_BYTES);

    // A file smaller than the speculative window is fetched whole.
    let small = tail_range(500, None, None);
    assert_eq!((small.len, small.start), (500, 0));
}

#[test]
fn bulk_read_family_returns_the_first_unreadable_granule_error() {
    let mark = ColumnMark {
        codec_pipeline_id: PipelineId(0),
        column_id: 7,
        compressed_offset: 64,
        compressed_size: 8,
        first_value_offset: None,
        granule_id: 3,
        page_count: 1,
        projection_id: 0,
        row_count: 1,
        uncompressed_offset: 0,
        uncompressed_size: 8,
    };
    let file = file_with_mark(mark, Vec::new(), 0);

    assert!(matches!(
        file.bulk_read_family(7),
        Err(FormatError::Truncated { what: "column block" })
    ));
}

#[test]
fn read_page_on_multi_page_mark_without_directory_fails_instead_of_recursing() {
    let mark = ColumnMark {
        codec_pipeline_id: PipelineId(0),
        column_id: 7,
        compressed_offset: 0,
        compressed_size: 0,
        first_value_offset: None,
        granule_id: 3,
        page_count: 2,
        projection_id: 0,
        row_count: 2,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let file = file_with_mark(mark, Vec::new(), optional_features::PER_PAGE_MARKS);

    assert!(matches!(
        file.read_page(7, 3, 1),
        Err(FormatError::RefOutOfRange {
            what: "no page directory entry for multi-page mark"
        })
    ));
    assert!(matches!(
        file.read_column(7, 3),
        Err(FormatError::RefOutOfRange {
            what: "no page directory entry for multi-page mark"
        })
    ));
}

/// When a speculative fetch underflows the footer, `open_speculative` reports the precise object-tail length to
/// re-fetch — the footer region from the length word plus any outboard tree trailer that followed it — so the reader
/// retries exactly once at the exact length instead of growing the fetch blindly.
#[test]
fn speculative_open_reports_exact_retry_length() {
    // A 12-byte tail carries only the length word and magic: the footer blob it names (1000 bytes) is not present.
    let short: Vec<u8> = [&1000u64.to_le_bytes()[..], b"HEF1"].concat();
    match HefFooter::open_speculative(&short).unwrap() {
        SpeculativeTail::NeedsExactTail { tail_len } => assert_eq!(tail_len, 1012),
        other => panic!("expected an exact-retry request, got {other:?}"),
    }

    // The same underflow behind an outboard tree trailer adds the trailer's bytes to the retry length.
    let tree = vec![0u8; 20];
    let mut object = short.clone();
    object.extend_from_slice(&tree);
    object.extend_from_slice(&(tree.len() as u64).to_le_bytes());
    object.extend_from_slice(&TREE_TRAILER_MAGIC);
    match HefFooter::open_speculative(&object).unwrap() {
        SpeculativeTail::NeedsExactTail { tail_len } => assert_eq!(tail_len, 1012 + 20 + TREE_TRAILER_LEN as u64),
        other => panic!("expected an exact-retry request, got {other:?}"),
    }
}

/// When the outboard tree alone is bigger than the speculative window, the tree body — not just its length word — is
/// missing from the fetched bytes, so the footer beneath it is unreachable yet. `open_speculative` must still ask for
/// an exact retry (sized to the whole tree plus one more speculative footer window) instead of failing closed on the
/// tree's own truncation (issue #9857).
#[test]
fn speculative_open_asks_for_a_retry_when_the_tree_alone_exceeds_the_window() {
    let footer_region: Vec<u8> = [&1000u64.to_le_bytes()[..], b"HEF1"].concat();
    let tree = vec![0u8; 5_000];
    let mut object = footer_region;
    object.extend_from_slice(&tree);
    object.extend_from_slice(&(tree.len() as u64).to_le_bytes());
    object.extend_from_slice(&TREE_TRAILER_MAGIC);

    // Only the last 200 bytes are "fetched" — far short of the 5,000-byte tree, so the trailer's declared tree_len is
    // readable (it sits in the fixed 12-byte trailer) but the tree body is not.
    let short_tail = &object[object.len() - 200..];
    match HefFooter::open_speculative(short_tail).unwrap() {
        SpeculativeTail::NeedsExactTail { tail_len } => {
            assert_eq!(
                tail_len,
                tree.len() as u64 + TREE_TRAILER_LEN as u64 + SPECULATIVE_TAIL_BYTES
            );
        }
        other => panic!("expected an exact-retry request, got {other:?}"),
    }

    // Retrying with the whole tree present (a real object never exceeds its own length) reaches the underlying
    // footer underflow instead of the tree-truncation error.
    match HefFooter::open_speculative(&object).unwrap() {
        SpeculativeTail::NeedsExactTail { tail_len } => {
            assert_eq!(tail_len, 1012 + tree.len() as u64 + TREE_TRAILER_LEN as u64)
        }
        other => panic!("expected an exact-retry request, got {other:?}"),
    }
}

/// A forged footer-length word near `u64::MAX` must refuse as a format error on every open path: evaluating
/// `12 + blob_len` before the bounds check would overflow `usize` and abort the reader instead (issue #3991).
#[test]
fn forged_footer_length_near_u64_max_refuses() {
    let tail: Vec<u8> = [&u64::MAX.to_le_bytes()[..], b"HEF1"].concat();
    assert!(matches!(
        HefFooter::open(&tail),
        Err(FormatError::Truncated { what: "footer" })
    ));
    // The speculative path saturates the exact-retry length instead of overflowing it.
    match HefFooter::open_speculative(&tail).unwrap() {
        SpeculativeTail::NeedsExactTail { tail_len } => assert_eq!(tail_len, u64::MAX),
        other => panic!("expected an exact-retry request, got {other:?}"),
    }
    // The sealed-footer path runs the same arithmetic on the declared ciphertext length.
    assert!(matches!(
        decrypt_footer_tail(&tail, &[0u8; 32], 0),
        Err(FormatError::Truncated {
            what: "encrypted footer"
        })
    ));
}

/// A forged mark whose stripe-relative `compressed_offset` is `u64::MAX` must refuse as a range error: adding
/// the stripe base unchecked would overflow `u64` and abort the reader instead (issue #3992).
#[test]
fn forged_stripe_relative_offset_near_u64_max_refuses() {
    let mark = ColumnMark {
        codec_pipeline_id: PipelineId(0),
        column_id: 7,
        compressed_offset: u64::MAX,
        compressed_size: 8,
        first_value_offset: None,
        granule_id: 3,
        page_count: 1,
        projection_id: 0,
        row_count: 1,
        uncompressed_offset: 0,
        uncompressed_size: 8,
    };
    let mut file = file_with_mark(mark, vec![0u8; 64], 0);
    file.stripe_relative = true;
    file.granule_stripe_base.insert(3, 1);
    assert!(matches!(
        file.read_column_raw(7, 3),
        Err(FormatError::RefOutOfRange {
            what: "offset beyond file range"
        })
    ));
}

#[test]
fn validate_block_counts_requires_exact_dense_row_count() {
    // A dense block stores a value for every row and no presence bitmap, so its decoded value count must match the
    // declared rows exactly. A block that declares 1000 rows but decodes 900 must be rejected, not silently accepted.
    assert!(validate_block_counts(&[], 1000, 1000).is_ok());
    assert!(
        validate_block_counts(&[], 900, 1000).is_err(),
        "a dense block that decodes fewer values than it declares must be rejected"
    );
    assert!(
        validate_block_counts(&[], 1001, 1000).is_err(),
        "a dense block that decodes more values than it declares must be rejected"
    );
}

#[test]
fn validate_block_counts_matches_sparse_values_to_set_bits() {
    // A sparse block stores only present rows' values behind a presence bitmap: the decoded value count must equal the
    // number of set bits, and the bitmap must cover the declared rows with no present bit beyond them.
    // 0b0000_1011 -> rows 0, 1, 3 present (3 set bits) covering 4 declared rows.
    let presence = [0b0000_1011u8];
    assert!(validate_block_counts(&presence, 3, 4).is_ok());
    assert!(
        validate_block_counts(&presence, 2, 4).is_err(),
        "fewer decoded values than set bits must be rejected"
    );
    assert!(
        validate_block_counts(&presence, 4, 4).is_err(),
        "more decoded values than set bits must be rejected"
    );
}

#[test]
fn validate_block_counts_rejects_a_short_or_overlong_sparse_bitmap() {
    // The bitmap must be wide enough to cover every declared row, and no present bit may land at or beyond them.
    // One byte covers 8 bits, so a 9-row block needs a second byte.
    assert!(
        validate_block_counts(&[0xFFu8], 8, 9).is_err(),
        "a one-byte bitmap cannot cover nine declared rows"
    );
    // A present bit at row 4 with only 4 rows declared places a value outside the block.
    assert!(
        validate_block_counts(&[0b0001_0001u8], 2, 4).is_err(),
        "a present bit at or beyond the declared row count must be rejected"
    );
}

fn freetext_row(i: u64, note: Option<&str>) -> HefRow {
    let mut payload = std::collections::BTreeMap::new();
    if let Some(note) = note {
        payload.insert("note".to_owned(), VariantValue::String(note.to_owned()));
    }
    payload.insert("amount".to_owned(), VariantValue::Int(i as i64));
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            envelope: EventEnvelope {
                event_id: EventId::new_test_id(0xF00D_0000 + u128::from(i)),
                tenant_id: TenantId::new_test_id(3),
                stream_id: StreamId(1),
                stream_sequence: i,
                occurred_at: TimestampValue::from_physical_nanos(1_000 + i as i64),
                ingested_at: TimestampValue::from_physical_nanos(2_000 + i as i64),
                source: "crm".to_owned(),
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

/// A free-text point lookup through the per-row byte-offset index must return the same value the whole-granule decode
/// path yields, for both a row that carries the field and one that does not; a reader that does not declare
/// `typed_column_row_offsets` must fall back to the whole-granule decode and get byte-identical values. The build opts
/// into the index, which a default build does not emit. Implements `hef-column-design` — "Per-row byte-offset index
/// makes wide typed columns point-accessible" and "Free-text shredded by schema declaration".
#[test]
fn freetext_point_lookup_matches_whole_granule_decode_and_falls_back_identically() {
    let rows: Vec<_> = (0..20)
        .map(|i| freetext_row(i, (!i.is_multiple_of(5)).then(|| "free text body")))
        .collect();
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration {
            fields: vec!["note".to_owned()],
        },
        freetext_row_offset_index: true,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 6,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    let built = build_hef_file(rows.clone(), &config).unwrap();
    assert_ne!(
        built.footer.optional_feature_flags & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0
    );
    assert!(!built.footer.freetext_row_offsets.is_empty());

    let with_index = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let mut without_index = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    without_index.usable_optional_features &= !optional_features::TYPED_COLUMN_ROW_OFFSETS;
    assert_eq!(
        without_index.usable_optional_features & optional_features::TYPED_COLUMN_ROW_OFFSETS,
        0
    );

    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadInput::Variant(VariantValue::Object(expected)) = &input_row.event.payload else {
            panic!("variant object input");
        };
        let expected_note = expected.get("note").cloned();

        let PayloadRead::Value(VariantValue::Object(via_index)) = with_index.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        let PayloadRead::Value(VariantValue::Object(via_fallback)) = without_index.payload(ordinal as u64).unwrap()
        else {
            panic!("payload present");
        };
        assert_eq!(via_index.get("note").cloned(), expected_note, "row {ordinal} via index");
        assert_eq!(
            via_fallback.get("note").cloned(),
            expected_note,
            "row {ordinal} via whole-granule fallback"
        );
    }
}

/// A declared free-text field is moved out of the residual into its own column at build time, so `payload_path` must
/// answer it from that column exactly as `payload` does. The two public accessors must never disagree for a row that
/// carries the field or for one that does not.
#[test]
fn payload_path_and_payload_agree_on_a_freetext_declared_field() {
    let rows: Vec<_> = (0..12)
        .map(|i| freetext_row(i, (!i.is_multiple_of(4)).then(|| "free text body")))
        .collect();
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
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
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 6,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    let built = build_hef_file(rows.clone(), &config).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    for (ordinal, input_row) in rows.iter().enumerate() {
        let PayloadInput::Variant(VariantValue::Object(expected)) = &input_row.event.payload else {
            panic!("variant object input");
        };
        let expected_note = expected.get("note").cloned();

        let via_path = file.payload_path(ordinal as u64, "note").unwrap();
        assert_eq!(via_path, expected_note, "payload_path row {ordinal}");

        let PayloadRead::Value(VariantValue::Object(via_payload)) = file.payload(ordinal as u64).unwrap() else {
            panic!("payload present");
        };
        assert_eq!(
            via_payload.get("note").cloned(),
            via_path,
            "payload vs payload_path row {ordinal}"
        );
    }
}

/// A build config declaring `note` as free text, with granules small enough that several are produced.
fn freetext_config() -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
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
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 6,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    }
}

/// A declared free-text field that is present but empty is moved out of the residual like any other, so if the column
/// reads it back as absent the value is lost for good. A present empty string and an absent field both have length
/// zero, and the per-row index used to tell them apart by length alone (issue #7483). The build opts into that index,
/// which a default build does not emit, so both it and the fallback decode are checked on the same rows.
#[test]
fn a_present_but_empty_freetext_field_is_not_reconstructed_as_missing() {
    // Row 0 carries an empty note, row 1 a real one, row 2 none at all — the first granule alone covers all three
    // cases, and the whole file has several granules.
    let notes = [Some(""), Some("free text body"), None];
    let rows: Vec<_> = (0..12)
        .map(|i| freetext_row(i, notes[i as usize % notes.len()]))
        .collect();
    let mut config = freetext_config();
    config.freetext_row_offset_index = true;
    let built = build_hef_file(rows.clone(), &config).unwrap();
    assert!(!built.footer.freetext_row_offsets.is_empty());

    let with_index = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let mut without_index = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    without_index.usable_optional_features &= !optional_features::TYPED_COLUMN_ROW_OFFSETS;

    for (ordinal, note) in (0..rows.len()).zip(notes.iter().cycle()) {
        let expected = note.map(|text| VariantValue::String(text.to_owned()));
        assert_eq!(
            with_index.payload_path(ordinal as u64, "note").unwrap(),
            expected,
            "row {ordinal} through the per-row index"
        );
        assert_eq!(
            without_index.payload_path(ordinal as u64, "note").unwrap(),
            expected,
            "row {ordinal} through the whole-granule decode"
        );
        let PayloadRead::Value(VariantValue::Object(reconstructed)) = with_index.payload(ordinal as u64).unwrap()
        else {
            panic!("payload present");
        };
        assert_eq!(
            reconstructed.get("note").cloned(),
            expected,
            "row {ordinal} payload reconstruction"
        );
    }
}

/// A row whose payload lives outside the file stores the reference itself in the residual slot. `payload` checks the
/// external-reference flag before decoding; `payload_path` did not, so it handed reference bytes to the Variant
/// decoder and returned a format error or fabricated values (issue #7484).
#[test]
fn payload_path_reports_no_inline_path_for_an_external_reference_row() {
    let mut rows: Vec<_> = (0..12).map(|i| freetext_row(i, Some("free text body"))).collect();
    rows[2].event.payload = PayloadInput::ExternalRef("hej://tenant/3/blob/2".to_owned());
    let built = build_hef_file(rows.clone(), &freetext_config()).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    assert_eq!(
        file.payload(2).unwrap(),
        PayloadRead::External("hej://tenant/3/blob/2".to_owned())
    );
    assert_eq!(
        file.payload_path(2, "amount").unwrap(),
        None,
        "an external-reference row carries no inline path"
    );
    assert_eq!(file.payload_path(2, "note").unwrap(), None);
    // Its neighbours are unaffected.
    assert_eq!(file.payload_path(1, "amount").unwrap(), Some(VariantValue::Int(1)));
}

/// Rows across several granules, with one row storing its payload outside the file, one storing no payload at all,
/// and a residual-only field on every third row — so a batch spans all three places a path's value can live.
fn mixed_payload_rows(count: u64) -> Vec<HefRow> {
    let mut rows: Vec<_> = (0..count)
        .map(|i| freetext_row(i, (!i.is_multiple_of(4)).then_some("free text body")))
        .collect();
    for (ordinal, row) in rows.iter_mut().enumerate() {
        if ordinal.is_multiple_of(3)
            && let PayloadInput::Variant(VariantValue::Object(fields)) = &mut row.event.payload
        {
            fields.insert("extra".to_owned(), VariantValue::String(format!("extra-{ordinal}")));
        }
    }
    rows[7].event.payload = PayloadInput::ExternalRef("hej://tenant/3/blob/7".to_owned());
    rows[13].event.payload = PayloadInput::None;
    rows
}

/// The batched single-path read is the per-row read with the granule work amortized, so it must never disagree with
/// it: one value per reference in the caller's order, for a shredded path, a declared free-text path, a residual-only
/// path, and a path the file does not carry at all.
#[test]
fn read_payload_paths_agrees_with_the_per_row_path_read() {
    let built = build_hef_file(mixed_payload_rows(24), &freetext_config()).unwrap();
    assert!(
        built.footer.shredded.iter().any(|entry| entry.path == "amount"),
        "`amount` must auto-shred for this to cover the shredded path"
    );
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    // Scrambled across granules, with duplicates, so the caller's order and the row order differ; rows 7 and 13 are
    // the external-reference and no-payload rows.
    let refs: Vec<PayloadRef> = [21, 3, 13, 7, 3, 16, 0, 9, 23, 6]
        .into_iter()
        .map(|row_ordinal| PayloadRef { row_ordinal })
        .collect();

    for path in ["amount", "note", "extra", "absent-everywhere"] {
        let batched = file.read_payload_paths(&refs, path).unwrap();
        assert_eq!(batched.len(), refs.len(), "one value per reference for `{path}`");
        for (reference, value) in refs.iter().zip(&batched) {
            assert_eq!(
                *value,
                file.payload_path(reference.row_ordinal, path).unwrap(),
                "`{path}` of row {} in the caller's position",
                reference.row_ordinal
            );
        }
    }
    // The batch really did carry values, absences, and the two special rows — not four columns of `None`.
    let amounts = file.read_payload_paths(&refs, "amount").unwrap();
    assert_eq!(amounts[0], Some(VariantValue::Int(21)));
    assert_eq!(amounts[2], None, "the row storing no payload has no value for the path");
    assert_eq!(amounts[3], None, "an external-reference row carries no inline path");
    assert_eq!(
        file.read_payload_paths(&refs, "extra").unwrap()[6],
        Some(VariantValue::String("extra-0".to_owned())),
        "a residual-only path comes back from the residual"
    );
}

/// The point of the batch: each granule the references reach into is visited once. The per-row read fetches the
/// path's column block again for every row it answers, which is what the batch is there to stop paying.
#[test]
fn read_payload_paths_reads_each_touched_granules_block_once() {
    let rows: Vec<_> = (0..24).map(|i| freetext_row(i, Some("free text body"))).collect();
    let built = build_hef_file(rows, &freetext_config()).unwrap();
    assert!(built.footer.shredded.iter().any(|entry| entry.path == "amount"));
    // Under index granularity 6: rows 0..6, 6..12 and 12..18 — three of the file's four granules, four rows each.
    let refs: Vec<PayloadRef> = [14, 1, 6, 3, 17, 8, 4, 12, 10, 0, 15, 7]
        .into_iter()
        .map(|row_ordinal| PayloadRef { row_ordinal })
        .collect();

    let batched = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let before = batched.column_block_reads();
    batched.read_payload_paths(&refs, "amount").unwrap();
    assert_eq!(
        batched.column_block_reads() - before,
        3,
        "one block read per granule the references touch, not one per reference"
    );

    let per_row = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    let before = per_row.column_block_reads();
    for reference in &refs {
        per_row.payload_path(reference.row_ordinal, "amount").unwrap();
    }
    assert!(
        per_row.column_block_reads() - before > 3,
        "the per-row read re-reads the block per row — this is the cost the batch removes"
    );
}

/// Names and pins the read-mode invariant the module docs state: a cold scan must never pay the search cache's
/// initialization cost. `bulk_read_family` is HEF's cold-scan surface — it walks every granule of a family through
/// `read_column` directly — while `payload` reaches a shredded column through `cached_column`, the search-cache init
/// path a repeated point lookup pays for once.
#[test]
fn cold_scan_never_initializes_the_search_cache_a_point_lookup_pays_for() {
    let rows: Vec<_> = (0..20).map(|i| freetext_row(i, Some("free text body"))).collect();
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
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
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 6,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    let built = build_hef_file(rows.clone(), &config).unwrap();
    // "amount" is present in every row with a consistent kind, so statistics-driven shredding picks it up
    // automatically; "note" is excluded because it is declared free text instead.
    let shredded_column_id = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount auto-shreds")
        .column_id;

    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    file.bulk_read_family(shredded_column_id).unwrap();
    assert!(
        file.column_cache.is_empty(),
        "a cold scan must not initialize the search cache"
    );

    file.payload(0).unwrap();
    assert!(
        !file.column_cache.is_empty(),
        "a point lookup must initialize the search cache"
    );
}

fn small_stripe_config(stripe_target_bytes: usize) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
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
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 4,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    }
}

/// Compares a decoded footer against the writer's in-memory one, ignoring how the marks are represented: the writer
/// keeps the eager `marks`/`page_directory` vectors it encoded from, while a decoded `columnar_marks` footer carries
/// the two-level directory and raw pages and materializes marks lazily. Everything else must match exactly.
fn assert_footers_match_modulo_marks_form(decoded: &Footer, built: &Footer) {
    let mut decoded = decoded.clone();
    let mut built = built.clone();
    for footer in [&mut decoded, &mut built] {
        footer.marks = Vec::new();
        footer.marks_directory = Vec::new();
        footer.marks_pages = Vec::new();
        footer.page_directory = Vec::new();
    }
    assert_eq!(decoded, built);
}

/// A pruned stripe's marks page must never be fetched or decoded: a `columnar_marks` file's `HefFile::mark` decodes a
/// stripe's directory-entry pages lazily, the first time one of its granules is looked up, and every other stripe
/// stays undecoded. Surviving-stripe reads yield identical data to the row-oriented form. Implements
/// `hef-file-layout` — "Granule directory and authoritative marks": "Pruned stripe costs zero marks bytes" and "Marks
/// decode from columnar arrays".
#[test]
fn columnar_marks_pruned_stripe_never_decodes_its_marks_page() {
    let rows: Vec<_> = (0..40).map(|i| freetext_row(i, Some("free text body"))).collect();
    // A stripe_target_bytes of 1 rolls a new stripe at every granule, so distinct granules land in distinct stripes.
    let built = build_hef_file(rows.clone(), &small_stripe_config(1)).unwrap();
    assert!(
        built.footer.stripes.len() > 1,
        "test needs multiple stripes to prove pruning skips one"
    );
    let shredded_column_id = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount auto-shreds")
        .column_id;

    // The writer emits the columnar form natively, so the built file is the columnar file under test. The
    // row-oriented comparison reader is the same file with its marks swapped for an eager map built from the
    // writer's in-memory marks — the pre-columnar form.
    let columnar = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let mut row_oriented = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    row_oriented.marks = Marks::Eager(
        built
            .footer
            .marks
            .iter()
            .map(|mark| ((mark.column_id, mark.projection_id, mark.granule_id), *mark))
            .collect(),
    );
    row_oriented.page_marks = built
        .footer
        .page_directory
        .iter()
        .map(|entry| {
            (
                (entry.column_id, entry.projection_id, entry.granule_id, entry.page_index),
                *entry,
            )
        })
        .collect();

    let surviving_granule = columnar.footer.granules[0];
    let pruned_stripe_id = columnar
        .footer
        .granules
        .iter()
        .map(|granule| granule.stripe_id)
        .find(|stripe_id| *stripe_id != surviving_granule.stripe_id)
        .expect("test built more than one stripe");
    let pruned_granule = columnar
        .footer
        .granules
        .iter()
        .find(|granule| granule.stripe_id == pruned_stripe_id)
        .expect("a granule sits in the pruned stripe")
        .granule_id;

    let got = columnar
        .read_column(shredded_column_id, surviving_granule.granule_id)
        .expect("surviving stripe reads");
    let expected = row_oriented
        .read_column(shredded_column_id, surviving_granule.granule_id)
        .expect("row-oriented reads");
    assert_eq!(got.data, expected.data, "columnar and row-oriented marks must agree");
    assert_eq!(got.presence, expected.presence);

    let got_page = columnar
        .read_page(shredded_column_id, surviving_granule.granule_id, 0)
        .expect("surviving stripe's page reads");
    let expected_page = row_oriented
        .read_page(shredded_column_id, surviving_granule.granule_id, 0)
        .expect("row-oriented page reads");
    assert_eq!(
        got_page.data, expected_page.data,
        "columnar and row-oriented per-page directories must agree"
    );
    assert_eq!(got_page.presence, expected_page.presence);

    let Marks::Columnar(state) = &columnar.marks else {
        panic!("expected columnar marks");
    };
    let guard = state.read().unwrap();
    assert_eq!(
        guard.shared_extents_found,
        HashSet::from([surviving_granule.stripe_id]),
        "only the surviving stripe may be touched at all"
    );
    assert!(
        !guard.shared_extents_found.contains(&pruned_stripe_id),
        "a pruned stripe's marks pages must never be fetched or decoded"
    );
    assert_eq!(
        guard.decoded_pages,
        HashSet::from([(shredded_column_id, 0, surviving_granule.stripe_id)]),
        "reads of one column decode that column's marks page, not every column's in the stripe"
    );
    assert!(
        !guard
            .page_marks
            .keys()
            .any(|(_, _, granule_id, _)| *granule_id == pruned_granule),
        "a pruned stripe's per-page directory must never be fetched or decoded"
    );
}

/// A required feature this reader does not understand fails the whole open, closed — the same contract
/// `columnar_marks` itself relies on (see `compat::check_features`). Implements `hef-file-layout` — "Granule
/// directory and authoritative marks": "Old reader refuses on columnar marks".
#[test]
fn open_refuses_on_a_required_feature_beyond_what_this_reader_knows() {
    let rows: Vec<_> = (0..4).map(|i| freetext_row(i, Some("free text body"))).collect();
    let built = build_hef_file(rows.clone(), &small_stripe_config(4096)).unwrap();

    let mut footer = built.footer.clone();
    let unknown_bit = 1u64 << 40;
    footer.required_feature_flags |= unknown_bit;
    let new_footer_blob = crate::layout::footer::encode_footer(&footer);

    assert_eq!(&built.bytes[built.bytes.len() - 4..], b"HEF1");
    let old_footer_len = u64::from_le_bytes(
        built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let stripe_region_end = built.bytes.len() - (old_footer_len + 12);
    let mut tampered = built.bytes[..stripe_region_end].to_vec();
    tampered.extend_from_slice(&new_footer_blob);
    tampered.extend_from_slice(&(new_footer_blob.len() as u64).to_le_bytes());
    tampered.extend_from_slice(b"HEF1");

    let err = HefFile::open(tampered, None).expect_err("an unknown required feature must refuse");
    assert_eq!(err, FormatError::UnknownRequiredFeature { bits: unknown_bit });
}

/// `GranuleEntry.first_row_ordinal` comes straight off the wire and is never bounded, so the ascending-directory
/// check `open` runs over `footer.granules` must reject a row range that overflows `u64` as a format error instead
/// of panicking on the addition — the same contract every neighbouring offset computation in this file already
/// keeps via `checked_add`.
#[test]
fn open_rejects_a_granule_directory_entry_whose_row_range_overflows_instead_of_panicking() {
    let rows: Vec<_> = (0..12).map(|i| freetext_row(i, Some("free text body"))).collect();
    let built = build_hef_file(rows, &small_stripe_config(1)).unwrap();
    assert!(
        built.footer.granules.len() >= 2,
        "test needs at least two granules to exercise the pairwise ascending check"
    );

    let mut footer = built.footer.clone();
    footer.granules[0].first_row_ordinal = u64::MAX;
    let new_footer_blob = crate::layout::footer::encode_footer(&footer);

    let old_footer_len = u64::from_le_bytes(
        built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let stripe_region_end = built.bytes.len() - (old_footer_len + 12);
    let mut tampered = built.bytes[..stripe_region_end].to_vec();
    tampered.extend_from_slice(&new_footer_blob);
    tampered.extend_from_slice(&(new_footer_blob.len() as u64).to_le_bytes());
    tampered.extend_from_slice(b"HEF1");

    let err = HefFile::open(tampered, None).expect_err("a granule row range that overflows u64 must refuse, not panic");
    assert_eq!(
        err,
        FormatError::Structural {
            rule: "granule directory must be strictly ascending by first_row_ordinal with no overlap",
        }
    );
}

/// Whether an extent is shared by more than one mark (mark aliasing) is a fact about the whole stripe, so a reader
/// that decodes only the columns it reads must still find it from every column of the stripe. Two byte-identical
/// blocks under `columnar_marks` must be fetched and decoded once between them, exactly as under the row-oriented
/// form — while the marks of the columns nobody read stay undecoded.
#[test]
fn aliased_extents_are_found_without_decoding_every_columns_marks() {
    let rows: Vec<_> = (0..8).map(|i| freetext_row(i, Some("free text body"))).collect();
    let mut config = freetext_config();
    let twin = |column_id: u32, name: &str| AnalyticalColumn {
        column_id,
        data: ColumnData::I64((0..8).map(|i| 5_000 + i).collect()),
        internal_only: false,
        kind: ColumnKind::I64,
        name: name.to_owned(),
        substring_searchable: false,
    };
    config.analytical_columns = vec![
        twin(column_ids::CONTEXT_BASE, "left"),
        twin(column_ids::CONTEXT_BASE + 1, "right"),
    ];
    let built = build_hef_file(rows, &config).unwrap();
    assert!(built.aliased_block_bytes > 0, "identical blocks must be aliased");

    // Re-serialize the same stripe bytes under a footer declaring `columnar_marks`, so the marks decode lazily.
    let mut columnar_footer = built.footer.clone();
    columnar_footer.required_feature_flags |= required_features::COLUMNAR_MARKS;
    let blob = crate::layout::footer::encode_footer(&columnar_footer);
    let old_footer_len = u64::from_le_bytes(
        built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let mut columnar_bytes = built.bytes[..built.bytes.len() - (old_footer_len + 12)].to_vec();
    columnar_bytes.extend_from_slice(&blob);
    columnar_bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
    columnar_bytes.extend_from_slice(b"HEF1");

    let file = HefFile::open(columnar_bytes, None).unwrap();
    let expected = ColumnData::I64((0..6).map(|i| 5_000 + i).collect());
    for column_id in [column_ids::CONTEXT_BASE, column_ids::CONTEXT_BASE + 1] {
        assert_eq!(file.read_column(column_id, 0).unwrap().data, expected);
    }
    assert_eq!(
        file.column_block_reads(),
        1,
        "an aliased extent must be fetched and decoded once, found from the stripe's other columns"
    );

    let Marks::Columnar(state) = &file.marks else {
        panic!("expected columnar marks");
    };
    let decoded = state.read().unwrap().decoded_pages.clone();
    assert_eq!(
        decoded.len(),
        2,
        "only the two columns read have their marks decoded, not the whole stripe's"
    );
}

/// A cache hit for a shared aliased extent must be checked against the row count the asking mark declares, not
/// served verbatim from whichever aliasing column decoded it first. Two marks referencing the same bytes but
/// declaring different row counts — only reachable via a corrupt or hostile file, since the honest writer never
/// aliases blocks with mismatched declared row counts — must not let the first mark's cached decode silently stand
/// in for the second.
#[test]
fn aliased_extent_cache_hit_is_revalidated_against_the_asking_mark() {
    let data = ColumnData::I64((0..8).collect());
    let encoded = encode_block(&data, false);
    let mut body = Writer::with_capacity(4 + encoded.bytes.len());
    crate::layout::encode_presence(&[], 8, &mut body);
    body.put_slice(&encoded.bytes);
    let block = body.into_bytes();
    let block_len = block.len() as u64;

    let mark_a = ColumnMark {
        codec_pipeline_id: encoded.pipeline,
        column_id: 5000,
        compressed_offset: 0,
        compressed_size: block_len,
        first_value_offset: None,
        granule_id: 0,
        page_count: 1,
        projection_id: 0,
        row_count: 8,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let mark_b = ColumnMark {
        column_id: 5001,
        row_count: 4096,
        ..mark_a
    };

    let mut file = file_with_mark(mark_a, block, 0);
    file.marks = Marks::Eager([((mark_a.column_id, 0, 0), mark_a), ((mark_b.column_id, 0, 0), mark_b)].into());
    file.aliased_extents = [(0u64, block_len)].into_iter().collect();

    let first = file.read_column(mark_a.column_id, 0).unwrap();
    assert_eq!(first.data, data);

    let second = file.read_column(mark_b.column_id, 0);
    assert!(
        second.is_err(),
        "a cache hit for column {} must be re-validated against its own row count instead of reusing column {}'s cached decode",
        mark_b.column_id,
        mark_a.column_id
    );
}

/// Task 7.2 (issue #2052): a genuinely columnar-only file is served by a reader that understands `columnar_marks` and
/// refuses for one that does not. Re-serializing a built file with `columnar_marks` declared folds the marks and
/// per-page directory into the two-level per-stripe columnar block (`encode_footer`), so the row-oriented sections are
/// absent. This reader opens that file, reads a surviving stripe byte-identically to the row-oriented build, and never
/// decodes a pruned stripe's marks page; the same file carrying one extra required feature this reader does not know
/// fails the open, closed, through the `compat::check_features` gate that decides whether the file is served at all.
/// Implements `hef-file-layout` — "Granule directory and authoritative marks": "Old reader refuses on columnar
/// marks" and "Pruned stripe costs zero marks bytes".
#[test]
fn columnar_only_file_is_served_when_understood_and_refuses_when_not() {
    // stripe_target_bytes of 1 rolls a new stripe at every granule, so distinct granules land in distinct stripes and
    // pruning can skip a whole stripe's marks page.
    let rows: Vec<_> = (0..12).map(|i| freetext_row(i, Some("free text body"))).collect();
    let built = build_hef_file(rows.clone(), &small_stripe_config(1)).unwrap();
    assert!(
        built.footer.stripes.len() > 1,
        "test needs multiple stripes to prove pruning skips one"
    );
    let shredded_column_id = built
        .footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount auto-shreds")
        .column_id;

    // Replace the footer region with `footer` re-serialized, keeping the header and stripe bytes intact.
    let reframe = |footer: &Footer| -> Vec<u8> {
        let blob = crate::layout::footer::encode_footer(footer);
        let old_footer_len = u64::from_le_bytes(
            built.bytes[built.bytes.len() - 12..built.bytes.len() - 4]
                .try_into()
                .unwrap(),
        ) as usize;
        let stripe_region_end = built.bytes.len() - (old_footer_len + 12);
        let mut out = built.bytes[..stripe_region_end].to_vec();
        out.extend_from_slice(&blob);
        out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        out.extend_from_slice(b"HEF1");
        out
    };

    let mut columnar_footer = built.footer.clone();
    columnar_footer.required_feature_flags |= required_features::COLUMNAR_MARKS;
    let columnar_bytes = reframe(&columnar_footer);

    // Served when understood: this reader knows `columnar_marks` and opens the columnar-only file.
    let columnar = HefFile::open(columnar_bytes, None).expect("this reader understands columnar_marks");
    assert_ne!(
        columnar.footer().required_feature_flags & required_features::COLUMNAR_MARKS,
        0
    );
    assert!(
        columnar.footer().marks.is_empty() && columnar.footer().page_directory.is_empty(),
        "a columnar-only file carries no row-oriented marks or page directory"
    );

    let row_oriented = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let surviving = columnar.footer().granules[0];
    let got = columnar
        .read_column(shredded_column_id, surviving.granule_id)
        .expect("surviving stripe reads through the columnar path");
    let expected = row_oriented
        .read_column(shredded_column_id, surviving.granule_id)
        .expect("row-oriented reads");
    assert_eq!(got.data, expected.data, "columnar and row-oriented reads must agree");
    assert_eq!(got.presence, expected.presence);

    // Pruned stripe costs zero marks bytes: reading one granule decoded only that granule's stripe page; every other
    // stripe stays untouched through the real open→read path.
    let pruned_stripe_id = columnar
        .footer()
        .granules
        .iter()
        .map(|granule| granule.stripe_id)
        .find(|stripe_id| *stripe_id != surviving.stripe_id)
        .expect("more than one stripe");
    let Marks::Columnar(state) = &columnar.marks else {
        panic!("expected columnar marks");
    };
    let guard = state.read().unwrap();
    assert_eq!(
        guard.shared_extents_found,
        HashSet::from([surviving.stripe_id]),
        "only the surviving stripe may be touched at all"
    );
    assert!(
        !guard.shared_extents_found.contains(&pruned_stripe_id),
        "a pruned stripe's marks pages must never be fetched or decoded"
    );
    assert_eq!(
        guard.decoded_pages,
        HashSet::from([(shredded_column_id, 0, surviving.stripe_id)]),
        "reading one column decodes that column's marks page, not every column's in the stripe"
    );
    drop(guard);

    // Refuses when not understood: a reader predating `columnar_marks` sees it as an unknown required bit.
    // Simulate that with one extra required feature outside this reader's known set on the same columnar-only file —
    // the open is rejected before the file is served.
    let unknown_bit = 1u64 << 40;
    let mut unknown_footer = columnar_footer.clone();
    unknown_footer.required_feature_flags |= unknown_bit;
    let err = HefFile::open(reframe(&unknown_footer), None).expect_err("an unknown required feature must refuse");
    assert_eq!(err, FormatError::UnknownRequiredFeature { bits: unknown_bit });

    // The gate itself: a required set this reader fully knows (columnar_marks included) is served; add a bit it does
    // not know and it is rejected — exactly what an old reader hits on a columnar_marks file it predates.
    assert!(crate::compat::check_features(required_features::COLUMNAR_MARKS, 0).is_ok());
    assert_eq!(
        crate::compat::check_features(required_features::COLUMNAR_MARKS | unknown_bit, 0),
        Err(FormatError::UnknownRequiredFeature { bits: unknown_bit })
    );
}

/// `embedding_row_slot`/`embedding_row_bytes` mirror `freetext_row_slot`/`freetext_row_bytes`: given a file that
/// carries a per-row byte-offset index for an internal embedding/vector column, each row's `(offset, len)` resolves
/// directly from the index and reads back that row's exact bytes.
#[test]
fn embedding_row_slot_resolves_offset_and_len_from_the_index() {
    let mark = ColumnMark {
        codec_pipeline_id: PipelineId(0),
        column_id: 5000,
        compressed_offset: 0,
        compressed_size: 0,
        first_value_offset: None,
        granule_id: 0,
        page_count: 1,
        projection_id: 0,
        row_count: 2,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let mut bytes = vec![0u8; 220];
    // Offsets block at file offset 100: row 0 -> (0, 4), row 1 -> (4, 6).
    bytes[100..108].copy_from_slice(&[0, 0, 0, 0, 4, 0, 0, 0]);
    bytes[108..116].copy_from_slice(&[4, 0, 0, 0, 6, 0, 0, 0]);
    // Bytes block at file offset 200: row 0's 4 bytes, then row 1's 6 bytes.
    bytes[200..204].copy_from_slice(&[1, 2, 3, 4]);
    bytes[204..210].copy_from_slice(&[5, 6, 7, 8, 9, 10]);

    let mut file = file_with_mark(mark, bytes, optional_features::TYPED_COLUMN_ROW_OFFSETS);
    file.embedding_row_offsets = [(
        (5000, 0),
        EmbeddingRowOffsets {
            bytes_len: 10,
            bytes_offset: 200,
            column_id: 5000,
            granule_id: 0,
            offsets_len: 16,
            offsets_offset: 100,
        },
    )]
    .into();

    let (offset, len) = file.embedding_row_slot(5000, 0, 0).unwrap().expect("row 0 has a value");
    assert_eq!((offset, len), (0, 4));
    assert_eq!(
        file.embedding_row_bytes(5000, 0, offset, len).unwrap(),
        vec![1, 2, 3, 4]
    );

    let (offset, len) = file.embedding_row_slot(5000, 0, 1).unwrap().expect("row 1 has a value");
    assert_eq!((offset, len), (4, 6));
    assert_eq!(
        file.embedding_row_bytes(5000, 0, offset, len).unwrap(),
        vec![5, 6, 7, 8, 9, 10]
    );
}

#[test]
fn embedding_row_slot_is_none_without_an_index_for_the_column_or_granule() {
    let mark = ColumnMark {
        codec_pipeline_id: PipelineId(0),
        column_id: 5000,
        compressed_offset: 0,
        compressed_size: 0,
        first_value_offset: None,
        granule_id: 0,
        page_count: 1,
        projection_id: 0,
        row_count: 1,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let file = file_with_mark(mark, Vec::new(), 0);
    assert_eq!(file.embedding_row_slot(5000, 0, 0).unwrap(), None);
}

/// `embedding_value_for_row` is the exact per-row vector fetch keyed by row ordinal: given a file that carries the
/// per-row byte-offset index for an embedding/vector column, it reads the row's bytes in bounded IOPs via
/// `embedding_row_slot`/`embedding_row_bytes` rather than decoding the whole vector block.
#[test]
fn embedding_value_for_row_reads_via_the_index_in_bounded_iops() {
    let mark = ColumnMark {
        codec_pipeline_id: PipelineId(0),
        column_id: 5000,
        compressed_offset: 0,
        compressed_size: 0,
        first_value_offset: None,
        granule_id: 0,
        page_count: 1,
        projection_id: 0,
        row_count: 2,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let mut bytes = vec![0u8; 220];
    // Offsets block at file offset 100: row 0 -> (0, 4), row 1 -> (4, 6).
    bytes[100..108].copy_from_slice(&[0, 0, 0, 0, 4, 0, 0, 0]);
    bytes[108..116].copy_from_slice(&[4, 0, 0, 0, 6, 0, 0, 0]);
    // Bytes block at file offset 200: row 0's 4 bytes, then row 1's 6 bytes.
    bytes[200..204].copy_from_slice(&[1, 2, 3, 4]);
    bytes[204..210].copy_from_slice(&[5, 6, 7, 8, 9, 10]);

    let mut file = file_with_mark(mark, bytes, optional_features::TYPED_COLUMN_ROW_OFFSETS);
    file.embedding_row_offsets = [(
        (5000, 0),
        EmbeddingRowOffsets {
            bytes_len: 10,
            bytes_offset: 200,
            column_id: 5000,
            granule_id: 0,
            offsets_len: 16,
            offsets_offset: 100,
        },
    )]
    .into();

    assert_eq!(file.embedding_value_for_row(5000, 0).unwrap(), Some(vec![1, 2, 3, 4]));
    assert_eq!(
        file.embedding_value_for_row(5000, 1).unwrap(),
        Some(vec![5, 6, 7, 8, 9, 10])
    );
}

/// Without the per-row byte-offset index, `embedding_value_for_row` falls back to decoding the whole granule block —
/// the same values a caller reading the column through `read_column` directly would get.
#[test]
fn embedding_value_for_row_falls_back_to_whole_block_decode_without_the_index() {
    let data = ColumnData::Strings(vec![Some("row-zero-vector".to_owned()), Some("row-one-vector".to_owned())].into());
    let encoded = encode_block(&data, false);
    let mut body = Writer::with_capacity(4 + encoded.bytes.len());
    crate::layout::encode_presence(&[], 2, &mut body);
    body.put_slice(&encoded.bytes);
    let block = body.into_bytes();

    let mark = ColumnMark {
        codec_pipeline_id: encoded.pipeline,
        column_id: 5000,
        compressed_offset: 0,
        compressed_size: block.len() as u64,
        first_value_offset: None,
        granule_id: 0,
        page_count: 1,
        projection_id: 0,
        row_count: 2,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let file = file_with_mark(mark, block, 0);

    assert_eq!(
        file.embedding_value_for_row(5000, 0).unwrap(),
        Some(b"row-zero-vector".to_vec())
    );
    assert_eq!(
        file.embedding_value_for_row(5000, 1).unwrap(),
        Some(b"row-one-vector".to_vec())
    );
}

/// A block written before the `compressed_presence` feature frames presence as `u32 length | bitmap`. A file whose
/// footer does not declare the feature reads that legacy frame end to end — old files stay readable forever.
#[test]
fn a_file_without_compressed_presence_reads_the_legacy_presence_frame() {
    let data = ColumnData::Strings(vec![Some("first".to_owned()), Some("second".to_owned())].into());
    let encoded = encode_block(&data, false);
    // The legacy frame: a `u32` presence length (zero for a dense block), then the encoded body.
    let mut body = Writer::with_capacity(4 + encoded.bytes.len());
    body.put_u32(0);
    body.put_slice(&encoded.bytes);
    let block = body.into_bytes();

    let mark = ColumnMark {
        codec_pipeline_id: encoded.pipeline,
        column_id: 5000,
        compressed_offset: 0,
        compressed_size: block.len() as u64,
        first_value_offset: None,
        granule_id: 0,
        page_count: 1,
        projection_id: 0,
        row_count: 2,
        uncompressed_offset: 0,
        uncompressed_size: 0,
    };
    let mut file = file_with_mark(mark, block, 0);
    file.footer.required_feature_flags &= !required_features::COMPRESSED_PRESENCE;

    let read = file.read_column(5000, 0).unwrap();
    assert_eq!(read.data, data);
    assert!(read.presence.is_empty(), "a dense legacy block has no presence bitmap");
}

/// Concatenating the pages of a multi-page decimal column requires identical scales. Two pages carrying different
/// scales are a structural inconsistency — later values would be read at the wrong scale — so the merge is rejected
/// instead of silently keeping the first page's scale. Equal scales concatenate. Reproduces #2748.
#[test]
fn concat_rejects_decimal_pages_with_differing_scales() {
    let err = concat_column_data(
        ColumnData::Decimal {
            values: vec![15, 20],
            scale: 1,
        },
        ColumnData::Decimal {
            values: vec![150],
            scale: 2,
        },
    )
    .unwrap_err();
    assert!(matches!(
        err,
        FormatError::Structural { rule } if rule == "multi-page mark pages must have the same decimal scale"
    ));

    let merged = concat_column_data(
        ColumnData::Decimal {
            values: vec![15, 20],
            scale: 1,
        },
        ColumnData::Decimal {
            values: vec![7],
            scale: 1,
        },
    )
    .unwrap();
    assert_eq!(
        merged,
        ColumnData::Decimal {
            values: vec![15, 20, 7],
            scale: 1
        }
    );
}

/// Whether `needle` appears anywhere in `haystack`.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && haystack.windows(needle.len()).any(|window| window == needle)
}

/// Sealing the footer makes the column schema opaque at rest yet fully round-trippable with the file DEK: the footer
/// region bytes are not the plaintext `encode_footer` output and carry no column names; `open`/`open_with_keys(None)`
/// refuse; and `open_with_keys` with the key recovers a footer equal to the plaintext build and reads every row
/// back identically. Implements `hef-security-and-isolation` — "Footer encryption for sensitive schemas".
#[test]
fn encrypted_footer_round_trips_and_is_opaque_at_rest() {
    let rows: Vec<_> = (0..20).map(|i| freetext_row(i, Some("free text body"))).collect();
    let dek = [7u8; 32];
    let plain = build_hef_file(rows.clone(), &small_stripe_config(4096)).unwrap();
    let mut enc_config = small_stripe_config(4096);
    enc_config.footer_encryption = crate::security::FooterEncryption::Encrypted;
    enc_config.footer_dek = Some(dek);
    let enc = build_hef_file(rows.clone(), &enc_config).unwrap();

    // The header advertises the sealed footer, and the footer region is not the plaintext blob.
    assert_ne!(enc.header.feature_flags & required_features::FOOTER_ENCRYPTED, 0);
    let end = enc.bytes.len() - 12;
    let start = end - (enc.footer_len as usize - 12);
    let enc_region = &enc.bytes[start..end];
    let plaintext_blob = crate::layout::footer::encode_footer(&enc.footer);
    assert_ne!(
        enc_region,
        plaintext_blob.as_slice(),
        "the sealed footer must not be the plaintext blob"
    );
    assert!(
        !contains_subslice(&enc.bytes, b"epoch"),
        "a sealed footer must not leak column names at rest",
    );

    // Refuse without the key: neither plain `open` nor `open_with_keys(None)` may read ciphertext as a footer.
    assert!(HefFile::open(enc.bytes.clone(), Some(&enc.file_seal)).is_err());
    assert!(HefFile::open_with_keys(enc.bytes.clone(), Some(&enc.file_seal), None).is_err());

    // Open with the key: the decrypted footer equals the plaintext build, and payload reads match byte for byte.
    let opened = HefFile::open_with_keys(enc.bytes.clone(), Some(&enc.file_seal), Some(&dek)).unwrap();
    assert_footers_match_modulo_marks_form(opened.footer(), &plain.footer);
    let plain_file = HefFile::open(plain.bytes.clone(), Some(&plain.file_seal)).unwrap();
    for row in 0..rows.len() as u64 {
        assert_eq!(
            opened.payload(row).unwrap(),
            plain_file.payload(row).unwrap(),
            "row {row}"
        );
    }
}

/// A file of several stripes with alignment gaps between them, so a corruption test can aim at every stripe and gap.
fn multi_stripe_file_with_gaps() -> (Vec<u8>, [u8; 32], Footer) {
    let rows: Vec<_> = (0..40).map(|i| freetext_row(i, Some("free text body"))).collect();
    let mut config = small_stripe_config(1);
    config.io_alignment_bytes = 4096;
    let built = build_hef_file(rows, &config).unwrap();
    assert!(built.footer.stripes.len() > 1, "test needs several stripes");
    assert!(
        built.footer.integrity_gaps.iter().any(|gap| gap.byte_len > 0),
        "test needs an alignment gap to corrupt"
    );
    (built.bytes, built.file_seal, built.footer)
}

fn read_shape(file: &HefFile, column_id: u32, granule_id: u32) -> Result<(ColumnData, Vec<u8>), FormatError> {
    file.read_column(column_id, granule_id)
        .map(|read| (read.data, read.presence))
}

/// One flipped byte anywhere in the data area refuses the file: an eager open refuses at open, and a lazy open
/// refuses a corrupt gap at open and a corrupt stripe on the first read that touches it — while its other stripes
/// still read. No byte of the corrupt stripe is ever served.
#[test]
fn eager_and_lazy_opens_refuse_a_corrupt_byte_in_every_stripe_and_gap() {
    let (bytes, seal, footer) = multi_stripe_file_with_gaps();
    let amount = footer
        .shredded
        .iter()
        .find(|entry| entry.path == "amount")
        .expect("amount auto-shreds")
        .column_id;
    let granule_in = |stripe_id: u32| {
        footer
            .granules
            .iter()
            .find(|granule| granule.stripe_id == stripe_id)
            .expect("every stripe holds a granule")
            .granule_id
    };

    for stripe in &footer.stripes {
        let mut corrupt = bytes.clone();
        corrupt[(stripe.file_offset + stripe.byte_len / 2) as usize] ^= 0x5A;
        assert!(
            matches!(
                HefFile::open(corrupt.clone(), Some(&seal)),
                Err(FormatError::Blake3Mismatch { scope: "stripe" })
            ),
            "eager open must refuse stripe {}",
            stripe.stripe_id
        );
        let lazy = HefFile::open_lazy(corrupt, Some(&seal), None).expect("lazy open defers stripe hashing");
        assert!(
            matches!(
                read_shape(&lazy, amount, granule_in(stripe.stripe_id)),
                Err(FormatError::Blake3Mismatch { scope: "stripe" })
            ),
            "lazy open must refuse the first read into stripe {}",
            stripe.stripe_id
        );
        let intact = footer
            .stripes
            .iter()
            .find(|other| other.stripe_id != stripe.stripe_id)
            .expect("several stripes");
        read_shape(&lazy, amount, granule_in(intact.stripe_id)).expect("an intact stripe still reads");
    }

    for gap in footer.integrity_gaps.iter().filter(|gap| gap.byte_len > 0) {
        let mut corrupt = bytes.clone();
        corrupt[(gap.file_offset + gap.byte_len / 2) as usize] ^= 0x5A;
        for opened in [
            HefFile::open(corrupt.clone(), Some(&seal)),
            HefFile::open_lazy(corrupt, Some(&seal), None),
        ] {
            assert!(
                matches!(opened, Err(FormatError::Blake3Mismatch { scope: "data gap" })),
                "both opens must refuse the gap at {}",
                gap.file_offset
            );
        }
    }
}

/// A lazy open and a reader made from it serve exactly what an eager open serves, for every block of the file.
#[test]
fn lazy_open_and_fresh_reader_read_what_an_eager_open_reads() {
    let (bytes, seal, footer) = multi_stripe_file_with_gaps();
    let eager = HefFile::open(bytes.clone(), Some(&seal)).unwrap();
    let lazy = HefFile::open_lazy(bytes, Some(&seal), None).unwrap();
    let fresh = lazy.fresh_reader().unwrap();
    for granule in &footer.granules {
        for column in &footer.columns {
            let expected = read_shape(&eager, column.column_id, granule.granule_id).ok();
            assert_eq!(read_shape(&lazy, column.column_id, granule.granule_id).ok(), expected);
            assert_eq!(read_shape(&fresh, column.column_id, granule.granule_id).ok(), expected);
        }
    }
    assert!(eager.column_block_reads() > 0);
}

/// A reader made from a template shares the template's verified bytes but none of its decoded state: it starts as
/// empty as a fresh open would.
#[test]
fn fresh_reader_starts_with_nothing_decoded() {
    let (bytes, seal, _) = multi_stripe_file_with_gaps();
    let template = HefFile::open(bytes, Some(&seal)).unwrap();
    let expected = template.payload(0).unwrap();
    assert!(template.column_block_reads() > 0);

    let fresh = template.fresh_reader().unwrap();
    assert_eq!(fresh.column_block_reads(), 0);
    assert_eq!(fresh.decoded_cache_bytes(), 0);
    assert_eq!(fresh.granule_dictionary_decodes(), 0);
    assert_eq!(fresh.footer(), template.footer());
    assert_eq!(fresh.payload(0).unwrap(), expected);
}

/// The proof appendix is non-authoritative, but its exact geometry is authenticated in the footer. An arbitrary tree
/// cannot be appended to an otherwise valid object and silently treated as proof metadata.
#[test]
fn whole_file_open_rejects_an_undeclared_tree_appendix() {
    let rows: Vec<_> = (0..4).map(|i| freetext_row(i, Some("free text body"))).collect();
    let built = build_hef_file(rows.clone(), &small_stripe_config(4096)).unwrap();
    let expected = built.file_seal;

    let tree = vec![0xABu8; 40];
    let mut object = built.bytes.clone();
    object.extend_from_slice(&tree);
    object.extend_from_slice(&(tree.len() as u64).to_le_bytes());
    object.extend_from_slice(&TREE_TRAILER_MAGIC);

    assert!(HefFile::open(object, Some(&expected)).is_err());
}

/// Encrypted footers enforce the same authenticated proof geometry before exposing metadata.
#[test]
fn encrypted_whole_file_open_rejects_an_undeclared_tree_appendix() {
    let rows: Vec<_> = (0..4).map(|i| freetext_row(i, Some("free text body"))).collect();
    let dek = [7u8; 32];
    let mut enc_config = small_stripe_config(4096);
    enc_config.footer_encryption = crate::security::FooterEncryption::Encrypted;
    enc_config.footer_dek = Some(dek);
    let enc = build_hef_file(rows.clone(), &enc_config).unwrap();
    let expected = enc.file_seal;

    let tree = vec![0xCDu8; 24];
    let mut object = enc.bytes.clone();
    object.extend_from_slice(&tree);
    object.extend_from_slice(&(tree.len() as u64).to_le_bytes());
    object.extend_from_slice(&TREE_TRAILER_MAGIC);

    assert!(HefFile::open_with_keys(object, Some(&expected), Some(&dek)).is_err());
}

/// A wrong file DEK fails the AEAD tag rather than surfacing altered footer bytes.
#[test]
fn encrypted_footer_rejects_the_wrong_key() {
    let rows: Vec<_> = (0..8).map(|i| freetext_row(i, Some("free text body"))).collect();
    let mut enc_config = small_stripe_config(4096);
    enc_config.footer_encryption = crate::security::FooterEncryption::Encrypted;
    enc_config.footer_dek = Some([7u8; 32]);
    let enc = build_hef_file(rows.clone(), &enc_config).unwrap();

    let wrong = [9u8; 32];
    assert!(HefFile::open_with_keys(enc.bytes, Some(&enc.file_seal), Some(&wrong)).is_err());
}

/// An old reader that does not understand a required feature declared in the fixed header fails the whole open, closed,
/// before the footer is decoded — the same refusing gate that makes `FOOTER_ENCRYPTED` reject on a reader that
/// predates it. Simulated with an unknown header bit, mirroring the footer-side gate test above.
#[test]
fn open_refuses_on_a_required_header_feature_beyond_what_this_reader_knows() {
    let rows: Vec<_> = (0..4).map(|i| freetext_row(i, Some("free text body"))).collect();
    let built = build_hef_file(rows.clone(), &small_stripe_config(4096)).unwrap();

    let mut header = built.header.clone();
    let unknown_bit = 1u64 << 40;
    header.feature_flags |= unknown_bit;
    let new_header = crate::layout::encode_header(&header);
    let mut tampered = built.bytes.clone();
    tampered[..HEADER_BLOCK_LEN].copy_from_slice(&new_header);

    let err = HefFile::open(tampered, None).expect_err("an unknown required header feature must refuse");
    assert_eq!(err, FormatError::UnknownRequiredFeature { bits: unknown_bit });
}

/// Two publications under the same file DEK must not seal their footers under the same nonce.
///
/// The nonce was derived from a fixed `(block_id = 0, epoch = 0)` pair, so reusing a build configuration — the DEK
/// arrives inside one, cloneable and caller-owned — encrypted distinct footers under the same `(key, nonce)`. That
/// leaks plaintext relationships and opens the door to tag forgery; binding `file_id` as associated data does not
/// restore uniqueness (issue #8459).
///
/// The nonce is now derived by a keyed hash over the file identity and the plaintext footer, so distinct footers still
/// get distinct nonces while an identical rebuild reproduces the file byte-for-byte — which is what lets a publication
/// retry match the manifest's BLAKE3 (issue #9610).
#[test]
fn two_footers_sealed_under_one_file_dek_use_different_nonces() {
    let dek = [7u8; 32];
    let mut enc_config = small_stripe_config(4096);
    enc_config.footer_encryption = crate::security::FooterEncryption::Encrypted;
    enc_config.footer_dek = Some(dek);

    let sealed_footer = |rows: &[HefRow]| {
        let built = build_hef_file(rows.to_vec(), &enc_config).unwrap();
        let end = built.bytes.len() - 12;
        let start = end - (built.footer_len as usize - 12);
        built.bytes[start..end].to_vec()
    };

    let first = sealed_footer(&(0..8).map(|i| freetext_row(i, Some("first"))).collect::<Vec<_>>());
    let second = sealed_footer(&(0..8).map(|i| freetext_row(i, Some("second"))).collect::<Vec<_>>());

    // AES-256-GCM's nonce is the first 12 bytes of the sealed region.
    assert_ne!(first[..12], second[..12], "distinct footers never share a nonce");
    // The same rows sealed twice reproduce the same nonce and the same sealed bytes: the repeated `(key, nonce)` pair
    // is the same footer for the same file, which reveals nothing the identical plaintext did not already fix, and it
    // is what makes a publication retry rebuild a byte-identical file.
    let repeat_a = sealed_footer(&(0..8).map(|i| freetext_row(i, Some("same"))).collect::<Vec<_>>());
    let repeat_b = sealed_footer(&(0..8).map(|i| freetext_row(i, Some("same"))).collect::<Vec<_>>());
    assert_eq!(repeat_a, repeat_b, "rebuilding an identical file is deterministic");
}

/// The zero-copy view read of a string column yields exactly the values the materializing read decodes — presence
/// bitmap, row count, nulls, and text — for both a dense identity column and a sparse presence-gated free-text
/// column, and declines (`None`) for a non-string column so callers fall back to `read_column` identically.
#[test]
fn read_column_string_views_matches_the_materializing_read() {
    let notes = ["free text body", "another note entirely"];
    let rows: Vec<HefRow> = (0..24)
        .map(|i| freetext_row(i, (i % 3 != 0).then(|| notes[i as usize % 2])))
        .collect();
    let built = build_hef_file(rows, &freetext_config()).unwrap();
    let file = HefFile::open(built.bytes, None).unwrap();

    let freetext_column = crate::columns::column_ids::FREETEXT_BASE;
    for granule in &file.footer().granules {
        let materialized = file.read_column(freetext_column, granule.granule_id).unwrap();
        let (presence, views) = file
            .read_column_string_views(freetext_column, granule.granule_id)
            .unwrap()
            .expect("a string column has a view read");
        assert_eq!(presence, materialized.presence);
        let ColumnData::Strings(values) = &materialized.data else {
            panic!("free-text columns decode as strings");
        };
        assert_eq!(views.len(), values.len());
        for (index, value) in values.iter().enumerate() {
            match value {
                Some(text) => assert_eq!(views.value(index), text, "row {index}"),
                None => assert!(views.is_null(index), "row {index} must be null"),
            }
        }

        // A numeric column has no view path; the caller falls back to the materializing read.
        assert!(
            file.read_column_string_views(crate::columns::column_ids::SEQUENCE, granule.granule_id)
                .unwrap()
                .is_none()
        );
    }
}

/// A whole-column view read crosses page boundaries without materializing strings. This also exercises mixed
/// page-local presence forms: pages where every value is present carry no bitmap, while sparse neighbours do, and the
/// combined bitmap must still match the owned decoder exactly.
#[test]
fn multi_page_string_views_match_the_materializing_read() {
    let rows: Vec<HefRow> = (0..24)
        .map(|i| {
            let note = match i % 6 {
                0 | 1 => Some("a long repeated note that stays outside an inline Arrow view"),
                2 => None,
                3 => Some(""),
                4 | 5 => Some("another long repeated note that exercises the next page"),
                _ => unreachable!(),
            };
            freetext_row(i, note)
        })
        .collect();
    let mut config = freetext_config();
    config.page_size_rows = 2;
    let built = build_hef_file(rows, &config).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    let column_id = crate::columns::column_ids::FREETEXT_BASE;

    assert_eq!(file.prepared_string_view_dictionary_count(), 0);
    for granule in &file.footer().granules {
        let mark = file.mark(column_id, 0, granule.granule_id).unwrap().unwrap();
        assert!(mark.page_count > 1, "the fixture must exercise multi-page assembly");
        let materialized = file.read_column(column_id, granule.granule_id).unwrap();
        let (presence, views) = file
            .read_column_string_views(column_id, granule.granule_id)
            .unwrap()
            .expect("every string page is view-decodable");
        assert_eq!(presence, materialized.presence);
        let ColumnData::Strings(values) = materialized.data else {
            panic!("free-text columns decode as strings");
        };
        assert_eq!(views.len(), values.len());
        for (index, expected) in values.iter().enumerate() {
            assert_eq!(views.is_null(index), expected.is_none(), "row {index} nullness");
            if let Some(expected) = expected {
                assert_eq!(views.value(index), expected, "row {index}");
            }
        }
    }
    if file.shared_alphabet(column_id).is_some() {
        assert_eq!(
            file.prepared_string_view_dictionary_count(),
            1,
            "a used shared dictionary is prepared once and then reused"
        );
    }
}

/// File-scope Arrow dictionaries are projection-driven: opening the file and reading owned or unrelated columns does
/// not prepare one, while the first view read prepares it and every later granule reuses that same arena.
#[test]
fn shared_string_view_dictionaries_are_prepared_lazily() {
    let rows: Vec<HefRow> = (0..64)
        .map(|i| {
            let mut row = freetext_row(i, Some("note"));
            let PayloadInput::Variant(VariantValue::Object(payload)) = &mut row.event.payload else {
                unreachable!("the fixture carries an object payload")
            };
            payload.insert("kind".to_owned(), VariantValue::String(format!("k{}", i % 4)));
            row
        })
        .collect();
    let column_id = crate::columns::column_ids::PROMOTED_BASE;
    let mut config = freetext_config();
    config.targets.index_granularity = 16;
    config.promotion.columns.push(PromotedColumn {
        kind: crate::layout::footer::ColumnKind::String,
        name: "kind".to_owned(),
        path: "kind".to_owned(),
        since_schema_version: 1,
        substring_searchable: false,
    });
    let built = build_hef_file(rows, &config).unwrap();
    assert!(
        built
            .footer
            .shared_dictionaries
            .iter()
            .any(|entry| entry.column_id == column_id),
        "the fixture must use a file-scope dictionary"
    );
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert_eq!(file.prepared_string_view_dictionary_count(), 0);

    let first = file.footer().granules[0].granule_id;
    file.read_column(crate::columns::column_ids::SEQUENCE, first).unwrap();
    file.read_column(column_id, first).unwrap();
    assert_eq!(
        file.prepared_string_view_dictionary_count(),
        0,
        "owned and unrelated reads must not prepare Arrow dictionaries"
    );
    file.read_column_string_views(column_id, first)
        .unwrap()
        .expect("the shared dictionary block is view-decodable");
    assert_eq!(file.prepared_string_view_dictionary_count(), 1);
    let second = file.footer().granules[1].granule_id;
    file.read_column_string_views(column_id, second)
        .unwrap()
        .expect("later blocks reuse the prepared dictionary");
    assert_eq!(file.prepared_string_view_dictionary_count(), 1);
}

/// A thread and its reverse lookups resolve as plain equality filters over the relationship columns, and a reference
/// whose target is not in the file resolves to nothing. Implements `hef-logical-event-model` — "Event relationship
/// references column family", "Thread reconstruction without recursion", and "Relationship references accepted
/// without referential integrity".
#[test]
fn relationship_lookups_filter_threads_and_resolve_references() {
    use crate::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};

    // Row 0 is the thread root; every later row declares its parent (the previous row) and the denormalized root.
    // 20 rows over a granularity of 6 spread the thread across four granules, so the lookups walk granule boundaries.
    let root_id: u128 = 0xF00D_0000;
    let rows: Vec<_> = (0..20)
        .map(|i| {
            let mut row = freetext_row(i, None);
            if i > 0 {
                row.event.relationships = Some(
                    EventRelationships::new(vec![
                        RelationshipRef::to_event(RelationshipKind::Parent, root_id + u128::from(i) - 1),
                        RelationshipRef::to_event(RelationshipKind::Root, root_id),
                    ])
                    .unwrap(),
                );
            }
            row
        })
        .collect();
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 6,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    let built = build_hef_file(rows, &config).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    assert!(file.footer().granules.len() > 1, "the thread spans granules");

    // The whole conversation is one equality lookup on the root reference, in file order; the root itself carries no
    // reference and is fetched by its own identity.
    let root_bytes = root_id.to_be_bytes();
    assert_eq!(
        file.thread_rows(TargetIdSpace::EventId, &root_bytes).unwrap(),
        (1..20).collect::<Vec<u64>>()
    );
    // Children of the root are the rows whose parent reference names it.
    assert_eq!(
        file.rows_referencing(RelationshipKind::Parent, TargetIdSpace::EventId, &root_bytes)
            .unwrap(),
        vec![1]
    );
    // A declared reference resolves to its target row by equality in its identifier space...
    let to_row_seven = RelationshipRef::to_event(RelationshipKind::Parent, root_id + 7);
    assert_eq!(file.resolve_reference(&to_row_seven).unwrap(), vec![7]);
    // ...and a dangling reference resolves to nothing, never an error.
    let dangling = RelationshipRef::to_event(RelationshipKind::Parent, 0xDEAD_BEEF);
    assert_eq!(file.resolve_reference(&dangling).unwrap(), Vec::<u64>::new());
}

/// A `link` cell holds every reference the row declares, separated by spaces, so the lookup must count a hit only
/// where the delimiters make it a whole reference. `event_id:<32 hex>` also occurs verbatim inside
/// `protocol_event_id:<the same 32 hex>…`, and a row declaring only that is not a referrer.
#[test]
fn link_lookups_match_whole_references_only() {
    use crate::events::relationships::{EventRelationships, RelationshipKind, RelationshipRef, TargetIdSpace};

    let target: u128 = 0xABCD_0000_0000_0000_0000_0000_0000_0001;
    let other: u128 = 0x1234_0000_0000_0000_0000_0000_0000_0002;
    // A protocol identifier whose leading sixteen bytes are the target's, so its stored value contains the needle.
    let mut decoy_bytes = target.to_be_bytes().to_vec();
    decoy_bytes.extend_from_slice(&[0u8; 16]);
    let decoy: [u8; 32] = decoy_bytes.try_into().unwrap();

    let declared = [
        Vec::new(),
        vec![
            RelationshipRef::to_event(RelationshipKind::Link, other),
            RelationshipRef::to_event(RelationshipKind::Link, target),
        ],
        vec![RelationshipRef::to_event(RelationshipKind::Link, target)],
        vec![RelationshipRef::to_protocol(RelationshipKind::Link, decoy)],
        vec![
            RelationshipRef::to_event(RelationshipKind::Link, target),
            RelationshipRef::to_event(RelationshipKind::Link, other),
        ],
    ];
    let rows: Vec<_> = declared
        .into_iter()
        .enumerate()
        .map(|(i, refs)| {
            let mut row = freetext_row(i as u64, None);
            if !refs.is_empty() {
                row.event.relationships = Some(EventRelationships::new(refs).unwrap());
            }
            row
        })
        .collect();
    let config = HefBuildConfig {
        analytical_columns: Vec::new(),
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: crate::security::FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        promotion: PromotionPlan::default(),
        targets: LayoutTargets {
            index_granularity: 6,
            index_granularity_bytes: 1 << 20,
            stripe_target_bytes: 4096,
            max_stripe_bytes: 512 * 1024 * 1024,
            min_bytes_for_wide: 10 * 1024 * 1024,
        },
        tenant_id: TenantId::new_test_id(3),
    };
    let built = build_hef_file(rows, &config).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    // Rows 1, 2 and 4 name the target — last in a pair, alone, and first in a pair. Row 3 only contains its text.
    assert_eq!(
        file.rows_referencing(RelationshipKind::Link, TargetIdSpace::EventId, &target.to_be_bytes())
            .unwrap(),
        vec![1, 2, 4]
    );
    // The decoy resolves as itself, in its own space.
    assert_eq!(
        file.rows_referencing(RelationshipKind::Link, TargetIdSpace::ProtocolEventId, &decoy)
            .unwrap(),
        vec![3]
    );
}

/// A long-lived reader's decoded working set must stay within its byte budget while every read stays correct:
/// eviction only ever costs a re-decode. Implements `hef-apis` — "Reader decoded-block caches are bounded".
#[test]
fn decoded_block_cache_stays_within_its_budget_and_reads_stay_correct() {
    let notes = [Some("a body of some length"), Some("another note"), None];
    let rows: Vec<_> = (0..36)
        .map(|i| freetext_row(i, notes[i as usize % notes.len()]))
        .collect();
    let built = build_hef_file(rows.clone(), &freetext_config()).unwrap();
    // A budget far below the file's decoded size forces eviction while the reader reconstructs every granule twice:
    // the same reads keep about 1.9 KiB resident when the budget is lifted. It cannot go much lower — the block a read
    // just touched always survives its own eviction pass, and one decoded block here is already over 128 bytes.
    let budget = 256u64;
    let file = HefFile::open(built.bytes, Some(&built.file_seal))
        .unwrap()
        .with_decoded_cache_budget(budget);

    for pass in 0..2 {
        for (ordinal, note) in (0..rows.len() as u64).zip(notes.iter().cycle()) {
            let mut expected = std::collections::BTreeMap::new();
            expected.insert("amount".to_owned(), VariantValue::Int(ordinal as i64));
            if let Some(note) = note {
                expected.insert("note".to_owned(), VariantValue::String((*note).to_owned()));
            }
            // Full payload reconstruction drives the search-cache path, decoding and caching blocks per granule.
            let value = file.payload(ordinal).unwrap();
            assert_eq!(
                value,
                PayloadRead::Value(VariantValue::Object(expected)),
                "pass {pass} row {ordinal}"
            );
            assert!(
                file.decoded_cache_bytes() <= budget,
                "cache exceeded its budget at pass {pass} row {ordinal}: {} bytes",
                file.decoded_cache_bytes()
            );
        }
    }
}

/// Repeated point reads into one granule decode that granule's payload key dictionary once. The reader holds the
/// decoded dictionary per granule, so the second and every later row reuses it instead of redoing the decode.
#[test]
fn a_granules_payload_dictionary_is_decoded_once_for_repeated_point_reads() {
    // Index granularity 6: rows 0..6 are the first granule, 6..12 the second.
    let rows: Vec<_> = (0..12).map(|i| freetext_row(i, Some("a note"))).collect();
    let built = build_hef_file(rows, &freetext_config()).unwrap();
    let file = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();

    for row in 0..6 {
        assert!(matches!(file.payload(row).unwrap(), PayloadRead::Value(_)), "row {row}");
    }
    assert_eq!(
        file.granule_dictionary_decodes(),
        1,
        "one decode for the granule, not one per row"
    );

    // A single-path read into the same granule reuses that dictionary too.
    assert_eq!(
        file.payload_path(3, "amount").unwrap(),
        Some(VariantValue::Int(3)),
        "the held dictionary resolves the same value a cold decode would"
    );
    assert_eq!(file.granule_dictionary_decodes(), 1);

    // The next granule pays its own single decode.
    assert!(matches!(file.payload(7).unwrap(), PayloadRead::Value(_)));
    assert!(matches!(file.payload(8).unwrap(), PayloadRead::Value(_)));
    assert_eq!(file.granule_dictionary_decodes(), 2);
}

/// A recorded pipeline outside its availability window for the file's declared format version is refused before any
/// byte of the block is decoded. Implements `hef-reader-compatibility` — "Encoding availability windows gate
/// conformance".
#[test]
fn a_block_recording_an_out_of_window_pipeline_is_refused() {
    let mut mark = ColumnMark {
        column_id: 1,
        projection_id: 0,
        granule_id: 0,
        compressed_offset: 0,
        compressed_size: 8,
        uncompressed_offset: 0,
        uncompressed_size: 8,
        row_count: 1,
        page_count: 1,
        first_value_offset: None,
        codec_pipeline_id: PipelineId(11),
    };
    let file = file_with_mark(mark, vec![0u8; 64], 0);
    assert!(file.read_column_raw(1, 0).is_err(), "retired transform id 11 refuses");

    mark.codec_pipeline_id = PipelineId(200);
    let file = file_with_mark(mark, vec![0u8; 64], 0);
    assert!(file.read_column_raw(1, 0).is_err(), "an unknown family has no window");
}

/// A file written before the identity-hash filters existed carries none, and a reader that does not declare the
/// feature must not prune on filters it is ignoring: every granule stays a candidate and the lookup scans as it
/// always did. Checked against the same file read both ways, so the two paths differ only in the declaration.
#[test]
fn a_reader_not_declaring_the_entity_hash_feature_keeps_every_granule() {
    let rows: Vec<_> = (0..24).map(|i| freetext_row(i, Some("note"))).collect();
    let built = build_hef_file(rows, &freetext_config()).unwrap();
    assert!(!built.footer.entity_hash_filters.is_empty());

    let with_filters = HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap();
    let mut without_filters = HefFile::open(built.bytes, Some(&built.file_seal)).unwrap();
    without_filters.usable_optional_features &= !optional_features::ENTITY_HASH_POINT_FILTERS;

    let all_granules = without_filters.footer().granules.len();
    assert!(all_granules > 1, "the fixture must span several granules");
    assert_eq!(without_filters.granules_for_entity_hash(9).len(), all_granules);
    assert_eq!(without_filters.granules_for_entity_hash(9_999).len(), all_granules);
    assert!(with_filters.granules_for_entity_hash(9).len() < all_granules);
    assert!(with_filters.granules_for_entity_hash(9_999).is_empty());
}

/// A one-stripe file of 64 rows whose free-text notes are 32 KiB of incompressible text each, so the stripe spans
/// several proof leaves and a range read has a tree to walk.
fn streaming_built() -> crate::writer::build::BuiltHef {
    let rows: Vec<_> = (0..64u64)
        .map(|i| {
            let mut state = i + 1;
            let note: String = (0..32 * 1024)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    char::from(33 + ((state >> 32) % 94) as u8)
                })
                .collect();
            freetext_row(i, Some(&note))
        })
        .collect();
    let built = build_hef_file(rows, &small_stripe_config(64 << 20)).unwrap();
    assert!(built.tree_len.is_some(), "a multi-megabyte stripe carries proof nodes");
    built
}

/// The exact cold-open tail of a built file: its footer region plus the proof appendix.
fn tail_of(built: &crate::writer::build::BuiltHef) -> &[u8] {
    &built.bytes[built.bytes.len() - (built.footer_len + built.tree_len.unwrap_or(0)) as usize..]
}

fn note_column(footer: &Footer) -> u32 {
    footer
        .freetext
        .iter()
        .find(|entry| entry.declared_field == "note")
        .expect("note is declared free text")
        .column_id
}

/// A footer authenticated from the tail and the manifest's header commitment alone is the footer the header-backed
/// open yields, plans and proves the same range reads, and refuses a commitment or seal for another file.
#[test]
fn open_authenticated_tail_binds_the_seal_without_the_header() {
    let built = streaming_built();
    let tail = tail_of(&built);
    let header = &built.bytes[..HEADER_BLOCK_LEN];
    let commitment = HeaderCommitment::from_header_block(header).unwrap();
    let with_header = HefFooter::open_authenticated(header, tail, built.total_len, &built.file_seal, None).unwrap();
    let tail_only =
        HefFooter::open_authenticated_tail(tail, built.total_len, &built.file_seal, &commitment, None).unwrap();
    assert!(tail_only.is_authenticated());
    assert_eq!(tail_only.footer(), with_header.footer());

    let mut other_header = commitment;
    other_header.blake3[0] ^= 1;
    assert!(matches!(
        HefFooter::open_authenticated_tail(tail, built.total_len, &built.file_seal, &other_header, None),
        Err(FormatError::Blake3Mismatch { scope: "hef file seal" })
    ));
    let mut other_seal = built.file_seal;
    other_seal[0] ^= 1;
    assert!(HefFooter::open_authenticated_tail(tail, built.total_len, &other_seal, &commitment, None).is_err());

    let stripe = &built.footer.stripes[0];
    let offset = crate::file::constant::CHUNK_GROUP_BYTES as u64 - 31;
    let read = tail_only
        .plan_verified_stripe_read(stripe.stripe_id, offset, 97)
        .unwrap();
    assert_eq!(
        read,
        with_header
            .plan_verified_stripe_read(stripe.stripe_id, offset, 97)
            .unwrap()
    );
    let fetched = &built.bytes[read.file_offset as usize..][..read.length as usize];
    assert_eq!(
        tail_only.verify_stripe_range(&read, fetched).unwrap(),
        &built.bytes[(stripe.file_offset + offset) as usize..][..97]
    );
}

/// Several planned reads of one stripe can arrive in one coalesced fetch: each is proven and sliced out of it. A
/// fetch that misses a planned leaf, or a flipped content byte, is corruption; a damaged proof node makes the tree
/// unusable, and the whole-stripe fallback then proves the same bytes against the stripe checksum instead.
#[test]
fn verify_stripe_range_in_serves_ranges_out_of_one_fetch_and_names_the_fault() {
    let built = streaming_built();
    let header = &built.bytes[..HEADER_BLOCK_LEN];
    let footer =
        HefFooter::open_authenticated(header, tail_of(&built), built.total_len, &built.file_seal, None).unwrap();
    let stripe = &built.footer.stripes[0];
    let group = crate::file::constant::CHUNK_GROUP_BYTES as u64;
    let expected = |offset: u64| &built.bytes[(stripe.file_offset + offset) as usize..][..64];

    let a = footer.plan_verified_stripe_read(stripe.stripe_id, 100, 64).unwrap();
    let b = footer
        .plan_verified_stripe_read(stripe.stripe_id, group + 100, 64)
        .unwrap();
    let fetch_start = a.file_offset;
    let fetched = &built.bytes[fetch_start as usize..(b.file_offset + b.length) as usize];
    assert_eq!(
        footer.verify_stripe_range_in(&a, fetched, fetch_start).unwrap(),
        expected(100)
    );
    assert_eq!(
        footer.verify_stripe_range_in(&b, fetched, fetch_start).unwrap(),
        expected(group + 100)
    );
    assert_eq!(
        footer.verify_stripe_range_in(&b, &fetched[..group as usize], fetch_start),
        Err(RangeFault::Corrupt),
        "a fetch that does not cover the planned leaves is never trusted"
    );
    let mut corrupt = fetched.to_vec();
    corrupt[10] ^= 1;
    assert_eq!(
        footer.verify_stripe_range_in(&a, &corrupt, fetch_start),
        Err(RangeFault::Corrupt)
    );

    let mut damaged = built.bytes.clone();
    let tree_start = damaged.len() - built.tree_len.unwrap() as usize;
    damaged[tree_start] ^= 1;
    let damaged_tail = &damaged[damaged.len() - tail_of(&built).len()..];
    let damaged_footer =
        HefFooter::open_authenticated(header, damaged_tail, built.total_len, &built.file_seal, None).unwrap();
    assert_eq!(
        damaged_footer.verify_stripe_range_in(&a, fetched, fetch_start),
        Err(RangeFault::TreeUnusable)
    );
    let whole = damaged_footer
        .plan_whole_stripe_read(stripe.stripe_id, 100, 64)
        .unwrap();
    assert_eq!((whole.file_offset, whole.length), (stripe.file_offset, stripe.byte_len));
    let stripe_bytes = &built.bytes[stripe.file_offset as usize..][..stripe.byte_len as usize];
    assert_eq!(
        damaged_footer
            .verify_stripe_range_in(&whole, stripe_bytes, whole.file_offset)
            .unwrap(),
        expected(100)
    );
    let mut corrupt_stripe = stripe_bytes.to_vec();
    corrupt_stripe[5_000] ^= 1;
    assert_eq!(
        damaged_footer.verify_stripe_range_in(&whole, &corrupt_stripe, whole.file_offset),
        Err(RangeFault::Corrupt)
    );
}

/// An object whose proof appendix was stripped is still the sealed content: whole-file opens never needed the tree,
/// and a cold open authenticates, reports the trees absent, and plans every range read as a whole-stripe fetch that
/// verifies against the stripe checksum.
#[test]
fn absent_proof_appendix_opens_and_plans_whole_stripe_reads() {
    let built = streaming_built();
    let content_len = built.bytes.len() - built.tree_len.unwrap() as usize;
    let stripped = built.bytes[..content_len].to_vec();
    let note = note_column(&built.footer);
    let eager = HefFile::open(stripped.clone(), Some(&built.file_seal)).unwrap();
    let lazy = HefFile::open_lazy(stripped.clone(), Some(&built.file_seal), None).unwrap();
    assert_eq!(
        read_shape(&lazy, note, 0).unwrap(),
        read_shape(&eager, note, 0).unwrap()
    );

    let tail = &stripped[content_len - built.footer_len as usize..];
    let footer = HefFooter::open_authenticated(
        &stripped[..HEADER_BLOCK_LEN],
        tail,
        content_len as u64,
        &built.file_seal,
        None,
    )
    .unwrap();
    assert!(!footer.proof_trees_present());
    let stripe = &built.footer.stripes[0];
    let read = footer.plan_verified_stripe_read(stripe.stripe_id, 100, 64).unwrap();
    assert_eq!((read.file_offset, read.length), (stripe.file_offset, stripe.byte_len));
    let fetched = &stripped[read.file_offset as usize..][..read.length as usize];
    assert_eq!(
        footer.verify_stripe_range(&read, fetched).unwrap(),
        &stripped[(stripe.file_offset + 100) as usize..][..64]
    );
    let mut corrupt = fetched.to_vec();
    corrupt[7] ^= 1;
    assert!(footer.verify_stripe_range(&read, &corrupt).is_err());
}

/// A byte flipped in the footer refuses both opens (the seal covers the footer region); a byte flipped in the
/// outboard tree refuses neither, because the tree is non-authoritative and a whole-file open never consults it.
#[test]
fn eager_and_lazy_opens_refuse_a_corrupt_footer_byte_and_tolerate_a_corrupt_tree() {
    let built = streaming_built();
    let content_len = built.bytes.len() - built.tree_len.unwrap() as usize;
    let footer_start = content_len - built.footer_len as usize;
    let mut corrupt = built.bytes.clone();
    corrupt[footer_start + built.footer_len as usize / 2] ^= 0x5A;
    assert!(HefFile::open(corrupt.clone(), Some(&built.file_seal)).is_err());
    assert!(HefFile::open_lazy(corrupt, Some(&built.file_seal), None).is_err());

    let mut damaged = built.bytes.clone();
    damaged[content_len + 3] ^= 0x5A;
    let note = note_column(&built.footer);
    let eager = HefFile::open(damaged.clone(), Some(&built.file_seal)).unwrap();
    let lazy = HefFile::open_lazy(damaged, Some(&built.file_seal), None).unwrap();
    for granule in &built.footer.granules {
        assert_eq!(
            read_shape(&lazy, note, granule.granule_id).unwrap(),
            read_shape(&eager, note, granule.granule_id).unwrap()
        );
    }
}

/// A footer opened cold locates every block — through the stripe's marks page for a column, decoded from the bytes
/// the marks-page range names — exactly where the whole-file reader finds it through its own mark lookup.
#[test]
fn cold_footer_locates_the_same_blocks_as_the_whole_file_reader() {
    let (bytes, seal, built_footer) = multi_stripe_file_with_gaps();
    let file = HefFile::open(bytes.clone(), Some(&seal)).unwrap();
    let SpeculativeTail::Opened(cold) = HefFooter::open_speculative(&bytes).unwrap() else {
        panic!("the whole file is a long enough tail");
    };
    for column in &built_footer.columns {
        for granule in &built_footer.granules {
            let stripe = built_footer
                .stripes
                .iter()
                .find(|stripe| stripe.stripe_id == granule.stripe_id)
                .unwrap();
            let page = cold
                .marks_page_range(column.column_id, 0, stripe.stripe_id)
                .map(|range| {
                    bytes[(stripe.file_offset + range.stripe_offset) as usize..][..range.length as usize].to_vec()
                });
            let marks = cold
                .stripe_marks(column.column_id, 0, stripe.stripe_id, page.as_deref())
                .unwrap();
            let found = marks.iter().find(|mark| mark.granule_id == granule.granule_id).copied();
            assert_eq!(
                found,
                file.mark(column.column_id, 0, granule.granule_id).unwrap(),
                "column {} granule {}",
                column.column_id,
                granule.granule_id
            );
            if let Some(mark) = found {
                let range = cold.block_range(&mark).unwrap();
                let via_range = &bytes[(stripe.file_offset + range.stripe_offset) as usize..][..range.length as usize];
                let position = file.file_position(granule.granule_id, mark.compressed_offset).unwrap();
                assert_eq!(via_range, &file.bytes[position..][..mark.compressed_size as usize]);
            }
        }
    }
}
