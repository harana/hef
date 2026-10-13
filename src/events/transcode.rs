//! Translates an incoming event body from its source format into the one canonical payload format the engine stores.
//!
//! Every accepted body is converted to the canonical value before it is written to the journal; the original source
//! bytes are not kept, unless the stream opts into the raw-payload column that stores them beside the canonical value.
//! JSON, Protobuf, rkyv, and raw-bytes converters are wired up; Protobuf decodes against the message schema the ingest
//! route registered for the stream, and an rkyv body is an archived [`VariantValue`]. The Avro and MessagePack
//! converters land with the connector-service work (design D7). The single-format rule is already enforced everywhere:
//! the journal rejects every other payload encoding.

use super::constant::RKYV_MAX_SUBTREE_DEPTH;
use super::envelope::TimestampValue;
use super::variant::{ArchivedVariantValue, MAX_DEPTH, VariantValue};
use crate::error::FormatError;
use prost_reflect::{DynamicMessage, EnumDescriptor, MapKey, MessageDescriptor, ReflectMessage, Value};
use rkyv::rancor::Error as RkyvError;
use rkyv::util::AlignedVec;
use rkyv::validation::{Validator, archive::ArchiveValidator, shared::SharedValidator};
use serde::de::{Deserialize, DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;
use std::num::NonZeroUsize;

/// Source formats accepted at the ingest edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat<'a> {
    Json,
    /// A binary Protobuf message, decoded against the schema registered for the ingest route.
    Protobuf(&'a MessageDescriptor),
    /// Raw or undecodable binary bodies: stored as a single Variant binary scalar.
    RawBytes,
    /// A [`VariantValue`] serialized with `rkyv::to_bytes`, validated before it is read.
    Rkyv,
}

/// Transcodes one source body into the canonical variant value. Deterministic: object keys collect into the shared
/// dictionary at encode time, numbers map to the narrowest lossless variant numeric type, and raw bytes become one
/// binary scalar.
pub fn transcode(format: SourceFormat<'_>, body: &[u8]) -> Result<VariantValue, FormatError> {
    let mut scratch = Vec::new();
    transcode_with_scratch(format, body, &mut scratch)
}

/// Same as [`transcode`], but reuses `scratch` for JSON's in-place parse buffer instead of allocating a fresh one
/// every call — pass the same buffer across a worker's calls (cleared here, not dropped) so steady-state ingest
/// allocates nothing for it.
pub fn transcode_with_scratch(
    format: SourceFormat<'_>,
    body: &[u8],
    scratch: &mut Vec<u8>,
) -> Result<VariantValue, FormatError> {
    match format {
        SourceFormat::Json => {
            // simd-json requires a mutable buffer (in-place SIMD tokenisation); we copy once here so the caller's
            // bytes are unchanged.
            scratch.clear();
            scratch.extend_from_slice(body);
            // Depth is enforced inside the visitor (`visit_seq`/`visit_map`, mirroring `decode_at_depth`) rather than
            // by a separate pre-scan: simd-json's own tokenizer is iterative (an explicit heap stack, not Rust call
            // frames), so the visitor's own recursion — which we already bound — is the only stack-depth risk.
            let parsed: JsonBody = simd_json::serde::from_slice(scratch).map_err(|_| FormatError::Structural {
                rule: "declared-JSON ingest body did not parse as JSON",
            })?;
            Ok(parsed.0)
        }
        SourceFormat::Protobuf(descriptor) => {
            let message = DynamicMessage::decode(descriptor.clone(), body).map_err(|_| FormatError::Structural {
                rule: "declared-Protobuf ingest body did not decode against its registered schema",
            })?;
            protobuf_message(message)
        }
        SourceFormat::RawBytes => Ok(VariantValue::Binary(body.to_vec())),
        SourceFormat::Rkyv => rkyv_body(body),
    }
}

/// Validates an archived [`VariantValue`] and reads it out. rkyv reads values in place, so the body is first copied
/// to an address aligned for the archive's widest field. Validation follows at most [`RKYV_MAX_SUBTREE_DEPTH`]
/// nested hops, so a hostile body cannot exhaust the stack, and a value nested past [`MAX_DEPTH`] is refused as JSON's
/// is.
fn rkyv_body(body: &[u8]) -> Result<VariantValue, FormatError> {
    let mut aligned = AlignedVec::<16>::with_capacity(body.len());
    aligned.extend_from_slice(body);
    let mut validator = Validator::new(
        ArchiveValidator::with_max_depth(&aligned, NonZeroUsize::new(RKYV_MAX_SUBTREE_DEPTH)),
        SharedValidator::new(),
    );
    let value = rkyv::api::access_with_context::<ArchivedVariantValue, _, RkyvError>(&aligned, &mut validator)
        .and_then(rkyv::deserialize::<VariantValue, RkyvError>)
        .map_err(|_| FormatError::Structural {
            rule: "declared-rkyv ingest body is not a valid archived variant value",
        })?;
    if nesting_depth(&value) > MAX_DEPTH {
        return Err(FormatError::Structural {
            rule: "declared-rkyv ingest body exceeds the maximum nesting depth",
        });
    }
    Ok(value)
}

/// How many arrays and objects deep `value` nests; a scalar is 0.
fn nesting_depth(value: &VariantValue) -> usize {
    match value {
        VariantValue::Array(items) => 1 + items.iter().map(nesting_depth).max().unwrap_or(0),
        VariantValue::Object(fields) => 1 + fields.values().map(nesting_depth).max().unwrap_or(0),
        _ => 0,
    }
}

/// Converts one decoded Protobuf message into a canonical object keyed by field name. Only fields present in the
/// message are kept; unknown fields (numbers the schema does not declare) are dropped. A
/// `google.protobuf.Timestamp` becomes a timestamp value rather than an object. Nesting needs no bound here: the
/// Protobuf decoder already refuses messages nested past 100 levels, under the canonical format's limit.
fn protobuf_message(mut message: DynamicMessage) -> Result<VariantValue, FormatError> {
    if message.descriptor().full_name() == "google.protobuf.Timestamp" {
        return protobuf_timestamp(&message);
    }
    let mut fields = BTreeMap::new();
    // Drains the decoded values so strings and map keys move into the canonical tree instead of being cloned.
    for (field, value) in message.take_fields() {
        // Resolved once per field, not once per repeated item or map value. For a map field the enum comes from the
        // entry's value field.
        let enum_kind = match field.kind().as_message() {
            Some(entry) if field.is_map() => entry.map_entry_value_field().kind(),
            _ => field.kind(),
        }
        .as_enum()
        .cloned();
        let enum_kind = enum_kind.as_ref();
        let value = match value {
            Value::List(items) => {
                let mut array = Vec::with_capacity(items.len());
                for item in items {
                    array.push(protobuf_value(enum_kind, item)?);
                }
                VariantValue::Array(array)
            }
            Value::Map(entries) => {
                let mut object = BTreeMap::new();
                for (key, item) in entries {
                    object.insert(protobuf_map_key(key), protobuf_value(enum_kind, item)?);
                }
                VariantValue::Object(object)
            }
            other => protobuf_value(enum_kind, other)?,
        };
        fields.insert(field.name().to_owned(), value);
    }
    Ok(VariantValue::Object(fields))
}

/// Converts one scalar, enum, or nested-message value. `enum_kind` is the field's enum, if it holds one, which names
/// the enum numbers.
fn protobuf_value(enum_kind: Option<&EnumDescriptor>, value: Value) -> Result<VariantValue, FormatError> {
    Ok(match value {
        Value::Bool(value) => VariantValue::Bool(value),
        Value::Bytes(value) => VariantValue::Binary(value.to_vec()),
        Value::EnumNumber(number) => match enum_kind.and_then(|kind| kind.get_value(number)) {
            Some(known) => VariantValue::String(known.name().to_owned()),
            None => VariantValue::Int(i64::from(number)),
        },
        Value::F32(value) => VariantValue::Float(value),
        Value::F64(value) => VariantValue::Double(value),
        Value::I32(value) => VariantValue::Int(i64::from(value)),
        Value::I64(value) => VariantValue::Int(value),
        Value::List(_) | Value::Map(_) => {
            return Err(FormatError::Structural {
                rule: "declared-Protobuf ingest body nests a repeated or map value directly",
            });
        }
        Value::Message(message) => protobuf_message(message)?,
        Value::String(value) => VariantValue::String(value),
        Value::U32(value) => VariantValue::Int(i64::from(value)),
        Value::U64(value) => match i64::try_from(value) {
            Ok(value) => VariantValue::Int(value),
            // Beyond i64: preserved losslessly as a scale-0 decimal, as JSON does.
            Err(_) => VariantValue::Decimal {
                unscaled: i128::from(value),
                scale: 0,
            },
        },
    })
}

fn protobuf_map_key(key: MapKey) -> String {
    match key {
        MapKey::Bool(value) => value.to_string(),
        MapKey::I32(value) => value.to_string(),
        MapKey::I64(value) => value.to_string(),
        MapKey::String(value) => value,
        MapKey::U32(value) => value.to_string(),
        MapKey::U64(value) => value.to_string(),
    }
}

fn protobuf_timestamp(message: &DynamicMessage) -> Result<VariantValue, FormatError> {
    let seconds = message
        .get_field_by_name("seconds")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let nanos = message
        .get_field_by_name("nanos")
        .and_then(|value| value.as_i32())
        .unwrap_or(0);
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(i64::from(nanos)))
        .map(|nanos| VariantValue::Timestamp(TimestampValue::from_physical_nanos(nanos)))
        .ok_or(FormatError::Structural {
            rule: "declared-Protobuf timestamp is outside the representable range",
        })
}

