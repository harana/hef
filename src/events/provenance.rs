//! Keeps everything needed to prove, years later, who wrote an event that arrived over a signed protocol.
//!
//! An event that reaches the platform over a signed wire protocol (Nostr, or Matrix federation) carries the signers'
//! public keys, their signatures, and the protocol's own content-derived identifier. Those travel with the event into
//! storage and stay byte-exact, so a reader holding nothing but the stored form can rebuild the exact bytes the author
//! signed, recompute the identifier, and check the signature — without the original wire message and without trusting
//! this store.
//!
//! There is deliberately no stored "verified" flag: verification is a precondition of writing the event, and any later
//! check recomputes it from the bytes.
//!
//! See: hef-logical-event-model/spec.md

use super::constant::{NANOS_PER_SECOND, PROTOCOL_ID_BYTES, SIGNATURE_BYTES};
use super::envelope::TimestampValue;
use super::variant::VariantValue;
use crate::error::ProvenanceError;
use k256::schnorr::signature::hazmat::PrehashVerifier;
use k256::schnorr::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fmt::Write as _;

/// Payload field holding the protocol's message text.
pub const CONTENT_FIELD: &str = "content";

/// Payload field holding the protocol's tag list.
pub const TAGS_FIELD: &str = "tags";

/// The signature algorithms the provenance family recognises. The tag is registry-controlled: an event whose scheme is
/// not listed here cannot be written, so no stored signature is ever un-checkable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SignatureScheme {
    /// BIP-340 Schnorr over secp256k1, with the NIP-01 canonical serialization — the Nostr contract.
    Bip340SchnorrSecp256k1,
    /// Ed25519 over the canonical bytes themselves (no pre-hash) - the Matrix federation contract.
    Ed25519,
}

impl SignatureScheme {
    /// The stored tag for this scheme, as it appears in the `signature_scheme` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            SignatureScheme::Bip340SchnorrSecp256k1 => "bip340-schnorr-secp256k1",
            SignatureScheme::Ed25519 => "ed25519",
        }
    }

    /// The scheme a stored tag names, or `None` when the tag is not in the registry.
    pub fn from_str(tag: &str) -> Option<Self> {
        match tag {
            "bip340-schnorr-secp256k1" => Some(SignatureScheme::Bip340SchnorrSecp256k1),
            "ed25519" => Some(SignatureScheme::Ed25519),
            _ => None,
        }
    }
}

/// What a signed protocol event carries beyond the ordinary envelope: who signed it, the signature, and the
/// protocol's own identifier and claimed time.
///
/// Absent for streams whose events carry no signatures — those materialize no provenance columns at all. `claimed_at`
/// is author-supplied and is stored as data only: ordering, pruning, and retention read the envelope's ingest-side
/// fields, never this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedEventProvenance {
    pub author_pubkey: [u8; PROTOCOL_ID_BYTES],
    pub claimed_at: TimestampValue,
    pub protocol_event_id: [u8; PROTOCOL_ID_BYTES],
    pub protocol_kind: u32,
    pub scheme: SignatureScheme,
    pub signature: [u8; SIGNATURE_BYTES],
}

impl SignedEventProvenance {
    /// The author-claimed timestamp in the whole seconds the protocol serializes, as it entered the canonical bytes.
    pub fn claimed_at_seconds(&self) -> i64 {
        self.claimed_at.physical_nanos().div_euclid(NANOS_PER_SECOND)
    }

