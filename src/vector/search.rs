//! Searches many segment files at once: prunes whole segments and granules by room, sender and time, scans the
//! coarse 1-bit column with the filter applied during the scan, re-scores the best candidates with the finest column
//! each file holds, and merges the results.

use super::constant::*;
use super::error::VectorError;
use super::model::{EmbeddingModel, SegmentManifest, VectorMetric};
use super::segment::{ColumnStats, SegmentReader, SegmentSource};
use crate::indexes::bitmap::{RoaringRangeBitmap, RowRange};

/// Which rows a search may return. Every set condition must hold; an unset one allows everything.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchFilter {
    pub room_ids: Option<Vec<String>>,
    pub senders: Option<Vec<String>>,
    /// Inclusive `(from, to)` on the `timestamp` column.
    pub time_range: Option<(i64, i64)>,
}

impl SearchFilter {
    fn is_empty(&self) -> bool {
        self.room_ids.is_none() && self.senders.is_none() && self.time_range.is_none()
    }
}

/// One vector search over many segments.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone)]
pub struct VectorSearch<'a> {
    pub filter: SearchFilter,
    pub k: usize,
    pub model: &'a EmbeddingModel,
    /// Candidates the coarse scan keeps per wanted result; [`DEFAULT_OVERSAMPLE`] is a good start.
    pub oversample: usize,
    pub query: &'a [f32],
}

/// One search result. Higher scores are closer; `segment` is the index into the slice that was searched.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub event_id: String,
    pub row: u64,
    pub score: f32,
    pub segment: usize,
}

/// Runs `search` over every segment and returns the best `k` rows across all of them, best first. Fails if any
/// segment was made by a different model or metric than `search.model`; pick out stale segments with
/// [`segments_to_reembed`] first.
pub fn search_segments<S: SegmentSource>(
    segments: &[SegmentReader<S>],
    search: &VectorSearch<'_>,
) -> Result<Vec<SearchHit>, VectorError> {
    let mut hits: Vec<(usize, super::model::ScoredRow)> = Vec::new();
    for (index, segment) in segments.iter().enumerate() {
        segment.manifest().model.require_same(search.model)?;
        let Some(filter) = row_filter(segment, &search.filter)? else {
            continue;
        };
        let representations = segment.representations(EMBEDDING_COLUMN_ID);
        let (Some(coarse), Some(fine)) = (representations.first(), representations.last()) else {
            continue;
        };
        let filter = (!search.filter.is_empty() || filter.count() < segment.row_count()).then_some(&filter);
        let scored = if coarse.column_id == fine.column_id {
            segment.top_k(fine.column_id, search.query, search.model, filter, search.k)?
        } else {
            let wanted = search.k.saturating_mul(search.oversample.max(1));
            let candidates = segment.top_k(coarse.column_id, search.query, search.model, filter, wanted)?;
            let rows: Vec<u64> = candidates.iter().map(|row| row.row).collect();
            segment.rescore(fine.column_id, search.query, search.model, &rows)?
        };
        hits.extend(scored.into_iter().map(|row| (index, row)));
    }
    hits.sort_by(|left, right| {
        right
            .1
            .score
            .total_cmp(&left.1.score)
            .then(left.0.cmp(&right.0))
            .then(left.1.row.cmp(&right.1.row))
    });
    hits.truncate(search.k);
    hits.into_iter()
        .map(|(index, row)| {
            let segment = segments.get(index).ok_or(VectorError::InvalidInput {
                rule: "segment index".to_owned(),
            })?;
            Ok(SearchHit {
                event_id: segment.event_ids(&[row.row])?.into_iter().next().unwrap_or_default(),
                row: row.row,
                score: row.score,
                segment: index,
            })
        })
        .collect()
}

