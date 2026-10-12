//! Stores each event's envelope and external id a second time, row by row, so fetching one event reads one short
//! record instead of a whole block of every envelope column.
//!
//! The envelope columns stay the authoritative copy; the records are droppable acceleration state under the
//! `EVENT_RECORDS` optional feature, and a reader that skips them assembles the identical envelope from the columns.
//! Each granule carries one record block in the data area, right after its payload arenas:
//!
//! `row_count u32 | occurred_at base i64 | ingested_at base i64 | offsets u32 x (row_count + 1) | records`
//!
//! Offsets are measured from the first record, so row `r`'s record is `records[offsets[r]..offsets[r + 1]]`. A record
//! is, in order: the external id as a varint length (0 for none) and its bytes; the event id as 16 little-endian
//! bytes; `occurred_at` and `ingested_at` as zigzag varints of their distance from the block's bases; the stream id,
//! stream sequence, source, event type and entity type dictionary ids, flags and schema version as varints; a one-byte
//! mask of which of the seven identity hashes are non-zero, then each non-zero one as 8 little-endian bytes; and the
//! account, actor and entity ids, each as a varint of its length plus one (0 for none) and its UTF-8 bytes.
//!
//! See: hef-file-layout/spec.md

use super::optional_features;
use super::reader::HefFile;
use crate::error::FormatError;
use crate::events::{EventEnvelope, EventFlags, EventId, StreamId, TimestampValue};
use crate::file::bytes::{Reader, Writer, slice};
use uuid::Uuid;

/// Bytes before a block's offset table: the row count and the two timestamp bases.
const BLOCK_HEADER_LEN: usize = 4 + 8 + 8;

/// One event's envelope and external id as a record stores them: the three dictionary-coded strings as their file
/// dictionary ids, every other field as the envelope holds it. Strings borrow from the block they were read from.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRecord<'a> {
    pub account_id: Option<&'a str>,
    pub account_id_hash_low: u64,
    pub actor_id: Option<&'a str>,
    pub actor_id_hash_low: u64,
    pub dedupe_hash_high: u64,
    pub dedupe_hash_low: u64,
    pub entity_id: Option<&'a str>,
    pub entity_id_hash_high: u64,
    pub entity_id_hash_low: u64,
    pub entity_type_id: u64,
    pub event_id: u128,
    pub event_type_id: u64,
    pub external_id: Option<&'a [u8]>,
    pub flags: u32,
    pub ingested_at: i64,
    pub occurred_at: i64,
    pub schema_version: u32,
    pub source_id: u64,
    pub stream_id: u64,
    pub stream_sequence: u64,
    pub trace_id_hash_low: u64,
}

impl EventRecord<'_> {
    /// The seven identity hashes in the order the record's mask numbers them.
    fn hashes(&self) -> [u64; 7] {
        [
            self.account_id_hash_low,
            self.actor_id_hash_low,
            self.dedupe_hash_high,
            self.dedupe_hash_low,
            self.entity_id_hash_high,
            self.entity_id_hash_low,
            self.trace_id_hash_low,
        ]
    }
}

/// Builds one granule's record block a row at a time, in row order.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Default)]
pub struct EventRecordBlock {
    bases: Option<(i64, i64)>,
    offsets: Vec<u32>,
    records: Vec<u8>,
}

impl EventRecordBlock {
    /// Appends the next row's record. The first row's timestamps become the block's bases.
    pub fn push(&mut self, record: &EventRecord<'_>) {
        let (occurred_base, ingested_base) = *self.bases.get_or_insert((record.occurred_at, record.ingested_at));
        self.offsets.push(self.records.len() as u32);
        let out = &mut self.records;
        put_bytes(out, record.external_id, 0);
        out.extend_from_slice(&record.event_id.to_le_bytes());
        put_varint(out, zigzag(record.occurred_at.wrapping_sub(occurred_base)));
        put_varint(out, zigzag(record.ingested_at.wrapping_sub(ingested_base)));
        for value in [
            record.stream_id,
            record.stream_sequence,
            record.source_id,
            record.event_type_id,
            record.entity_type_id,
            u64::from(record.flags),
            u64::from(record.schema_version),
        ] {
            put_varint(out, value);
        }
        let hashes = record.hashes();
        let mask = hashes
            .iter()
            .enumerate()
            .fold(0u8, |mask, (bit, &hash)| if hash == 0 { mask } else { mask | 1 << bit });
        out.push(mask);
        for hash in hashes.into_iter().filter(|&hash| hash != 0) {
            out.extend_from_slice(&hash.to_le_bytes());
        }
        for text in [record.account_id, record.actor_id, record.entity_id] {
            put_bytes(out, text.map(str::as_bytes), 1);
        }
    }

