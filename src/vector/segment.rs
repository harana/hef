//! Writes and reads vector segment files: one encrypted HEF file holding a batch of embeddings with their event ids
//! and the room, sender and time columns a search filters by.
//!
//! Layout: `[clear header][sealed block, 64-byte aligned]…[sealed footer][footer offset u64][footer len u64]["HEFV"]`.
//! Only the header (magic, version, nonce prefix, wrapped data key) is in the clear. Every block and the footer are
//! sealed on their own nonce, so a reader decrypts only the blocks it touches.

use super::constant::*;
use super::error::VectorError;
use super::kernel::{PreparedQuery, TopK, top_k};
use super::model::*;
use super::quantize::encode_row;
use super::seal::{FileKeys, WrappingKey};
use crate::file::bytes::{Reader, Writer};
use crate::indexes::bitmap::RoaringRangeBitmap;
use crate::layout::footer::ColumnKind;
use arrow_array::types::{Float16Type, Float32Type, UInt8Type};
use arrow_array::{ArrayRef, FixedSizeListArray, PrimitiveArray};
use arrow_buffer::{Buffer, ScalarBuffer};
use arrow_schema::Field;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use zeroize::Zeroizing;

/// What a new segment is and who made it.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone)]
pub struct SegmentSpec {
    pub model: EmbeddingModel,
    pub producer_device: String,
    /// The element types to store; at least one. `[U1, I8]` is the default pair: a 1-bit column to scan and an int8
    /// column to re-score with.
    pub representations: Vec<VectorElement>,
    pub room_id: Option<String>,
}

/// The rows of a new segment, column by column. Every column has one value per event id.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, Default)]
pub struct SegmentInput {
    pub event_ids: Vec<String>,
    pub integer_columns: Vec<(String, Vec<i64>)>,
    pub string_columns: Vec<(String, Vec<String>)>,
    /// One `dimension`-long vector per row, in the model's metric (normalized for cosine).
    pub vectors: Vec<Vec<f32>>,
}

/// The vector column id of `element`'s stored representation.
pub fn representation_column_id(element: VectorElement) -> u32 {
    EMBEDDING_COLUMN_ID + 1 + u32::from(element.code())
}

