//! Reads stored rows back as whole events - the envelope every event carries plus its payload - from a published file
//! or from the not-yet-published overlay, and finds the granules that can hold one entity's events.
//!
//! See: hef-apis/spec.md

use super::footer::GranuleEntry;
use super::reader::{ColumnRead, HefFile, PayloadRead};
use crate::artifacts::batch::{PAYLOAD_FLAG_EXTERNAL_REF, decode_variant_dictionary};
use crate::artifacts::overlay::LiveOverlaySegment;
use crate::columns::column_ids;
use crate::encoding::{ColumnData, StringColumn};
use crate::error::FormatError;
use crate::events::variant::VariantRef;
use crate::events::{EventEnvelope, EventFlags, EventId, SequencePoint, StreamId, TimestampValue};
use crate::file::bytes::slice;
use arrow_array::*;
use hashbrown::HashMap;
use uuid::Uuid;

impl HefFile {
    /// The envelope of the event stored at `row_ordinal` - its identity, times, source, type, entity, and flags -
    /// exactly as it was written.
    pub fn envelope(&self, row_ordinal: u64) -> Result<EventEnvelope, FormatError> {
        let granule = self.granule_for_row(row_ordinal)?;
        let index = row_ordinal - granule.first_row_ordinal;
        EnvelopeColumns::read(self, granule.granule_id)?.envelope(self, index as usize)
    }

    /// The event stored at `row_ordinal`: its envelope and its payload, the two halves the writer was handed.
    pub fn event(&self, row_ordinal: u64) -> Result<(EventEnvelope, PayloadRead), FormatError> {
        Ok((self.envelope(row_ordinal)?, self.payload(row_ordinal)?))
    }

    /// The envelopes of every event in one granule, in row order. Reading a run of rows from the same granule through
    /// this decodes each envelope column once instead of once per row.
    pub fn granule_envelopes(&self, granule_id: u32) -> Result<Vec<EventEnvelope>, FormatError> {
        let columns = EnvelopeColumns::read(self, granule_id)?;
        (0..columns.rows).map(|row| columns.envelope(self, row)).collect()
    }

    /// The granules of this file that can hold events of the entity whose identity hashes are given, at or between
    /// `from` and `to` in sequence order, in file order.
    ///
    /// Answered from the footer alone: each granule's identity-hash filter and the min/max of its two entity hash
    /// columns rule out granules that hold no row of the entity, and the sequence bounds of the granules holding only
    /// that entity rule out its rows outside the range. In an entity projection, where an entity's rows sit side by
    /// side, a short range of one entity's history comes back as one or two granules. The answer may include a granule
    /// with no matching row, never omit one that has one, so the caller still compares each row.
    pub fn entity_granules(
        &self,
        entity_id_hash_low: u64,
        entity_id_hash_high: u64,
        from: SequencePoint,
        to: SequencePoint,
    ) -> Vec<&GranuleEntry> {
        let bounds: HashMap<(u32, u32), (i128, i128)> = self
            .footer()
            .page_stats
            .iter()
            .filter(|stats| {
                stats.column_id == column_ids::ENTITY_ID_HASH_LOW || stats.column_id == column_ids::ENTITY_ID_HASH_HIGH
            })
            .filter_map(|stats| Some(((stats.column_id, stats.granule_id), (stats.min_i128?, stats.max_i128?))))
            .collect();
        // Per column: whether the granule's min/max admits the value, and whether it holds nothing but the value. A
        // granule without stats admits it and is not known to hold only it.
        let admits = |column_id: u32, granule_id: u32, value: u64| match bounds.get(&(column_id, granule_id)) {
            Some(&(min, max)) => {
                let value = i128::from(value);
                (min <= value && value <= max, min == value && max == value)
            }
            None => (true, false),
        };
        // Each candidate carries its granule, whether it holds only the entity, and the upper bound on the entity's
        // rows in it, tightened below by the reverse pass.
        let mut candidates: Vec<(&GranuleEntry, bool, SequencePoint)> = self
            .granules_for_entity_hash(entity_id_hash_low)
            .into_iter()
            .filter_map(|granule| {
                let (low_admits, low_only) =
                    admits(column_ids::ENTITY_ID_HASH_LOW, granule.granule_id, entity_id_hash_low);
                let (high_admits, high_only) =
                    admits(column_ids::ENTITY_ID_HASH_HIGH, granule.granule_id, entity_id_hash_high);
                (low_admits && high_admits).then_some((granule, low_only && high_only, last_point(granule)))
            })
            .collect();
        // Whether a file is in `(epoch, sequence)` order or entity order, one entity's rows run through it in
        // `(epoch, sequence)` order. So the entity's rows in a mixed granule come after every row of the last
        // granule before it that holds only the entity, and before every row of the next such granule after it. A
        // reverse pass carries the next such granule's first point, a forward pass the previous one's last point.
        let mut next_first = None;
        for (granule, only_entity, highest) in candidates.iter_mut().rev() {
            if *only_entity {
                next_first = Some(first_point(granule));
            } else if let Some(next_first) = next_first {
                *highest = (*highest).min(next_first);
            }
        }
        let mut previous_last = None;
        candidates
            .into_iter()
            .filter(|&(granule, only_entity, highest)| {
                let mut lowest = first_point(granule);
                if only_entity {
                    previous_last = Some(last_point(granule));
                } else if let Some(previous_last) = previous_last {
                    lowest = lowest.max(previous_last);
                }
                lowest <= highest && lowest <= to && from <= highest
            })
            .map(|(granule, _, _)| granule)
            .collect()
    }

