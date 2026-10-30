//! The domain types of vector columns: what a stored vector is, how it is compared, and which model made it.

use super::constant::*;
use super::error::VectorError;
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};

/// How each element of a stored vector is written.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VectorElement {
    F16,
    F32,
    /// Signed bytes with a per-row `scale | offset` trailer.
    I8,
    /// One sign bit per dimension with a per-row `norm | alignment` trailer.
    U1,
}

impl VectorElement {
    /// The element's one-byte code in the footer.
    pub fn code(self) -> u8 {
        match self {
            VectorElement::F16 => 0,
            VectorElement::F32 => 1,
            VectorElement::I8 => 2,
            VectorElement::U1 => 3,
        }
    }

    /// Reads an element type from its footer code, or an error for an unknown value.
    pub fn from_code(code: u8) -> Result<Self, FormatError> {
        Ok(match code {
            0 => VectorElement::F16,
            1 => VectorElement::F32,
            2 => VectorElement::I8,
            3 => VectorElement::U1,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown vector element type",
                });
            }
        })
    }

    /// How close to the original vector this element type keeps scores; higher is finer. Used to pick the column a
    /// search re-scores with.
    pub fn precision(self) -> u8 {
        match self {
            VectorElement::U1 => 0,
            VectorElement::I8 => 1,
            VectorElement::F16 => 2,
            VectorElement::F32 => 3,
        }
    }

    /// Bytes one row of `dimension` elements takes, trailer included. Every row of a column has this width, so row
    /// `n` starts at `n × width`.
    pub fn row_width(self, dimension: u32) -> usize {
        let dimension = dimension as usize;
        match self {
            VectorElement::F16 => dimension * 2,
            VectorElement::F32 => dimension * 4,
            VectorElement::I8 => dimension + I8_TRAILER_BYTES,
            VectorElement::U1 => dimension.div_ceil(8) + U1_TRAILER_BYTES,
        }
    }
}

/// How two vectors are compared. Scores are always "higher is closer".
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VectorMetric {
    /// Cosine similarity; vectors must be stored normalized, so it is computed as a dot product.
    Cosine,
    Dot,
    /// Scored as the negated squared distance.
    L2,
}

impl VectorMetric {
    /// The metric's one-byte code in the footer.
    pub fn code(self) -> u8 {
        match self {
            VectorMetric::Cosine => 0,
            VectorMetric::Dot => 1,
            VectorMetric::L2 => 2,
        }
    }

    /// Reads a metric from its footer code, or an error for an unknown value.
    pub fn from_code(code: u8) -> Result<Self, FormatError> {
        Ok(match code {
            0 => VectorMetric::Cosine,
            1 => VectorMetric::Dot,
            2 => VectorMetric::L2,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown vector metric",
                });
            }
        })
    }
}

/// The model that produced a set of vectors. Vectors are comparable only when every field matches.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmbeddingModel {
    pub dimension: u32,
    /// A digest of the model weights or a version string's hash; two builds of one model id differ here.
    pub digest: [u8; 32],
    pub id: String,
    pub metric: VectorMetric,
}

impl EmbeddingModel {
    /// Fails unless `other` is the same model with the same metric and dimension.
    pub fn require_same(&self, other: &EmbeddingModel) -> Result<(), VectorError> {
        if self == other {
            return Ok(());
        }
        Err(VectorError::ModelMismatch {
            detail: format!(
                "stored {}/{:?}/{} vs requested {}/{:?}/{}",
                self.id, self.metric, self.dimension, other.id, other.metric, other.dimension
            ),
        })
    }

    pub(crate) fn encode_into(&self, out: &mut Writer) {
        out.put_u32(self.dimension);
        out.put_slice(&self.digest);
        put_str(out, &self.id);
        out.put_u8(self.metric.code());
    }

    pub(crate) fn decode_from(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        let dimension = reader.u32("vector dimension")?;
        let mut digest = [0u8; 32];
        digest.copy_from_slice(reader.take(32, "vector model digest")?);
        let id = take_str(reader, "vector model id")?;
        let metric = VectorMetric::from_code(reader.u8("vector metric")?)?;
        if dimension == 0 {
            return Err(FormatError::Structural {
                rule: "vector dimension must be positive",
            });
        }
        Ok(EmbeddingModel {
            dimension,
            digest,
            id,
            metric,
        })
    }
}

