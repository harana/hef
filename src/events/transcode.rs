//! Translates an incoming event body from its source format into the one canonical payload format the engine stores.
//!
//! Every accepted body is converted to the canonical value before it is written to the journal; the original source
//! bytes are not kept. This change wires up the JSON and raw-bytes converters; the Protobuf, Avro, and MessagePack
//! converters need registered source schemas and land with the connector-service work (design D7). The single-format
//! rule is already enforced everywhere: the journal rejects every other payload encoding.

use super::variant::{MAX_DEPTH, VariantValue};
use crate::error::FormatError;
use serde::de::{Deserialize, DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use std::collections::BTreeMap;
use std::fmt;

/// Source formats accepted at the ingest edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
    Json,
    /// Raw or undecodable binary bodies: stored as a single Variant binary scalar.
    RawBytes,
}

/// Transcodes one source body into the canonical variant value. Deterministic: object keys collect into the shared
/// dictionary at encode time, numbers map to the narrowest lossless variant numeric type, and raw bytes become one
/// binary scalar.
pub fn transcode(format: SourceFormat, body: &[u8]) -> Result<VariantValue, FormatError> {
    let mut scratch = Vec::new();
    transcode_with_scratch(format, body, &mut scratch)
}

/// Same as [`transcode`], but reuses `scratch` for JSON's in-place parse buffer instead of allocating a fresh one
/// every call — pass the same buffer across a worker's calls (cleared here, not dropped) so steady-state ingest
/// allocates nothing for it.
pub fn transcode_with_scratch(
    format: SourceFormat,
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
        SourceFormat::RawBytes => Ok(VariantValue::Binary(body.to_vec())),
    }
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