    /// Rebuilds the exact bytes the author signed, from the stored provenance and the stored payload.
    ///
    /// For the Nostr contract this is the NIP-01 canonical array over `pubkey`, `created_at`, `kind`, `tags`, and
    /// `content`. Fails when the payload does not hold the tags and content in the shape the protocol requires, since
    /// a guessed reconstruction would produce an identifier that does not match.
    pub fn canonical_bytes(&self, payload: &VariantValue) -> Result<Vec<u8>, ProvenanceError> {
        let content = payload_content(payload)?;
        let tags = payload_tags(payload)?;
        let mut out = String::with_capacity(content.len() + 128);
        out.push_str("[0,\"");
        // Appends hex straight into `out` instead of allocating a separate hex string to copy from.
        hex_simd::encode_append(self.author_pubkey, &mut out, hex_simd::AsciiCase::Lower);
        out.push_str("\",");
        // `String`'s `fmt::Write` impl never fails; the workspace still denies `expect_used`, so it's spelled out.
        #[allow(clippy::expect_used)]
        write!(out, "{}", self.claimed_at_seconds()).expect("String writes never fail");
        out.push(',');
        #[allow(clippy::expect_used)]
        write!(out, "{}", self.protocol_kind).expect("String writes never fail");
        out.push(',');
        push_tags(&mut out, &tags);
        out.push(',');
        push_json_string(&mut out, content);
        out.push(']');
        Ok(out.into_bytes())
    }

    /// The identifier those canonical bytes hash to — what `protocol_event_id` must equal.
    pub fn recompute_protocol_event_id(canonical: &[u8]) -> [u8; PROTOCOL_ID_BYTES] {
        let mut hasher = Sha256::new();
        hasher.update(canonical);
        hasher.finalize().into()
    }

    /// Re-verifies this event offline from its stored form alone: rebuilds the canonical bytes, recomputes the
    /// protocol identifier, and checks the signature against the author's public key.
    ///
    /// Returns `Ok(())` only when both the identifier and the signature check out. Ingest calls this before the event
    /// may reach the journal, and an auditor calls the same function on an archived event years later.
    pub fn verify(&self, payload: &VariantValue) -> Result<(), ProvenanceError> {
        let canonical = self.canonical_bytes(payload)?;
        let recomputed = Self::recompute_protocol_event_id(&canonical);
        if recomputed != self.protocol_event_id {
            return Err(ProvenanceError::EventIdMismatch);
        }
        match self.scheme {
            // BIP-340 signs the message hash, which for this protocol is the event identifier itself.
            SignatureScheme::Bip340SchnorrSecp256k1 => {
                verify_bip340_prehash(&self.author_pubkey, &recomputed, &self.signature)
            }
            // Ed25519 signs the message itself, so it checks the canonical bytes rather than their hash.
            SignatureScheme::Ed25519 => verify_ed25519(&self.author_pubkey, &canonical, &self.signature),
        }
    }
}

/// Checks a BIP-340 Schnorr signature made over an already-hashed 32-byte message.
///
/// Event verification is the main caller, and it passes the event identifier as the hash. Protocol extensions that
/// sign something other than an event — an owner's statement authorising an agent key, say — hash their own preimage
/// and call this with the digest, so every Schnorr check on the platform runs through one implementation.
pub fn verify_bip340_prehash(
    author_pubkey: &[u8; PROTOCOL_ID_BYTES],
    prehash: &[u8; PROTOCOL_ID_BYTES],
    signature: &[u8; SIGNATURE_BYTES],
) -> Result<(), ProvenanceError> {
    let key = VerifyingKey::from_slice(author_pubkey).map_err(|_| ProvenanceError::MalformedKey)?;
    let signature = Signature::try_from(signature.as_slice()).map_err(|_| ProvenanceError::MalformedSignature)?;
    key.verify_prehash(prehash, &signature)
        .map_err(|_| ProvenanceError::SignatureRejected)
}

/// Checks an Ed25519 signature over `message`, rejecting the weak keys and non-canonical signatures the strict rules
/// refuse, so a signature that verifies here verifies the same way in every other strict implementation.
pub fn verify_ed25519(
    public_key: &[u8; PROTOCOL_ID_BYTES],
    message: &[u8],
    signature: &[u8; SIGNATURE_BYTES],
) -> Result<(), ProvenanceError> {
    let key = ed25519_dalek::VerifyingKey::from_bytes(public_key).map_err(|_| ProvenanceError::MalformedKey)?;
    let signature = ed25519_dalek::Signature::from_bytes(signature);
    key.verify_strict(message, &signature)
        .map_err(|_| ProvenanceError::SignatureRejected)
}

