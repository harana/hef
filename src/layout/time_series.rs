//! Answers the count, sum, minimum and maximum of a metric over a time range from a time-series file, mostly from
//! small per-interval summaries instead of the rows themselves.
//!
//! A time-series file ([`Profile::TimeSeries`]) holds metric readings (usage, stats, the inputs of Pulse views) with
//! its rows in `occurred_at` order. Its footer may carry rollups: for every fixed-width time bucket that holds rows,
//! the bucket's row range and, per float column, the count, sum, minimum and maximum of the bucket's values. An
//! aggregate over a time range takes each bucket the range covers whole from the rollups and reads rows only for the
//! buckets the range cuts through, at most one at each end. Rollups are droppable: a reader without them reads every
//! row in the range and gets the same answer bit for bit.
//!
//! See: hef-file-layout/spec.md

use super::Profile;
use super::optional_features;
use super::reader::HefFile;
use crate::columns::column_ids;
use crate::encoding::ColumnData;
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use std::collections::BTreeMap;
use std::ops::Range;

/// The count, sum, minimum and maximum of some float values.
///
/// `count` counts every value, NaN included, and `sum` adds them all, so one NaN makes the sum NaN. `min` and `max`
/// skip NaN and order the rest with `-0.0` below `0.0`; they are NaN when no other value was seen. Minimum and maximum
/// therefore come out the same whatever order values arrive in, while a sum depends on the order of its additions,
/// which is why [`HefFile::time_series_aggregate`] fixes that order.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FloatAggregate {
    pub count: u64,
    pub max: f64,
    pub min: f64,
    pub sum: f64,
}

impl Default for FloatAggregate {
    fn default() -> Self {
        Self {
            count: 0,
            max: f64::NAN,
            min: f64::NAN,
            sum: 0.0,
        }
    }
}

impl FloatAggregate {
    /// Takes one more value into the aggregate.
    pub fn add(&mut self, value: f64) {
        self.count += 1;
        self.sum += value;
        self.widen(value, value);
    }

    /// Takes another aggregate's values into this one: counts and sums add, the extremes widen.
    pub fn merge(&mut self, other: &FloatAggregate) {
        self.count += other.count;
        self.sum += other.sum;
        self.widen(other.min, other.max);
    }

    fn widen(&mut self, min: f64, max: f64) {
        if !min.is_nan() && (self.min.is_nan() || min.total_cmp(&self.min).is_lt()) {
            self.min = min;
        }
        if !max.is_nan() && (self.max.is_nan() || max.total_cmp(&self.max).is_gt()) {
            self.max = max;
        }
    }
}

/// One time bucket of a time-series file's rollups: the rows whose `occurred_at` falls in
/// `[bucket * bucket_nanos, (bucket + 1) * bucket_nanos)`, which sit side by side because the rows are in time order,
/// and one aggregate of their values per rolled-up column.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, PartialEq)]
pub struct RollupBucket {
    /// One per entry of [`TimeSeriesRollups::column_ids`], in the same order. Each adds its values in row order.
    pub aggregates: Vec<FloatAggregate>,
    /// The bucket's index: its start time divided by the bucket width, rounded down.
    pub bucket: i64,
    pub first_row_ordinal: u64,
    /// Never zero: only buckets that hold rows are stored.
    pub row_count: u64,
}

/// A time-series file's per-bucket summaries of its float columns, stored in the footer.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, PartialEq)]
pub struct TimeSeriesRollups {
    /// The width of every bucket, in nanoseconds; positive. Chosen by the writer's configuration.
    pub bucket_nanos: i64,
    /// Ascending by bucket, and together covering every row of the file in order.
    pub buckets: Vec<RollupBucket>,
    /// The float columns summarised, ascending.
    pub column_ids: Vec<u32>,
}

