//! Measures the column encoding kernels on one 65,536-row block each: compressed-data numeric, float and string
//! filters, byte-stream-split and ALP-RD float codecs, delta decode, and presence bitmap scans.
//!
//! Every input is generated deterministically, encoded once outside the timed loop with no trailing compression stage,
//! and then decoded or filtered inside it, so a run measures only the kernel named by the benchmark id.
//!
//! Run: `cargo bench --bench encoding`, or name a group to run only it, e.g.
//! `cargo bench --bench encoding -- filter/for`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use hef::encoding::predicate::*;
use hef::encoding::*;
use hef::indexes::bitmap::RoaringRangeBitmap;
use std::hint::black_box;

const ROWS: usize = 65_536;

/// A deterministic xorshift64* stream, so every run benchmarks the same bytes.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// Encodes `data` with `transform` and no trailing stage, returning its pipeline and body.
fn encode(data: &ColumnData, transform: Transform) -> (PipelineId, Vec<u8>) {
    let block = encode_block_forced_for_conformance(data, false, CascadeStrategy::NoTrailing, transform).unwrap();
    assert_eq!(block.pipeline.transform().unwrap(), transform);
    assert_eq!(block.pipeline.compression().unwrap(), Compression::None);
    (block.pipeline, block.bytes)
}

fn range(lower: i128, upper: i128) -> NumericPredicate {
    NumericPredicate::Range {
        lower: Some(NumericBound {
            inclusive: true,
            value: lower,
        }),
        upper: Some(NumericBound {
            inclusive: false,
            value: upper,
        }),
    }
}

/// The numeric predicates every integer filter benchmark runs: one per specialised lane test.
fn numeric_predicates(signed: bool) -> Vec<(&'static str, NumericPredicate)> {
    let (lo, hi) = if signed {
        (-200_000, 300_000)
    } else {
        (200_000, 700_000)
    };
    vec![
        ("eq", NumericPredicate::Equals(if signed { -12_345 } else { 12_345 })),
        ("ne", NumericPredicate::NotEquals(if signed { -12_345 } else { 12_345 })),
        ("range", range(lo, hi)),
        ("in4", NumericPredicate::InSet(vec![lo + 1, lo + 1_000, hi - 7, hi + 9])),
        (
            "in32",
            NumericPredicate::InSet((0..32).map(|i| lo + i * 15_013).collect()),
        ),
    ]
}

fn float_predicates() -> Vec<(&'static str, FloatPredicate)> {
    vec![
        ("eq", FloatPredicate::Equals(512.25)),
        ("ne", FloatPredicate::NotEquals(512.25)),
        (
            "range",
            FloatPredicate::Range {
                lower: Some(FloatBound {
                    inclusive: true,
                    value: 250.0,
                }),
                upper: Some(FloatBound {
                    inclusive: false,
                    value: 750.0,
                }),
            },
        ),
        ("in4", FloatPredicate::InSet(vec![1.5, 250.25, 512.25, 999.75])),
    ]
}

fn u64_values(rng: &mut Rng) -> Vec<u64> {
    (0..ROWS).map(|_| rng.next() % 1_000_000).collect()
}

fn i64_values(rng: &mut Rng) -> Vec<i64> {
    (0..ROWS).map(|_| (rng.next() % 1_000_000) as i64 - 500_000).collect()
}

/// Ascending timestamps with jittered gaps, the shape a delta block is chosen for.
fn timestamps(rng: &mut Rng) -> Vec<u64> {
    let mut now = 1_700_000_000_000u64;
    (0..ROWS)
        .map(|_| {
            now += 1 + rng.next() % 2_000;
            now
        })
        .collect()
}

/// Signed values that wander up and down, so the zigzag deltas carry both signs.
fn random_walk(rng: &mut Rng) -> Vec<i64> {
    let mut value = 0i64;
    (0..ROWS)
        .map(|_| {
            value += (rng.next() % 2_001) as i64 - 1_000;
            value
        })
        .collect()
}

fn decimal_floats(rng: &mut Rng) -> Vec<f64> {
    (0..ROWS).map(|_| (rng.next() % 100_000) as f64 / 100.0).collect()
}

/// Full-precision doubles in a narrow exponent band, which ALP rejects and ALP-RD or byte-stream-split take.
fn noisy_floats(rng: &mut Rng) -> Vec<f64> {
    (0..ROWS)
        .map(|_| 1.0 + (rng.next() >> 11) as f64 / (1u64 << 53) as f64 * 1_000.0)
        .collect()
}