/// Writes a sealed segment file: rows split into granules, one sealed block per column per granule, every vector
/// block 64-byte aligned, and a sealed footer with the manifest and the per-granule min/max of every non-vector
/// column. The file's random data key is wrapped under `wrapping`.
///
/// Fails when a column or vector has the wrong length, a value is not finite, or a cosine vector is not normalized.
pub fn write_segment(spec: &SegmentSpec, input: &SegmentInput, wrapping: &WrappingKey) -> Result<Vec<u8>, VectorError> {
    validate(spec, input)?;
    let keys = FileKeys::random()?;
    let rows = input.event_ids.len();
    let granules: Vec<(usize, usize)> = if rows == 0 {
        vec![(0, 0)]
    } else {
        (0..rows.div_ceil(GRANULE_ROWS))
            .map(|granule| {
                let start = granule * GRANULE_ROWS;
                (start, (start + GRANULE_ROWS).min(rows))
            })
            .collect()
    };

    let mut file = keys.header(wrapping)?;
    let mut columns = Vec::new();
    let mut block_index = 0u64;
    let mut seal_block = |file: &mut Vec<u8>, plaintext: &[u8]| -> Result<BlockRef, VectorError> {
        file.resize(file.len().next_multiple_of(BLOCK_ALIGNMENT), 0);
        let sealed = keys.seal(block_index, plaintext)?;
        let reference = BlockRef {
            index: block_index,
            offset: file.len() as u64,
            sealed_len: sealed.len() as u64,
        };
        block_index += 1;
        file.extend_from_slice(&sealed);
        Ok(reference)
    };

    let mut string_columns: Vec<(&str, &[String])> = vec![(EVENT_ID_COLUMN, &input.event_ids)];
    string_columns.extend(
        input
            .string_columns
            .iter()
            .map(|(name, values)| (name.as_str(), values.as_slice())),
    );
    for (position, (name, values)) in string_columns.iter().enumerate() {
        let mut blocks = Vec::with_capacity(granules.len());
        for (start, end) in &granules {
            let slice = values.get(*start..*end).unwrap_or_default();
            let plaintext = Zeroizing::new(encode_strings(slice));
            let stats = string_stats(slice);
            blocks.push((seal_block(&mut file, &plaintext)?, stats));
        }
        columns.push(StoredColumn {
            blocks,
            column_id: position as u32,
            kind: ColumnKind::String,
            name: (*name).to_owned(),
        });
    }
    for (offset, (name, values)) in input.integer_columns.iter().enumerate() {
        let mut blocks = Vec::with_capacity(granules.len());
        for (start, end) in &granules {
            let slice = values.get(*start..*end).unwrap_or_default();
            let plaintext = Zeroizing::new(slice.iter().flat_map(|value| value.to_le_bytes()).collect::<Vec<u8>>());
            let stats = match (slice.iter().min(), slice.iter().max()) {
                (Some(min), Some(max)) => Some(ColumnStats::Integer { max: *max, min: *min }),
                _ => None,
            };
            blocks.push((seal_block(&mut file, &plaintext)?, stats));
        }
        columns.push(StoredColumn {
            blocks,
            column_id: (string_columns.len() + offset) as u32,
            kind: ColumnKind::I64,
            name: name.clone(),
        });
    }
    let mut vector_columns = Vec::with_capacity(spec.representations.len());
    for element in &spec.representations {
        let meta = VectorColumnMeta {
            column_id: representation_column_id(*element),
            element: *element,
            model: spec.model.clone(),
            representation_of: EMBEDDING_COLUMN_ID,
        };
        let mut blocks = Vec::with_capacity(granules.len());
        for (start, end) in &granules {
            let mut plaintext = Zeroizing::new(Vec::with_capacity((end - start) * meta.row_width()));
            for vector in input.vectors.get(*start..*end).unwrap_or_default() {
                encode_row(*element, vector, &mut plaintext);
            }
            blocks.push((seal_block(&mut file, &plaintext)?, None));
        }
        columns.push(StoredColumn {
            blocks,
            column_id: meta.column_id,
            kind: ColumnKind::Vector,
            name: format!("embedding_{:?}", element).to_lowercase(),
        });
        vector_columns.push(meta);
    }

    file.resize(file.len().next_multiple_of(BLOCK_ALIGNMENT), 0);
    let content_hash = *blake3::hash(file.get(HEADER_LEN..).unwrap_or_default()).as_bytes();
    let footer = SegmentFooter {
        columns,
        granule_rows: granules.iter().map(|(start, end)| (end - start) as u32).collect(),
        manifest: SegmentManifest {
            content_hash,
            model: spec.model.clone(),
            producer_device: spec.producer_device.clone(),
            representations: spec.representations.clone(),
            row_count: rows as u64,
            window: SegmentWindow {
                first_event: input.event_ids.first().cloned().unwrap_or_default(),
                last_event: input.event_ids.last().cloned().unwrap_or_default(),
                room_id: spec.room_id.clone(),
            },
        },
        vector_columns,
    };
    let plaintext = Zeroizing::new(footer.encode());
    let sealed = keys.seal(FOOTER_BLOCK_INDEX, &plaintext)?;
    let footer_offset = file.len() as u64;
    file.extend_from_slice(&sealed);
    file.extend_from_slice(&footer_offset.to_le_bytes());
    file.extend_from_slice(&(sealed.len() as u64).to_le_bytes());
    file.extend_from_slice(SEGMENT_MAGIC);
    Ok(file)
}

/// Replaces a segment's wrapped data key: unwraps it with `old` and wraps it under `new`, leaving every sealed block,
/// the footer and the content hash untouched. Used when the user memory key is rotated.
pub fn rewrap_segment(bytes: &[u8], old: &WrappingKey, new: &WrappingKey) -> Result<Vec<u8>, VectorError> {
    let header = bytes
        .get(..HEADER_LEN)
        .ok_or(VectorError::Format(crate::error::FormatError::Truncated {
            what: "vector segment header",
        }))?;
    let keys = FileKeys::from_header(header, old)?;
    let mut out = keys.header(new)?;
    out.extend_from_slice(bytes.get(HEADER_LEN..).unwrap_or_default());
    Ok(out)
}

