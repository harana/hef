//! Writes an audit file's hash chain and keeps audit files from ever being rewritten.
//!
//! An audit file is built like any other, from rows the caller hands in, but the build also hashes every row into a
//! chain that starts where the previous audit file ended (see [`crate::layout::audit`]) and records the chain's ends
//! and the file's Object Lock retention in the footer. Once written, an audit file is final: every path that would
//! replace one - an update, a stripe-reusing rewrite, an entity projection, a compaction - refuses it.
//!
//! See: hef-file-layout/spec.md

use super::build::{BuildPayload, BuildRow, HefBuildConfig};
use super::constant::{AUDIT_DIGEST_BATCH_ROWS, AUDIT_REWRITE_REFUSED};
use super::projection::RowOrder;
use crate::error::FormatError;
use crate::events::variant::{VariantRef, VariantValue};
use crate::file::bytes::Writer;
use crate::layout::Profile;
use crate::layout::audit::{DigestEnvelope, DigestPayload, envelope_row_digest, row_hash};
use crate::layout::constant::ROW_DIGEST_INPUT_CAPACITY;
use crate::layout::footer::{AuditTrail, ObjectLockRetention};
use rayon::prelude::*;

/// What the caller supplies to build an audit file: where its chain starts and how the object store must keep it.
///
/// See: hef-file-layout/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditSettings {
    /// The previous audit file's last row hash (its footer's `chain_end`), or
    /// [`AUDIT_GENESIS`](crate::layout::constant::AUDIT_GENESIS) for the first file of a trail.
    pub chain_start: [u8; 32],
    pub legal_hold: bool,
    pub retention: Option<ObjectLockRetention>,
}

/// Refuses a rewrite of `source` into a file built with `profile` when either is an audit file.
pub(crate) fn refuse_audit_rewrite(source: Profile, profile: Profile) -> Result<(), FormatError> {
    if source == Profile::Audit || profile == Profile::Audit {
        return Err(AUDIT_REWRITE_REFUSED);
    }
    Ok(())
}

/// The audit trail a build of `rows` records, or `None` for a file that is not an audit file. Hashes every row, so
/// it runs before the build takes the rows apart.
///
/// Refuses an audit build with no [`AuditSettings`], settings on any other profile, an audit file in entity order, and
/// anything an audit row hash would not cover: analytical columns, and rows carrying an external id, a raw payload,
/// signed or multi-signer provenance, or relationships.
pub(crate) fn audit_trail(
    rows: &[BuildRow],
    config: &HefBuildConfig,
    order: RowOrder,
) -> Result<Option<AuditTrail>, FormatError> {
    let settings = match (config.profile, config.audit) {
        (Profile::Audit, Some(settings)) => settings,
        (Profile::Audit, None) => {
            return Err(FormatError::Structural {
                rule: "an audit file needs its audit settings",
            });
        }
        (_, Some(_)) => {
            return Err(FormatError::Structural {
                rule: "audit settings apply only to an audit file",
            });
        }
        (_, None) => return Ok(None),
    };
    if order == RowOrder::Entity {
        return Err(AUDIT_REWRITE_REFUSED);
    }
    if !config.analytical_columns.is_empty() {
        return Err(FormatError::Structural {
            rule: "an audit file carries no analytical columns",
        });
    }
    Ok(Some(AuditTrail {
        chain_end: chain_end(rows, settings.chain_start, AUDIT_DIGEST_BATCH_ROWS)?,
        chain_start: settings.chain_start,
        legal_hold: settings.legal_hold,
        retention: settings.retention,
    }))
}

/// The hash of the last of `rows`, chained from `chain_start`. Digests up to `batch_rows` rows at a time in parallel
/// into one reused buffer and folds each batch into the chain in row order, so the scratch stays bounded however many
/// rows the file holds. Each parallel job encodes its rows through one buffer of its own rather than one per row. The
/// first row that fails to digest fails the whole chain.
fn chain_end(rows: &[BuildRow], chain_start: [u8; 32], batch_rows: usize) -> Result<[u8; 32], FormatError> {
    let mut digests = Vec::with_capacity(batch_rows.min(rows.len()));
    let mut hash = chain_start;
    for batch in rows.chunks(batch_rows) {
        batch
            .par_iter()
            .map_init(|| Writer::with_capacity(ROW_DIGEST_INPUT_CAPACITY), build_row_digest)
            .collect_into_vec(&mut digests);
        for digest in digests.drain(..) {
            hash = row_hash(&hash, &digest?);
        }
    }
    Ok(hash)
}

/// The digest of one row as the verifier will read it back: its envelope, and its payload in decoded form.
fn build_row_digest(out: &mut Writer, row: &BuildRow) -> Result<[u8; 32], FormatError> {
    if row.external_id.is_some()
        || row.matrix_provenance.is_some()
        || row.provenance.is_some()
        || row.raw_payload.is_some()
        || row.relationships.is_some()
    {
        return Err(FormatError::Structural {
            rule: "an audit row carries only its envelope and payload",
        });
    }
    let envelope = &row.envelope;
    let decoded: VariantValue;
    let fields: Vec<(&str, &VariantValue)>;
    let payload = match &row.payload {
        BuildPayload::Encoded(encoded) => {
            decoded = VariantRef::new(&encoded.bytes).decode(&encoded.dictionary)?;
            DigestPayload::Value(&decoded)
        }
        BuildPayload::ExternalRef(reference) => DigestPayload::External(reference),
        BuildPayload::None => DigestPayload::None,
        BuildPayload::Object(object) => {
            fields = object.iter().map(|(name, value)| (&**name, value)).collect();
            DigestPayload::Object(&fields)
        }
        BuildPayload::Whole(value) => DigestPayload::Value(value),
    };
    let envelope = DigestEnvelope {
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
    };
    envelope_row_digest(out, row.epoch, row.sequence, &envelope, payload)
}

#[cfg(test)]
#[path = "test/audit.rs"]
mod tests;
