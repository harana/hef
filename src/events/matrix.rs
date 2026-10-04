//! Re-verifies Matrix federation events offline, from nothing but the event's original JSON bytes, its room version,
//! and the signing servers' public keys.
//!
//! A Matrix event (a PDU) is signed by its origin server and often by other servers that relay it. Every server signs
//! a redacted copy of the event in canonical JSON, the event carries a hash of its full content, and from room version
//! 3 on the event's own id is the hash of that same redacted copy. This module rebuilds those exact bytes from the
//! stored original, so an auditor can redo every check years later without the wire message and without trusting the
//! store.
//!
//! See: hef-logical-event-model/spec.md

use super::constant::{MATRIX_MAX_SAFE_INTEGER, MATRIX_ROOM_VERSIONS};
use super::provenance::{SignerSignature, push_json_string, sha256_digest};
use crate::error::ProvenanceError;
use base64ct::{Base64Unpadded, Base64UrlUnpadded, Encoding};
use serde_json::{Map, Value};

/// What a Matrix event carries beyond its JSON to prove who sent it: the room version that fixes the redaction and
/// event-id rules, and every server signature with the key that made it.
///
/// Built only through [`MatrixProvenance::new`], so a stored value always names a room version this engine can
/// re-verify and carries at least one signature.
///
/// See: hef-logical-event-model/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatrixProvenance {
    room_version: String,
    signatures: Vec<SignerSignature>,
}

impl MatrixProvenance {
    /// The provenance of one event in a room of `room_version`, signed by `signatures`. Refuses a room version this
    /// engine has no rules for and an empty signature list, since either would store a claim nothing can check.
    pub fn new(room_version: &str, signatures: Vec<SignerSignature>) -> Result<Self, ProvenanceError> {
        RoomRules::for_version(room_version)?;
        if signatures.is_empty() {
            return Err(ProvenanceError::NoSignatures);
        }
        Ok(Self {
            room_version: room_version.to_owned(),
            signatures,
        })
    }

    /// The room version, as Matrix spells it (`"10"`, say).
    pub fn room_version(&self) -> &str {
        &self.room_version
    }

    /// Every server signature, in the order they were declared.
    pub fn signatures(&self) -> &[SignerSignature] {
        &self.signatures
    }

    /// Re-verifies the event from its original JSON bytes and the id it is stored under: the content hash the event
    /// carries must match its content, every stored signature must verify over the redacted canonical form, and
    /// `event_id` must be the event's id - recomputed from the reference hash in room version 3 and later, read from
    /// the signed event itself in versions 1 and 2.
    ///
    /// Returns `Ok(())` only when every check passes.
    pub fn verify(&self, raw_event: &[u8], event_id: &[u8]) -> Result<(), ProvenanceError> {
        let rules = RoomRules::for_version(&self.room_version)?;
        let event = parse_event(raw_event)?;
        check_content_hash(&event)?;
        let signed = redacted_canonical(rules, &event)?;
        for signature in &self.signatures {
            signature.verify(&signed)?;
        }
        if event_id_of(rules, &event, &signed)?.as_bytes() != event_id {
            return Err(ProvenanceError::EventIdMismatch);
        }
        Ok(())
    }
}

/// The canonical JSON form of an event's original bytes: object keys sorted by code point, no insignificant
/// whitespace, UTF-8 throughout, and only the escapes the form requires. Refuses anything canonical JSON cannot hold,
/// such as fractional numbers or integers beyond 2^53 - 1.
pub fn canonical_json(raw_event: &[u8]) -> Result<Vec<u8>, ProvenanceError> {
    let value: Value = serde_json::from_slice(raw_event).map_err(|_| ProvenanceError::MalformedEvent {
        rule: "the stored bytes are not JSON",
    })?;
    let mut out = String::with_capacity(raw_event.len());
    push_canonical(&mut out, &value)?;
    Ok(out.into_bytes())
}

/// The exact bytes every server signed: the event redacted under `room_version`'s rules, without its signatures and
/// unsigned data, in canonical JSON. Hashing these bytes gives the event's reference hash.
pub fn signing_bytes(room_version: &str, raw_event: &[u8]) -> Result<Vec<u8>, ProvenanceError> {
    redacted_canonical(RoomRules::for_version(room_version)?, &parse_event(raw_event)?)
}