/// The footer metadata of one vector column: which model made it, how it is stored, and which logical embedding it is
/// a representation of.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorColumnMeta {
    pub column_id: u32,
    pub element: VectorElement,
    pub model: EmbeddingModel,
    /// The logical embedding column this one stores a version of; every 1-bit, int8 and float copy of the same
    /// vectors names the same id, so a reader finds the finer copy without naming conventions.
    pub representation_of: u32,
}

impl VectorColumnMeta {
    /// Bytes one row of this column takes, trailer included.
    pub fn row_width(&self) -> usize {
        self.element.row_width(self.model.dimension)
    }

    pub(crate) fn encode_into(&self, out: &mut Writer) {
        out.put_u32(self.column_id);
        out.put_u8(self.element.code());
        self.model.encode_into(out);
        out.put_u32(self.representation_of);
    }

    pub(crate) fn decode_from(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        Ok(VectorColumnMeta {
            column_id: reader.u32("vector column id")?,
            element: VectorElement::from_code(reader.u8("vector element")?)?,
            model: EmbeddingModel::decode_from(reader)?,
            representation_of: reader.u32("vector representation link")?,
        })
    }
}

/// Which room and events a segment covers. Readable from the manifest without scanning any block.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentWindow {
    pub first_event: String,
    pub last_event: String,
    /// `None` for a file that is not tied to one room, such as a user's memory file.
    pub room_id: Option<String>,
}

/// What a segment holds, sealed in its footer: enough to announce, check, replace and de-duplicate the file between
/// devices without reading its rows.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentManifest {
    /// BLAKE3 over the sealed blocks (every byte between the clear header and the sealed footer). Rewrapping the data
    /// key under a new user key leaves it unchanged.
    pub content_hash: [u8; 32],
    pub model: EmbeddingModel,
    pub producer_device: String,
    pub representations: Vec<VectorElement>,
    pub row_count: u64,
    pub window: SegmentWindow,
}

impl SegmentManifest {
    pub(crate) fn encode_into(&self, out: &mut Writer) {
        out.put_slice(&self.content_hash);
        self.model.encode_into(out);
        put_str(out, &self.producer_device);
        out.put_u32(self.representations.len() as u32);
        for element in &self.representations {
            out.put_u8(element.code());
        }
        out.put_u64(self.row_count);
        put_str(out, &self.window.first_event);
        put_str(out, &self.window.last_event);
        match &self.window.room_id {
            Some(room) => {
                out.put_u8(1);
                put_str(out, room);
            }
            None => out.put_u8(0),
        }
    }

    pub(crate) fn decode_from(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        let mut content_hash = [0u8; 32];
        content_hash.copy_from_slice(reader.take(32, "manifest content hash")?);
        let model = EmbeddingModel::decode_from(reader)?;
        let producer_device = take_str(reader, "manifest producer device")?;
        let count = reader.u32("manifest representation count")? as usize;
        let mut representations = Vec::with_capacity(reader.capacity_hint(count, 1));
        for _ in 0..count {
            representations.push(VectorElement::from_code(reader.u8("manifest representation")?)?);
        }
        let row_count = reader.u64("manifest row count")?;
        let first_event = take_str(reader, "manifest first event")?;
        let last_event = take_str(reader, "manifest last event")?;
        let room_id = match reader.u8("manifest room tag")? {
            0 => None,
            1 => Some(take_str(reader, "manifest room id")?),
            _ => {
                return Err(FormatError::Structural {
                    rule: "manifest room tag must be 0 or 1",
                });
            }
        };
        Ok(SegmentManifest {
            content_hash,
            model,
            producer_device,
            representations,
            row_count,
            window: SegmentWindow {
                first_event,
                last_event,
                room_id,
            },
        })
    }
}

/// One search result: which row of which segment matched and how closely. Higher scores are closer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoredRow {
    pub row: u64,
    pub score: f32,
}

pub(crate) fn put_str(out: &mut Writer, value: &str) {
    out.put_u32(value.len() as u32);
    out.put_slice(value.as_bytes());
}

pub(crate) fn take_str(reader: &mut Reader<'_>, what: &'static str) -> Result<String, FormatError> {
    let len = reader.u32(what)? as usize;
    let bytes = reader.take(len, what)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| FormatError::InvalidUtf8 { what })
}
