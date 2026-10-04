//! Reads one entity's events - a chat room, an account, a deal - back in sequence order, across published files and
//! the not-yet-published overlay, with deletes and corrections already applied.
//!
//! A scan finds the entity's granules from each file's footer, keeps the rows whose entity and `(epoch, sequence)`
//! point match, and merges them with the overlay's matching rows. Where the same point appears more than once - an
//! ingest file and the compacted file that replaced it, or a file and an overlay segment not yet evicted - the copy
//! from the newest file generation wins, and any published copy wins over the overlay. Rows the application deleted
//! are skipped, and an event the application corrected is served as its latest correction.
//!
//! See: hef-apis/spec.md

use super::event::{overlay_column, read_u64s};
use super::footer::SortDirection;
use super::reader::{HefFile, PayloadRead};
use crate::artifacts::overlay::{LiveOverlaySegment, LiveOverlayStore};
use crate::columns::column_ids;
use crate::deletes::{CorrectionMetadata, CorrectionType, DeletionVector};
use crate::error::{EntityScanError, FormatError};
use crate::events::variant::VariantValue;
use crate::events::{EventEnvelope, EventId, SequencePoint, TenantId};
use arrow_array::UInt64Array;
use hashbrown::HashMap;
use std::ops::RangeInclusive;

/// The deletes and corrections the application has recorded, read by an entity scan so it never serves a deleted or
/// superseded event.
///
/// Stored files are immutable, so deletes and corrections are published beside them rather than written into them;
/// the application keeps them in its own durable storage and backs this interface with it.
///
/// See: hef-deletes-and-corrections/spec.md
pub trait DeletesAndCorrections {
    /// The rows deleted from the stored file `file_id`, in that file's own row ordinals, or `None` when none are.
    fn deletion_vector(&self, tenant_id: TenantId, file_id: u128) -> Result<Option<DeletionVector>, EntityScanError>;

    /// True when the event at `point` was written as a correction of another event. A scan serves it in place of the
    /// event it corrects, never a second time at its own place.
    fn is_correction(&self, tenant_id: TenantId, point: SequencePoint) -> Result<bool, EntityScanError>;

    /// The newest correction recorded against `event_id`, or `None` when it was never corrected.
    fn latest_correction(
        &self,
        tenant_id: TenantId,
        event_id: EventId,
    ) -> Result<Option<CorrectionMetadata>, EntityScanError>;
}

/// One request for an entity's events: whose, over which stretch of sequence order, in which direction, and how many.
///
/// See: hef-apis/spec.md
#[derive(Debug, Clone)]
pub struct EntityScan {
    /// `Ascending` reads oldest first; `Descending` reads newest first, so a limit keeps the newest events.
    pub direction: SortDirection,
    pub entity_id_hash_high: u64,
    pub entity_id_hash_low: u64,
    /// At most this many events; `None` returns every match.
    pub limit: Option<usize>,
    /// Inclusive at both ends.
    pub sequence_range: RangeInclusive<SequencePoint>,
    pub tenant_id: TenantId,
}

/// One event an entity scan returns.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityEvent {
    pub envelope: EventEnvelope,
    pub payload: PayloadRead,
    /// Where the event sits in sequence order. A corrected event keeps the place of the event it corrects.
    pub point: SequencePoint,
}

/// Where one row is read from.
#[derive(Debug, Clone, Copy)]
enum RowSource {
    File {
        file: usize,
        granule_id: u32,
        row_in_granule: usize,
        row_ordinal: u64,
    },
    Overlay {
        row: usize,
        segment: usize,
    },
}

/// One row that matched the scan, before copies of the same point are collapsed.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    point: SequencePoint,
    /// Higher wins among copies of one point: `(1, generation)` for a published file, `(0, 0)` for the overlay.
    rank: (u8, u64),
    source: RowSource,
}

