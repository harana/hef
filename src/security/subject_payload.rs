//! Locks each data subject's event payloads under that subject's own content key, so erasing a subject is one key
//! destruction: every payload sealed under it reads back as a tombstone while every other row of the same file still
//! reads.
//!
//! The subject is an opaque id the caller chooses, and that choice sets how finely erasure can cut. Naming one subject
//! per sender (for Matrix: the sender, or the room plus the sender) erases everything a person sent with one key, at
//! one stored key per sender. Naming one subject per event erases exactly that event, at one stored key per event, so
//! the key store grows with the event count. HEF never picks for the caller; it only seals and opens.
//!
//! A sealed payload is stored as the row's whole payload: a binary value holding a fixed marker, the subject id in the
//! clear (so a reader knows which key to ask for), and the sealed blob. The seal binds the row's event id, so a blob
//! copied onto another row fails authentication instead of opening there. Because the payload is opaque bytes, none of
//! its fields are shredded, promoted, or indexed: a field that must stay queryable after erasure belongs in the
//! envelope, not in a sealed payload.
//!
//! See: hef-security-and-isolation/spec.md

use super::constant::SEALED_PAYLOAD_MAGIC;
use crate::artifacts::batch::{EncodedPayload, decode_variant_dictionary, encode_variant_dictionary};
use crate::columns::column_ids;
use crate::deletes::{RebuiltPayload, SubjectContentKey, SubjectId, rebuild_payload};
use crate::encoding::ColumnData;
use crate::error::{FormatError, StorageError, SubjectPayloadError};
use crate::events::EventId;
use crate::events::variant::{VariantRef, VariantValue};
use crate::file::bytes::{Reader, Writer};
use crate::layout::reader::{HefFile, PayloadRead};

/// Where each data subject's content key lives. The application backs this with its durable key store, so a key
/// survives restarts, every node sees the same key, and destroying a key is permanent.
///
/// HEF keeps no subject keys of its own: it asks this store for a key when it seals a payload and again when it opens
/// one. How many keys the store holds is the caller's choice of subject (see the module docs).
///
/// See: hef-security-and-isolation/spec.md
pub trait SubjectKeyStore {
    /// The key to open `subject`'s sealed payloads with, or `None` once that key has been destroyed (or if it never
    /// existed). `None` makes every payload sealed for the subject read as a tombstone.
    fn opening_key(&self, subject: SubjectId) -> Result<Option<SubjectContentKey>, StorageError>;

    /// The key to seal `subject`'s new payloads with, created on first use. Returns `None` once the subject's key has
    /// been destroyed: an erased subject never gets a fresh key, so nothing new is written for them. The key must
    /// resume past every epoch an earlier checkout sealed under (see [`SubjectContentKey::from_sealing_key`]), or a
    /// restarted writer can repeat a nonce.
    fn sealing_key(&self, subject: SubjectId) -> Result<Option<SubjectContentKey>, StorageError>;
}

/// One row's payload as read through a [`SubjectKeyStore`].
#[derive(Debug, Clone, PartialEq)]
pub enum SubjectPayloadRead {
    /// The payload was sealed for a subject whose key is live; this is the payload as it was written.
    Opened(VariantValue),
    /// The payload was not sealed for any subject; this is what [`HefFile::payload`] returns for the row.
    Plain(PayloadRead),
    /// The subject's key is live but the sealed bytes failed authentication: tampered, or copied from another row.
    Rejected,
    /// The subject's key has been destroyed: the payload is gone for good. The row's envelope still reads.
    Tombstone,
}

/// Seals `payload` for `subject` as the payload of event `event_id`, using the subject's key from `keys`. Returns the
/// bytes to store as the row's whole payload (as a [`VariantValue::Binary`]); [`read_subject_payload`] opens them.
///
/// Fails with [`SubjectPayloadError::SubjectErased`] when the subject's key has been destroyed.
pub fn seal_subject_payload(
    subject: SubjectId,
    event_id: EventId,
    payload: &EncodedPayload,
    keys: &dyn SubjectKeyStore,
) -> Result<Vec<u8>, SubjectPayloadError> {
    let mut key = keys
        .sealing_key(subject)
        .map_err(SubjectPayloadError::KeyStore)?
        .ok_or(SubjectPayloadError::SubjectErased)?;
    if key.is_destroyed() {
        return Err(SubjectPayloadError::SubjectErased);
    }
    let dictionary = encode_variant_dictionary(&payload.dictionary);
    let dictionary_len = u32::try_from(dictionary.len()).map_err(|_| {
        SubjectPayloadError::Format(FormatError::Structural {
            rule: "a sealed payload's key dictionary fits in u32 bytes",
        })
    })?;
    let mut plaintext = Writer::with_capacity(4 + dictionary.len() + payload.bytes.len());
    plaintext.put_u32(dictionary_len);
    plaintext.put_slice(&dictionary);
    plaintext.put_slice(&payload.bytes);
    let event_id = event_id.uuid().as_u128();
    let sealed = key
        .encrypt(seal_block_id(event_id), event_id, plaintext.bytes())
        .ok_or(SubjectPayloadError::SealRefused)?;
    let mut stored = Writer::with_capacity(SEALED_PAYLOAD_MAGIC.len() + 16 + sealed.len());
    stored.put_slice(SEALED_PAYLOAD_MAGIC);
    stored.put_u128(subject.uuid().as_u128());
    stored.put_slice(&sealed);
    Ok(stored.into_bytes())
}