fn validate(spec: &SegmentSpec, input: &SegmentInput) -> Result<(), VectorError> {
    let invalid = |rule: String| Err(VectorError::InvalidInput { rule });
    let rows = input.event_ids.len();
    if spec.representations.is_empty() {
        return invalid("a segment stores at least one vector representation".to_owned());
    }
    let mut seen = Vec::new();
    for element in &spec.representations {
        if seen.contains(element) {
            return invalid(format!("representation {element:?} listed twice"));
        }
        seen.push(*element);
    }
    if input.vectors.len() != rows {
        return invalid(format!("{} vectors for {rows} event ids", input.vectors.len()));
    }
    let mut names = vec![EVENT_ID_COLUMN];
    for (name, len) in input
        .string_columns
        .iter()
        .map(|(name, values)| (name, values.len()))
        .chain(input.integer_columns.iter().map(|(name, values)| (name, values.len())))
    {
        if names.contains(&name.as_str()) {
            return invalid(format!("column {name} declared twice"));
        }
        names.push(name);
        if len != rows {
            return invalid(format!("column {name} has {len} values for {rows} rows"));
        }
    }
    let dimension = spec.model.dimension as usize;
    for (row, vector) in input.vectors.iter().enumerate() {
        if vector.len() != dimension {
            return invalid(format!("row {row} has width {} not {dimension}", vector.len()));
        }
        if vector.iter().any(|value| !value.is_finite()) {
            return invalid(format!("row {row} holds a value that is not finite"));
        }
        if spec.model.metric == VectorMetric::Cosine {
            let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            if (norm - 1.0).abs() > NORMALIZATION_TOLERANCE {
                return invalid(format!("row {row} is not normalized (length {norm}) for cosine"));
            }
        }
    }
    Ok(())
}

/// Where one sealed block sits in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BlockRef {
    index: u64,
    offset: u64,
    sealed_len: u64,
}

/// The min and max of one non-vector column over one granule, for pruning granules and whole segments.
///
/// See: hef-vector-columns/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnStats {
    Integer { max: i64, min: i64 },
    String { max: String, min: String },
}

#[derive(Debug, Clone)]
struct StoredColumn {
    blocks: Vec<(BlockRef, Option<ColumnStats>)>,
    column_id: u32,
    kind: ColumnKind,
    name: String,
}

#[derive(Debug, Clone)]
struct SegmentFooter {
    columns: Vec<StoredColumn>,
    granule_rows: Vec<u32>,
    manifest: SegmentManifest,
    vector_columns: Vec<VectorColumnMeta>,
}

impl SegmentFooter {
    fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        self.manifest.encode_into(&mut out);
        out.put_u32(self.granule_rows.len() as u32);
        self.granule_rows.iter().for_each(|rows| out.put_u32(*rows));
        out.put_u32(self.columns.len() as u32);
        for column in &self.columns {
            out.put_u32(column.column_id);
            out.put_u8(column.kind as u8);
            put_str(&mut out, &column.name);
            for (block, stats) in &column.blocks {
                out.put_u64(block.index);
                out.put_u64(block.offset);
                out.put_u64(block.sealed_len);
                match stats {
                    None => out.put_u8(0),
                    Some(ColumnStats::Integer { max, min }) => {
                        out.put_u8(1);
                        out.put_i64(*min);
                        out.put_i64(*max);
                    }
                    Some(ColumnStats::String { max, min }) => {
                        out.put_u8(2);
                        put_str(&mut out, min);
                        put_str(&mut out, max);
                    }
                }
            }
        }
        out.put_u32(self.vector_columns.len() as u32);
        self.vector_columns.iter().for_each(|meta| meta.encode_into(&mut out));
        out.into_bytes()
    }

    fn decode(bytes: &[u8]) -> Result<Self, VectorError> {
        let mut reader = Reader::new(bytes);
        let manifest = SegmentManifest::decode_from(&mut reader)?;
        let granule_count = reader.u32("granule count")? as usize;
        let granule_rows = reader.u32_vec(granule_count, "granule rows")?;
        let column_count = reader.u32("column count")? as usize;
        let mut columns = Vec::with_capacity(reader.capacity_hint(column_count, 9));
        for _ in 0..column_count {
            let column_id = reader.u32("column id")?;
            let kind = ColumnKind::from_u8(reader.u8("column kind")?)?;
            let name = take_str(&mut reader, "column name")?;
            let mut blocks = Vec::with_capacity(reader.capacity_hint(granule_count, 25));
            for _ in 0..granule_count {
                let block = BlockRef {
                    index: reader.u64("block index")?,
                    offset: reader.u64("block offset")?,
                    sealed_len: reader.u64("block length")?,
                };
                let stats = match reader.u8("stats tag")? {
                    0 => None,
                    1 => Some(ColumnStats::Integer {
                        min: reader.i64("stats min")?,
                        max: reader.i64("stats max")?,
                    }),
                    2 => Some(ColumnStats::String {
                        min: take_str(&mut reader, "stats min")?,
                        max: take_str(&mut reader, "stats max")?,
                    }),
                    _ => {
                        return Err(VectorError::Format(crate::error::FormatError::Structural {
                            rule: "unknown vector segment stats tag",
                        }));
                    }
                };
                blocks.push((block, stats));
            }
            columns.push(StoredColumn {
                blocks,
                column_id,
                kind,
                name,
            });
        }
        let meta_count = reader.u32("vector column count")? as usize;
        let mut vector_columns = Vec::with_capacity(reader.capacity_hint(meta_count, 46));
        for _ in 0..meta_count {
            vector_columns.push(VectorColumnMeta::decode_from(&mut reader)?);
        }
        if reader.remaining() != 0 {
            return Err(VectorError::Format(crate::error::FormatError::Structural {
                rule: "vector segment footer has trailing bytes",
            }));
        }
        let footer = SegmentFooter {
            columns,
            granule_rows,
            manifest,
            vector_columns,
        };
        if footer.granule_rows.iter().map(|rows| u64::from(*rows)).sum::<u64>() != footer.manifest.row_count
            || !footer.columns.iter().any(|column| column.name == EVENT_ID_COLUMN)
        {
            return Err(VectorError::Format(crate::error::FormatError::Structural {
                rule: "vector segment footer rows and event id column agree",
            }));
        }
        Ok(footer)
    }
}

