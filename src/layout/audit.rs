//! Proves that the rows of an audit file are the ones that were written, in the order they were written, and that a
//! run of audit files is complete and in order.
//!
//! Every audit row has a hash that covers its own contents and the hash of the row before it, its `prev_row_hash`.
//! Altering, dropping or reordering a row therefore changes the hash of every row after it, and the chain runs on from
//! one audit file into the next. A file's footer records the chain's two ends ([`AuditTrail`]); the verifier here
//! recomputes every row's hash from the rows themselves and checks each file starts where the one before it ended.
//!
//! A row's hash is `BLAKE3-derive-key(AUDIT_ROW_HASH_CONTEXT, prev_row_hash || row_digest)`, where `row_digest` is
//! the plain BLAKE3 of a fixed encoding of the row: epoch, sequence, every envelope field in name order, and the
//! payload in the variant encoding against a dictionary of the payload's own keys.
//!
//! See: hef-file-layout/spec.md

use super::constant::{AUDIT_ROW_HASHER, ROW_DIGEST_INPUT_CAPACITY};
use super::event::read_u64s;
use super::footer::AuditTrail;
use super::reader::{HefFile, PayloadRead};
use crate::artifacts::batch::encode_variant_dictionary;
use crate::columns::column_ids;
use crate::error::{AuditChainError, FormatError};
use crate::events::EventEnvelope;
use crate::events::variant::{KeyDictionary, VariantValue, encode_object_fields_into, encode_value_into};
use crate::file::bytes::Writer;
use std::collections::BTreeSet;

/// Checks a run of audit files, handed over in manifest order, and returns the hash of the run's last row.
///
/// `chain_start` is the `prev_row_hash` the run's first row must carry:
/// [`AUDIT_GENESIS`](super::constant::AUDIT_GENESIS) when the run starts the trail, or the last row hash of the audit
/// file just before the run. Each file must start where the one before it ended, so a missing, extra or reordered
/// file is caught; and every row's hash is recomputed from the row itself, so an altered, dropped or reordered row is
/// caught. Open each file against its manifest entry's `file_seal` first: the seal is what ties the chain ends in the
/// footer to the manifest. To check one file on its own, pass it as a one-element slice with its own chain start.
pub fn verify_audit_files(chain_start: [u8; 32], files: &[HefFile]) -> Result<[u8; 32], AuditChainError> {
    let mut expected = chain_start;
    for (file_index, file) in files.iter().enumerate() {
        let Some(trail) = file.footer().audit else {
            return Err(AuditChainError::NotAudit { file_index });
        };
        if trail.chain_start != expected {
            return Err(AuditChainError::ChainBreak { file_index });
        }
        let recomputed =
            recomputed_chain_end(file, &trail).map_err(|error| AuditChainError::Format { error, file_index })?;
        if recomputed != trail.chain_end {
            return Err(AuditChainError::RowsAltered { file_index });
        }
        expected = trail.chain_end;
    }
    Ok(expected)
}

