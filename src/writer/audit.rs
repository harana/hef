//! Writes an audit file's hash chain and keeps audit files from ever being rewritten.
//!
//! An audit file is built like any other, from rows the caller hands in, but the build also hashes every row into a
//! chain that starts where the previous audit file ended (see [`crate::layout::audit`]) and records the chain's ends
//! and the file's Object Lock retention in the footer. Once written, an audit file is final: every path that would
//! replace one - an update, a stripe-reusing rewrite, an entity projection, a compaction - refuses it.
//!
//! See: hef-file-layout/spec.md

use super::build::{BuildPayload, BuildRow, HefBuildConfig};
use super::constant::AUDIT_REWRITE_REFUSED;
use super::projection::RowOrder;
use crate::error::FormatError;
use crate::events::EventEnvelope;
use crate::events::variant::{VariantRef, VariantValue};
use crate::layout::Profile;
use crate::layout::audit::{DigestPayload, row_digest, row_hash};
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
    let digests = rows.par_iter().map(build_row_digest).collect::<Result<Vec<_>, _>>()?;
    let chain_end = digests.iter().fold(settings.chain_start, |prev_row_hash, digest| {
        row_hash(&prev_row_hash, digest)
    });
    Ok(Some(AuditTrail {
        chain_end,
        chain_start: settings.chain_start,
        legal_hold: settings.legal_hold,
        retention: settings.retention,
    }))
}

/// The digest of one row as the verifier will read it back: its envelope, and its payload in decoded form.
fn build_row_digest(row: &BuildRow) -> Result<[u8; 32], FormatError> {
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
    let envelope = EventEnvelope {
        account_id: envelope.account_id.clone(),
        account_id_hash_low: envelope.account_id_hash_low,
        actor_id: envelope.actor_id.clone(),
        actor_id_hash_low: envelope.actor_id_hash_low,
        dedupe_hash_high: envelope.dedupe_hash_high,
        dedupe_hash_low: envelope.dedupe_hash_low,
        entity_id: envelope.entity_id.clone(),
        entity_id_hash_high: envelope.entity_id_hash_high,
        entity_id_hash_low: envelope.entity_id_hash_low,
        entity_type: envelope.entity_type.to_string(),
        event_id: envelope.event_id,
        event_type: envelope.event_type.to_string(),
        flags: envelope.flags,
        ingested_at: envelope.ingested_at,
        occurred_at: envelope.occurred_at,
        schema_version: envelope.schema_version,
        source: envelope.source.to_string(),
        stream_id: envelope.stream_id,
        stream_sequence: envelope.stream_sequence,
        tenant_id: envelope.tenant_id,
        trace_id_hash_low: envelope.trace_id_hash_low,
    };
    row_digest(row.epoch, row.sequence, &envelope, payload)
}

#[cfg(test)]
#[path = "test/audit.rs"]
mod tests;