/// Where a reader gets a segment's bytes: a whole file in memory, or ranged reads from a file or object store.
///
/// See: hef-vector-columns/spec.md
pub trait SegmentSource {
    /// The file's total length in bytes.
    fn byte_len(&self) -> Result<u64, VectorError>;

    /// Reads `len` bytes starting at `offset`.
    fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, VectorError>;
}

impl<T: AsRef<[u8]>> SegmentSource for T {
    fn byte_len(&self) -> Result<u64, VectorError> {
        Ok(self.as_ref().len() as u64)
    }

    fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>, VectorError> {
        let start = usize::try_from(offset).map_err(|_| truncated())?;
        self.as_ref()
            .get(start..start.checked_add(len).ok_or_else(truncated)?)
            .map(<[u8]>::to_vec)
            .ok_or_else(truncated)
    }
}

fn truncated() -> VectorError {
    VectorError::Format(crate::error::FormatError::Truncated { what: "vector segment" })
}

/// An open, authenticated segment file. Opening reads only the header and footer; each later read decrypts just the
/// blocks it needs.
///
/// See: hef-vector-columns/spec.md
pub struct SegmentReader<S> {
    blocks_opened: AtomicU64,
    footer: SegmentFooter,
    keys: FileKeys,
    source: S,
}

impl<S: SegmentSource> SegmentReader<S> {
    /// Opens a segment: unwraps its data key with `wrapping` and authenticates its footer. Fails on the wrong key, a
    /// truncated file or any tampered byte of the header or footer.
    pub fn open(source: S, wrapping: &WrappingKey) -> Result<Self, VectorError> {
        let len = source.byte_len()?;
        if len < (HEADER_LEN + TRAILER_LEN) as u64 {
            return Err(truncated());
        }
        let keys = FileKeys::from_header(&source.read_range(0, HEADER_LEN)?, wrapping)?;
        let trailer = source.read_range(len - TRAILER_LEN as u64, TRAILER_LEN)?;
        if trailer.get(16..20) != Some(SEGMENT_MAGIC.as_slice()) {
            return Err(VectorError::Format(crate::error::FormatError::BadMagic {
                expected: "HEFV",
            }));
        }
        let mut reader = Reader::new(&trailer);
        let footer_offset = reader.u64("footer offset")?;
        let footer_len = reader.u64("footer length")?;
        if footer_offset.checked_add(footer_len) != Some(len - TRAILER_LEN as u64) {
            return Err(truncated());
        }
        let sealed = source.read_range(footer_offset, usize::try_from(footer_len).map_err(|_| truncated())?)?;
        let footer = SegmentFooter::decode(&keys.open(FOOTER_BLOCK_INDEX, &sealed)?)?;
        Ok(SegmentReader {
            blocks_opened: AtomicU64::new(0),
            footer,
            keys,
            source,
        })
    }

