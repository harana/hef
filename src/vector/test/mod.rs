use super::*;
use crate::indexes::bitmap::RoaringRangeBitmap;
use arrow_array::Array;

fn model(dimension: u32, metric: VectorMetric) -> EmbeddingModel {
    EmbeddingModel {
        dimension,
        digest: [7u8; 32],
        id: "test-model".to_owned(),
        metric,
    }
}

/// A deterministic pseudo-random unit vector for `seed`.
fn unit(seed: u64, dimension: u32) -> Vec<f32> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let raw: Vec<f32> = (0..dimension)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            ((state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
        })
        .collect();
    let norm = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
    raw.into_iter().map(|value| value / norm).collect()
}

fn key(byte: u8) -> WrappingKey {
    WrappingKey::new([byte; 32])
}

fn input(rows: usize, dimension: u32) -> SegmentInput {
    SegmentInput {
        event_ids: (0..rows).map(|row| format!("$event{row}")).collect(),
        integer_columns: vec![(
            TIMESTAMP_COLUMN.to_owned(),
            (0..rows as i64).map(|row| 1_000 + row).collect(),
        )],
        string_columns: vec![
            (
                ROOM_ID_COLUMN.to_owned(),
                (0..rows)
                    .map(|row| format!("!room{}:secret.example", row % 2))
                    .collect(),
            ),
            (
                SENDER_COLUMN.to_owned(),
                (0..rows)
                    .map(|row| format!("@user{}:secret.example", row % 3))
                    .collect(),
            ),
        ],
        vectors: (0..rows).map(|row| unit(row as u64, dimension)).collect(),
    }
}

fn spec(model: EmbeddingModel, representations: Vec<VectorElement>) -> SegmentSpec {
    SegmentSpec {
        model,
        producer_device: "DEVICE".to_owned(),
        representations,
        room_id: Some("!room0:secret.example".to_owned()),
    }
}

const ALL: [VectorElement; 4] = [
    VectorElement::U1,
    VectorElement::I8,
    VectorElement::F16,
    VectorElement::F32,
];

#[test]
fn wrong_width_is_rejected() {
    let mut rows = input(3, 16);
    rows.vectors[1] = vec![0.25; 15];
    let result = write_segment(
        &spec(model(16, VectorMetric::Dot), vec![VectorElement::F32]),
        &rows,
        &key(1),
    );
    assert!(matches!(result, Err(VectorError::InvalidInput { .. })));
}

#[test]
fn unnormalized_vector_is_rejected_for_cosine() {
    let mut rows = input(3, 16);
    rows.vectors[2] = vec![1.0; 16];
    let result = write_segment(
        &spec(model(16, VectorMetric::Cosine), vec![VectorElement::F32]),
        &rows,
        &key(1),
    );
    assert!(matches!(result, Err(VectorError::InvalidInput { .. })));
    let accepted = write_segment(
        &spec(model(16, VectorMetric::Dot), vec![VectorElement::F32]),
        &rows,
        &key(1),
    );
    assert!(accepted.is_ok());
}

#[test]
fn empty_and_single_row_segments_are_written_and_read() {
    for rows in [0, 1] {
        let bytes = write_segment(
            &spec(model(8, VectorMetric::Cosine), ALL.to_vec()),
            &input(rows, 8),
            &key(1),
        )
        .unwrap();
        let reader = SegmentReader::open(bytes, &key(1)).unwrap();
        assert_eq!(reader.row_count(), rows as u64);
        assert_eq!(reader.granule_count(), 1);
        for meta in reader.vector_columns() {
            let granule = reader
                .vector_granule(meta.column_id, 0, &model(8, VectorMetric::Cosine))
                .unwrap();
            assert_eq!(granule.len(), rows);
        }
    }
}

#[test]
fn vector_blocks_start_on_64_byte_boundaries() {
    let bytes = write_segment(
        &spec(model(5, VectorMetric::Dot), ALL.to_vec()),
        &input(GRANULE_ROWS + 3, 5),
        &key(1),
    )
    .unwrap();
    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    for column in &reader.footer_columns_for_test() {
        for offset in column {
            assert_eq!(offset % 64, 0);
        }
    }
}