    /// The granule holding `row_ordinal`.
    pub(crate) fn granule_for_row(&self, row_ordinal: u64) -> Result<&GranuleEntry, FormatError> {
        let granules = &self.footer().granules;
        let index = granules.partition_point(|granule| granule.first_row_ordinal <= row_ordinal);
        index
            .checked_sub(1)
            .and_then(|index| granules.get(index))
            .filter(|granule| row_ordinal < granule.first_row_ordinal + u64::from(granule.row_count))
            .ok_or(FormatError::RefOutOfRange { what: "row ordinal" })
    }
}

impl LiveOverlaySegment {
    /// The event at `row` of this not-yet-published segment: its envelope and its payload, read the same way as from a
    /// published file.
    pub fn event(&self, row: usize) -> Result<(EventEnvelope, PayloadRead), FormatError> {
        if row >= self.batch.num_rows() {
            return Err(FormatError::RefOutOfRange { what: "overlay row" });
        }
        let u64_at = |name| overlay_column::<UInt64Array>(self, name).map(|column| column.value(row));
        let u32_at = |name| overlay_column::<UInt32Array>(self, name).map(|column| column.value(row));
        let time_at = |name| {
            overlay_column::<TimestampNanosecondArray>(self, name)
                .map(|column| TimestampValue::from_physical_nanos(column.value(row)))
        };
        let text_at = |name| {
            overlay_column::<StringArray>(self, name)
                .map(|column| column.is_valid(row).then(|| column.value(row).to_owned()))
        };
        let required_text_at = |name| {
            text_at(name)?.ok_or(FormatError::Structural {
                rule: "LiveOverlay required string column holds a null",
            })
        };
        let event_id: [u8; 16] = overlay_column::<FixedSizeBinaryArray>(self, "event_id")?
            .value(row)
            .try_into()
            .map_err(|_| FormatError::Structural {
                rule: "event_id must be 16 bytes",
            })?;
        let envelope = EventEnvelope {
            account_id: text_at("account_id")?,
            account_id_hash_low: u64_at("account_id_hash_low")?,
            actor_id: text_at("actor_id")?,
            actor_id_hash_low: u64_at("actor_id_hash_low")?,
            dedupe_hash_high: u64_at("dedupe_hash_high")?,
            dedupe_hash_low: u64_at("dedupe_hash_low")?,
            entity_id: text_at("entity_id")?,
            entity_id_hash_high: u64_at("entity_id_hash_high")?,
            entity_id_hash_low: u64_at("entity_id_hash_low")?,
            entity_type: required_text_at("entity_type")?,
            event_id: EventId::from_uuid(Uuid::from_u128(u128::from_le_bytes(event_id))),
            event_type: required_text_at("event_type")?,
            flags: EventFlags(u32_at("flags")?),
            ingested_at: time_at("ingested_at")?,
            occurred_at: time_at("occurred_at")?,
            schema_version: u32_at("schema_version")?,
            source: required_text_at("source")?,
            stream_id: StreamId(u64_at("stream_id")?),
            stream_sequence: u64_at("stream_sequence")?,
            tenant_id: self.meta.tenant_id,
            trace_id_hash_low: u64_at("trace_id_hash_low")?,
        };
        // payload_ref = (payload_len << 32) | payload_offset into this segment's arena; a zero length is no payload.
        let payload_ref = u64_at("payload_ref")?;
        let len = (payload_ref >> 32) as usize;
        let payload = if len == 0 {
            PayloadRead::None
        } else {
            let bytes = slice(
                &self.meta.payload_arena,
                (payload_ref & 0xFFFF_FFFF) as usize,
                len,
                "overlay payload",
            )?;
            if u32_at("payload_flags")? & PAYLOAD_FLAG_EXTERNAL_REF != 0 {
                PayloadRead::External(String::from_utf8(bytes.to_vec()).map_err(|_| FormatError::InvalidUtf8 {
                    what: "overlay external payload reference",
                })?)
            } else {
                let dictionary = decode_variant_dictionary(&self.meta.variant_dictionary)?;
                PayloadRead::Value(VariantRef::new(bytes).decode(&dictionary)?)
            }
        };
        Ok((envelope, payload))
    }
}