    /// The finished block, ready for the data area.
    pub fn finish(mut self) -> Vec<u8> {
        let rows = self.offsets.len();
        self.offsets.push(self.records.len() as u32);
        let (occurred_base, ingested_base) = self.bases.unwrap_or_default();
        let mut out = Writer::with_capacity(BLOCK_HEADER_LEN + self.offsets.len() * 4 + self.records.len());
        out.put_u32(rows as u32);
        out.put_i64(occurred_base);
        out.put_i64(ingested_base);
        out.put_u32_slice(&self.offsets);
        out.put_slice(&self.records);
        out.into_bytes()
    }
}

/// Rewrites a record block with each row's dedupe hashes replaced by `dedupe[row]` as `(high, low)`: what a sparse
/// update needs, since dedupe hashes are the only envelope fields it may change.
pub(crate) fn with_dedupe_hashes(block: &[u8], dedupe: &[(u64, u64)]) -> Result<Vec<u8>, FormatError> {
    let rows = block_rows(block)?;
    if rows as usize != dedupe.len() {
        return Err(FormatError::Structural {
            rule: "a rewritten event record block keeps its row count",
        });
    }
    let mut rewritten = EventRecordBlock::default();
    for (row, &(high, low)) in dedupe.iter().enumerate() {
        let (bases, bytes) = record_bytes(block, row as u32)?;
        let record = decode_record(bytes, bases)?;
        rewritten.push(&EventRecord {
            dedupe_hash_high: high,
            dedupe_hash_low: low,
            ..record
        });
    }
    Ok(rewritten.finish())
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

/// `bytes` as a varint of its length plus `bias` and the bytes themselves, or a single 0 for `None`. A bias of 1 lets
/// an empty string differ from no string; external ids are never empty, so they take a bias of 0.
fn put_bytes(out: &mut Vec<u8>, bytes: Option<&[u8]>, bias: u64) {
    match bytes {
        Some(bytes) => {
            put_varint(out, bytes.len() as u64 + bias);
            out.extend_from_slice(bytes);
        }
        None => out.push(0),
    }
}

fn varint(r: &mut Reader<'_>) -> Result<u64, FormatError> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = r.u8("event record varint")?;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(FormatError::Structural {
        rule: "an event record varint is at most 10 bytes",
    })
}

fn optional_bytes<'a>(r: &mut Reader<'a>, bias: u64) -> Result<Option<&'a [u8]>, FormatError> {
    let len = varint(r)?;
    if len < bias.max(1) {
        return Ok(None);
    }
    Ok(Some(r.take((len - bias) as usize, "event record bytes")?))
}

fn optional_text<'a>(r: &mut Reader<'a>) -> Result<Option<&'a str>, FormatError> {
    optional_bytes(r, 1)?
        .map(|bytes| simdutf8::basic::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 { what: "event record" }))
        .transpose()
}

fn block_rows(block: &[u8]) -> Result<u32, FormatError> {
    Ok(Reader::new(block).u32("event record row count")?)
}

/// Row `row`'s record bytes and the block's timestamp bases, read out of a whole block.
fn record_bytes(block: &[u8], row: u32) -> Result<((i64, i64), &[u8]), FormatError> {
    let mut r = Reader::new(block);
    let rows = r.u32("event record row count")?;
    let bases = (
        r.i64("event record occurred base")?,
        r.i64("event record ingested base")?,
    );
    if row >= rows {
        return Err(FormatError::RefOutOfRange {
            what: "event record row",
        });
    }
    let offsets = slice(block, BLOCK_HEADER_LEN + row as usize * 4, 8, "event record offsets")?;
    let mut offsets = Reader::new(offsets);
    let (start, end) = (offsets.u32("event record offset")?, offsets.u32("event record end")?);
    let records = BLOCK_HEADER_LEN + (rows as usize + 1) * 4;
    let len = end.checked_sub(start).ok_or(FormatError::Structural {
        rule: "event record offsets ascend",
    })?;
    Ok((
        bases,
        slice(block, records + start as usize, len as usize, "event record")?,
    ))
}

fn decode_record(bytes: &[u8], (occurred_base, ingested_base): (i64, i64)) -> Result<EventRecord<'_>, FormatError> {
    let mut r = Reader::new(bytes);
    let external_id = optional_bytes(&mut r, 0)?;
    let event_id = r.u128("event record event id")?;
    let occurred_at = occurred_base.wrapping_add(unzigzag(varint(&mut r)?));
    let ingested_at = ingested_base.wrapping_add(unzigzag(varint(&mut r)?));
    let stream_id = varint(&mut r)?;
    let stream_sequence = varint(&mut r)?;
    let source_id = varint(&mut r)?;
    let event_type_id = varint(&mut r)?;
    let entity_type_id = varint(&mut r)?;
    let flags = u32::try_from(varint(&mut r)?).map_err(|_| FormatError::Structural {
        rule: "event record flags fit 32 bits",
    })?;
    let schema_version = u32::try_from(varint(&mut r)?).map_err(|_| FormatError::Structural {
        rule: "event record schema version fits 32 bits",
    })?;
    let mask = r.u8("event record hash mask")?;
    let mut hashes = [0u64; 7];
    for (bit, hash) in hashes.iter_mut().enumerate() {
        if mask & (1 << bit) != 0 {
            *hash = r.u64("event record hash")?;
        }
    }
    let [
        account_id_hash_low,
        actor_id_hash_low,
        dedupe_hash_high,
        dedupe_hash_low,
        entity_id_hash_high,
        entity_id_hash_low,
        trace_id_hash_low,
    ] = hashes;
    let account_id = optional_text(&mut r)?;
    let actor_id = optional_text(&mut r)?;
    let entity_id = optional_text(&mut r)?;
    if r.remaining() != 0 {
        return Err(FormatError::Structural {
            rule: "an event record ends where its offsets say",
        });
    }
    Ok(EventRecord {
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
        external_id,
        flags,
        ingested_at,
        occurred_at,
        schema_version,
        source_id,
        stream_id,
        stream_sequence,
        trace_id_hash_low,
    })
}