impl<S: SegmentSource> SegmentReader<S> {
    fn footer_columns_for_test(&self) -> Vec<Vec<u64>> {
        self.footer_block_offsets()
    }
}

#[test]
fn granules_and_single_rows_match_what_was_written_for_every_element_type() {
    let dimension = 12;
    let rows = input(GRANULE_ROWS + 10, dimension);
    let model = model(dimension, VectorMetric::Cosine);
    let bytes = write_segment(&spec(model.clone(), ALL.to_vec()), &rows, &key(1)).unwrap();
    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    for element in ALL {
        let column = representation_column_id(element);
        let mut expected = Vec::new();
        for vector in &rows.vectors {
            encode_row(element, vector, &mut expected);
        }
        let width = element.row_width(dimension);
        for row in [
            0u64,
            1,
            GRANULE_ROWS as u64 - 1,
            GRANULE_ROWS as u64,
            GRANULE_ROWS as u64 + 9,
        ] {
            let start = row as usize * width;
            assert_eq!(
                reader.vector_row(column, row, &model).unwrap(),
                expected[start..start + width]
            );
        }
        let granule = reader.vector_granule(column, 1, &model).unwrap();
        assert_eq!(granule.len(), 10);
        let values = granule.values().to_data();
        let stored = values.buffers()[0].as_slice();
        assert_eq!(stored, &expected[GRANULE_ROWS * width..]);
        if element == VectorElement::F32 {
            assert_eq!(decode_row(element, dimension, &expected[..width]), rows.vectors[0]);
        }
    }
}

#[test]
fn representation_lookup_returns_every_linked_column_coarsest_first() {
    let bytes = write_segment(
        &spec(
            model(8, VectorMetric::Cosine),
            vec![VectorElement::F32, VectorElement::U1, VectorElement::I8],
        ),
        &input(4, 8),
        &key(1),
    )
    .unwrap();
    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    let elements: Vec<VectorElement> = reader
        .representations(EMBEDDING_COLUMN_ID)
        .iter()
        .map(|meta| meta.element)
        .collect();
    assert_eq!(elements, vec![VectorElement::U1, VectorElement::I8, VectorElement::F32]);
}

#[test]
fn reading_or_scanning_with_a_different_model_or_metric_fails() {
    let stored = model(8, VectorMetric::Cosine);
    let bytes = write_segment(&spec(stored.clone(), vec![VectorElement::I8]), &input(4, 8), &key(1)).unwrap();
    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    let column = representation_column_id(VectorElement::I8);
    let mut other_id = stored.clone();
    other_id.id = "other".to_owned();
    let mut other_metric = stored.clone();
    other_metric.metric = VectorMetric::L2;
    for wrong in [other_id, other_metric] {
        assert!(matches!(
            reader.vector_row(column, 0, &wrong),
            Err(VectorError::ModelMismatch { .. })
        ));
        assert!(matches!(
            reader.top_k(column, &unit(1, 8), &wrong, None, 3),
            Err(VectorError::ModelMismatch { .. })
        ));
    }
}

fn brute_force(
    meta: &VectorColumnMeta,
    rows: &[Vec<f32>],
    query: &[f32],
    allowed: impl Fn(u64) -> bool,
    k: usize,
) -> Vec<ScoredRow> {
    let prepared = PreparedQuery::new(query, meta.model.metric);
    let mut scored: Vec<ScoredRow> = rows
        .iter()
        .enumerate()
        .filter(|(row, _)| allowed(*row as u64))
        .map(|(row, vector)| {
            let mut bytes = Vec::new();
            encode_row(meta.element, vector, &mut bytes);
            ScoredRow {
                row: row as u64,
                score: prepared.score(meta.element, &bytes),
            }
        })
        .collect();
    scored.sort_by(|left, right| right.score.total_cmp(&left.score).then(left.row.cmp(&right.row)));
    scored.truncate(k);
    scored
}

