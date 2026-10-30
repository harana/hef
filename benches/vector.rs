//! Measures vector segment scans over 100,000 rows of 384-dimension embeddings: a 1-bit Hamming scan and an int8 dot
//! scan, each over the whole segment and with a filter to one of ten rooms. Blocks are decrypted inside the timed loop,
//! as a real search would.
//!
//! Run: `cargo bench --bench vector`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use criterion::{Criterion, criterion_group, criterion_main};
use hef::indexes::bitmap::RoaringRangeBitmap;
use hef::vector::*;
use std::hint::black_box;

const ROWS: usize = 100_000;
const DIMENSION: u32 = 384;

fn unit(state: &mut u64) -> Vec<f32> {
    let raw: Vec<f32> = (0..DIMENSION)
        .map(|_| {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            ((*state >> 40) as f32 / (1u64 << 24) as f32) - 0.5
        })
        .collect();
    let norm = raw.iter().map(|value| value * value).sum::<f32>().sqrt();
    raw.into_iter().map(|value| value / norm).collect()
}

fn scans(c: &mut Criterion) {
    let mut state = 0x2545_F491_4F6C_DD1D;
    let model = EmbeddingModel {
        dimension: DIMENSION,
        digest: [0; 32],
        id: "bench".to_owned(),
        metric: VectorMetric::Cosine,
    };
    let input = SegmentInput {
        event_ids: (0..ROWS).map(|row| format!("$e{row}")).collect(),
        integer_columns: Vec::new(),
        string_columns: vec![(
            ROOM_ID_COLUMN.to_owned(),
            (0..ROWS).map(|row| format!("!r{}", row % 10)).collect(),
        )],
        vectors: (0..ROWS).map(|_| unit(&mut state)).collect(),
    };
    let spec = SegmentSpec {
        model: model.clone(),
        producer_device: "bench".to_owned(),
        representations: vec![VectorElement::U1, VectorElement::I8],
        room_id: None,
    };
    let key = WrappingKey::new([1; 32]);
    let reader = SegmentReader::open(write_segment(&spec, &input, &key).unwrap(), &key).unwrap();
    let query = unit(&mut state);
    let room = RoaringRangeBitmap::from_rows((0..ROWS as u64).filter(|row| row % 10 == 3));
    let mut group = c.benchmark_group("vector_scan_100k_x384");
    for element in [VectorElement::U1, VectorElement::I8] {
        let column = representation_column_id(element);
        group.bench_function(format!("{element:?}/all"), |b| {
            b.iter(|| black_box(reader.top_k(column, &query, &model, None, 100).unwrap()))
        });
        group.bench_function(format!("{element:?}/one_room"), |b| {
            b.iter(|| black_box(reader.top_k(column, &query, &model, Some(&room), 100).unwrap()))
        });
    }
    group.finish();
}

criterion_group!(benches, scans);
criterion_main!(benches);