/// A row's payload as its digest covers it, borrowed from wherever the row lives, so neither the writer nor the
/// verifier copies a payload to hash it.
///
/// See: hef-file-layout/spec.md
pub(crate) enum DigestPayload<'a> {
    External(&'a str),
    None,
    /// A top-level object as its fields in ascending key order with no key repeated. Digested exactly as the
    /// equivalent [`VariantValue::Object`].
    Object(&'a [(&'a str, &'a VariantValue)]),
    Value(&'a VariantValue),
}

impl<'a> From<&'a PayloadRead> for DigestPayload<'a> {
    fn from(payload: &'a PayloadRead) -> Self {
        match payload {
            PayloadRead::External(reference) => DigestPayload::External(reference),
            PayloadRead::None => DigestPayload::None,
            PayloadRead::Value(value) => DigestPayload::Value(value),
        }
    }
}

/// One row's envelope as its digest covers it, borrowed from wherever the row lives, so neither the writer nor the
/// verifier copies the envelope's text to hash it.
///
/// Text fields borrow; everything else is the plain scalar the digest encodes.
///
/// See: hef-file-layout/spec.md
pub(crate) struct DigestEnvelope<'a> {
    pub(crate) account_id: Option<&'a str>,
    pub(crate) account_id_hash_low: u64,
    pub(crate) actor_id: Option<&'a str>,
    pub(crate) actor_id_hash_low: u64,
    pub(crate) dedupe_hash_high: u64,
    pub(crate) dedupe_hash_low: u64,
    pub(crate) entity_id: Option<&'a str>,
    pub(crate) entity_id_hash_high: u64,
    pub(crate) entity_id_hash_low: u64,
    pub(crate) entity_type: &'a str,
    pub(crate) event_id: u128,
    pub(crate) event_type: &'a str,
    pub(crate) flags: u32,
    /// Physical nanoseconds.
    pub(crate) ingested_at: i64,
    /// Physical nanoseconds.
    pub(crate) occurred_at: i64,
    pub(crate) schema_version: u32,
    pub(crate) source: &'a str,
    pub(crate) stream_id: u64,
    pub(crate) stream_sequence: u64,
    pub(crate) tenant_id: u128,
    pub(crate) trace_id_hash_low: u64,
}

impl<'a> From<&'a EventEnvelope> for DigestEnvelope<'a> {
    fn from(envelope: &'a EventEnvelope) -> Self {
        DigestEnvelope {
            account_id: envelope.account_id.as_deref(),
            account_id_hash_low: envelope.account_id_hash_low,
            actor_id: envelope.actor_id.as_deref(),
            actor_id_hash_low: envelope.actor_id_hash_low,
            dedupe_hash_high: envelope.dedupe_hash_high,
            dedupe_hash_low: envelope.dedupe_hash_low,
            entity_id: envelope.entity_id.as_deref(),
            entity_id_hash_high: envelope.entity_id_hash_high,
            entity_id_hash_low: envelope.entity_id_hash_low,
            entity_type: &envelope.entity_type,
            event_id: envelope.event_id.uuid().as_u128(),
            event_type: &envelope.event_type,
            flags: envelope.flags.0,
            ingested_at: envelope.ingested_at.physical_nanos(),
            occurred_at: envelope.occurred_at.physical_nanos(),
            schema_version: envelope.schema_version,
            source: &envelope.source,
            stream_id: envelope.stream_id.0,
            stream_sequence: envelope.stream_sequence,
            tenant_id: envelope.tenant_id.uuid().as_u128(),
            trace_id_hash_low: envelope.trace_id_hash_low,
        }
    }
}

/// The digest of one row's contents, the half of its hash that does not depend on the rows before it. A function of
/// the row alone, so the writer and a verifier reading the row back out of the file compute the same digest.
pub(crate) fn row_digest(
    epoch: u64,
    sequence: u64,
    envelope: &EventEnvelope,
    payload: DigestPayload<'_>,
) -> Result<[u8; 32], FormatError> {
    envelope_row_digest(epoch, sequence, &envelope.into(), payload)
}