impl TimeSeriesRollups {
    /// Serializes the rollups for the footer's time-series rollups section. Pairs with [`Self::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_i64(self.bucket_nanos);
        out.put_u32(self.column_ids.len() as u32);
        for column_id in &self.column_ids {
            out.put_u32(*column_id);
        }
        out.put_u32(self.buckets.len() as u32);
        for bucket in &self.buckets {
            out.put_i64(bucket.bucket);
            out.put_u64(bucket.first_row_ordinal);
            out.put_u64(bucket.row_count);
            for aggregate in &bucket.aggregates {
                out.put_u64(aggregate.count);
                out.put_u64(aggregate.sum.to_bits());
                out.put_u64(aggregate.min.to_bits());
                out.put_u64(aggregate.max.to_bits());
            }
        }
        out.into_bytes()
    }

    /// Rebuilds rollups from [`Self::encode`] output. Refuses a width that is not positive, columns that are not
    /// strictly ascending, buckets that are not strictly ascending or leave a gap or overlap between their rows, an
    /// empty bucket, a count above its bucket's rows, and truncated or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let malformed = FormatError::Structural {
            rule: "malformed time-series rollups",
        };
        let mut reader = Reader::new(bytes);
        let bucket_nanos = reader.i64("rollup bucket width")?;
        let column_count = reader.u32("rollup column count")? as usize;
        if bucket_nanos <= 0 || column_count > reader.remaining() / 4 {
            return Err(malformed);
        }
        let column_ids = reader.u32_vec(column_count, "rollup column id")?;
        if column_ids.iter().zip(column_ids.iter().skip(1)).any(|(a, b)| a >= b) {
            return Err(malformed);
        }
        let bucket_count = reader.u32("rollup bucket count")? as usize;
        let bucket_bytes = 24 + 32 * column_count;
        if bucket_count > reader.remaining() / bucket_bytes {
            return Err(malformed);
        }
        let mut buckets: Vec<RollupBucket> = Vec::with_capacity(bucket_count);
        for _ in 0..bucket_count {
            let bucket = reader.i64("rollup bucket")?;
            let first_row_ordinal = reader.u64("rollup first row")?;
            let row_count = reader.u64("rollup row count")?;
            let follows = match buckets.last() {
                Some(previous) => {
                    previous.bucket < bucket
                        && previous.first_row_ordinal.checked_add(previous.row_count) == Some(first_row_ordinal)
                }
                None => first_row_ordinal == 0,
            };
            if !follows || row_count == 0 || first_row_ordinal.checked_add(row_count).is_none() {
                return Err(malformed);
            }
            let mut aggregates = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                let aggregate = FloatAggregate {
                    count: reader.u64("rollup count")?,
                    sum: f64::from_bits(reader.u64("rollup sum")?),
                    min: f64::from_bits(reader.u64("rollup min")?),
                    max: f64::from_bits(reader.u64("rollup max")?),
                };
                if aggregate.count > row_count {
                    return Err(malformed);
                }
                aggregates.push(aggregate);
            }
            buckets.push(RollupBucket {
                aggregates,
                bucket,
                first_row_ordinal,
                row_count,
            });
        }
        if reader.remaining() != 0 {
            return Err(malformed);
        }
        Ok(Self {
            bucket_nanos,
            buckets,
            column_ids,
        })
    }

    /// The number of rows the buckets cover.
    pub fn row_count(&self) -> u64 {
        self.buckets
            .last()
            .map_or(0, |bucket| bucket.first_row_ordinal.saturating_add(bucket.row_count))
    }
}

impl HefFile {
    /// This file's rollups, or `None` when it carries none or this reader may not use them.
    pub fn time_series_rollups(&self) -> Option<&TimeSeriesRollups> {
        self.footer()
            .time_series_rollups
            .as_ref()
            .filter(|_| self.usable_optional_features() & optional_features::TIME_SERIES_ROLLUPS != 0)
    }