/// Reads row `row_ordinal`'s payload, opening it with the subject's key from `keys` when it was sealed for a subject.
/// A row that was never sealed comes back exactly as [`HefFile::payload`] returns it; a row whose subject has been
/// erased comes back as a tombstone, never as an error.
pub fn read_subject_payload(
    file: &HefFile,
    row_ordinal: u64,
    keys: &dyn SubjectKeyStore,
) -> Result<SubjectPayloadRead, SubjectPayloadError> {
    let payload = file.payload(row_ordinal).map_err(SubjectPayloadError::Format)?;
    if let PayloadRead::Value(VariantValue::Binary(stored)) = &payload
        && let Some((subject, sealed)) = split_sealed(stored)
    {
        let event_id = row_event_id(file, row_ordinal).map_err(SubjectPayloadError::Format)?;
        return open_sealed(subject, event_id, sealed, keys);
    }
    Ok(SubjectPayloadRead::Plain(payload))
}

/// The block id a sealed payload binds: the low 64 bits of its event id, the random half of a UUIDv7, so payloads of
/// different events under one subject key derive different nonces and subkeys.
fn seal_block_id(event_id: u128) -> u64 {
    event_id as u64
}

/// Splits a stored payload into the subject it was sealed for and the sealed blob, or `None` when it does not carry
/// the sealed-payload marker (an ordinary binary payload).
fn split_sealed(stored: &[u8]) -> Option<(SubjectId, &[u8])> {
    let rest = stored.strip_prefix(SEALED_PAYLOAD_MAGIC.as_slice())?;
    let mut reader = Reader::new(rest);
    let subject = reader.u128("sealed payload subject").ok()?;
    let sealed = reader.take(reader.remaining(), "sealed payload blob").ok()?;
    Some((SubjectId::from_uuid(uuid::Uuid::from_u128(subject)), sealed))
}

/// The event id stored for row `row_ordinal`, which the payload's seal is bound to.
fn row_event_id(file: &HefFile, row_ordinal: u64) -> Result<u128, FormatError> {
    let granules = &file.footer().granules;
    let index = granules.partition_point(|granule| granule.first_row_ordinal <= row_ordinal);
    let granule = index
        .checked_sub(1)
        .and_then(|index| granules.get(index))
        .ok_or(FormatError::RefOutOfRange {
            what: "row ordinal beyond granule directory",
        })?;
    let read = file.read_column(column_ids::EVENT_ID, granule.granule_id)?;
    let ColumnData::U128(event_ids) = &read.data else {
        return Err(FormatError::Structural {
            rule: "the event id column holds 128-bit ids",
        });
    };
    row_ordinal
        .checked_sub(granule.first_row_ordinal)
        .and_then(|row| usize::try_from(row).ok())
        .and_then(|row| event_ids.get(row))
        .copied()
        .ok_or(FormatError::RefOutOfRange {
            what: "row ordinal beyond its granule's event ids",
        })
}

/// Opens a blob sealed for `subject` as the payload of event `event_id`: the payload when the subject's key is live
/// and the blob authenticates, a tombstone once the key is gone, a rejection when the blob was tampered with or
/// belongs to another event.
fn open_sealed(
    subject: SubjectId,
    event_id: u128,
    sealed: &[u8],
    keys: &dyn SubjectKeyStore,
) -> Result<SubjectPayloadRead, SubjectPayloadError> {
    let Some(key) = keys.opening_key(subject).map_err(SubjectPayloadError::KeyStore)? else {
        return Ok(SubjectPayloadRead::Tombstone);
    };
    match rebuild_payload(&key, seal_block_id(event_id), event_id, sealed) {
        RebuiltPayload::Payload(plaintext) => decode_plaintext(&plaintext)
            .map(SubjectPayloadRead::Opened)
            .map_err(SubjectPayloadError::Format),
        RebuiltPayload::Rejected => Ok(SubjectPayloadRead::Rejected),
        RebuiltPayload::Tombstone => Ok(SubjectPayloadRead::Tombstone),
    }
}

/// Decodes an opened payload: its key dictionary's length (`u32`), the dictionary, then the encoded value.
fn decode_plaintext(plaintext: &[u8]) -> Result<VariantValue, FormatError> {
    let mut reader = Reader::new(plaintext);
    let dictionary_len = reader.u32("sealed payload dictionary length")? as usize;
    let dictionary = decode_variant_dictionary(reader.take(dictionary_len, "sealed payload dictionary")?)?;
    let value = reader.take(reader.remaining(), "sealed payload value")?;
    VariantRef::new(value).decode(&dictionary)
}

#[cfg(test)]
#[path = "test/subject_payload.rs"]
mod tests;