impl EntityScan {
    /// Reads the entity's events from `files` and `overlay`, in `direction` order, up to `limit` of them.
    ///
    /// Hand it the files of the snapshot being read - files of another tenant are ignored - and the overlay of
    /// fresher rows. Deleted rows are left out and corrected events come back as their latest correction, both as
    /// recorded in `deletes`: a retraction drops the event, a replacement serves the correcting event, and an amendment
    /// serves the original with the amendment's payload fields laid over its own.
    pub fn run(
        &self,
        files: &[&HefFile],
        overlay: &LiveOverlayStore,
        deletes: &dyn DeletesAndCorrections,
    ) -> Result<Vec<EntityEvent>, EntityScanError> {
        let files: Vec<&HefFile> = files
            .iter()
            .copied()
            .filter(|file| file.header().tenant_id == self.tenant_id)
            .collect();
        let segments: Vec<&LiveOverlaySegment> = overlay
            .segments()
            .filter(|segment| segment.meta.tenant_id == self.tenant_id)
            .collect();
        let mut candidates = Vec::new();
        for (index, file) in files.iter().enumerate() {
            self.file_candidates(index, file, &mut candidates)?;
        }
        for (index, segment) in segments.iter().enumerate() {
            self.overlay_candidates(index, segment, &mut candidates)?;
        }
        candidates.sort_by(|a, b| a.point.cmp(&b.point).then(b.rank.cmp(&a.rank)));
        candidates.dedup_by(|later, earlier| later.point == earlier.point);
        if self.direction == SortDirection::Descending {
            candidates.reverse();
        }

        let deletion_vectors = files
            .iter()
            .map(|file| deletes.deletion_vector(self.tenant_id, file.header().file_id))
            .collect::<Result<Vec<_>, _>>()?;
        let mut reader = EventReader {
            envelopes: HashMap::new(),
            files: &files,
            segments: &segments,
        };
        let mut events = Vec::new();
        for candidate in candidates {
            if self.limit.is_some_and(|limit| events.len() >= limit) {
                break;
            }
            if let RowSource::File { file, row_ordinal, .. } = candidate.source
                && deletion_vectors
                    .get(file)
                    .and_then(Option::as_ref)
                    .is_some_and(|vector| vector.is_deleted(row_ordinal))
            {
                continue;
            }
            if deletes.is_correction(self.tenant_id, candidate.point)? {
                continue;
            }
            let (envelope, payload) = reader.read(candidate.source)?;
            let served = match deletes.latest_correction(self.tenant_id, envelope.event_id)? {
                None => Some((envelope, payload)),
                Some(correction) => {
                    let at = SequencePoint {
                        epoch: correction.correction_epoch,
                        sequence: correction.correction_sequence,
                    };
                    match correction.correction_type {
                        CorrectionType::Amendment => {
                            let (_, changes) = reader.read(locate(at, &files, &segments)?)?;
                            Some((envelope, amend(payload, changes)))
                        }
                        CorrectionType::Replacement => Some(reader.read(locate(at, &files, &segments)?)?),
                        CorrectionType::Retraction => None,
                    }
                }
            };
            if let Some((envelope, payload)) = served {
                events.push(EntityEvent {
                    envelope,
                    payload,
                    point: candidate.point,
                });
            }
        }
        Ok(events)
    }

    /// Adds the rows of `file` that belong to the entity and fall in the range.
    fn file_candidates(&self, index: usize, file: &HefFile, out: &mut Vec<Candidate>) -> Result<(), FormatError> {
        let from = *self.sequence_range.start();
        let to = *self.sequence_range.end();
        let generation = file.header().generation_id;
        for granule in file.entity_granules(self.entity_id_hash_low, self.entity_id_hash_high, from, to) {
            let rows = granule.row_count as usize;
            let epochs = read_u64s(file, column_ids::EPOCH, granule.granule_id, rows)?;
            let sequences = read_u64s(file, column_ids::SEQUENCE, granule.granule_id, rows)?;
            let lows = read_u64s(file, column_ids::ENTITY_ID_HASH_LOW, granule.granule_id, rows)?;
            let highs = read_u64s(file, column_ids::ENTITY_ID_HASH_HIGH, granule.granule_id, rows)?;
            let keys = epochs.iter().zip(&sequences).zip(lows.iter().zip(&highs));
            for (row_in_granule, ((&epoch, &sequence), (&low, &high))) in keys.enumerate() {
                let point = SequencePoint { epoch, sequence };
                if low == self.entity_id_hash_low
                    && high == self.entity_id_hash_high
                    && self.sequence_range.contains(&point)
                {
                    out.push(Candidate {
                        point,
                        rank: (1, generation),
                        source: RowSource::File {
                            file: index,
                            granule_id: granule.granule_id,
                            row_in_granule,
                            row_ordinal: granule.first_row_ordinal + row_in_granule as u64,
                        },
                    });
                }
            }
        }
        Ok(())
    }

    /// Adds the rows of overlay `segment` that belong to the entity and fall in the range.
    fn overlay_candidates(
        &self,
        index: usize,
        segment: &LiveOverlaySegment,
        out: &mut Vec<Candidate>,
    ) -> Result<(), FormatError> {
        let meta = &segment.meta;
        let first = SequencePoint {
            epoch: meta.epoch,
            sequence: meta.first_sequence,
        };
        let last = SequencePoint {
            epoch: meta.epoch,
            sequence: meta.last_sequence,
        };
        if last < *self.sequence_range.start() || *self.sequence_range.end() < first {
            return Ok(());
        }
        let epochs = overlay_column::<UInt64Array>(segment, "epoch")?.values();
        let sequences = overlay_column::<UInt64Array>(segment, "sequence")?.values();
        let lows = overlay_column::<UInt64Array>(segment, "entity_id_hash_low")?.values();
        let highs = overlay_column::<UInt64Array>(segment, "entity_id_hash_high")?.values();
        let keys = epochs.iter().zip(sequences.iter()).zip(lows.iter().zip(highs.iter()));
        for (row, ((&epoch, &sequence), (&low, &high))) in keys.enumerate() {
            let point = SequencePoint { epoch, sequence };
            if low == self.entity_id_hash_low
                && high == self.entity_id_hash_high
                && self.sequence_range.contains(&point)
            {
                out.push(Candidate {
                    point,
                    rank: (0, 0),
                    source: RowSource::Overlay { row, segment: index },
                });
            }
        }
        Ok(())
    }
}