/// URL-like values long enough that their FSST block stores per-value prefix keys and fingerprints.
fn urls(rng: &mut Rng) -> StringColumn {
    let mut column = StringColumn::new();
    for row in 0..ROWS {
        if row % 16 == 15 {
            column.push(None);
            continue;
        }
        let a = rng.next();
        let b = rng.next();
        let value = format!(
            "https://cdn{}.example.org/assets/{:08x}/{:012x}/file-{}.js",
            a % 8,
            b,
            a >> 20,
            row % 977
        );
        column.push(Some(&value));
    }
    column
}

fn bench_numeric_filters(c: &mut Criterion) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut group = c.benchmark_group("filter");
    group.throughput(Throughput::Elements(ROWS as u64));
    let blocks = [
        (
            "for_u64",
            encode(&ColumnData::U64(u64_values(&mut rng)), Transform::ForBitpack),
            false,
        ),
        (
            "for_i64",
            encode(&ColumnData::I64(i64_values(&mut rng)), Transform::ForBitpack),
            true,
        ),
        (
            "delta_u64",
            encode(&ColumnData::U64(timestamps(&mut rng)), Transform::DeltaBitpack),
            false,
        ),
        (
            "delta_i64",
            encode(&ColumnData::I64(random_walk(&mut rng)), Transform::DeltaBitpack),
            true,
        ),
    ];
    for (name, (pipeline, body), signed) in &blocks {
        for (predicate_name, predicate) in numeric_predicates(*signed) {
            group.bench_function(format!("{name}/{predicate_name}"), |b| {
                b.iter(|| filter_numeric_block_with_body(*pipeline, black_box(body), &predicate).unwrap())
            });
        }
    }
    let alp = encode(&ColumnData::F64(decimal_floats(&mut rng)), Transform::Alp);
    for (predicate_name, predicate) in float_predicates() {
        group.bench_function(format!("alp/{predicate_name}"), |b| {
            b.iter(|| filter_float_block_with_body(alp.0, black_box(&alp.1), &predicate).unwrap())
        });
    }
    group.finish();
}

fn bench_decoded_filters(c: &mut Criterion) {
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    let mut group = c.benchmark_group("decoded");
    group.throughput(Throughput::Elements(ROWS as u64));
    let columns = [
        ("u64", ColumnData::U64(u64_values(&mut rng)), false),
        ("i64", ColumnData::I64(i64_values(&mut rng)), true),
    ];
    for (name, data, signed) in &columns {
        for (predicate_name, predicate) in numeric_predicates(*signed) {
            group.bench_function(format!("{name}/{predicate_name}"), |b| {
                b.iter(|| predicate.filter_decoded(black_box(data)))
            });
        }
    }
    let floats = decimal_floats(&mut rng);
    for (predicate_name, predicate) in float_predicates() {
        group.bench_function(format!("f64/{predicate_name}"), |b| {
            b.iter(|| predicate.filter_decoded(black_box(&floats)))
        });
    }
    // A plain-encoded block has no compressed-data kernel, so the filter decodes and then runs the reference path.
    let plain = encode(&ColumnData::U64(u64_values(&mut rng)), Transform::PlainU64);
    let predicate = range(200_000, 700_000);
    group.bench_function("plain_u64_or_decode/range", |b| {
        b.iter(|| filter_numeric_block_or_decode(plain.0, black_box(&plain.1), &predicate).unwrap())
    });
    group.finish();
}

fn bench_float_codecs(c: &mut Criterion) {
    let mut rng = Rng(0xA076_1D64_78BD_642F);
    let mut group = c.benchmark_group("codec");
    group.throughput(Throughput::Elements(ROWS as u64));
    let noisy = ColumnData::F64(noisy_floats(&mut rng));
    group.bench_function("byte_stream_split/encode", |b| {
        b.iter(|| encode(black_box(&noisy), Transform::ByteStreamSplit))
    });
    let split = encode(&noisy, Transform::ByteStreamSplit);
    group.bench_function("byte_stream_split/decode", |b| {
        b.iter(|| decode_block(split.0, black_box(&split.1)).unwrap())
    });
    let alp_rd = encode(&noisy, Transform::AlpRd);
    group.bench_function("alp_rd/decode", |b| {
        b.iter(|| decode_block(alp_rd.0, black_box(&alp_rd.1)).unwrap())
    });
    let delta_u64 = encode(&ColumnData::U64(timestamps(&mut rng)), Transform::DeltaBitpack);
    group.bench_function("delta_u64/decode", |b| {
        b.iter(|| decode_block(delta_u64.0, black_box(&delta_u64.1)).unwrap())
    });
    let delta_i64 = encode(&ColumnData::I64(random_walk(&mut rng)), Transform::DeltaBitpack);
    group.bench_function("delta_i64/decode", |b| {
        b.iter(|| decode_block(delta_i64.0, black_box(&delta_i64.1)).unwrap())
    });
    group.finish();
}