/// The event's reference hash: the SHA-256 of [`signing_bytes`].
pub fn reference_hash(room_version: &str, raw_event: &[u8]) -> Result<[u8; 32], ProvenanceError> {
    Ok(sha256_digest(&signing_bytes(room_version, raw_event)?))
}

/// The event's id: `$` and its reference hash in unpadded base64 in room version 3, the URL-safe alphabet from
/// version 4 on, and the `event_id` the event itself carries in versions 1 and 2.
pub fn event_id(room_version: &str, raw_event: &[u8]) -> Result<String, ProvenanceError> {
    let rules = RoomRules::for_version(room_version)?;
    let event = parse_event(raw_event)?;
    event_id_of(rules, &event, &redacted_canonical(rules, &event)?)
}

/// The content hash Matrix puts in `hashes.sha256`: the SHA-256 of the event's canonical JSON without its
/// `hashes`, `signatures`, and `unsigned` keys, in unpadded base64.
pub fn content_hash(raw_event: &[u8]) -> Result<String, ProvenanceError> {
    Ok(Base64Unpadded::encode_string(&content_digest(&parse_event(
        raw_event,
    )?)?))
}

/// The redaction and event-id rules of one room version, kept as its number since every rule is a version
/// threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RoomRules {
    version: usize,
}

impl RoomRules {
    fn for_version(room_version: &str) -> Result<Self, ProvenanceError> {
        MATRIX_ROOM_VERSIONS
            .iter()
            .position(|known| *known == room_version)
            .map(|index| Self { version: index + 1 })
            .ok_or(ProvenanceError::UnknownProtocolVersion)
    }

    /// Whether redaction keeps this top-level key.
    fn keeps_top_level(self, key: &str) -> bool {
        match key {
            "auth_events" | "content" | "depth" | "event_id" | "hashes" | "origin_server_ts" | "prev_events"
            | "room_id" | "sender" | "signatures" | "state_key" | "type" => true,
            // Room version 11 stopped protecting these three.
            "membership" | "origin" | "prev_state" => self.version < 11,
            _ => false,
        }
    }

    /// The content keys redaction keeps for an event of `event_type`.
    fn content_keys(self, event_type: &str) -> Vec<&'static str> {
        let mut keys = Vec::new();
        match event_type {
            "m.room.aliases" if self.version <= 5 => keys.push("aliases"),
            "m.room.create" => keys.push("creator"),
            "m.room.history_visibility" => keys.push("history_visibility"),
            "m.room.join_rules" => {
                keys.push("join_rule");
                if self.version >= 8 {
                    keys.push("allow");
                }
            }
            "m.room.member" => {
                keys.push("membership");
                if self.version >= 9 {
                    keys.push("join_authorised_via_users_server");
                }
            }
            "m.room.power_levels" => {
                keys.extend([
                    "ban",
                    "events",
                    "events_default",
                    "kick",
                    "redact",
                    "state_default",
                    "users",
                    "users_default",
                ]);
                if self.version >= 11 {
                    keys.push("invite");
                }
            }
            "m.room.redaction" if self.version >= 11 => keys.push("redacts"),
            _ => {}
        }
        keys
    }
}

fn parse_event(raw_event: &[u8]) -> Result<Map<String, Value>, ProvenanceError> {
    match serde_json::from_slice(raw_event) {
        Ok(Value::Object(event)) => Ok(event),
        _ => Err(ProvenanceError::MalformedEvent {
            rule: "the stored bytes are not a JSON object",
        }),
    }
}