    /// The count, sum, minimum and maximum of float column `column_id` over the rows whose `occurred_at` falls in
    /// `[from_nanos, to_nanos)`. Rows where the column holds no value are skipped.
    ///
    /// The values are grouped into `bucket_nanos`-wide time buckets: each bucket adds its values in row order, then
    /// the bucket totals are added in time order. Pass the width your view rolls up by. When it equals the file's
    /// rollup width, every bucket the range covers whole is answered from the footer and rows are read only for the
    /// buckets cut by the range's ends; otherwise, or without rollups, every row in the range is read. Both ways add
    /// the same values in the same order, so the answer is identical bit for bit. Refuses a file that is not a
    /// time-series file and a width that is not positive.
    pub fn time_series_aggregate(
        &self,
        column_id: u32,
        from_nanos: i64,
        to_nanos: i64,
        bucket_nanos: i64,
    ) -> Result<FloatAggregate, FormatError> {
        if self.footer().profile != Profile::TimeSeries {
            return Err(FormatError::Structural {
                rule: "a time-series aggregate needs a time-series file",
            });
        }
        if bucket_nanos <= 0 {
            return Err(FormatError::Structural {
                rule: "a time-series bucket width is positive",
            });
        }
        let range = from_nanos..to_nanos;
        let mut buckets: BTreeMap<i64, FloatAggregate> = BTreeMap::new();
        let rollups = self.time_series_rollups().and_then(|rollups| {
            let column = rollups.column_ids.binary_search(&column_id).ok()?;
            (rollups.bucket_nanos == bucket_nanos).then_some((rollups, column))
        });
        match rollups {
            Some((rollups, column)) => {
                let width = i128::from(bucket_nanos);
                let (from, to) = (i128::from(from_nanos), i128::from(to_nanos));
                for bucket in &rollups.buckets {
                    let start = i128::from(bucket.bucket) * width;
                    let end = start + width;
                    if end <= from || to <= start {
                        continue;
                    }
                    if from <= start && end <= to {
                        let aggregate = bucket.aggregates.get(column).copied().unwrap_or_default();
                        buckets.insert(bucket.bucket, aggregate);
                    } else {
                        let rows = bucket.first_row_ordinal..bucket.first_row_ordinal + bucket.row_count;
                        self.scan_buckets(column_id, &rows, &range, bucket_nanos, &mut buckets)?;
                    }
                }
            }
            None => self.scan_buckets(column_id, &(0..u64::MAX), &range, bucket_nanos, &mut buckets)?,
        }
        let mut total = FloatAggregate::default();
        for aggregate in buckets.values() {
            total.merge(aggregate);
        }
        Ok(total)
    }

    /// Adds the values of `column_id` in the rows of `rows` whose `occurred_at` falls in `range` to their buckets,
    /// reading only the granules that overlap both.
    fn scan_buckets(
        &self,
        column_id: u32,
        rows: &Range<u64>,
        range: &Range<i64>,
        bucket_nanos: i64,
        buckets: &mut BTreeMap<i64, FloatAggregate>,
    ) -> Result<(), FormatError> {
        for granule in &self.footer().granules {
            let first = granule.first_row_ordinal;
            let granule_rows = first..first + u64::from(granule.row_count);
            if granule_rows.end <= rows.start
                || rows.end <= granule_rows.start
                || granule.max_occurred_at_physical < range.start
                || range.end <= granule.min_occurred_at_physical
            {
                continue;
            }
            let occurred = match self.read_column(column_ids::OCCURRED_AT, granule.granule_id)?.data {
                ColumnData::I64(values) => values,
                _ => {
                    return Err(FormatError::Structural {
                        rule: "occurred_at is an integer column",
                    });
                }
            };
            let read = self.read_column(column_id, granule.granule_id)?;
            let ColumnData::F64(values) = read.data else {
                return Err(FormatError::Structural {
                    rule: "a time-series aggregate reads a float column",
                });
            };
            let mut values = values.into_iter();
            for (row, at) in occurred.into_iter().enumerate() {
                let present = read.presence.is_empty()
                    || read
                        .presence
                        .get(row / 8)
                        .is_some_and(|byte| byte & (1 << (row % 8)) != 0);
                let value = if present { values.next() } else { None };
                if let Some(value) = value
                    && rows.contains(&(first + row as u64))
                    && range.contains(&at)
                {
                    buckets.entry(at.div_euclid(bucket_nanos)).or_default().add(value);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/time_series.rs"]
mod tests;
