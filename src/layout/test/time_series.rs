use super::*;
use crate::artifacts::batch::{EventInput, PayloadInput};
use crate::benchmarks::{TimeSeriesRollupMeasurement, time_series_rollup_gate_passes};
use crate::columns::{FreetextDeclaration, PromotedColumn, PromotionPlan};
use crate::encoding::Transform;
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TenantId, TimestampValue};
use crate::layout::footer::ColumnKind;
use crate::layout::{LayoutTargets, required_features};
use crate::security::FooterEncryption;
use crate::typed_id::TypedIdTestExt;
use crate::writer::build::{BuildLifecycle, BuiltHef, HefBuildConfig, HefRow, build_hef_file};
use std::collections::BTreeMap;

const ROWS: u64 = 2_000;
const BUCKET_NANOS: i64 = 1_000_000_000;
/// Starts before 1970 so buckets on both sides of zero are exercised.
const FIRST_NANOS: i64 = -20_000_000_000;
const CPU: u32 = column_ids::PROMOTED_BASE;
const TEMPERATURE: u32 = column_ids::PROMOTED_BASE + 1;

/// Deterministic pseudo-random bits.
fn mix(seed: u64) -> u64 {
    let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One row's reading.
#[derive(Clone, Copy)]
struct Sample {
    /// Never goes back from one row to the next, sometimes repeats, and moves about 37 ms a row.
    at: i64,
    /// Used memory over total: full double precision, often holding still.
    cpu: f64,
    /// On two rows in three, with a negative zero and one NaN among them.
    temperature: Option<f64>,
}

fn samples() -> Vec<Sample> {
    let mut at = FIRST_NANOS;
    let mut used = 6_000_000_000u64;
    (0..ROWS)
        .map(|i| {
            at += (mix(i) % 75) as i64 * 1_000_000;
            if mix(i + 7).is_multiple_of(2) {
                used += 40_960;
            }
            let temperature = match i {
                _ if i % 3 == 2 => None,
                700 => Some(f64::from_bits(0x7FF8_0000_0000_0BAD)),
                _ if i % 97 == 0 => Some(-0.0),
                _ => Some(21.0 + (i as f64 / 90.0).sin() * 3.0),
            };
            Sample {
                at,
                cpu: used as f64 / 16_000_000_000.0,
                temperature,
            }
        })
        .collect()
}

fn row(i: u64, sample: Sample) -> HefRow {
    let mut payload = BTreeMap::from([("cpu".to_owned(), VariantValue::Double(sample.cpu))]);
    if let Some(temperature) = sample.temperature {
        payload.insert("temperature".to_owned(), VariantValue::Double(temperature));
    }
    HefRow {
        epoch: 1,
        sequence: i + 1,
        event: EventInput {
            connector_delivery_hash_high: 0,
            connector_delivery_hash_low: i,
            envelope: EventEnvelope {
                account_id: None,
                account_id_hash_low: 0,
                actor_id: None,
                actor_id_hash_low: 0,
                dedupe_hash_high: 1,
                dedupe_hash_low: i,
                entity_id: Some("host-1".to_owned()),
                entity_id_hash_high: 1,
                entity_id_hash_low: 1,
                entity_type: "host".to_owned(),
                event_id: EventId::new_test_id(0x7000 + u128::from(i)),
                event_type: "usage".to_owned(),
                flags: EventFlags(0),
                ingested_at: TimestampValue::from_physical_nanos(sample.at + 5),
                occurred_at: TimestampValue::from_physical_nanos(sample.at),
                schema_version: 1,
                source: "agent".to_owned(),
                stream_id: StreamId(1),
                stream_sequence: i,
                tenant_id: TenantId::new_test_id(5),
                trace_id_hash_low: 0,
            },
            payload: PayloadInput::Variant(VariantValue::Object(payload)),
            provenance: None,
            relationships: None,
            source_delivery: None,
            source_schema: None,
        },
    }
}

fn float_column(name: &str) -> PromotedColumn {
    PromotedColumn {
        kind: ColumnKind::F64,
        name: name.to_owned(),
        path: name.to_owned(),
        since_schema_version: 1,
        substring_searchable: false,
    }
}

fn config(profile: Profile, bucket_nanos: Option<i64>) -> HefBuildConfig {
    HefBuildConfig {
        analytical_columns: Vec::new(),
        audit: None,
        created_at_physical: 0,
        footer_dek: None,
        footer_encryption: FooterEncryption::Plaintext,
        freetext: FreetextDeclaration::default(),
        freetext_row_offset_index: false,
        generation_id: 1,
        graph: None,
        io_alignment_bytes: 0,
        lifecycle: BuildLifecycle::FreshPublication,
        page_size_rows: 0,
        profile,
        promotion: PromotionPlan {
            columns: vec![float_column("cpu"), float_column("temperature")],
        },
        targets: LayoutTargets {
            index_granularity: 64,
            ..LayoutTargets::default()
        },
        tenant_id: TenantId::new_test_id(5),
        time_series_bucket_nanos: bucket_nanos,
    }
}

fn build(profile: Profile, bucket_nanos: Option<i64>) -> BuiltHef {
    let rows = samples()
        .into_iter()
        .enumerate()
        .map(|(i, sample)| row(i as u64, sample))
        .collect();
    build_hef_file(rows, &config(profile, bucket_nanos)).unwrap()
}

fn open(built: &BuiltHef) -> HefFile {
    HefFile::open(built.bytes.clone(), Some(&built.file_seal)).unwrap()
}

fn bits(aggregate: FloatAggregate) -> (u64, u64, u64, u64) {
    (
        aggregate.count,
        aggregate.sum.to_bits(),
        aggregate.min.to_bits(),
        aggregate.max.to_bits(),
    )
}

/// The aggregate computed straight from the input rows, grouped the way the reader groups them.
fn expected(samples: &[Sample], column: u32, from: i64, to: i64, bucket_nanos: i64) -> FloatAggregate {
    let mut buckets: BTreeMap<i64, FloatAggregate> = BTreeMap::new();
    for sample in samples {
        let value = if column == CPU {
            Some(sample.cpu)
        } else {
            sample.temperature
        };
        if let Some(value) = value
            && (from..to).contains(&sample.at)
        {
            buckets
                .entry(sample.at.div_euclid(bucket_nanos))
                .or_default()
                .add(value);
        }
    }
    let mut total = FloatAggregate::default();
    for aggregate in buckets.values() {
        total.merge(aggregate);
    }
    total
}

#[test]
fn a_time_series_file_rolls_up_its_float_columns_and_stores_xor_float_blocks() {
    let samples = samples();
    let built = build(Profile::TimeSeries, Some(BUCKET_NANOS));
    let file = open(&built);
    assert_eq!(file.header().profile, Profile::TimeSeries);
    let rollups = file.time_series_rollups().unwrap();
    assert_eq!(rollups.bucket_nanos, BUCKET_NANOS);
    assert_eq!(rollups.column_ids, vec![CPU, TEMPERATURE]);
    assert_eq!(rollups.row_count(), ROWS);
    assert_ne!(
        file.footer().optional_feature_flags & optional_features::TIME_SERIES_ROLLUPS,
        0
    );
    // Each bucket holds exactly the rows whose occurred_at falls in it.
    for bucket in &rollups.buckets {
        for ordinal in bucket.first_row_ordinal..bucket.first_row_ordinal + bucket.row_count {
            assert_eq!(samples[ordinal as usize].at.div_euclid(BUCKET_NANOS), bucket.bucket);
        }
    }
    // The full-precision cpu readings store as Chimp somewhere, and the file declares it in both feature words.
    let chimp_granules = file
        .footer()
        .granules
        .iter()
        .filter(|granule| {
            file.mark(CPU, 0, granule.granule_id)
                .unwrap()
                .is_some_and(|mark| mark.codec_pipeline_id.transform() == Ok(Transform::Chimp))
        })
        .count();
    assert!(chimp_granules > 0);
    assert_ne!(
        file.footer().required_feature_flags & required_features::XOR_FLOAT_BLOCKS,
        0
    );
    assert_ne!(file.header().feature_flags & required_features::XOR_FLOAT_BLOCKS, 0);
    // Every value reads back bit for bit.
    for granule in &file.footer().granules {
        let ColumnData::F64(values) = file.read_column(CPU, granule.granule_id).unwrap().data else {
            panic!("cpu is a float column");
        };
        for (offset, value) in values.iter().enumerate() {
            let sample = samples[(granule.first_row_ordinal + offset as u64) as usize];
            assert_eq!(value.to_bits(), sample.cpu.to_bits());
        }
    }
}

#[test]
fn an_event_file_of_the_same_rows_has_no_rollups_and_no_xor_float_blocks() {
    let file = open(&build(Profile::Event, None));
    assert!(file.footer().time_series_rollups.is_none());
    assert_eq!(
        file.footer().required_feature_flags & required_features::XOR_FLOAT_BLOCKS,
        0
    );
    assert_eq!(
        file.footer().optional_feature_flags & optional_features::TIME_SERIES_ROLLUPS,
        0
    );
    assert_eq!(
        file.time_series_aggregate(CPU, 0, 1, BUCKET_NANOS),
        Err(FormatError::Structural {
            rule: "a time-series aggregate needs a time-series file"
        })
    );
}

#[test]
fn rollup_answers_equal_full_scan_answers_bit_for_bit_over_random_ranges() {
    let with_rollups = open(&build(Profile::TimeSeries, Some(BUCKET_NANOS)));
    // Rolled up at another width, so a query at `BUCKET_NANOS` cannot use them and reads every row in range.
    let scan_only = open(&build(Profile::TimeSeries, Some(BUCKET_NANOS * 7 + 3)));
    let samples = samples();
    let first = samples[0].at;
    let last = samples[ROWS as usize - 1].at;
    let span = (last - first) as u64 + 4 * BUCKET_NANOS as u64;
    for trial in 0..300u64 {
        let a = first - 2 * BUCKET_NANOS + (mix(trial * 2) % span) as i64;
        let b = first - 2 * BUCKET_NANOS + (mix(trial * 2 + 1) % span) as i64;
        // Every fifth range starts and ends on bucket edges, so it is answered from rollups alone.
        let (from, to) = match trial % 5 {
            0 => (
                a.div_euclid(BUCKET_NANOS) * BUCKET_NANOS,
                b.div_euclid(BUCKET_NANOS) * BUCKET_NANOS,
            ),
            _ => (a, b),
        };
        let (from, to) = (from.min(to), from.max(to));
        for column in [CPU, TEMPERATURE] {
            let rolled = with_rollups
                .time_series_aggregate(column, from, to, BUCKET_NANOS)
                .unwrap();
            let scanned = scan_only.time_series_aggregate(column, from, to, BUCKET_NANOS).unwrap();
            assert_eq!(bits(rolled), bits(scanned), "column {column}, range {from}..{to}");
            assert_eq!(bits(rolled), bits(expected(&samples, column, from, to, BUCKET_NANOS)));
        }
    }
}

#[test]
fn a_range_of_whole_buckets_reads_no_rows_and_a_cut_range_reads_only_its_edge_buckets() {
    let built = build(Profile::TimeSeries, Some(BUCKET_NANOS));
    let samples = samples();
    let first_bucket = samples[0].at.div_euclid(BUCKET_NANOS);
    let last_bucket = samples[ROWS as usize - 1].at.div_euclid(BUCKET_NANOS);

    let file = open(&built);
    let whole = file
        .time_series_aggregate(
            CPU,
            first_bucket * BUCKET_NANOS,
            (last_bucket + 1) * BUCKET_NANOS,
            BUCKET_NANOS,
        )
        .unwrap();
    assert_eq!(whole.count, ROWS);
    let aligned_block_reads = file.column_block_reads();

    let (from, to) = (
        first_bucket * BUCKET_NANOS + BUCKET_NANOS / 2,
        last_bucket * BUCKET_NANOS - BUCKET_NANOS / 2,
    );
    let file = open(&built);
    let cut = file.time_series_aggregate(CPU, from, to, BUCKET_NANOS).unwrap();
    assert!(cut.count > 0);
    let granules = file.footer().granules.len() as u64;
    // Two edge buckets, each within at most two granules, each read as its time and value blocks.
    let cut_block_reads = file.column_block_reads();
    assert!(granules > 8);

    // A width other than the file's rollup width reads every row in the range.
    let file = open(&built);
    file.time_series_aggregate(CPU, from, to, 2 * BUCKET_NANOS).unwrap();
    let measurement = TimeSeriesRollupMeasurement {
        aligned_block_reads,
        cut_block_reads,
        scan_block_reads: file.column_block_reads(),
    };
    println!("time-series aggregate column block reads: {measurement:?}");
    assert!(time_series_rollup_gate_passes(&measurement), "{measurement:?}");
}

#[test]
fn an_aggregate_refuses_a_width_that_is_not_positive() {
    let file = open(&build(Profile::TimeSeries, None));
    assert_eq!(
        file.time_series_rollups().unwrap().bucket_nanos,
        crate::writer::constant::DEFAULT_TIME_SERIES_BUCKET_NANOS
    );
    assert!(file.time_series_aggregate(CPU, 0, 1, 0).is_err());
}

#[test]
fn rollups_round_trip_and_refuse_damaged_bytes() {
    let rollups = open(&build(Profile::TimeSeries, Some(BUCKET_NANOS)))
        .time_series_rollups()
        .unwrap()
        .clone();
    let bytes = rollups.encode();
    let decoded = TimeSeriesRollups::decode(&bytes).unwrap();
    assert_eq!(decoded.encode(), bytes);
    for cut in [0, 1, 8, 12, bytes.len() - 1] {
        assert!(TimeSeriesRollups::decode(&bytes[..cut]).is_err());
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert!(TimeSeriesRollups::decode(&trailing).is_err());
    // A gap between two buckets' rows.
    let mut gap = rollups.clone();
    gap.buckets[1].first_row_ordinal += 1;
    assert!(TimeSeriesRollups::decode(&gap.encode()).is_err());
    // A width that is not positive.
    let mut zero = rollups.clone();
    zero.bucket_nanos = 0;
    assert!(TimeSeriesRollups::decode(&zero.encode()).is_err());
    // A count above its bucket's rows.
    let mut over = rollups;
    over.buckets[0].aggregates[0].count = over.buckets[0].row_count + 1;
    assert!(TimeSeriesRollups::decode(&over.encode()).is_err());
}

#[test]
fn minimum_and_maximum_skip_nan_and_order_negative_zero_first() {
    let mut aggregate = FloatAggregate::default();
    for value in [0.0, f64::NAN, -0.0, 2.5, f64::NAN] {
        aggregate.add(value);
    }
    assert_eq!(aggregate.count, 5);
    assert!(aggregate.sum.is_nan());
    assert_eq!(aggregate.min.to_bits(), (-0.0f64).to_bits());
    assert_eq!(aggregate.max, 2.5);
    let mut empty = FloatAggregate::default();
    empty.merge(&FloatAggregate::default());
    assert!(empty.min.is_nan() && empty.max.is_nan());
    assert_eq!(empty.count, 0);
}

/// Empty, reversed and extreme ranges give the same answer from rollups as from a full scan.
#[test]
fn rollup_answers_equal_full_scan_answers_for_empty_reversed_and_extreme_ranges() {
    let with_rollups = open(&build(Profile::TimeSeries, Some(BUCKET_NANOS)));
    let scan_only = open(&build(Profile::TimeSeries, Some(BUCKET_NANOS * 7 + 3)));
    let samples = samples();
    let first = samples[0].at;
    let last = samples[ROWS as usize - 1].at;
    for (from, to) in [
        (first, first),
        (last, first),
        (i64::MIN, i64::MAX),
        (i64::MAX, i64::MIN),
        (i64::MIN, first + BUCKET_NANOS),
        (last - BUCKET_NANOS, i64::MAX),
    ] {
        for column in [CPU, TEMPERATURE] {
            let rolled = with_rollups
                .time_series_aggregate(column, from, to, BUCKET_NANOS)
                .unwrap();
            let scanned = scan_only.time_series_aggregate(column, from, to, BUCKET_NANOS).unwrap();
            assert_eq!(bits(rolled), bits(scanned), "column {column}, range {from}..{to}");
        }
    }
}

/// An edge-bucket scan that finds its granules by binary search over the granule directory adds the same values to
/// the same buckets, bit for bit, as a pass over every row would, for row ranges starting and ending at, between,
/// before and after granule bounds, empty and reversed row ranges, and both a whole and a cut time range.
#[test]
fn a_row_bounded_scan_matches_every_row_of_its_range() {
    let file = open(&build(Profile::TimeSeries, Some(BUCKET_NANOS)));
    let samples = samples();
    let granules = &file.footer().granules;
    assert!(granules.len() > 8);
    let mut bounds = vec![0, ROWS, u64::MAX];
    for index in [0, 1, granules.len() / 2, granules.len() - 1] {
        let granule = granules[index];
        let end = granule.first_row_ordinal + u64::from(granule.row_count);
        for bound in [granule.first_row_ordinal, end] {
            bounds.extend([bound.saturating_sub(1), bound, bound + 1]);
        }
    }
    bounds.sort_unstable();
    bounds.dedup();
    let cut = samples[100].at..samples[ROWS as usize - 100].at;
    for range in [i64::MIN..i64::MAX, cut] {
        for &start in &bounds {
            for &end in &bounds {
                let rows = start..end;
                let mut by_row: BTreeMap<i64, FloatAggregate> = BTreeMap::new();
                for (row, sample) in samples.iter().enumerate() {
                    if let Some(value) = sample.temperature
                        && rows.contains(&(row as u64))
                        && range.contains(&sample.at)
                    {
                        by_row.entry(sample.at.div_euclid(BUCKET_NANOS)).or_default().add(value);
                    }
                }
                let mut scanned = BTreeMap::new();
                file.scan_buckets(TEMPERATURE, &rows, &range, BUCKET_NANOS, &mut scanned)
                    .unwrap();
                let bits_of = |buckets: BTreeMap<i64, FloatAggregate>| -> Vec<(i64, (u64, u64, u64, u64))> {
                    buckets
                        .into_iter()
                        .map(|(bucket, aggregate)| (bucket, bits(aggregate)))
                        .collect()
                };
                assert_eq!(bits_of(scanned), bits_of(by_row), "rows {rows:?}, time {range:?}");
            }
        }
    }
}