/// Reads rows as events, decoding each file granule's envelopes once however many of its rows are read.
struct EventReader<'a> {
    envelopes: HashMap<(usize, u32), Vec<EventEnvelope>>,
    files: &'a [&'a HefFile],
    segments: &'a [&'a LiveOverlaySegment],
}

impl EventReader<'_> {
    fn read(&mut self, source: RowSource) -> Result<(EventEnvelope, PayloadRead), FormatError> {
        match source {
            RowSource::File {
                file: index,
                granule_id,
                row_in_granule,
                row_ordinal,
            } => {
                let file = self
                    .files
                    .get(index)
                    .copied()
                    .ok_or(FormatError::RefOutOfRange { what: "scanned file" })?;
                if !self.envelopes.contains_key(&(index, granule_id)) {
                    self.envelopes
                        .insert((index, granule_id), file.granule_envelopes(granule_id)?);
                }
                let envelope = self
                    .envelopes
                    .get(&(index, granule_id))
                    .and_then(|envelopes| envelopes.get(row_in_granule))
                    .cloned()
                    .ok_or(FormatError::RefOutOfRange { what: "row in granule" })?;
                Ok((envelope, file.payload(row_ordinal)?))
            }
            RowSource::Overlay { row, segment } => self
                .segments
                .get(segment)
                .copied()
                .ok_or(FormatError::RefOutOfRange {
                    what: "overlay segment",
                })?
                .event(row),
        }
    }
}

/// Where the event at `point` is stored: the newest file generation holding it, else the overlay. The point need not
/// belong to the scanned entity or range - a correction can sit anywhere in sequence order.
fn locate(
    point: SequencePoint,
    files: &[&HefFile],
    segments: &[&LiveOverlaySegment],
) -> Result<RowSource, EntityScanError> {
    let mut newest: Option<(u64, RowSource)> = None;
    for (index, file) in files.iter().enumerate() {
        let generation = file.header().generation_id;
        for granule in file.granules_for_sequence(point.epoch, point.sequence, point.sequence) {
            let rows = granule.row_count as usize;
            let epochs = read_u64s(file, column_ids::EPOCH, granule.granule_id, rows)?;
            let sequences = read_u64s(file, column_ids::SEQUENCE, granule.granule_id, rows)?;
            let found = epochs
                .iter()
                .zip(&sequences)
                .position(|(&epoch, &sequence)| epoch == point.epoch && sequence == point.sequence);
            if let Some(row_in_granule) = found
                && newest.is_none_or(|(newest, _)| generation > newest)
            {
                newest = Some((
                    generation,
                    RowSource::File {
                        file: index,
                        granule_id: granule.granule_id,
                        row_in_granule,
                        row_ordinal: granule.first_row_ordinal + row_in_granule as u64,
                    },
                ));
            }
        }
    }
    if let Some((_, source)) = newest {
        return Ok(source);
    }
    segments
        .iter()
        .enumerate()
        .find(|(_, segment)| {
            segment.meta.epoch == point.epoch
                && segment.meta.first_sequence <= point.sequence
                && point.sequence <= segment.meta.last_sequence
        })
        .map(|(index, segment)| RowSource::Overlay {
            row: (point.sequence - segment.meta.first_sequence) as usize,
            segment: index,
        })
        .ok_or(EntityScanError::CorrectionNotFound {
            epoch: point.epoch,
            sequence: point.sequence,
        })
}

/// An amendment laid over the event it amends: payload fields the amendment carries replace the original's, the rest
/// stay. An amendment whose payload is not an object replaces the payload whole; one with no payload changes nothing.
fn amend(original: PayloadRead, amendment: PayloadRead) -> PayloadRead {
    match (original, amendment) {
        (PayloadRead::Value(VariantValue::Object(mut fields)), PayloadRead::Value(VariantValue::Object(changed))) => {
            fields.extend(changed);
            PayloadRead::Value(VariantValue::Object(fields))
        }
        (original, PayloadRead::None) => original,
        (_, amendment) => amendment,
    }
}

#[cfg(test)]
#[path = "test/entity_scan.rs"]
mod tests;