/// One JSON body deserialized straight into the canonical value, so a parse builds the stored tree once instead of
/// building a `serde_json::Value` tree and copying it into a second one.
///
/// The mapping is the one the canonical format pins: JSON integers become variant integers, an unsigned integer past
/// `i64` is preserved losslessly as a scale-0 decimal, everything else fractional becomes a double, and object keys
/// sort as the canonical object requires.
struct JsonBody(VariantValue);

/// Drives one recursive step of the JSON-to-canonical conversion at `depth`, so nesting depth is enforced as the
/// serde bridge descends — the same place [`super::variant::VariantRef::decode`] enforces it — instead of by a
/// separate pre-scan pass over the whole body.
struct JsonBodySeed {
    depth: usize,
}

impl<'de> DeserializeSeed<'de> for JsonBodySeed {
    type Value = JsonBody;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(JsonBodyVisitor { depth: self.depth })
    }
}

struct JsonBodyVisitor {
    depth: usize,
}

impl<'de> Visitor<'de> for JsonBodyVisitor {
    type Value = JsonBody;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::Int(value)))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(JsonBody(match i64::try_from(value) {
            Ok(value) => VariantValue::Int(value),
            // Beyond i64: preserved losslessly as a scale-0 decimal.
            Err(_) => VariantValue::Decimal {
                unscaled: i128::from(value),
                scale: 0,
            },
        }))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::Double(value)))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::String(value)))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::Null))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(JsonBody(VariantValue::Null))
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(JsonBodyVisitor { depth: self.depth })
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let depth = self.depth + 1;
        if depth > MAX_DEPTH {
            return Err(A::Error::custom(
                "declared-JSON ingest body exceeds the maximum nesting depth",
            ));
        }
        let mut items = Vec::with_capacity(access.size_hint().unwrap_or(0));
        while let Some(JsonBody(item)) = access.next_element_seed(JsonBodySeed { depth })? {
            items.push(item);
        }
        Ok(JsonBody(VariantValue::Array(items)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
        let depth = self.depth + 1;
        if depth > MAX_DEPTH {
            return Err(A::Error::custom(
                "declared-JSON ingest body exceeds the maximum nesting depth",
            ));
        }
        let mut fields = BTreeMap::new();
        while let Some(key) = access.next_key::<String>()? {
            let JsonBody(value) = access.next_value_seed(JsonBodySeed { depth })?;
            fields.insert(key, value);
        }
        Ok(JsonBody(VariantValue::Object(fields)))
    }
}

impl<'de> Deserialize<'de> for JsonBody {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(JsonBodyVisitor { depth: 0 })
    }
}

#[cfg(test)]
#[path = "test/transcode.rs"]
mod tests;