/// One party's signature over an event: who signed (a server name, say), which of their keys signed, that key's
/// public bytes, and the signature itself.
///
/// An event a protocol lets several parties sign carries one of these per signature, so every signature re-verifies
/// offline from the stored key alone, years after the signer rotated or retired that key. `signer` and `key_id` are
/// non-empty and hold no whitespace, which is what lets their stored text form be split back apart.
///
/// See: hef-logical-event-model/spec.md
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SignerSignature {
    pub key_id: String,
    pub public_key: [u8; PROTOCOL_ID_BYTES],
    pub scheme: SignatureScheme,
    pub signature: [u8; SIGNATURE_BYTES],
    pub signer: String,
}

impl SignerSignature {
    /// A signature entry, or [`ProvenanceError::MalformedSignerSignature`] when the signer or key id is empty or holds
    /// whitespace.
    pub fn new(
        signer: &str,
        key_id: &str,
        scheme: SignatureScheme,
        public_key: [u8; PROTOCOL_ID_BYTES],
        signature: [u8; SIGNATURE_BYTES],
    ) -> Result<Self, ProvenanceError> {
        if !is_token(signer) || !is_token(key_id) {
            return Err(ProvenanceError::MalformedSignerSignature);
        }
        Ok(Self {
            key_id: key_id.to_owned(),
            public_key,
            scheme,
            signature,
            signer: signer.to_owned(),
        })
    }

    /// Checks this signature over `message`, the exact bytes the protocol says the signer signed. A BIP-340 signer
    /// signs the SHA-256 of those bytes; an Ed25519 signer signs the bytes themselves.
    pub fn verify(&self, message: &[u8]) -> Result<(), ProvenanceError> {
        match self.scheme {
            SignatureScheme::Bip340SchnorrSecp256k1 => {
                verify_bip340_prehash(&self.public_key, &sha256_digest(message), &self.signature)
            }
            SignatureScheme::Ed25519 => verify_ed25519(&self.public_key, message, &self.signature),
        }
    }

    /// Reads one stored entry back: `<scheme> <signer> <key id> <public key hex> <signature hex>`, as
    /// [`Display`](fmt::Display) writes it.
    pub fn parse(text: &str) -> Result<Self, ProvenanceError> {
        let mut parts = text.split(' ');
        let (Some(scheme), Some(signer), Some(key_id), Some(public_key), Some(signature), None) = (
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
            parts.next(),
        ) else {
            return Err(ProvenanceError::MalformedSignerSignature);
        };
        let scheme = SignatureScheme::from_str(scheme).ok_or(ProvenanceError::MalformedSignerSignature)?;
        Self::new(signer, key_id, scheme, hex_bytes(public_key)?, hex_bytes(signature)?)
    }

    /// The stored text of several signatures: one entry per line, in the given order.
    pub fn join(signatures: &[SignerSignature]) -> String {
        signatures
            .iter()
            .map(SignerSignature::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Reads [`join`](Self::join)'s text back into its signatures.
    pub fn split(text: &str) -> Result<Vec<SignerSignature>, ProvenanceError> {
        text.split('\n').map(SignerSignature::parse).collect()
    }
}

impl fmt::Display for SignerSignature {
    /// The stored text form: `<scheme> <signer> <key id> <public key hex> <signature hex>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} {} {} {}",
            self.scheme.as_str(),
            self.signer,
            self.key_id,
            hex_lower(&self.public_key),
            hex_lower(&self.signature)
        )
    }
}

/// Whether `text` can stand as one space-separated field of a stored signature entry.
fn is_token(text: &str) -> bool {
    !text.is_empty() && !text.chars().any(char::is_whitespace)
}

/// The SHA-256 digest of `preimage` — the hash a caller of [`verify_bip340_prehash`] signs when what it is signing is
/// not an event.
pub fn sha256_digest(preimage: &[u8]) -> [u8; PROTOCOL_ID_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(preimage);
    hasher.finalize().into()
}

/// The lowercase hex form the protocol uses for byte-valued fields, and the form the provenance columns store.
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::new();
    hex_lower_into(bytes, &mut out);
    out
}