/// The rows of `segment` that `filter` allows, or `None` when footer statistics rule out the whole segment. Granules
/// the statistics rule out are never decrypted.
fn row_filter<S: SegmentSource>(
    segment: &SegmentReader<S>,
    filter: &SearchFilter,
) -> Result<Option<RoaringRangeBitmap>, VectorError> {
    let mut ranges = Vec::new();
    let mut rows = Vec::new();
    for granule in 0..segment.granule_count() {
        let first = segment.granule_first_row(granule);
        let count = segment.granule_rows(granule);
        if count == 0 || !granule_may_match(segment, granule, filter) {
            continue;
        }
        if filter.is_empty() {
            ranges.push(RowRange {
                start: first,
                end: first + count as u64,
            });
            continue;
        }
        let rooms = load_strings(segment, ROOM_ID_COLUMN, granule, filter.room_ids.is_some())?;
        let senders = load_strings(segment, SENDER_COLUMN, granule, filter.senders.is_some())?;
        let times = match filter.time_range {
            Some(_) => Some(segment.integers(TIMESTAMP_COLUMN, granule)?),
            None => None,
        };
        for local in 0..count {
            let room_ok = matches(&rooms, filter.room_ids.as_deref(), local);
            let sender_ok = matches(&senders, filter.senders.as_deref(), local);
            let time_ok = match (filter.time_range, &times) {
                (Some((from, to)), Some(times)) => times.get(local).is_some_and(|time| (from..=to).contains(time)),
                _ => true,
            };
            if room_ok && sender_ok && time_ok {
                rows.push(first + local as u64);
            }
        }
    }
    let bitmap = if filter.is_empty() {
        RoaringRangeBitmap::from_ranges(ranges)
    } else {
        RoaringRangeBitmap::from_rows(rows)
    };
    Ok((!bitmap.is_empty()).then_some(bitmap))
}

fn load_strings<S: SegmentSource>(
    segment: &SegmentReader<S>,
    name: &str,
    granule: usize,
    wanted: bool,
) -> Result<Option<Vec<String>>, VectorError> {
    if !wanted {
        return Ok(None);
    }
    match segment.strings(name, granule) {
        Ok(values) => Ok(Some(values)),
        Err(VectorError::MissingColumn { .. }) => Ok(Some(Vec::new())),
        Err(error) => Err(error),
    }
}

fn matches(values: &Option<Vec<String>>, allowed: Option<&[String]>, local: usize) -> bool {
    match (values, allowed) {
        (Some(values), Some(allowed)) => values.get(local).is_some_and(|value| allowed.contains(value)),
        _ => true,
    }
}

fn granule_may_match<S: SegmentSource>(segment: &SegmentReader<S>, granule: usize, filter: &SearchFilter) -> bool {
    let strings_may_match = |name: &str, allowed: &Option<Vec<String>>| match allowed {
        None => true,
        Some(allowed) => match segment.column_stats(name, granule) {
            Some(ColumnStats::String { max, min }) => allowed.iter().any(|value| value >= min && value <= max),
            _ => false,
        },
    };
    let time_may_match = match filter.time_range {
        None => true,
        Some((from, to)) => match segment.column_stats(TIMESTAMP_COLUMN, granule) {
            Some(ColumnStats::Integer { max, min }) => *min <= to && *max >= from,
            _ => false,
        },
    };
    strings_may_match(ROOM_ID_COLUMN, &filter.room_ids)
        && strings_may_match(SENDER_COLUMN, &filter.senders)
        && time_may_match
}

/// The indexes of the `n` segments whose centroid is closest to `query` under `model`'s metric, closest first, so a
/// search over many segments can scan only the likeliest ones. Segments without a centroid can't be ruled out and
/// come first. Fails if any segment was made by a different model.
pub fn nearest_segments<S: SegmentSource>(
    segments: &[SegmentReader<S>],
    query: &[f32],
    model: &EmbeddingModel,
    n: usize,
) -> Result<Vec<usize>, VectorError> {
    let mut scored = Vec::with_capacity(segments.len());
    for (index, segment) in segments.iter().enumerate() {
        segment.manifest().model.require_same(model)?;
        let score = segment.centroid().map_or(f32::INFINITY, |centroid| {
            let pairs = centroid.iter().zip(query);
            match model.metric {
                VectorMetric::Cosine | VectorMetric::Dot => pairs.map(|(left, right)| left * right).sum(),
                VectorMetric::L2 => -pairs.map(|(left, right)| (left - right) * (left - right)).sum::<f32>(),
            }
        });
        scored.push((index, score));
    }
    scored.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
    Ok(scored.into_iter().take(n).map(|(index, _)| index).collect())
}

/// The keys of every segment built with a model other than `current`, so the caller re-embeds them when the
/// embedding model changes. Feed it the manifests the application keeps for its files.
pub fn segments_to_reembed<'a, K>(
    manifests: impl IntoIterator<Item = (K, &'a SegmentManifest)>,
    current: &EmbeddingModel,
) -> Vec<K> {
    manifests
        .into_iter()
        .filter(|(_, manifest)| manifest.model != *current)
        .map(|(key, _)| key)
        .collect()
}
