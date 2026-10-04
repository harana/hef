//! Seals the payloads of rows that name a data subject before they are built into a file, so each such payload is
//! stored only as ciphertext under that subject's content key and destroying the key erases it.
//!
//! See: hef-security-and-isolation/spec.md

use crate::artifacts::batch::EncodedPayload;
use crate::error::SubjectPayloadError;
use crate::events::variant::{KeyDictionary, VariantValue, encode_value};
use crate::security::{SubjectKeyStore, seal_subject_payload};
use crate::writer::build::{BuildPayload, BuildRow};
use std::collections::BTreeSet;

/// Seals the payload of every row that names a subject under that subject's key from `keys`, then clears the row's
/// subject. Call it on the rows before building them: the build refuses a row that still names a subject, so a payload
/// meant to be sealed is never written in the clear.
///
/// A sealed row's payload becomes opaque bytes, so none of its fields are shredded, promoted, or indexed; read it back
/// with [`read_subject_payload`](crate::security::read_subject_payload). A row with no payload has nothing to seal. A
/// row whose payload is an external reference keeps the reference as it is: the body it points at must be sealed
/// where it is stored.
///
/// Fails without building anything when a subject has been erased or the key store cannot answer; rows already sealed
/// stay sealed and the failing row keeps its payload and subject.
pub fn seal_subject_rows(rows: &mut [BuildRow], keys: &dyn SubjectKeyStore) -> Result<(), SubjectPayloadError> {
    for row in rows.iter_mut() {
        let Some(subject) = row.subject else {
            continue;
        };
        let encoded_here;
        let encoded = match &row.payload {
            BuildPayload::Encoded(encoded) => encoded,
            BuildPayload::ExternalRef(_) | BuildPayload::None => {
                row.subject = None;
                continue;
            }
            BuildPayload::Object(fields) => {
                encoded_here = encode(&VariantValue::Object(
                    fields
                        .iter()
                        .map(|(name, value)| (name.to_string(), value.clone()))
                        .collect(),
                ))?;
                &encoded_here
            }
            BuildPayload::Whole(value) => {
                encoded_here = encode(value)?;
                &encoded_here
            }
        };
        let sealed = seal_subject_payload(subject, row.envelope.event_id, encoded, keys)?;
        row.payload = BuildPayload::Whole(VariantValue::Binary(sealed));
        row.subject = None;
    }
    Ok(())
}

/// Encodes `value` against a key dictionary of exactly the keys it uses.
fn encode(value: &VariantValue) -> Result<EncodedPayload, SubjectPayloadError> {
    let mut keys = BTreeSet::new();
    value.collect_keys(&mut keys);
    let dictionary = KeyDictionary::build(keys.into_iter().map(str::to_owned));
    let bytes = encode_value(value, &dictionary).map_err(SubjectPayloadError::Format)?;
    Ok(EncodedPayload { bytes, dictionary })
}

#[cfg(test)]
#[path = "test/subject_seal.rs"]
mod tests;