fn bench_string_filters(c: &mut Criterion) {
    let mut rng = Rng(0xE703_7ED1_A0B4_28DB);
    let mut group = c.benchmark_group("filter");
    group.throughput(Throughput::Elements(ROWS as u64));
    let column = ColumnData::Strings(urls(&mut rng));
    let block = encode_block_forced_for_conformance(&column, false, CascadeStrategy::NoTrailing, Transform::FsstString)
        .unwrap();
    assert_eq!(block.pipeline.side_stream().unwrap(), SideStream::FsstValueKeys);
    let predicates = [
        (
            "contains_rare",
            StringPredicate::Contains(ContainsMatcher::new(vec!["file-976.js".to_owned()], false)),
        ),
        (
            "contains_common",
            StringPredicate::Contains(ContainsMatcher::new(vec!["cdn3".to_owned()], false)),
        ),
        (
            "contains_absent",
            StringPredicate::Contains(ContainsMatcher::new(vec!["zzqx".to_owned()], false)),
        ),
        (
            "range",
            StringPredicate::Range {
                lower: Some(StringBound {
                    inclusive: true,
                    value: "https://cdn2.example.org/assets/4".to_owned(),
                }),
                upper: Some(StringBound {
                    inclusive: false,
                    value: "https://cdn5.example.org/assets/b".to_owned(),
                }),
            },
        ),
        ("eq", StringPredicate::Equals("https://cdn1.example.org/x".to_owned())),
    ];
    for (name, predicate) in &predicates {
        group.bench_function(format!("fsst/{name}"), |b| {
            b.iter(|| filter_string_block(block.pipeline, black_box(&block.bytes), predicate).unwrap())
        });
    }
    group.finish();
}

fn bench_presence(c: &mut Criterion) {
    let mut rng = Rng(0x8EBC_6AF0_9C88_C6E3);
    let mut group = c.benchmark_group("bitmap");
    let bytes = ROWS / 8;
    group.throughput(Throughput::Elements(ROWS as u64));
    // Long uniform stretches broken by a few mixed bytes, the shape of a mostly-present column.
    let mostly_present: Vec<u8> = (0..bytes)
        .map(|i| if i % 97 == 0 { rng.next() as u8 } else { 0xFF })
        .collect();
    let mixed: Vec<u8> = (0..bytes).map(|_| rng.next() as u8).collect();
    // Runs of 4 to 35 rows, so most bytes hold a transition but few hold more than one.
    let mut runs = Vec::with_capacity(bytes);
    let mut bit = 0usize;
    let mut set = false;
    let mut word = 0u8;
    while runs.len() < bytes {
        let len = 4 + (rng.next() % 32) as usize;
        for _ in 0..len {
            if set {
                word |= 1 << (bit % 8);
            }
            bit += 1;
            if bit.is_multiple_of(8) {
                runs.push(word);
                word = 0;
            }
        }
        set = !set;
    }
    runs.truncate(bytes);
    for (name, bits) in [("mostly_present", &mostly_present), ("mixed", &mixed), ("runs", &runs)] {
        group.bench_function(format!("from_packed_bits/{name}"), |b| {
            b.iter(|| RoaringRangeBitmap::from_packed_bits(black_box(bits)))
        });
    }
    // Decoding a nullable block counts its presence bitmap; a raw string block keeps the rest of the decode cheap.
    let mut column = StringColumn::new();
    for row in 0..ROWS {
        column.push((mixed[row / 8] >> (row % 8) & 1 == 1).then_some("v"));
    }
    let raw = encode(&ColumnData::Strings(column), Transform::RawString);
    group.bench_function("nullable_raw_string/decode", |b| {
        b.iter(|| decode_block(raw.0, black_box(&raw.1)).unwrap())
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_numeric_filters,
    bench_string_filters,
    bench_decoded_filters,
    bench_float_codecs,
    bench_presence
);
criterion_main!(benches);