/// Appends [`hex_lower`]'s encoding of `bytes` to `out` instead of allocating a fresh `String` — for a caller
/// encoding many values in a row through one reused scratch buffer.
pub fn hex_lower_into(bytes: &[u8], out: &mut String) {
    hex_simd::encode_append(bytes, out, hex_simd::AsciiCase::Lower);
}

/// Reads a lowercase-hex field back into its bytes. Uppercase hex is refused: the protocol's canonical serialization
/// is lowercase, so accepting anything else would store bytes whose identifier can never be recomputed.
pub fn hex_bytes<const N: usize>(text: &str) -> Result<[u8; N], ProvenanceError> {
    if text.len() != N * 2 || text.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(ProvenanceError::MalformedHex);
    }
    let bytes = hex_simd::decode_to_vec(text).map_err(|_| ProvenanceError::MalformedHex)?;
    bytes.try_into().map_err(|_| ProvenanceError::MalformedHex)
}

fn payload_content(payload: &VariantValue) -> Result<&str, ProvenanceError> {
    match payload {
        VariantValue::Object(fields) => match fields.get(CONTENT_FIELD) {
            Some(VariantValue::String(content)) => Ok(content),
            _ => Err(ProvenanceError::PayloadShape { field: CONTENT_FIELD }),
        },
        _ => Err(ProvenanceError::PayloadShape { field: CONTENT_FIELD }),
    }
}

fn payload_tags(payload: &VariantValue) -> Result<Vec<Vec<&str>>, ProvenanceError> {
    let VariantValue::Object(fields) = payload else {
        return Err(ProvenanceError::PayloadShape { field: TAGS_FIELD });
    };
    let Some(VariantValue::Array(tags)) = fields.get(TAGS_FIELD) else {
        return Err(ProvenanceError::PayloadShape { field: TAGS_FIELD });
    };
    let mut out = Vec::with_capacity(tags.len());
    for tag in tags {
        let VariantValue::Array(items) = tag else {
            return Err(ProvenanceError::PayloadShape { field: TAGS_FIELD });
        };
        let mut row = Vec::with_capacity(items.len());
        for item in items {
            let VariantValue::String(value) = item else {
                return Err(ProvenanceError::PayloadShape { field: TAGS_FIELD });
            };
            row.push(value.as_str());
        }
        out.push(row);
    }
    Ok(out)
}

fn push_tags(out: &mut String, tags: &[Vec<&str>]) {
    out.push('[');
    for (index, tag) in tags.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('[');
        for (item_index, item) in tag.iter().enumerate() {
            if item_index > 0 {
                out.push(',');
            }
            push_json_string(out, item);
        }
        out.push(']');
    }
    out.push(']');
}

/// Writes one JSON string with exactly the escapes the protocol's canonical form mandates: line feed, double quote,
/// backslash, carriage return, tab, backspace, and form feed get their short escapes, every other control character
/// gets `\uXXXX`, and nothing else — in particular no escaping of non-ASCII — so the bytes match what the author
/// hashed.
pub(crate) fn push_json_string(out: &mut String, value: &str) {
    out.push('"');
    // Copies each run of characters that needs no escaping in one push_str instead of one push per character; only
    // an escape boundary flushes the run so far.
    let mut start = 0usize;
    for (index, character) in value.char_indices() {
        let escape = match character {
            '\n' => "\\n",
            '"' => "\\\"",
            '\\' => "\\\\",
            '\r' => "\\r",
            '\t' => "\\t",
            '\u{8}' => "\\b",
            '\u{c}' => "\\f",
            character if (character as u32) < 0x20 => {
                out.push_str(&value[start..index]);
                #[allow(clippy::expect_used)]
                write!(out, "\\u{:04x}", character as u32).expect("String writes never fail");
                start = index + character.len_utf8();
                continue;
            }
            _ => continue,
        };
        out.push_str(&value[start..index]);
        out.push_str(escape);
        start = index + character.len_utf8();
    }
    out.push_str(&value[start..]);
    out.push('"');
}

#[cfg(test)]
#[path = "test/provenance.rs"]
mod tests;