#[test]
fn top_k_equals_a_brute_force_reference_with_and_without_a_filter() {
    let dimension = 32;
    let rows = input(GRANULE_ROWS + 500, dimension);
    for metric in [VectorMetric::Cosine, VectorMetric::Dot, VectorMetric::L2] {
        let model = model(dimension, metric);
        let bytes = write_segment(
            &spec(
                model.clone(),
                vec![VectorElement::U1, VectorElement::I8, VectorElement::F32],
            ),
            &rows,
            &key(1),
        )
        .unwrap();
        let reader = SegmentReader::open(bytes, &key(1)).unwrap();
        let filter = RoaringRangeBitmap::from_rows((0..rows.event_ids.len() as u64).filter(|row| row % 7 == 3));
        let query = unit(99_999, dimension);
        for meta in reader.vector_columns().to_vec() {
            let all = reader.top_k(meta.column_id, &query, &model, None, 20).unwrap();
            assert_eq!(all, brute_force(&meta, &rows.vectors, &query, |_| true, 20));
            let filtered = reader.top_k(meta.column_id, &query, &model, Some(&filter), 20).unwrap();
            assert_eq!(
                filtered,
                brute_force(&meta, &rows.vectors, &query, |row| filter.contains(row), 20)
            );
            assert!(filtered.iter().all(|row| row.row % 7 == 3));
        }
    }
}

#[test]
fn k_larger_than_the_matching_rows_returns_all_of_them() {
    let model = model(8, VectorMetric::Cosine);
    let bytes = write_segment(&spec(model.clone(), vec![VectorElement::U1]), &input(50, 8), &key(1)).unwrap();
    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    let filter = RoaringRangeBitmap::from_rows([3, 9, 27]);
    let column = representation_column_id(VectorElement::U1);
    let found = reader.top_k(column, &unit(5, 8), &model, Some(&filter), 100).unwrap();
    let mut rows: Vec<u64> = found.iter().map(|row| row.row).collect();
    rows.sort_unstable();
    assert_eq!(rows, vec![3, 9, 27]);
}