/// [`row_digest`] over a borrowed envelope: the one encoder every row digest goes through.
pub(crate) fn envelope_row_digest(
    epoch: u64,
    sequence: u64,
    envelope: &DigestEnvelope<'_>,
    payload: DigestPayload<'_>,
) -> Result<[u8; 32], FormatError> {
    let mut out = Writer::with_capacity(ROW_DIGEST_INPUT_CAPACITY);
    out.put_u64(epoch);
    out.put_u64(sequence);
    put_optional_text(&mut out, envelope.account_id);
    out.put_u64(envelope.account_id_hash_low);
    put_optional_text(&mut out, envelope.actor_id);
    out.put_u64(envelope.actor_id_hash_low);
    out.put_u64(envelope.dedupe_hash_high);
    out.put_u64(envelope.dedupe_hash_low);
    put_optional_text(&mut out, envelope.entity_id);
    out.put_u64(envelope.entity_id_hash_high);
    out.put_u64(envelope.entity_id_hash_low);
    put_text(&mut out, envelope.entity_type);
    out.put_u128(envelope.event_id);
    put_text(&mut out, envelope.event_type);
    out.put_u32(envelope.flags);
    out.put_i64(envelope.ingested_at);
    out.put_i64(envelope.occurred_at);
    out.put_u32(envelope.schema_version);
    put_text(&mut out, envelope.source);
    out.put_u64(envelope.stream_id);
    out.put_u64(envelope.stream_sequence);
    out.put_u128(envelope.tenant_id);
    out.put_u64(envelope.trace_id_hash_low);
    match payload {
        DigestPayload::None => out.put_u8(0),
        DigestPayload::External(reference) => {
            out.put_u8(1);
            put_text(&mut out, reference);
        }
        DigestPayload::Object(fields) => {
            out.put_u8(2);
            let mut keys = BTreeSet::new();
            for (name, value) in fields {
                keys.insert(*name);
                value.collect_keys(&mut keys);
            }
            let dictionary = put_dictionary(&mut out, keys)?;
            encode_object_fields_into(fields, &dictionary, &mut out)?;
        }
        DigestPayload::Value(value) => {
            out.put_u8(2);
            let mut keys = BTreeSet::new();
            value.collect_keys(&mut keys);
            let dictionary = put_dictionary(&mut out, keys)?;
            encode_value_into(value, &dictionary, &mut out)?;
        }
    }
    Ok(*blake3::hash(out.bytes()).as_bytes())
}

/// Writes the dictionary of a payload's own keys and returns it for encoding the payload against.
fn put_dictionary(out: &mut Writer, keys: BTreeSet<&str>) -> Result<KeyDictionary, FormatError> {
    let dictionary = KeyDictionary::from_sorted_unique(keys.into_iter().map(str::to_owned).collect())?;
    out.put_slice(&encode_variant_dictionary(&dictionary));
    Ok(dictionary)
}

/// A row's hash, from the hash of the row before it and the digest of its own contents.
pub(crate) fn row_hash(prev_row_hash: &[u8; 32], row_digest: &[u8; 32]) -> [u8; 32] {
    let mut hasher = AUDIT_ROW_HASHER.clone();
    hasher.update(prev_row_hash);
    hasher.update(row_digest);
    *hasher.finalize().as_bytes()
}

/// The hash of `file`'s last row, recomputed from its rows with the first one chained from the trail's start.
fn recomputed_chain_end(file: &HefFile, trail: &AuditTrail) -> Result<[u8; 32], FormatError> {
    let mut hash = trail.chain_start;
    for granule in &file.footer().granules {
        let rows = granule.row_count as usize;
        let envelopes = file.granule_envelopes(granule.granule_id)?;
        let epochs = read_u64s(file, column_ids::EPOCH, granule.granule_id, rows)?;
        let sequences = read_u64s(file, column_ids::SEQUENCE, granule.granule_id, rows)?;
        for (ordinal, ((envelope, epoch), sequence)) in
            (granule.first_row_ordinal..).zip(envelopes.iter().zip(epochs).zip(sequences))
        {
            let payload = file.payload(ordinal)?;
            hash = row_hash(&hash, &row_digest(epoch, sequence, envelope, (&payload).into())?);
        }
    }
    Ok(hash)
}

fn put_text(out: &mut Writer, text: &str) {
    out.put_u32(text.len() as u32);
    out.put_slice(text.as_bytes());
}

fn put_optional_text(out: &mut Writer, text: Option<&str>) {
    match text {
        Some(text) => {
            out.put_u8(1);
            put_text(out, text);
        }
        None => out.put_u8(0),
    }
}

#[cfg(test)]
#[path = "test/audit.rs"]
mod tests;