/// The event with everything redaction strips removed, per the room version's rules.
fn redact(rules: RoomRules, event: &Map<String, Value>) -> Map<String, Value> {
    let mut kept: Map<String, Value> = event
        .iter()
        .filter(|(key, _)| rules.keeps_top_level(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
    let content = event.get("content").and_then(Value::as_object);
    let mut kept_content = Map::new();
    if let Some(content) = content {
        if event_type == "m.room.create" && rules.version >= 11 {
            // Room version 11 protects the whole create event content.
            kept_content = content.clone();
        } else {
            for key in rules.content_keys(event_type) {
                if let Some(value) = content.get(key) {
                    kept_content.insert(key.to_owned(), value.clone());
                }
            }
            // Room version 11 also keeps the signed block of a third-party invite, and nothing else of it.
            if event_type == "m.room.member"
                && rules.version >= 11
                && let Some(signed) = content
                    .get("third_party_invite")
                    .and_then(Value::as_object)
                    .and_then(|invite| invite.get("signed"))
            {
                let mut invite = Map::new();
                invite.insert("signed".to_owned(), signed.clone());
                kept_content.insert("third_party_invite".to_owned(), Value::Object(invite));
            }
        }
    }
    kept.insert("content".to_owned(), Value::Object(kept_content));
    kept
}

/// The redacted event without its signatures and unsigned data, in canonical JSON: what every server signed.
fn redacted_canonical(rules: RoomRules, event: &Map<String, Value>) -> Result<Vec<u8>, ProvenanceError> {
    let mut redacted = redact(rules, event);
    redacted.remove("signatures");
    redacted.remove("unsigned");
    let mut out = String::new();
    push_canonical(&mut out, &Value::Object(redacted))?;
    Ok(out.into_bytes())
}

fn content_digest(event: &Map<String, Value>) -> Result<[u8; 32], ProvenanceError> {
    let mut stripped = event.clone();
    stripped.remove("hashes");
    stripped.remove("signatures");
    stripped.remove("unsigned");
    let mut out = String::new();
    push_canonical(&mut out, &Value::Object(stripped))?;
    Ok(sha256_digest(out.as_bytes()))
}

fn check_content_hash(event: &Map<String, Value>) -> Result<(), ProvenanceError> {
    let carried = event
        .get("hashes")
        .and_then(Value::as_object)
        .and_then(|hashes| hashes.get("sha256"))
        .and_then(Value::as_str)
        .ok_or(ProvenanceError::MalformedEvent {
            rule: "the event carries no hashes.sha256",
        })?;
    let carried = Base64Unpadded::decode_vec(carried).map_err(|_| ProvenanceError::MalformedEvent {
        rule: "hashes.sha256 is not unpadded base64",
    })?;
    if carried != content_digest(event)? {
        return Err(ProvenanceError::ContentHashMismatch);
    }
    Ok(())
}

fn event_id_of(rules: RoomRules, event: &Map<String, Value>, signed: &[u8]) -> Result<String, ProvenanceError> {
    match rules.version {
        1 | 2 => {
            event
                .get("event_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(ProvenanceError::MalformedEvent {
                    rule: "room versions 1 and 2 carry the event id inside the event",
                })
        }
        3 => Ok(format!("${}", Base64Unpadded::encode_string(&sha256_digest(signed)))),
        _ => Ok(format!("${}", Base64UrlUnpadded::encode_string(&sha256_digest(signed)))),
    }
}

/// Appends `value` in canonical JSON.
fn push_canonical(out: &mut String, value: &Value) -> Result<(), ProvenanceError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            let integer = number
                .as_i64()
                .filter(|integer| (-MATRIX_MAX_SAFE_INTEGER..=MATRIX_MAX_SAFE_INTEGER).contains(integer))
                .ok_or(ProvenanceError::MalformedEvent {
                    rule: "canonical JSON numbers are integers within 2^53 - 1",
                })?;
            out.push_str(&integer.to_string());
        }
        Value::String(text) => push_json_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_canonical(out, item)?;
            }
            out.push(']');
        }
        Value::Object(fields) => {
            // Byte order of UTF-8 keys is code point order, which is what canonical JSON sorts by. Sorted here rather
            // than trusting the map's own order, which depends on how serde_json was built.
            let mut keys: Vec<&String> = fields.keys().collect();
            keys.sort_unstable();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                push_json_string(out, key);
                out.push(':');
                if let Some(field) = fields.get(key) {
                    push_canonical(out, field)?;
                }
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/matrix.rs"]
pub(crate) mod tests;