#[test]
fn manifest_round_trips_and_content_hash_matches_file_bytes() {
    let model = model(8, VectorMetric::Cosine);
    let rows = input(10, 8);
    let bytes = write_segment(
        &spec(model.clone(), vec![VectorElement::U1, VectorElement::I8]),
        &rows,
        &key(1),
    )
    .unwrap();
    let footer_len = u64::from_le_bytes(bytes[bytes.len() - 12..bytes.len() - 4].try_into().unwrap()) as usize;
    let body = &bytes[128..bytes.len() - 20 - footer_len];
    let reader = SegmentReader::open(bytes.clone(), &key(1)).unwrap();
    let manifest = reader.manifest();
    assert_eq!(manifest.content_hash, *blake3::hash(body).as_bytes());
    assert_eq!(manifest.model, model);
    assert_eq!(manifest.producer_device, "DEVICE");
    assert_eq!(manifest.representations, vec![VectorElement::U1, VectorElement::I8]);
    assert_eq!(manifest.row_count, 10);
    assert_eq!(manifest.window.first_event, "$event0");
    assert_eq!(manifest.window.last_event, "$event9");
    assert_eq!(manifest.window.room_id.as_deref(), Some("!room0:secret.example"));
    reader.verify_content().unwrap();
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn no_plaintext_value_room_id_or_footer_field_appears_in_the_file() {
    let model = model(8, VectorMetric::Cosine);
    let rows = input(20, 8);
    let bytes = write_segment(&spec(model, ALL.to_vec()), &rows, &key(1)).unwrap();
    for needle in [
        "secret.example",
        "$event1",
        "test-model",
        "DEVICE",
        "embedding_",
        "timestamp",
        "room_id",
    ] {
        assert!(!contains(&bytes, needle.as_bytes()), "{needle} leaked");
    }
    assert!(!contains(&bytes, &rows.vectors[0][0].to_le_bytes()));
    assert!(!contains(&bytes, &[7u8; 32]));
}

#[test]
fn a_ranged_read_decrypts_only_the_touched_blocks() {
    let model = model(8, VectorMetric::Cosine);
    let bytes = write_segment(&spec(model.clone(), ALL.to_vec()), &input(GRANULE_ROWS * 3, 8), &key(1)).unwrap();
    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    assert_eq!(reader.blocks_opened(), 0);
    reader
        .vector_row(
            representation_column_id(VectorElement::I8),
            GRANULE_ROWS as u64 + 1,
            &model,
        )
        .unwrap();
    assert_eq!(reader.blocks_opened(), 1);
    let filter = RoaringRangeBitmap::from_rows([5]);
    reader
        .top_k(
            representation_column_id(VectorElement::U1),
            &unit(1, 8),
            &model,
            Some(&filter),
            1,
        )
        .unwrap();
    assert_eq!(reader.blocks_opened(), 2);
}

#[test]
fn the_wrong_key_fails_to_open() {
    let bytes = write_segment(
        &spec(model(8, VectorMetric::Cosine), vec![VectorElement::U1]),
        &input(4, 8),
        &key(1),
    )
    .unwrap();
    assert!(matches!(
        SegmentReader::open(bytes, &key(2)),
        Err(VectorError::Authentication { .. })
    ));
}

#[test]
fn a_tampered_swapped_or_truncated_block_fails() {
    let model = model(8, VectorMetric::Cosine);
    let bytes = write_segment(
        &spec(model.clone(), vec![VectorElement::U1, VectorElement::I8]),
        &input(GRANULE_ROWS + 1, 8),
        &key(1),
    )
    .unwrap();
    let reader = SegmentReader::open(bytes.clone(), &key(1)).unwrap();
    let offsets = reader.footer_block_offsets();
    let column = representation_column_id(VectorElement::I8);

    let mut tampered = bytes.clone();
    tampered[offsets[0][0] as usize + 3] ^= 1;
    let reader = SegmentReader::open(tampered, &key(1)).unwrap();
    assert!(matches!(
        reader.strings(EVENT_ID_COLUMN, 0),
        Err(VectorError::Authentication { .. })
    ));

    // The leading bytes of two granules' blocks of one column, swapped in place.
    let mut swapped = bytes.clone();
    let room_first = offsets[1][0] as usize;
    let room_second = offsets[1][1] as usize;
    let len = 64;
    let a = swapped[room_first..room_first + len].to_vec();
    let b = swapped[room_second..room_second + len].to_vec();
    swapped[room_first..room_first + len].copy_from_slice(&b);
    swapped[room_second..room_second + len].copy_from_slice(&a);
    let reader = SegmentReader::open(swapped, &key(1)).unwrap();
    assert!(reader.strings(ROOM_ID_COLUMN, 0).is_err());

    let truncated = bytes[..bytes.len() - 1].to_vec();
    assert!(SegmentReader::open(truncated, &key(1)).is_err());

    let mut footer_tampered = bytes.clone();
    let at = footer_tampered.len() - 30;
    footer_tampered[at] ^= 1;
    assert!(SegmentReader::open(footer_tampered, &key(1)).is_err());

    let reader = SegmentReader::open(bytes, &key(1)).unwrap();
    assert!(reader.vector_row(column, 0, &model).is_ok());
}

#[test]
fn rewrapping_keeps_the_content_and_locks_out_the_old_key() {
    let model = model(8, VectorMetric::Cosine);
    let bytes = write_segment(&spec(model.clone(), vec![VectorElement::I8]), &input(4, 8), &key(1)).unwrap();
    let rewrapped = rewrap_segment(&bytes, &key(1), &key(2)).unwrap();
    assert!(SegmentReader::open(rewrapped.clone(), &key(1)).is_err());
    let reader = SegmentReader::open(rewrapped, &key(2)).unwrap();
    reader.verify_content().unwrap();
    let original = SegmentReader::open(bytes, &key(1)).unwrap();
    assert_eq!(reader.manifest(), original.manifest());
}

#[test]
fn changing_the_model_lists_every_file_built_with_the_old_one() {
    let old = model(8, VectorMetric::Cosine);
    let mut new = old.clone();
    new.digest = [9u8; 32];
    let files: Vec<SegmentManifest> = [&old, &new, &old]
        .iter()
        .map(|model| {
            let bytes = write_segment(&spec((*model).clone(), vec![VectorElement::U1]), &input(2, 8), &key(1)).unwrap();
            SegmentReader::open(bytes, &key(1)).unwrap().manifest().clone()
        })
        .collect();
    let stale = segments_to_reembed(files.iter().enumerate(), &new);
    assert_eq!(stale, vec![0, 2]);
}

#[test]
fn multi_segment_search_prunes_filters_and_merges() {
    let dimension = 64;
    let model = model(dimension, VectorMetric::Cosine);
    let mut segments = Vec::new();
    let mut all = Vec::new();
    for segment in 0..4u64 {
        let mut rows = input(300, dimension);
        rows.event_ids = (0..300).map(|row| format!("$s{segment}r{row}")).collect();
        rows.vectors = (0..300).map(|row| unit(segment * 1000 + row, dimension)).collect();
        rows.string_columns[0].1 = vec![format!("!room{segment}"); 300];
        all.extend(
            rows.event_ids
                .iter()
                .cloned()
                .zip(rows.vectors.iter().cloned())
                .map(|(id, v)| (id, v, segment)),
        );
        let bytes = write_segment(
            &spec(
                model.clone(),
                vec![VectorElement::U1, VectorElement::I8, VectorElement::F32],
            ),
            &rows,
            &key(1),
        )
        .unwrap();
        segments.push(SegmentReader::open(bytes, &key(1)).unwrap());
    }
    let query = unit(2042, dimension);
    let exact = |allowed: &dyn Fn(u64) -> bool| {
        let mut scored: Vec<(f32, String)> = all
            .iter()
            .filter(|(_, _, segment)| allowed(*segment))
            .map(|(id, vector, _)| (vector.iter().zip(&query).map(|(a, b)| a * b).sum::<f32>(), id.clone()))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0));
        scored.into_iter().take(10).map(|(_, id)| id).collect::<Vec<_>>()
    };
    let search = VectorSearch {
        filter: SearchFilter::default(),
        k: 10,
        model: &model,
        oversample: 20,
        query: &query,
    };
    let hits = search_segments(&segments, &search).unwrap();
    let truth = exact(&|_| true);
    let recall = hits.iter().filter(|hit| truth.contains(&hit.event_id)).count();
    assert!(recall >= 9, "recall {recall}/10");
    assert_eq!(hits[0].event_id, "$s2r42");

    let before: Vec<u64> = segments.iter().map(|segment| segment.blocks_opened()).collect();
    let filtered = VectorSearch {
        filter: SearchFilter {
            room_ids: Some(vec!["!room1".to_owned()]),
            ..SearchFilter::default()
        },
        ..search.clone()
    };
    let hits = search_segments(&segments, &filtered).unwrap();
    assert_eq!(hits.len(), 10);
    assert!(hits.iter().all(|hit| hit.segment == 1));
    for (index, segment) in segments.iter().enumerate() {
        if index != 1 {
            assert_eq!(
                segment.blocks_opened(),
                before[index],
                "pruned segment {index} was decrypted"
            );
        }
    }

    let timed = VectorSearch {
        filter: SearchFilter {
            senders: Some(vec!["@user0:secret.example".to_owned()]),
            time_range: Some((1_000, 1_009)),
            ..SearchFilter::default()
        },
        ..search
    };
    let hits = search_segments(&segments, &timed).unwrap();
    assert_eq!(hits.len(), 10);
    assert!(hits.iter().all(|hit| hit.row < 10 && hit.row % 3 == 0));
}

#[test]
fn u1_estimate_tracks_the_true_dot_product() {
    let dimension = 384;
    let query = unit(1, dimension);
    let mut error = 0.0f32;
    for seed in 2..200 {
        let vector = unit(seed, dimension);
        let mut row = Vec::new();
        encode_row(VectorElement::U1, &vector, &mut row);
        let truth: f32 = vector.iter().zip(&query).map(|(a, b)| a * b).sum();
        error += (u1_estimated_dot(&query, &row) - truth).abs();
    }
    assert!(error / 198.0 < 0.1);
}