    pub fn manifest(&self) -> &SegmentManifest {
        &self.footer.manifest
    }

    pub fn row_count(&self) -> u64 {
        self.footer.manifest.row_count
    }

    pub fn granule_count(&self) -> usize {
        self.footer.granule_rows.len()
    }

    /// Rows in `granule`, or zero past the last one.
    pub fn granule_rows(&self, granule: usize) -> usize {
        self.footer.granule_rows.get(granule).map_or(0, |rows| *rows as usize)
    }

    /// The file row of `granule`'s first row.
    pub fn granule_first_row(&self, granule: usize) -> u64 {
        self.footer
            .granule_rows
            .iter()
            .take(granule)
            .map(|rows| u64::from(*rows))
            .sum()
    }

    /// The metadata of every vector column in the file.
    pub fn vector_columns(&self) -> &[VectorColumnMeta] {
        &self.footer.vector_columns
    }

    /// Every stored representation of logical embedding column `embedding_column`, coarsest first: the 1-bit, int8,
    /// f16 and f32 copies the file holds.
    pub fn representations(&self, embedding_column: u32) -> Vec<&VectorColumnMeta> {
        let mut found: Vec<&VectorColumnMeta> = self
            .footer
            .vector_columns
            .iter()
            .filter(|meta| meta.representation_of == embedding_column)
            .collect();
        found.sort_by_key(|meta| meta.element.precision());
        found
    }

    /// The min and max of column `name` over `granule`, read from the footer without decrypting any block.
    pub fn column_stats(&self, name: &str, granule: usize) -> Option<&ColumnStats> {
        self.column(name).ok()?.blocks.get(granule)?.1.as_ref()
    }

    /// How many sealed blocks this reader has decrypted so far. A scan touches only the blocks it reads.
    pub fn blocks_opened(&self) -> u64 {
        self.blocks_opened.load(Ordering::Relaxed)
    }

    /// Checks the sealed blocks against the manifest's content hash. Call after a download before trusting the file.
    pub fn verify_content(&self) -> Result<(), VectorError> {
        let trailer = self
            .source
            .read_range(self.source.byte_len()? - TRAILER_LEN as u64, 8)?;
        let footer_offset = u64::from_le_bytes(trailer.try_into().unwrap_or([0; 8]));
        let body = self.source.read_range(
            HEADER_LEN as u64,
            usize::try_from(footer_offset.saturating_sub(HEADER_LEN as u64)).map_err(|_| truncated())?,
        )?;
        if *blake3::hash(&body).as_bytes() != self.footer.manifest.content_hash {
            return Err(VectorError::Authentication { what: "content hash" });
        }
        Ok(())
    }

    /// Decrypts and returns `granule` of string column `name`.
    pub fn strings(&self, name: &str, granule: usize) -> Result<Vec<String>, VectorError> {
        let column = self.typed_column(name, ColumnKind::String)?;
        decode_strings(&self.open_block(column, granule)?)
    }