/// The lowest `(epoch, sequence)` point a granule holds.
fn first_point(granule: &GranuleEntry) -> SequencePoint {
    SequencePoint {
        epoch: granule.first_epoch,
        sequence: granule.first_sequence,
    }
}

/// The highest `(epoch, sequence)` point a granule holds.
fn last_point(granule: &GranuleEntry) -> SequencePoint {
    SequencePoint {
        epoch: granule.last_epoch,
        sequence: granule.last_sequence,
    }
}

/// One named column of an overlay segment's batch, as its v1 Arrow type.
pub(crate) fn overlay_column<'a, T: Array + 'static>(
    segment: &'a LiveOverlaySegment,
    name: &'static str,
) -> Result<&'a T, FormatError> {
    segment
        .batch
        .column_by_name(name)
        .and_then(|column| column.as_any().downcast_ref::<T>())
        .ok_or(FormatError::Structural {
            rule: "LiveOverlay batch is missing a v1 column",
        })
}

/// One granule's block of a `u64` column, one value per row.
pub(crate) fn read_u64s(file: &HefFile, column_id: u32, granule_id: u32, rows: usize) -> Result<Vec<u64>, FormatError> {
    match file.read_column(column_id, granule_id)?.data {
        ColumnData::U64(values) if values.len() == rows => Ok(values),
        _ => Err(FormatError::Structural {
            rule: "an envelope column holds one u64 per row",
        }),
    }
}

fn read_i64s(file: &HefFile, column_id: u32, granule_id: u32, rows: usize) -> Result<Vec<i64>, FormatError> {
    match file.read_column(column_id, granule_id)?.data {
        ColumnData::I64(values) if values.len() == rows => Ok(values),
        _ => Err(FormatError::Structural {
            rule: "an envelope column holds one i64 per row",
        }),
    }
}

fn read_u128s(file: &HefFile, column_id: u32, granule_id: u32, rows: usize) -> Result<Vec<u128>, FormatError> {
    match file.read_column(column_id, granule_id)?.data {
        ColumnData::U128(values) if values.len() == rows => Ok(values),
        _ => Err(FormatError::Structural {
            rule: "an envelope column holds one u128 per row",
        }),
    }
}

