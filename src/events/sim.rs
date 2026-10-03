//! A stand-in author who really signs, so tests and simulations can produce events that verify for the right reason.
//!
//! Nothing in the platform signs events — authors do, out on the wire — but a test needs signed events that are
//! genuinely signed, not fixtures with plausible-looking bytes. This holds one deterministic key and produces
//! provenance whose identifier and signature check out against the same verification path a real event goes through.
//!
//! See: hef-logical-event-model/spec.md

use super::constant::{NANOS_PER_SECOND, PROTOCOL_ID_BYTES, SIGNATURE_BYTES};
use super::envelope::TimestampValue;
use super::provenance::{SignatureScheme, SignedEventProvenance};
use super::variant::VariantValue;
use crate::error::ProvenanceError;
use k256::schnorr::SigningKey;
use k256::schnorr::signature::hazmat::PrehashSigner;
use std::collections::BTreeMap;

/// A simulated event author with one fixed key.
#[derive(Debug, Clone)]
pub struct SimulatedEventAuthor {
    signing: SigningKey,
}

impl SimulatedEventAuthor {
    /// An author whose key is derived from `seed`, so the same seed always yields the same public key.
    pub fn from_seed(seed: u8) -> Self {
        Self {
            // Every non-zero 32-byte value below the curve order is a valid key; a repeated small byte is well inside
            // it, and the seed is nudged off zero so no caller can accidentally ask for the invalid key.
            #[allow(clippy::expect_used)]
            signing: SigningKey::from_bytes(&[seed.max(1); PROTOCOL_ID_BYTES].into())
                .expect("a repeated small byte is a valid key"),
        }
    }

    /// This author's public key, as the provenance family stores it.
    pub fn author_pubkey(&self) -> [u8; PROTOCOL_ID_BYTES] {
        self.signing.verifying_key().to_bytes().into()
    }

    /// Signs `payload` as an event of `kind` claimed at `claimed_at_seconds`, producing provenance whose identifier
    /// recomputes and whose signature verifies.
    pub fn sign(
        &self,
        kind: u32,
        claimed_at_seconds: i64,
        payload: &VariantValue,
    ) -> Result<SignedEventProvenance, ProvenanceError> {
        let mut provenance = SignedEventProvenance {
            author_pubkey: self.author_pubkey(),
            claimed_at: TimestampValue::from_physical_nanos(claimed_at_seconds * NANOS_PER_SECOND),
            protocol_event_id: [0u8; PROTOCOL_ID_BYTES],
            protocol_kind: kind,
            scheme: SignatureScheme::Bip340SchnorrSecp256k1,
            signature: [0u8; SIGNATURE_BYTES],
        };
        let canonical = provenance.canonical_bytes(payload)?;
        provenance.protocol_event_id = SignedEventProvenance::recompute_protocol_event_id(&canonical);
        let signature: k256::schnorr::Signature = self
            .signing
            .sign_prehash(&provenance.protocol_event_id)
            .map_err(|_| ProvenanceError::MalformedSignature)?;
        provenance.signature = signature.to_bytes();
        Ok(provenance)
    }

    /// Signs an already-hashed 32-byte message, for the protocol extensions whose signatures are made over something
    /// other than an event.
    pub fn sign_prehash(&self, prehash: &[u8; PROTOCOL_ID_BYTES]) -> Result<[u8; SIGNATURE_BYTES], ProvenanceError> {
        let signature: k256::schnorr::Signature = self
            .signing
            .sign_prehash(prehash)
            .map_err(|_| ProvenanceError::MalformedSignature)?;
        Ok(signature.to_bytes())
    }
}

/// A payload in the shape a signed protocol event carries: the message text and the tag list, in the canonical
/// payload format.
pub fn signed_event_payload(content: &str, tags: &[&[&str]]) -> VariantValue {
    let mut fields = BTreeMap::new();
    fields.insert(
        super::provenance::CONTENT_FIELD.to_owned(),
        VariantValue::String(content.to_owned()),
    );
    fields.insert(
        super::provenance::TAGS_FIELD.to_owned(),
        VariantValue::Array(
            tags.iter()
                .map(|tag| {
                    VariantValue::Array(
                        tag.iter()
                            .map(|item| VariantValue::String((*item).to_owned()))
                            .collect(),
                    )
                })
                .collect(),
        ),
    );
    VariantValue::Object(fields)
}