    /// Decrypts and returns `granule` of integer column `name`.
    pub fn integers(&self, name: &str, granule: usize) -> Result<Vec<i64>, VectorError> {
        let column = self.typed_column(name, ColumnKind::I64)?;
        let plaintext = self.open_block(column, granule)?;
        Ok(plaintext
            .chunks_exact(8)
            .map(|bytes| i64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
            .collect())
    }

    /// Reads `granule` of vector column `column_id` as an Arrow fixed-size list, after checking it was made by
    /// `model`. Float columns come out as lists of `dimension` floats; int8 and 1-bit columns as lists of each row's
    /// raw bytes, trailer included.
    pub fn vector_granule(
        &self,
        column_id: u32,
        granule: usize,
        model: &EmbeddingModel,
    ) -> Result<FixedSizeListArray, VectorError> {
        let meta = self.vector_meta(column_id, model)?;
        let column = self.column_by_id(column_id)?;
        let plaintext = self.open_block(column, granule)?;
        let buffer = Buffer::from(plaintext.as_slice());
        let (values, size): (ArrayRef, usize) = match meta.element {
            VectorElement::F32 => (
                Arc::new(PrimitiveArray::<Float32Type>::new(
                    ScalarBuffer::new(buffer, 0, plaintext.len() / 4),
                    None,
                )),
                meta.model.dimension as usize,
            ),
            VectorElement::F16 => (
                Arc::new(PrimitiveArray::<Float16Type>::new(
                    ScalarBuffer::new(buffer, 0, plaintext.len() / 2),
                    None,
                )),
                meta.model.dimension as usize,
            ),
            VectorElement::I8 | VectorElement::U1 => (
                Arc::new(PrimitiveArray::<UInt8Type>::new(
                    ScalarBuffer::new(buffer, 0, plaintext.len()),
                    None,
                )),
                meta.row_width(),
            ),
        };
        let field = Arc::new(Field::new("item", values.data_type().clone(), false));
        let size = i32::try_from(size).map_err(|_| VectorError::InvalidInput {
            rule: "vector row too wide for arrow".to_owned(),
        })?;
        FixedSizeListArray::try_new(field, size, values, None).map_err(|error| VectorError::InvalidInput {
            rule: error.to_string(),
        })
    }

    /// Reads file row `row` of vector column `column_id` as its stored bytes, located at `row × width` in its granule
    /// with no offset index.
    pub fn vector_row(&self, column_id: u32, row: u64, model: &EmbeddingModel) -> Result<Vec<u8>, VectorError> {
        let meta = self.vector_meta(column_id, model)?;
        let width = meta.row_width();
        let (granule, local) = self.locate(row)?;
        let plaintext = self.open_block(self.column_by_id(column_id)?, granule)?;
        plaintext
            .get(local * width..(local + 1) * width)
            .map(<[u8]>::to_vec)
            .ok_or_else(truncated)
    }

    /// The best `k` rows of vector column `column_id` for `query`, best first, scanning only the granules `granules`
    /// allows and only the rows `filter` allows. Fails if `model` is not the column's model and metric.
    pub fn top_k(
        &self,
        column_id: u32,
        query: &[f32],
        model: &EmbeddingModel,
        filter: Option<&RoaringRangeBitmap>,
        k: usize,
    ) -> Result<Vec<ScoredRow>, VectorError> {
        let meta = self.vector_meta(column_id, model)?;
        if query.len() != meta.model.dimension as usize {
            return Err(VectorError::InvalidInput {
                rule: format!("query width {} not {}", query.len(), meta.model.dimension),
            });
        }
        let prepared = PreparedQuery::new(query, meta.model.metric);
        let column = self.column_by_id(column_id)?;
        let mut best = TopK::new(k);
        let mut first_row = 0u64;
        for granule in 0..self.granule_count() {
            let rows = self.granule_rows(granule) as u64;
            let wanted = filter.is_none_or(|filter| {
                filter
                    .ranges()
                    .iter()
                    .any(|range| range.start < first_row + rows && range.end > first_row)
            });
            if wanted && rows > 0 {
                let plaintext = self.open_block(column, granule)?;
                top_k(meta, &plaintext, first_row, &prepared, filter, k)
                    .into_iter()
                    .for_each(|row| best.offer(row));
            }
            first_row += rows;
        }
        Ok(best.into_sorted())
    }

    /// Re-scores the given file rows with vector column `column_id`, decrypting only the granules they fall in.
    pub fn rescore(
        &self,
        column_id: u32,
        query: &[f32],
        model: &EmbeddingModel,
        rows: &[u64],
    ) -> Result<Vec<ScoredRow>, VectorError> {
        let meta = self.vector_meta(column_id, model)?;
        let prepared = PreparedQuery::new(query, meta.model.metric);
        let column = self.column_by_id(column_id)?;
        let width = meta.row_width();
        let mut sorted = rows.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut scored = Vec::with_capacity(sorted.len());
        let mut open: Option<(usize, Zeroizing<Vec<u8>>)> = None;
        for row in sorted {
            let (granule, local) = self.locate(row)?;
            if open.as_ref().is_none_or(|(current, _)| *current != granule) {
                open = Some((granule, self.open_block(column, granule)?));
            }
            let bytes = open
                .as_ref()
                .and_then(|(_, plaintext)| plaintext.get(local * width..(local + 1) * width))
                .ok_or_else(truncated)?;
            scored.push(ScoredRow {
                row,
                score: prepared.score(meta.element, bytes),
            });
        }
        Ok(scored)
    }

    /// The event id of each file row in `rows`.
    pub fn event_ids(&self, rows: &[u64]) -> Result<Vec<String>, VectorError> {
        let mut cache: Option<(usize, Vec<String>)> = None;
        rows.iter()
            .map(|row| {
                let (granule, local) = self.locate(*row)?;
                if cache.as_ref().is_none_or(|(current, _)| *current != granule) {
                    cache = Some((granule, self.strings(EVENT_ID_COLUMN, granule)?));
                }
                cache
                    .as_ref()
                    .and_then(|(_, ids)| ids.get(local).cloned())
                    .ok_or_else(truncated)
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn footer_block_offsets(&self) -> Vec<Vec<u64>> {
        self.footer
            .columns
            .iter()
            .map(|column| column.blocks.iter().map(|(block, _)| block.offset).collect())
            .collect()
    }

    fn locate(&self, row: u64) -> Result<(usize, usize), VectorError> {
        let mut first = 0u64;
        for (granule, rows) in self.footer.granule_rows.iter().enumerate() {
            let rows = u64::from(*rows);
            if row < first + rows {
                return Ok((granule, (row - first) as usize));
            }
            first += rows;
        }
        Err(VectorError::InvalidInput {
            rule: format!("row {row} past the segment's {first} rows"),
        })
    }

    fn vector_meta(&self, column_id: u32, model: &EmbeddingModel) -> Result<&VectorColumnMeta, VectorError> {
        let meta = self
            .footer
            .vector_columns
            .iter()
            .find(|meta| meta.column_id == column_id)
            .ok_or_else(|| VectorError::MissingColumn {
                name: column_id.to_string(),
            })?;
        meta.model.require_same(model)?;
        Ok(meta)
    }

    fn column(&self, name: &str) -> Result<&StoredColumn, VectorError> {
        self.footer
            .columns
            .iter()
            .find(|column| column.name == name)
            .ok_or_else(|| VectorError::MissingColumn { name: name.to_owned() })
    }

    fn typed_column(&self, name: &str, kind: ColumnKind) -> Result<&StoredColumn, VectorError> {
        let column = self.column(name)?;
        if column.kind != kind {
            return Err(VectorError::MissingColumn {
                name: format!("{name} as {kind:?}"),
            });
        }
        Ok(column)
    }

    fn column_by_id(&self, column_id: u32) -> Result<&StoredColumn, VectorError> {
        self.footer
            .columns
            .iter()
            .find(|column| column.column_id == column_id)
            .ok_or_else(|| VectorError::MissingColumn {
                name: column_id.to_string(),
            })
    }

    fn open_block(&self, column: &StoredColumn, granule: usize) -> Result<Zeroizing<Vec<u8>>, VectorError> {
        let (block, _) = column.blocks.get(granule).ok_or_else(|| VectorError::InvalidInput {
            rule: format!("granule {granule} past the segment's last"),
        })?;
        let sealed = self.source.read_range(
            block.offset,
            usize::try_from(block.sealed_len).map_err(|_| truncated())?,
        )?;
        self.blocks_opened.fetch_add(1, Ordering::Relaxed);
        self.keys.open(block.index, &sealed)
    }
}

impl<S> std::fmt::Debug for SegmentReader<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentReader")
            .field("rows", &self.footer.manifest.row_count)
            .finish_non_exhaustive()
    }
}

fn encode_strings(values: &[String]) -> Vec<u8> {
    let mut out = Writer::new();
    out.put_u32(values.len() as u32);
    values.iter().for_each(|value| put_str(&mut out, value));
    out.into_bytes()
}

fn decode_strings(bytes: &[u8]) -> Result<Vec<String>, VectorError> {
    let mut reader = Reader::new(bytes);
    let count = reader.u32("string count")? as usize;
    let mut values = Vec::with_capacity(reader.capacity_hint(count, 4));
    for _ in 0..count {
        values.push(take_str(&mut reader, "string value")?);
    }
    Ok(values)
}

fn string_stats(values: &[String]) -> Option<ColumnStats> {
    Some(ColumnStats::String {
        max: values.iter().max()?.clone(),
        min: values.iter().min()?.clone(),
    })
}