/// One granule's block of a nullable string column as stored: the presence bitmap that spreads a block storing only
/// its present values back out over the rows, and the values.
fn read_strings(file: &HefFile, column_id: u32, granule_id: u32) -> Result<(Vec<u8>, StringColumn), FormatError> {
    let ColumnRead { data, presence } = file.read_column(column_id, granule_id)?;
    let ColumnData::Strings(values) = data else {
        return Err(FormatError::Structural {
            rule: "an envelope identity column holds strings",
        });
    };
    Ok((presence, values))
}

/// The value at `row` of a nullable string column read by [`read_strings`] for a granule of `rows` rows.
fn string_at((presence, values): &(Vec<u8>, StringColumn), rows: usize, row: usize) -> Option<String> {
    let present = |row: usize| presence.get(row / 8).is_some_and(|byte| byte & (1 << (row % 8)) != 0);
    let index = if values.len() == rows {
        row
    } else if present(row) {
        (0..row).filter(|&row| present(row)).count()
    } else {
        return None;
    };
    values.get(index).flatten().map(str::to_owned)
}

/// The file dictionary entry named by row `row` of a dictionary-id column.
fn dictionary_entry<'a>(values: &'a [String], ids: &[u64], row: usize) -> Result<&'a str, FormatError> {
    let id = ids.get(row).copied().unwrap_or_default();
    values
        .get(id as usize)
        .map(String::as_str)
        .ok_or(FormatError::RefOutOfRange {
            what: "file dictionary id",
        })
}

/// Every envelope column of one granule, decoded and checked once, so one row's envelope or all of them can be built
/// from it.
///
/// See: hef-apis/spec.md
struct EnvelopeColumns {
    account_id: (Vec<u8>, StringColumn),
    account_id_hash_low: Vec<u64>,
    actor_id: (Vec<u8>, StringColumn),
    actor_id_hash_low: Vec<u64>,
    dedupe_hash_high: Vec<u64>,
    dedupe_hash_low: Vec<u64>,
    entity_id: (Vec<u8>, StringColumn),
    entity_id_hash_high: Vec<u64>,
    entity_id_hash_low: Vec<u64>,
    entity_type_id: Vec<u64>,
    event_id: Vec<u128>,
    event_type_id: Vec<u64>,
    flags: Vec<u64>,
    ingested_at: Vec<i64>,
    occurred_at: Vec<i64>,
    rows: usize,
    schema_version: Vec<u64>,
    source_id: Vec<u64>,
    stream_id: Vec<u64>,
    stream_sequence: Vec<u64>,
    trace_id_hash_low: Vec<u64>,
}