impl HefFile {
    /// The stored record of the event at `row_ordinal`: its envelope and external id, read from that row's bytes
    /// alone. `None` when the file stores no records, or none for the row's granule; the envelope columns then hold
    /// the same values.
    pub fn event_record(&self, row_ordinal: u64) -> Result<Option<EventRecord<'_>>, FormatError> {
        if self.usable_optional_features() & optional_features::EVENT_RECORDS == 0 {
            return Ok(None);
        }
        let granule = self.granule_for_row(row_ordinal)?;
        let records = &self.footer().event_records;
        let Ok(index) = records.binary_search_by_key(&granule.granule_id, |entry| entry.granule_id) else {
            return Ok(None);
        };
        let Some(entry) = records.get(index) else {
            return Ok(None);
        };
        let row = (row_ordinal - granule.first_row_ordinal) as usize;
        let start = self.file_position(granule.granule_id, entry.offset)?;
        // Three small reads — the header, the row's two offsets, its record — so a remote reader fetches the row's
        // bytes rather than the whole block.
        let header = self.data(start, BLOCK_HEADER_LEN, "event record header")?;
        let mut r = Reader::new(header);
        let rows = r.u32("event record row count")? as usize;
        let bases = (
            r.i64("event record occurred base")?,
            r.i64("event record ingested base")?,
        );
        if rows != granule.row_count as usize {
            return Err(FormatError::Structural {
                rule: "an event record block holds one record per granule row",
            });
        }
        let records_start = BLOCK_HEADER_LEN + (rows + 1) * 4;
        if records_start as u64 > entry.len {
            return Err(FormatError::Truncated {
                what: "event record offsets",
            });
        }
        let offsets = self.data(start + BLOCK_HEADER_LEN + row * 4, 8, "event record offsets")?;
        let mut offsets = Reader::new(offsets);
        let (from, to) = (
            offsets.u32("event record offset")? as usize,
            offsets.u32("event record end")? as usize,
        );
        if from > to || (records_start + to) as u64 > entry.len {
            return Err(FormatError::Structural {
                rule: "event record offsets ascend within the block",
            });
        }
        let bytes = self.data(start + records_start + from, to - from, "event record")?;
        decode_record(bytes, bases).map(Some)
    }

    /// The envelope a record stores, with its dictionary ids resolved against the file's dictionaries.
    pub(crate) fn record_envelope(&self, record: &EventRecord<'_>) -> Result<EventEnvelope, FormatError> {
        let dictionaries = &self.footer().dictionaries;
        let entry = |values: &[String], id: u64| {
            values.get(id as usize).cloned().ok_or(FormatError::RefOutOfRange {
                what: "file dictionary id",
            })
        };
        Ok(EventEnvelope {
            account_id: record.account_id.map(str::to_owned),
            account_id_hash_low: record.account_id_hash_low,
            actor_id: record.actor_id.map(str::to_owned),
            actor_id_hash_low: record.actor_id_hash_low,
            dedupe_hash_high: record.dedupe_hash_high,
            dedupe_hash_low: record.dedupe_hash_low,
            entity_id: record.entity_id.map(str::to_owned),
            entity_id_hash_high: record.entity_id_hash_high,
            entity_id_hash_low: record.entity_id_hash_low,
            entity_type: entry(&dictionaries.entity_type, record.entity_type_id)?,
            event_id: EventId::from_uuid(Uuid::from_u128(record.event_id)),
            event_type: entry(&dictionaries.event_type, record.event_type_id)?,
            flags: EventFlags(record.flags),
            ingested_at: TimestampValue::from_physical_nanos(record.ingested_at),
            occurred_at: TimestampValue::from_physical_nanos(record.occurred_at),
            schema_version: record.schema_version,
            source: entry(&dictionaries.source, record.source_id)?,
            stream_id: StreamId(record.stream_id),
            stream_sequence: record.stream_sequence,
            tenant_id: self.header().tenant_id,
            trace_id_hash_low: record.trace_id_hash_low,
        })
    }
}

#[cfg(test)]
#[path = "test/event_record.rs"]
mod tests;