impl EnvelopeColumns {
    /// Decodes granule `granule_id`'s envelope columns and checks every row's dictionary ids, so a bad id anywhere in
    /// the granule fails the read whichever row is asked for.
    fn read(file: &HefFile, granule_id: u32) -> Result<Self, FormatError> {
        let rows = file
            .footer()
            .granules
            .iter()
            .find(|granule| granule.granule_id == granule_id)
            .ok_or(FormatError::RefOutOfRange { what: "granule id" })?
            .row_count as usize;
        let u64s = |column_id| read_u64s(file, column_id, granule_id, rows);
        let stream_id = u64s(column_ids::STREAM_ID)?;
        let stream_sequence = u64s(column_ids::STREAM_SEQUENCE)?;
        let occurred_at = read_i64s(file, column_ids::OCCURRED_AT, granule_id, rows)?;
        let ingested_at = read_i64s(file, column_ids::INGESTED_AT, granule_id, rows)?;
        let source_id = u64s(column_ids::SOURCE_ID)?;
        let event_type_id = u64s(column_ids::EVENT_TYPE_ID)?;
        let entity_type_id = u64s(column_ids::ENTITY_TYPE_ID)?;
        let entity_id_hash_low = u64s(column_ids::ENTITY_ID_HASH_LOW)?;
        let entity_id_hash_high = u64s(column_ids::ENTITY_ID_HASH_HIGH)?;
        let flags = u64s(column_ids::FLAGS)?;
        let schema_version = u64s(column_ids::SCHEMA_VERSION)?;
        let event_id = read_u128s(file, column_ids::EVENT_ID, granule_id, rows)?;
        let entity_id = read_strings(file, column_ids::ENTITY_ID, granule_id)?;
        let actor_id = read_strings(file, column_ids::ACTOR_ID, granule_id)?;
        let account_id = read_strings(file, column_ids::ACCOUNT_ID, granule_id)?;
        let actor_id_hash_low = u64s(column_ids::ACTOR_ID_HASH_LOW)?;
        let account_id_hash_low = u64s(column_ids::ACCOUNT_ID_HASH_LOW)?;
        let trace_id_hash_low = u64s(column_ids::TRACE_ID_HASH_LOW)?;
        let dedupe_hash_low = u64s(column_ids::DEDUPE_HASH_LOW)?;
        let dedupe_hash_high = u64s(column_ids::DEDUPE_HASH_HIGH)?;
        let dictionary = &file.footer().dictionaries;
        for row in 0..rows {
            dictionary_entry(&dictionary.entity_type, &entity_type_id, row)?;
            dictionary_entry(&dictionary.event_type, &event_type_id, row)?;
            dictionary_entry(&dictionary.source, &source_id, row)?;
        }
        Ok(Self {
            account_id,
            account_id_hash_low,
            actor_id,
            actor_id_hash_low,
            dedupe_hash_high,
            dedupe_hash_low,
            entity_id,
            entity_id_hash_high,
            entity_id_hash_low,
            entity_type_id,
            event_id,
            event_type_id,
            flags,
            ingested_at,
            occurred_at,
            rows,
            schema_version,
            source_id,
            stream_id,
            stream_sequence,
            trace_id_hash_low,
        })
    }

    /// The envelope at `row` of the granule, cloning only that row's strings.
    fn envelope(&self, file: &HefFile, row: usize) -> Result<EventEnvelope, FormatError> {
        if row >= self.rows {
            return Err(FormatError::RefOutOfRange { what: "row ordinal" });
        }
        // Every column was checked to hold exactly `rows` values, so each lookup is in bounds.
        let at = |values: &[u64]| values.get(row).copied().unwrap_or_default();
        let dictionary = &file.footer().dictionaries;
        Ok(EventEnvelope {
            account_id: string_at(&self.account_id, self.rows, row),
            account_id_hash_low: at(&self.account_id_hash_low),
            actor_id: string_at(&self.actor_id, self.rows, row),
            actor_id_hash_low: at(&self.actor_id_hash_low),
            dedupe_hash_high: at(&self.dedupe_hash_high),
            dedupe_hash_low: at(&self.dedupe_hash_low),
            entity_id: string_at(&self.entity_id, self.rows, row),
            entity_id_hash_high: at(&self.entity_id_hash_high),
            entity_id_hash_low: at(&self.entity_id_hash_low),
            entity_type: dictionary_entry(&dictionary.entity_type, &self.entity_type_id, row)?.to_owned(),
            event_id: EventId::from_uuid(Uuid::from_u128(self.event_id.get(row).copied().unwrap_or_default())),
            event_type: dictionary_entry(&dictionary.event_type, &self.event_type_id, row)?.to_owned(),
            flags: EventFlags(at(&self.flags) as u32),
            ingested_at: TimestampValue::from_physical_nanos(self.ingested_at.get(row).copied().unwrap_or_default()),
            occurred_at: TimestampValue::from_physical_nanos(self.occurred_at.get(row).copied().unwrap_or_default()),
            schema_version: at(&self.schema_version) as u32,
            source: dictionary_entry(&dictionary.source, &self.source_id, row)?.to_owned(),
            stream_id: StreamId(at(&self.stream_id)),
            stream_sequence: at(&self.stream_sequence),
            tenant_id: file.header().tenant_id,
            trace_id_hash_low: at(&self.trace_id_hash_low),
        })
    }
}

#[cfg(test)]
#[path = "test/event.rs"]
mod tests;
