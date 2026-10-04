//! Squeezes each column of values into the smallest faithful byte layout, and reads it back exactly.
//!
//! For every block the writer tries the encoding families that suit the block's type on a sample, then keeps the
//! smallest valid one, recording the chosen pipeline id alongside the data so a reader knows how to decode it. A fixed
//! encoding is forced only where the spec demands one (money is always fixed-scale decimal). Every encoder here is the
//! software path — the correctness reference any future hardware acceleration must match byte-for-byte.
//!
//! Integer streams — frame-of-reference deltas, delta-of-delta residuals, ALP's scaled integers, and dictionary codes —
//! are bit-packed in the **FastLanes transposed layout**: the values are grouped into fixed 1024-value vectors and
//! reordered so a decoder recovers sixteen lanes at once with nothing but a shift and a mask. Requirement: "Bit-packed
//! integer streams use the FastLanes transposed layout".
//!
//! The hot kernels here — the FastLanes pack and unpack, the ALP scaling probe, and the ALP-RD split — are
//! `#[multiversion]`-dispatched: each is compiled once per instruction set (AVX-512, AVX2, NEON, portable) with its
//! loops shaped so the compiler vectorizes the sixteen independent lanes, and the best clone is picked from the CPU
//! features detected at runtime. Every clone computes the same shift/mask/IEEE operations, so all of them — and the
//! portable clone, which is the byte-for-byte software reference — produce identical bytes. Requirements: "SIMD
//! kernels dispatch on detected CPU features at runtime" and "Optional acceleration with mandatory software parity".

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer, slice};
use ahash::{AHashMap, AHashSet};
use arrow_array::StringViewArray;
use arrow_array::builder::{BooleanBufferBuilder, make_view};
use arrow_buffer::{BooleanBuffer, Buffer, NullBuffer, ScalarBuffer};
use constant::{
    ALP_CONVERT_MAGIC, ALP_EXACT_CONVERT_MAX_WIDTH, ALP_MAX_EXCEPTION_SAMPLE_DIVISOR, ALP_MAX_SCALED_MAGNITUDE,
    ALP_RD_CODE_MISS, ALP_RD_MAX_DICT_LEN, ALP_RD_MAX_RIGHT_WIDTH, ALP_RD_MIN_RIGHT_WIDTH, ALP_VECTOR_ESCAPE_SENTINEL,
    DECODE_OPTIMIZED_THRESHOLD_DEN, DECODE_OPTIMIZED_THRESHOLD_NUM, DICTIONARY_FSST_MAX_VALUE_LEN,
    DICTIONARY_MAX_CARDINALITY_DIVISOR, FASTLANES_SPARSE_MAX_VALUES, FSST_LOSSY_PHT_HASH_MULTIPLIER,
    FSST_LOSSY_PHT_HASH_SHIFT, FSST_MAX_AVERAGE_VALUE_LEN, FSST_MAX_SYMBOL_COUNT, FSST_MIN_SAMPLE_COUNT,
    FSST_VALUE_KEYS_MAX_PLAINTEXT_DIVISOR, MAX_RLE_EXPANSION_VALUES, PREFIX_KEY_BYTES, REPLAY_TRIP_DEN,
    REPLAY_TRIP_NUM, SIZE_OPTIMIZED_SAVINGS_DIVISOR, TRANSFORM_SAMPLE_RUNS, TRANSFORM_SAMPLE_SIZE,
    ZSTD_LENGTH_PREFIX_BYTES,
};
use multiversion::multiversion;
use std::borrow::Cow;
use std::cell::RefCell;
use zstd::{bulk::Compressor as ZstdCompressor, zstd_safe::compress_bound};

pub(crate) use presence::{PresenceRank, count_present_before, count_set_bits, present_position};
use scratch::{with_arena_buffer, with_unpack_buffer};
use sidecar::{PrefixKey, StringFingerprint};
pub use string_column::StringColumn;

mod constant;
pub mod decompressor;
pub mod deflate;
pub mod descriptor;
pub mod global_dict;
pub mod mini_block_directory;
pub mod predicate;
mod presence;
mod scratch;
pub mod seekable_zstd;
pub mod sidecar;
mod string_column;

/// Which trade-off the encoder makes when choosing a trailing compression stage for a column block.
///
/// The active strategy is determined by the part's lifecycle stage — the write path always uses `DecodeOptimized`;
/// rewrite and compaction use `SizeOptimized`. No per-query or per-operator switch selects it. Requirement:
/// "Lifecycle-selected cascade strategies". `NoTrailing` is a measurement control the benchmark harness reaches through
/// its own build lifecycle; no production path selects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CascadeStrategy {
    /// Biased toward fast decode. Trailing compression is applied only when it saves at least 25% of bytes.
    DecodeOptimized,
    /// Every choice `DecodeOptimized` makes, except that no trailing compression stage is ever applied — neither
    /// whole-block LZ4/Zstandard nor seekable frames — so a file built this way measures what the stage costs a reader.
    NoTrailing,
    /// Biased toward smaller stored bytes. Trailing compression is applied when it saves at least 5% of bytes.
    SizeOptimized,
}

impl CascadeStrategy {
    /// Returns the strategy a fresh write-path publication should use.
    pub fn for_fresh_publication() -> Self {
        Self::DecodeOptimized
    }

    /// Returns the strategy a rewrite or compaction pass should use.
    pub fn for_rewrite_or_compaction() -> Self {
        Self::SizeOptimized
    }
}

/// Decode-bomb guard: the most values one encoded block may claim where the claim consumes no input per value
/// (zero-width bitpacking). Far above any granule the writer produces (granularity targets are in the thousands), so it
/// rejects only forged counts. A documented named constant, not an operator knob.
const MAX_BLOCK_VALUES: usize = 1 << 24;

/// The most encoding levels one block's recorded cascade may hold: the top-level transform, one inner encoding of a
/// side stream (for example ALP's exception indexes), and a trailing compression stage. The format declares this bound
/// so a reader knows the recorded cascade is always this shallow; the encoder never records a deeper one. Requirement:
/// "Recursive cascade selection".
pub const MAX_CASCADE_DEPTH: usize = 3;

/// Whether the encoder may add one more cascade level on top of `depth` levels already chosen. Returns `false` at the
/// declared maximum, so a candidate level that would exceed [`MAX_CASCADE_DEPTH`] is never applied, however well it
/// samples.
pub fn cascade_level_fits(depth: usize) -> bool {
    depth < MAX_CASCADE_DEPTH
}

/// How many cascade levels a block's recorded pipeline id describes: the transform itself, plus one when a side
/// stream carries an inner encoding, plus one when a trailing compression stage ran. Never exceeds
/// [`MAX_CASCADE_DEPTH`].
pub fn cascade_depth(pipeline: PipelineId) -> Result<usize, FormatError> {
    let mut depth = 1;
    if pipeline.side_stream()? != SideStream::None {
        depth += 1;
    }
    if pipeline.compression()? != Compression::None {
        depth += 1;
    }
    Ok(depth)
}

/// The inner encoding recorded for an encoding's side stream (its exception, metadata, or secondary stream) — the
/// second cascade level, present only when sampling proved it shrinks that stream.
///
/// Variants are in strict alphabetical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideStream {
    /// A dictionary string block that stores only its code stream: the alphabet lives once in the footer's file-scope
    /// shared dictionary for the column, and codes keep the ascending sorted-order assignment against it. Value 4 is
    /// reserved for a future external (cross-file) scope and rejected if seen.
    FileScopeDictionary = 3,
    /// The side stream's values ride a frame-of-reference + FastLanes bitpacked inner encoding.
    ForBitpack = 1,
    /// A dictionary string block whose distinct-value stream is FSST-compressed; codes keep their sorted-order
    /// assignment, so only dictionary-value materialization pays the FSST decode.
    FsstDictionaryValues = 2,
    /// An FSST string block that carries per-value keys after its arena: a [`sidecar::PrefixKey`] and a
    /// [`sidecar::StringFingerprint`] for every present value, so range and substring filters are decided from the
    /// keys instead of from the values' text. A block without them keeps the plain FSST layout.
    FsstValueKeys = 5,
    /// The side stream is stored plainly; the cascade has no inner level.
    None = 0,
}

/// Transform stage of a pipeline.
///
/// Discriminant 11 is retired: it named the removed Vortex shredded-block serialization, and a block claiming it is
/// rejected as an unknown transform. It is never reassigned, so any surviving pipeline id from that era fails loudly
/// instead of decoding as something else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transform {
    /// ALP: adaptive lossless floating-point (decimal scaling with exact exceptions).
    Alp = 6,
    /// ALP-RD ("real doubles") for the high-entropy floats plain ALP rejects: each value is cut into a left part (sign,
    /// exponent, leading mantissa bits) that clusters into a tiny dictionary, and a right part bitpacked raw — so the
    /// shared top bits stop being stored per value while the noisy low bits cost exactly their width.
    AlpRd = 13,
    /// Byte-stream-split for floats ALP rejects: the 8 bytes of every value are regrouped into 8 same-significance
    /// planes (all byte 0s, then all byte 1s, ...), so the high-order planes form long runs the trailing granular
    /// compression stage can shrink, unlike the interleaved bytes of a raw value stream.
    ByteStreamSplit = 12,
    /// Mandatory representation for money: fixed-scale decimal128.
    Decimal128 = 7,
    /// FastLanes-style delta (zigzag) + bitpacking.
    DeltaBitpack = 2,
    /// Dictionary encoding for low-cardinality strings.
    DictionaryString = 8,
    /// FastLanes-style frame-of-reference + bitpacking.
    ForBitpack = 1,
    /// FSST for high-cardinality short strings (random-access preserved through per-value offsets).
    FsstString = 9,
    PlainF64 = 5,
    PlainU128 = 4,
    PlainU64 = 0,
    /// Raw byte arena plus offsets for long or opaque strings.
    RawString = 10,
    /// Run-length encoding for repeated values.
    Rle = 3,
}

impl Transform {
    /// True when this transform's own on-disk layout carries per-value or per-vector offsets, so a byte range can be
    /// extracted straight from the stored bytes without decoding the whole block first. Lance calls this property
    /// "transparent". [`decode_block_range`] fast-paths these transforms directly, independent of any trailing
    /// compression stage.
    pub fn is_per_value_addressable(self) -> bool {
        matches!(
            self,
            Transform::Alp | Transform::DictionaryString | Transform::FsstString | Transform::RawString
        )
    }

    /// True when this transform's fixed-width layout lets a byte range translate directly to a row range, so the
    /// seekable Zstandard family (independently decompressible frames, see [`seekable_zstd`]) can compress it without losing
    /// random access. The two plain transforms and byte-stream-split qualify — every other transform is either
    /// variable-width or already [`Transform::is_per_value_addressable`]. Consulted by both the encoder's
    /// framing gate and [`decode_block_range`] so the two stay in lockstep.
    pub fn supports_framed_range(self) -> bool {
        matches!(
            self,
            Transform::PlainU64 | Transform::PlainF64 | Transform::ByteStreamSplit
        )
    }

    /// True when the seekable Zstandard family can compress this transform's body without costing a reader the ability
    /// to reach one row's bytes on its own. Two layouts qualify: the fixed-width ones, where a row range *is* a byte
    /// range ([`Transform::supports_framed_range`]), and the two string layouts that store a byte-offset
    /// table over their value arena, where a reader resolves the row's bytes from the table and decompresses only the
    /// frames those bytes fall in ([`decode_string_framed_range`]). Consulted by both the encoder's
    /// framing gate and [`decode_block_range`] so the two stay in lockstep.
    pub fn keeps_range_access_under_framing(self) -> bool {
        self.supports_framed_range() || matches!(self, Transform::FsstString | Transform::RawString)
    }
}

/// Trailing compression stage, applied only when the sample proves a benefit. Whole-block LZ4/Zstandard never run on a
/// random-access-required column; the framed [`SeekableZstd`](Compression::SeekableZstd) family is the exception,
/// since its independently-decompressible frames preserve random access (see [`seekable_zstd`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    /// RFC-1951 deflate, split into independently-inflatable [`deflate::GRANULE_BYTES`] granules. No longer chosen by
    /// the encoder — [`SeekableZstd`](Compression::SeekableZstd) decompresses the same bodies about three times
    /// faster at the same size — but still decoded, and still the one family a QuickAssist device accelerates.
    /// Requirement: "QPL deflate as an IAA-accelerated, software-parity compression family".
    Deflate = 4,
    Lz4 = 1,
    None = 0,
    /// Zstandard in the Zstandard Seekable Format: [`seekable_zstd::FRAME_BYTES`] of plaintext per frame, each
    /// decompressible on its own, with a seek table recording where every frame starts. The trailing stage a
    /// random-access column takes, because a reader reaches one row's bytes by decompressing one frame.
    SeekableZstd = 5,
    Zstd1 = 2,
    Zstd3 = 3,
}

/// Logical value kind carried in the pipeline id so decode restores the original column type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Decimal = 4,
    F64 = 3,
    /// i64 zigzag-mapped onto the u64 transforms.
    I64 = 1,
    String = 5,
    U128 = 2,
    U64 = 0,
}

/// The pipeline id recorded in marks and page metadata:
/// `transform | compression << 8 | value_kind << 16 | side_stream << 24`. Together the four fields record the block's
/// full cascade, level by level, so a reader decodes it deterministically from this description alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineId(pub u32);

impl PipelineId {
    /// Packs the three choices that describe how a block was encoded into one id, ready to store in the marks and page
    /// metadata. The side-stream level defaults to [`SideStream::None`]; a cascaded block adds it with
    /// [`PipelineId::with_side_stream`].
    pub fn new(transform: Transform, compression: Compression, kind: ValueKind) -> Self {
        Self((transform as u32) | ((compression as u32) << 8) | ((kind as u32) << 16))
    }

    /// The same id with the recorded side-stream inner encoding replaced by `side` — how a cascaded block records its
    /// second level in the marks and page metadata.
    pub fn with_side_stream(self, side: SideStream) -> Self {
        Self((self.0 & 0x00FF_FFFF) | ((side as u32) << 24))
    }

    /// The inner encoding recorded for this block's side stream, or an error if the id is malformed.
    pub fn side_stream(self) -> Result<SideStream, FormatError> {
        Ok(match (self.0 >> 24) & 0xFF {
            0 => SideStream::None,
            1 => SideStream::ForBitpack,
            2 => SideStream::FsstDictionaryValues,
            3 => SideStream::FileScopeDictionary,
            5 => SideStream::FsstValueKeys,
            // 4 is the reserved external (cross-file) dictionary scope: rejected below until it is defined.
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown side stream in pipeline id",
                });
            }
        })
    }

    /// The transform stage this id encodes, or an error if the id is malformed.
    pub fn transform(self) -> Result<Transform, FormatError> {
        Ok(match self.0 & 0xFF {
            0 => Transform::PlainU64,
            1 => Transform::ForBitpack,
            2 => Transform::DeltaBitpack,
            3 => Transform::Rle,
            4 => Transform::PlainU128,
            5 => Transform::PlainF64,
            6 => Transform::Alp,
            7 => Transform::Decimal128,
            8 => Transform::DictionaryString,
            9 => Transform::FsstString,
            10 => Transform::RawString,
            // 11 is the retired Vortex shredded transform: rejected below as unknown, never reassigned.
            12 => Transform::ByteStreamSplit,
            13 => Transform::AlpRd,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown transform in pipeline id",
                });
            }
        })
    }

    /// The trailing compression stage this id encodes, or an error if the id is malformed.
    pub fn compression(self) -> Result<Compression, FormatError> {
        Ok(match (self.0 >> 8) & 0xFF {
            0 => Compression::None,
            1 => Compression::Lz4,
            2 => Compression::Zstd1,
            3 => Compression::Zstd3,
            4 => Compression::Deflate,
            5 => Compression::SeekableZstd,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown compression in pipeline id",
                });
            }
        })
    }

    /// The logical value kind this id encodes, so decoding restores the original column type; an error if the id is
    /// malformed.
    pub fn value_kind(self) -> Result<ValueKind, FormatError> {
        Ok(match (self.0 >> 16) & 0xFF {
            0 => ValueKind::U64,
            1 => ValueKind::I64,
            2 => ValueKind::U128,
            3 => ValueKind::F64,
            4 => ValueKind::Decimal,
            5 => ValueKind::String,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown value kind in pipeline id",
                });
            }
        })
    }

    /// True when this pipeline's stored bytes can be sliced to a row range without decoding the whole block —
    /// either because the transform itself is [`Transform::is_per_value_addressable`], because it is one of the
    /// plain transforms wrapped in the seekable Zstandard family ([`Transform::supports_framed_range`]), or because
    /// it is a plain u64/f64 body stored as a deflate page, whose granules inflate independently.
    /// [`decode_block_range`] consults this instead of re-deriving the invariant inline.
    pub fn supports_byte_range_extraction(self) -> Result<bool, FormatError> {
        let transform = self.transform()?;
        let compression = self.compression()?;
        Ok(transform.is_per_value_addressable()
            || (transform.supports_framed_range() && compression == Compression::SeekableZstd)
            || (matches!(transform, Transform::PlainU64 | Transform::PlainF64) && compression == Compression::Deflate))
    }
}

/// One column block's data.
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Decimal { values: Vec<i128>, scale: u8 },
    F64(Vec<f64>),
    I64(Vec<i64>),
    Strings(StringColumn),
    U128(Vec<u128>),
    U64(Vec<u64>),
}

impl ColumnData {
    /// How many values (rows) the block holds.
    pub fn row_count(&self) -> usize {
        match self {
            ColumnData::U64(values) => values.len(),
            ColumnData::I64(values) => values.len(),
            ColumnData::U128(values) => values.len(),
            ColumnData::F64(values) => values.len(),
            ColumnData::Decimal { values, .. } => values.len(),
            ColumnData::Strings(values) => values.len(),
        }
    }
}

/// Min/max/null-count statistics exposed without full decompression (page metadata; the granule-granularity minmax
/// SkipIndex source).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BlockStats {
    pub max_f64: Option<f64>,
    pub max_i128: Option<i128>,
    pub min_f64: Option<f64>,
    pub min_i128: Option<i128>,
    pub null_count: u32,
    pub row_count: u32,
}

/// An encoded block: pipeline id + bytes + stats.
#[derive(Debug, Clone)]
pub struct EncodedBlock {
    pub bytes: Vec<u8>,
    /// The symbol table an FSST block trained on its own values, handed out so later blocks of the column can replay
    /// it. A block that compressed with a replayed table carries none.
    pub fsst: Option<FsstTable>,
    pub pipeline: PipelineId,
    pub stats: BlockStats,
    /// Decoded byte length: the size the block occupies after its trailing compression stage is undone (the length of
    /// the buffer [`remove_trailing`] yields), so a reader can budget decode memory from the mark alone. Equals
    /// `bytes.len()` when no trailing compression was applied.
    pub uncompressed_len: u64,
}

fn bits_needed(max: u64) -> u32 {
    64 - max.leading_zeros()
}

/// FastLanes processes integers in fixed vectors of this many values; every bit-packed stream is a whole number of
/// these vectors, the final one zero-padded. The transposed layout below is defined over exactly 1024.
const FASTLANES_VECTOR: usize = 1024;

/// Number of independent lanes when the packed words are read as 64-bit integers: a notional 1024-bit register holds
/// `1024 / 64 = 16` lanes, and each lane decodes one value per step with just a shift and a mask.
const FASTLANES_LANES: usize = 16;

/// Rows in each 8×16 transposed sub-block; a 1024-value vector is eight of them.
const FASTLANES_ROWS: usize = 8;

/// Storage order of the eight 8×16 sub-blocks within a vector — the FastLanes "04261537" order, which is the
/// self-inverse bit-reversal of `0..8`. Laying the sub-blocks out in this order is what lets the very same bytes be
/// unpacked at any lane width (8/16/32/64 bits) with maximum independent work per lane.
const FASTLANES_ORDER: [usize; FASTLANES_ROWS] = [0, 4, 2, 6, 1, 5, 3, 7];

/// The 64-bit words a single lane occupies for `bits`-wide values: 64 values of `bits` bits each, which is exactly
/// `bits` words.
const fn fastlanes_words_per_lane(bits: usize) -> usize {
    bits
}

/// Writes `values` as a FastLanes transposed bit-packed stream: a `count` header, the bit `width`, then the packed
/// 64-bit words. The values are placed in the FastLanes order so a reader — this scalar path, or a SIMD/hardware one
/// elsewhere — recovers sixteen lanes' worth at a time with only shifts and masks. `width` must be the true bit width
/// of the data (every value fits in `width` bits); a zero width writes nothing but the count, since every value is then
/// zero.
fn bitpack(values: &[u64], width: u32, out: &mut Writer) {
    out.put_u32(values.len() as u32);
    out.put_u8(width as u8);
    if width == 0 {
        return;
    }
    let bits = width as usize;
    let words_per_vector = FASTLANES_LANES * fastlanes_words_per_lane(bits);
    let vectors = values.len().div_ceil(FASTLANES_VECTOR);
    let value_mask = mask(width);
    let mut packed = vec![0u64; vectors * words_per_vector];
    // Whole vectors are gathered straight out of `values` — nothing is copied to reach them.
    let mut whole = values.chunks_exact(FASTLANES_VECTOR);
    for (packed_vector, in_vector) in packed.chunks_exact_mut(words_per_vector).zip(whole.by_ref()) {
        bitpack_vector(in_vector, packed_vector, width, value_mask);
    }
    // A trailing partial vector is the one that needs a buffer of its own: its values go into a zero-filled vector,
    // exactly matching the tail fill the transposed layout defines, and are then gathered like any other.
    let tail = whole.remainder();
    if !tail.is_empty()
        && let Some(packed_vector) = packed.chunks_exact_mut(words_per_vector).nth(vectors - 1)
    {
        let mut padded = [0u64; FASTLANES_VECTOR];
        for (slot, value) in padded.iter_mut().zip(tail) {
            *slot = *value;
        }
        bitpack_vector(&padded, packed_vector, width, value_mask);
    }
    out.put_u64_slice(&packed);
}

/// [`bitpack`] for a caller whose values are not already a `&[u64]` slice: `lane(i)` produces value `i` on demand
/// instead of `bitpack` reading it out of a slice. This lets a transform that would otherwise materialise a full
/// transformed copy first (a zigzag mapping, a frame-of-reference base subtraction) fuse that transform straight into
/// the gather, at the cost of recomputing `lane` for a value already visited by an earlier full-column pass (finding
/// the bit width, say) — cheap arithmetic traded for a whole extra `Vec<u64>`.
fn bitpack_with(len: usize, width: u32, out: &mut Writer, lane: impl Fn(usize) -> u64) {
    out.put_u32(len as u32);
    out.put_u8(width as u8);
    if width == 0 {
        return;
    }
    let bits = width as usize;
    let words_per_vector = FASTLANES_LANES * fastlanes_words_per_lane(bits);
    let vectors = len.div_ceil(FASTLANES_VECTOR);
    let value_mask = mask(width);
    let mut packed = vec![0u64; vectors * words_per_vector];
    let mut buffer = [0u64; FASTLANES_VECTOR];
    for (vector_index, packed_vector) in packed.chunks_exact_mut(words_per_vector).enumerate() {
        let base = vector_index * FASTLANES_VECTOR;
        let this_len = FASTLANES_VECTOR.min(len.saturating_sub(base));
        for (offset, slot) in buffer.iter_mut().enumerate() {
            *slot = if offset < this_len { lane(base + offset) } else { 0 };
        }
        bitpack_vector(&buffer, packed_vector, width, value_mask);
    }
    out.put_u64_slice(&packed);
}

/// Gathers one whole 1024-value vector into the packed words of its own vector slot, in the FastLanes transposed order.
///
/// Compiled once per instruction set and dispatched on the CPU features detected at runtime; every clone runs the same
/// shift-and-mask row loops the vectorizer turns into sixteen-lane SIMD, so the packed words are identical whichever
/// clone runs. Requirement: "SIMD kernels dispatch on detected CPU features at runtime".
#[multiversion(targets("x86_64+avx512f+avx512bw+avx512dq+avx512vl", "x86_64+avx2", "aarch64+neon"))]
fn bitpack_vector(in_vector: &[u64], packed_vector: &mut [u64], width: u32, value_mask: u64) {
    let bits = width as usize;
    for (sub_block, in_sub) in in_vector.chunks_exact(FASTLANES_ROWS * FASTLANES_LANES).enumerate() {
        // The sub-block order is self-inverse, so the same table maps a logical sub-block to its storage slot.
        let vl_block = FASTLANES_ORDER.get(sub_block).copied().unwrap_or(0);
        for (row, in_row) in in_sub.chunks_exact(FASTLANES_LANES).enumerate() {
            let bit = (vl_block * FASTLANES_ROWS + row) * bits;
            let word = bit / 64;
            let offset = (bit % 64) as u32;
            let spill = offset + width > 64;
            let rows_needed = if spill { 2 } else { 1 };
            let Some(word_rows) = packed_vector.get_mut(word * FASTLANES_LANES..(word + rows_needed) * FASTLANES_LANES)
            else {
                continue;
            };
            let (low_row, high_row) = word_rows.split_at_mut(FASTLANES_LANES);
            for (slot, value) in low_row.iter_mut().zip(in_row) {
                *slot |= (value & value_mask) << offset;
            }
            if spill {
                for (slot, value) in high_row.iter_mut().zip(in_row) {
                    *slot |= (value & value_mask) >> (64 - offset);
                }
            }
        }
    }
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 { u64::MAX } else { (1u64 << bits) - 1 }
}

/// A bit-packed value stream read off the wire and bounds-checked, but not yet unpacked.
///
/// The packed bytes are taken from the reader before any output is allocated, so a forged count cannot drive an
/// allocation the block's real byte length could never fill (allocation-light decoding of untrusted bytes).
struct PackedStream<'a> {
    count: usize,
    packed: &'a [u8],
    width: u32,
}

/// Reads the header and packed bytes of a stream written by [`bitpack`], rejecting a width no lane layout can hold.
fn read_packed_stream<'a>(reader: &mut Reader<'a>) -> Result<PackedStream<'a>, FormatError> {
    let count = reader.u32("bitpack count")? as usize;
    let width = u32::from(reader.u8("bitpack width")?);
    if width > 64 {
        return Err(FormatError::Structural {
            rule: "bit width beyond 64",
        });
    }
    if width == 0 {
        // A zero-width stream stores no words, so the count is pure output amplification; refuse beyond the block
        // bound (allocation-light decoding of untrusted bytes).
        if count > MAX_BLOCK_VALUES {
            return Err(FormatError::Structural {
                rule: "zero-width bitpack count beyond the block bound",
            });
        }
        return Ok(PackedStream {
            count,
            packed: &[],
            width,
        });
    }
    let vectors = count.div_ceil(FASTLANES_VECTOR);
    let total_words = vectors * FASTLANES_LANES * fastlanes_words_per_lane(width as usize);
    let packed = reader.take(total_words.saturating_mul(8), "bitpack")?;
    Ok(PackedStream { count, packed, width })
}

impl PackedStream<'_> {
    /// How many slots [`Self::unpack_into`] writes: whole 1024-value vectors, the last one zero-padded, since the
    /// transposed layout is defined over whole vectors. A zero-width stream carries no words and writes its count.
    fn padded_count(&self) -> usize {
        if self.width == 0 {
            self.count
        } else {
            self.count.div_ceil(FASTLANES_VECTOR) * FASTLANES_VECTOR
        }
    }

    /// Unpacks the stream into `out` — which must hold [`Self::padded_count`] slots — in the values' original logical
    /// order, applying `lane` to every value as it is written.
    ///
    /// Each transposed row is recovered sixteen lanes at a time with only shifts and masks, and `lane` rides inside
    /// those writes, so a caller that wants the values in another form — unzigzagged, lifted off a frame-of-reference
    /// base, or even collapsed to a predicate verdict — gets it without a second pass over the output.
    fn unpack_into<T: Copy>(&self, out: &mut [T], lane: impl Fn(u64) -> T) {
        if self.width == 0 {
            out.fill(lane(0));
            return;
        }
        unpack_vectors(self.packed, self.width, out, lane);
    }
}

/// Unpacks whole FastLanes vectors from `packed` (written by [`bitpack`]) into `out`, applying `lane` to every value.
///
/// Compiled once per instruction set and dispatched on the CPU features detected at runtime. Each transposed row is
/// sixteen independent lanes behind one shift and mask, which the vectorizer turns into a handful of SIMD
/// instructions — with `lane`'s arithmetic fused into the same vector loop. Every clone computes the same integer
/// operations, so the decoded values are identical whichever clone runs. Requirement: "SIMD kernels dispatch on
/// detected CPU features at runtime".
#[multiversion(targets("x86_64+avx512f+avx512bw+avx512dq+avx512vl", "x86_64+avx2", "aarch64+neon"))]
fn unpack_vectors<T: Copy>(packed: &[u8], width: u32, out: &mut [T], lane: impl Fn(u64) -> T) {
    let bits = width as usize;
    let bytes_per_vector = FASTLANES_LANES * fastlanes_words_per_lane(bits) * 8;
    let value_mask = mask(width);
    for (out_vector, packed_vector) in out
        .chunks_exact_mut(FASTLANES_VECTOR)
        .zip(packed.chunks_exact(bytes_per_vector))
    {
        for (sub_block, out_sub) in out_vector
            .chunks_exact_mut(FASTLANES_ROWS * FASTLANES_LANES)
            .enumerate()
        {
            // The sub-block order is self-inverse, so the same table maps a logical sub-block back to its storage slot.
            let vl_block = FASTLANES_ORDER.get(sub_block).copied().unwrap_or(0);
            for (row, out_row) in out_sub.chunks_exact_mut(FASTLANES_LANES).enumerate() {
                let bit = (vl_block * FASTLANES_ROWS + row) * bits;
                let word = bit / 64;
                let offset = (bit % 64) as u32;
                let Some(low_row) = lane_row(packed_vector, word) else {
                    continue;
                };
                let low_words = lane_words(low_row);
                if offset + width <= 64 {
                    for (slot, low) in out_row.iter_mut().zip(low_words) {
                        *slot = lane((low >> offset) & value_mask);
                    }
                } else if let Some(high_row) = lane_row(packed_vector, word + 1) {
                    let high_words = lane_words(high_row);
                    for ((slot, low), high) in out_row.iter_mut().zip(low_words).zip(high_words) {
                        *slot = lane(((low >> offset) | (high << (64 - offset))) & value_mask);
                    }
                }
            }
        }
    }
}

/// The sixteen lanes' packed bytes at word index `word` of one vector.
fn lane_row(packed_vector: &[u8], word: usize) -> Option<&[u8]> {
    packed_vector.get(word * FASTLANES_LANES * 8..(word + 1) * FASTLANES_LANES * 8)
}

/// The sixteen lane words of one packed row, in lane order. Materialized as an array (rather than iterated lazily) so
/// the row loops over it stay countable and contiguous for the vectorizer.
fn lane_words(row: &[u8]) -> [u64; FASTLANES_LANES] {
    let mut words = [0u64; FASTLANES_LANES];
    for (slot, chunk) in words.iter_mut().zip(row.chunks_exact(8)) {
        *slot = u64::from_le_bytes(chunk.try_into().unwrap_or([0; 8]));
    }
    words
}

/// Reads a FastLanes transposed bit-packed stream written by [`bitpack`] and returns the values in their original
/// logical order, with `lane` applied to each one as it is unpacked.
fn bitunpack_into<T: Copy + Default>(reader: &mut Reader<'_>, lane: impl Fn(u64) -> T) -> Result<Vec<T>, FormatError> {
    let stream = read_packed_stream(reader)?;
    // Scatter whole vectors, then trim to the stream's count; the padded tail is under one vector.
    let mut values = vec![T::default(); stream.padded_count()];
    stream.unpack_into(&mut values, lane);
    values.truncate(stream.count);
    Ok(values)
}

/// Reads a FastLanes transposed bit-packed stream written by [`bitpack`] and returns the packed values as they were
/// stored.
fn bitunpack(reader: &mut Reader<'_>) -> Result<Vec<u64>, FormatError> {
    bitunpack_into(reader, |packed| packed)
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// `unzigzag(packed) as f64`, computed branch-free so an ALP reconstruction loop vectorizes: adding the integer to
/// [`ALP_CONVERT_MAGIC`]'s bits and subtracting the constant back is exact for integers within `±2^51`, which a
/// stream width of at most [`ALP_EXACT_CONVERT_MAX_WIDTH`] guarantees. A wider stream must use the scalar conversion
/// instead.
#[inline]
fn alp_narrow_int_to_f64(packed: u64) -> f64 {
    let int = unzigzag(packed);
    f64::from_bits(ALP_CONVERT_MAGIC.to_bits().wrapping_add(int as u64)) - ALP_CONVERT_MAGIC
}

/// One value of a decoded integer column.
///
/// A `u64` column holds its values as stored; an `i64` column holds them as zigzag codes, and undoing that mapping
/// here — as each value is written — is what lets a signed column decode in the same single pass as an unsigned one,
/// instead of mapping a finished `Vec<u64>` into a second vector.
trait IntValue: Copy + Default + bytemuck::AnyBitPattern {
    /// Reinterprets 64 raw bits as this value type, with no zigzag mapping. A delta stream is unpacked into its
    /// difference form this way, before the running sum turns those differences into values.
    fn from_bits(bits: u64) -> Self;

    /// Turns one code as stored on disk into the column's value.
    fn from_code(code: u64) -> Self;

    /// The raw bits back, so the delta pass can accumulate in the domain the codes were written in.
    fn to_bits(self) -> u64;
}

impl IntValue for i64 {
    fn from_bits(bits: u64) -> Self {
        bits as i64
    }

    fn from_code(code: u64) -> Self {
        unzigzag(code)
    }

    fn to_bits(self) -> u64 {
        self as u64
    }
}

impl IntValue for u64 {
    fn from_bits(bits: u64) -> Self {
        bits
    }

    fn from_code(code: u64) -> Self {
        code
    }

    fn to_bits(self) -> u64 {
        self
    }
}

fn encode_plain_u64(values: &[u64], out: &mut Writer) {
    out.put_u32(values.len() as u32);
    out.put_u64_slice(values);
}

fn decode_plain_u64<T: IntValue>(reader: &mut Reader<'_>) -> Result<Vec<T>, FormatError> {
    let count = reader.u32("plain count")? as usize;
    let mut values = reader.u64_vec(count, "plain value")?;
    // T is always 64 bits wide, so the code -> value mapping is done in place and the buffer reinterpreted as `T`
    // rather than collected into a second, freshly allocated vector.
    for value in &mut values {
        *value = T::from_code(*value).to_bits();
    }
    Ok(bytemuck::cast_vec(values))
}

/// Byte-identical to shifting `values` onto `base` into a `Vec<u64>` and calling [`bitpack`]: the subtraction fuses
/// into [`bitpack_with`]'s gather instead, so a u64 frame-of-reference block never materialises a shifted copy of
/// itself, matching [`encode_for_bitpack_i64`]'s shape.
fn encode_for_bitpack(values: &[u64], out: &mut Writer) {
    let base = values.iter().copied().min().unwrap_or(0);
    // `base` is the minimum, so neither subtraction can wrap; writing them wrapping keeps the release profile's
    // `overflow-checks` out of both loops, which is what lets them vectorize.
    let max_delta = values.iter().map(|v| v.wrapping_sub(base)).max().unwrap_or(0);
    out.put_u64(base);
    bitpack_with(values.len(), bits_needed(max_delta), out, |i| {
        values[i].wrapping_sub(base)
    });
}

/// [`encode_for_bitpack`] for `i64` values: the zigzag mapping onto the u64 domain and the frame-of-reference base
/// subtraction both fuse into [`bitpack_with`]'s gather, so an i64 block never materialises a full zigzag-mapped or
/// base-shifted copy of itself. Byte-identical to zigzag-mapping `values` and calling [`encode_for_bitpack`].
fn encode_for_bitpack_i64(values: &[i64], out: &mut Writer) {
    let zig_at = |i: usize| values.get(i).copied().map(zigzag).unwrap_or(0);
    let base = (0..values.len()).map(zig_at).min().unwrap_or(0);
    // `base` is the minimum, so the subtraction never wraps; writing it wrapping keeps the release profile's
    // `overflow-checks` out, which is what lets the gather vectorize.
    let max_delta = (0..values.len())
        .map(|i| zig_at(i).wrapping_sub(base))
        .max()
        .unwrap_or(0);
    out.put_u64(base);
    bitpack_with(values.len(), bits_needed(max_delta), out, |i| {
        zig_at(i).wrapping_sub(base)
    });
}

/// Decodes a frame-of-reference bit-packed stream, lifting each value off the base inside the unpack's lane writes so
/// the block lands in its final form in one output allocation.
fn decode_for_bitpack<T: IntValue>(reader: &mut Reader<'_>) -> Result<Vec<T>, FormatError> {
    let base = reader.u64("for base")?;
    let stream = read_packed_stream(reader)?;
    if base.checked_add(mask(stream.width)).is_none() {
        // A base this close to u64::MAX leaves room for a packed delta to carry it past the top. Only these blocks pay
        // the per-value overflow check; every other base makes it unreachable, which is what keeps the fused lane
        // write below free of it.
        let mut deltas = vec![0u64; stream.padded_count()];
        stream.unpack_into(&mut deltas, |packed| packed);
        deltas.truncate(stream.count);
        return deltas
            .into_iter()
            .map(|delta| {
                base.checked_add(delta)
                    .map(T::from_code)
                    .ok_or(FormatError::Structural {
                        rule: "frame-of-reference overflow",
                    })
            })
            .collect();
    }
    let mut values = vec![T::default(); stream.padded_count()];
    // No packed value can carry `base` past the top here, so the add is written wrapping to keep the lane write free
    // of the overflow check the release profile would otherwise emit.
    stream.unpack_into(&mut values, |packed| T::from_code(base.wrapping_add(packed)));
    values.truncate(stream.count);
    Ok(values)
}

/// Byte-identical to computing each zigzag delta into a `Vec<u64>` and calling [`bitpack`]: the delta fuses into
/// [`bitpack_with`]'s gather instead, so a u64 delta block never materialises a delta copy of itself, matching
/// [`encode_delta_bitpack_i64`]'s shape.
fn encode_delta_bitpack(values: &[u64], out: &mut Writer) {
    let first = values.first().copied().unwrap_or(0);
    out.put_u64(first);
    let len = values.len().saturating_sub(1);
    let delta_at = |i: usize| zigzag(values[i + 1].wrapping_sub(values[i]) as i64);
    let max = (0..len).map(delta_at).max().unwrap_or(0);
    bitpack_with(len, bits_needed(max), out, delta_at);
}

/// [`encode_delta_bitpack`] for `i64` values: each delta is computed straight off `values`' zigzag codes inside
/// [`bitpack_with`]'s gather, so an i64 block never materialises a full zigzag-mapped or delta copy of itself.
/// Byte-identical to zigzag-mapping `values` and calling [`encode_delta_bitpack`].
fn encode_delta_bitpack_i64(values: &[i64], out: &mut Writer) {
    let zig_at = |i: usize| values.get(i).copied().map(zigzag).unwrap_or(0);
    out.put_u64(zig_at(0));
    let len = values.len().saturating_sub(1);
    let delta_at = |i: usize| zigzag(zig_at(i + 1).wrapping_sub(zig_at(i)) as i64);
    let max = (0..len).map(delta_at).max().unwrap_or(0);
    bitpack_with(len, bits_needed(max), out, delta_at);
}

/// Decodes a delta bit-packed stream: the zigzag mapping comes off inside the unpack's lane writes, and the running
/// sum then walks that same buffer in place, so the block decodes into one output allocation.
fn decode_delta_bitpack<T: IntValue>(reader: &mut Reader<'_>) -> Result<Vec<T>, FormatError> {
    let first = reader.u64("delta first")?;
    let stream = read_packed_stream(reader)?;
    let mut values = vec![T::default(); stream.padded_count() + 1];
    if let Some((head, deltas)) = values.split_first_mut() {
        *head = T::from_code(first);
        stream.unpack_into(deltas, |packed| T::from_bits(unzigzag(packed) as u64));
    }
    values.truncate(stream.count + 1);
    let mut previous = first;
    for slot in values.iter_mut().skip(1) {
        previous = previous.wrapping_add(slot.to_bits());
        *slot = T::from_code(previous);
    }
    Ok(values)
}

fn encode_rle(values: &[u64], out: &mut Writer) {
    let mut runs: Vec<(u64, u32)> = Vec::new();
    for value in values {
        match runs.last_mut() {
            Some((current, count)) if current == value => *count += 1,
            _ => runs.push((*value, 1)),
        }
    }
    out.put_u32(runs.len() as u32);
    for (value, count) in runs {
        out.put_u64(value);
        out.put_u32(count);
    }
}

fn decode_rle<T: IntValue>(reader: &mut Reader<'_>) -> Result<Vec<T>, FormatError> {
    let run_count = reader.u32("rle run count")? as usize;
    let mut values = Vec::new();
    for _ in 0..run_count {
        let value = reader.u64("rle value")?;
        let count = reader.u32("rle count")? as usize;
        if values.len() + count > MAX_RLE_EXPANSION_VALUES {
            return Err(FormatError::Structural {
                rule: "rle expansion too large",
            });
        }
        values.extend(std::iter::repeat_n(T::from_code(value), count));
    }
    Ok(values)
}

fn rle_size(values: &[u64]) -> usize {
    let mut runs = 0usize;
    let mut previous: Option<u64> = None;
    for value in values {
        if previous != Some(*value) {
            runs += 1;
            previous = Some(*value);
        }
    }
    4 + runs * 12
}

/// Bytes a bit-packed stream of `packed_len` values really occupies, given the whole block runs to `block_len` values.
///
/// [`bitpack`] writes whole 1024-value FastLanes vectors, zero-padding the tail, so a block shorter than one vector
/// occupies that vector's full width however few values it holds — a handful of values costs the same as a thousand.
/// From one vector up the padding is a bounded sub-vector tail, and the information content is the right estimate for
/// a candidate scored on a sample.
fn packed_size(packed_len: usize, block_len: usize, width: u32) -> usize {
    if width == 0 {
        return 0;
    }
    if block_len < FASTLANES_VECTOR {
        return FASTLANES_LANES * fastlanes_words_per_lane(width as usize) * 8;
    }
    (packed_len * width as usize).div_ceil(8)
}

/// Estimated bytes for the frame-of-reference candidate, used only to pick the winning transform on a sample: base,
/// header, and the packed bits as [`packed_size`] prices them for a block of `block_len` values.
fn for_size(values: &[u64], block_len: usize) -> usize {
    let base = values.iter().copied().min().unwrap_or(0);
    // `base` is the minimum, so the subtraction never wraps; writing it wrapping keeps the release profile's
    // `overflow-checks` out of the loop, which is what lets it vectorize.
    let max_delta = values.iter().map(|v| v.wrapping_sub(base)).max().unwrap_or(0);
    8 + 5 + packed_size(values.len(), block_len, bits_needed(max_delta))
}

/// Estimated bytes for the delta candidate, used only to pick the winning transform on a sample: first value, header,
/// and the packed delta bits as [`packed_size`] prices them for a block of `block_len` values.
fn delta_size(values: &[u64], block_len: usize) -> usize {
    // Each delta reads its own pair, so the loop carries nothing from one step to the next and vectorizes.
    let max = values
        .windows(2)
        .filter_map(|pair| match pair {
            [previous, value] => Some(zigzag(value.wrapping_sub(*previous) as i64)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    8 + 5 + packed_size(values.len().saturating_sub(1), block_len, bits_needed(max))
}

/// Draws the transform-selection sample: `None` (use the block as-is) when the block holds at most
/// [`TRANSFORM_SAMPLE_SIZE`] values, otherwise [`TRANSFORM_SAMPLE_RUNS`] evenly spaced contiguous runs totalling
/// [`TRANSFORM_SAMPLE_SIZE`] values. The runs span the whole block at fixed, content-independent positions — never just
/// its head — so a distribution that shifts after the leading values (a constant or sorted prefix before noise, normal
/// for sequence-ordered event data) is seen by selection instead of the transform being mispicked on the head alone.
/// Fixed positions keep selection deterministic, so byte-identical encodes across nodes hold unchanged. Requirement:
/// "Stratified sampling through one shared statistics pass".
fn transform_sample<T: Copy>(values: &[T]) -> Option<Vec<T>> {
    if values.len() <= TRANSFORM_SAMPLE_SIZE {
        return None;
    }
    let run = TRANSFORM_SAMPLE_SIZE / TRANSFORM_SAMPLE_RUNS;
    let span = values.len() - run;
    let mut sample = Vec::with_capacity(TRANSFORM_SAMPLE_SIZE);
    for i in 0..TRANSFORM_SAMPLE_RUNS {
        let start = span * i / (TRANSFORM_SAMPLE_RUNS - 1);
        if let Some(window) = values.get(start..start + run) {
            sample.extend_from_slice(window);
        }
    }
    Some(sample)
}

/// Denominator of the per-family decode-cost multipliers in [`decode_cost_num`], so scores stay exact integers.
const DECODE_COST_DEN: usize = 16;

/// Fixed per-family decode-cost multiplier numerator (over [`DECODE_COST_DEN`]), pinned per lifecycle strategy and
/// applied to a candidate's estimated encoded size, so "the fastest valid pipeline" is honoured deterministically —
/// wall-clock is never a selection input, which would break byte-identical encodes across nodes. `DecodeOptimized`
/// taxes RLE (a branchy run walk, against the FastLanes kernels' straight-line unpack) enough that it must win by a
/// clear margin; `SizeOptimized` scores every family neutrally so smaller bytes win.
fn decode_cost_num(transform: Transform, strategy: CascadeStrategy) -> usize {
    match (strategy, transform) {
        (CascadeStrategy::DecodeOptimized | CascadeStrategy::NoTrailing, Transform::Rle) => 17,
        _ => DECODE_COST_DEN,
    }
}

/// Picks the winning u64 transform on a sample (the mandated monotonic / low-cardinality candidate families:
/// FOR+bitpack, DELTA+bitpack, RLE, plain fallback), scoring each candidate's estimated size through the strategy's
/// pinned decode-cost multipliers.
fn choose_u64_transform(values: &[u64], strategy: CascadeStrategy) -> Transform {
    let sample = transform_sample(values);
    let sample: &[u64] = sample.as_deref().unwrap_or(values);
    score_u64_candidates(sample, values.len(), strategy)
}

/// Picks the winning transform by estimated size — the scoring core shared by [`choose_u64_transform`] and
/// [`choose_i64_transform`]: `sample` is a (possibly reduced) run of the block's values in the u64 domain, and
/// `block_len` is the real row count the sample's per-row estimates scale to, which is not always `sample.len()`.
fn score_u64_candidates(sample: &[u64], block_len: usize, strategy: CascadeStrategy) -> Transform {
    if sample.is_empty() {
        return Transform::PlainU64;
    }
    let plain = 4 + sample.len() * 8;
    let candidates = [
        (Transform::ForBitpack, for_size(sample, block_len)),
        (Transform::DeltaBitpack, delta_size(sample, block_len)),
        (Transform::Rle, rle_size(sample)),
        (Transform::PlainU64, plain),
    ];
    candidates
        .into_iter()
        .min_by_key(|(transform, size)| *size * decode_cost_num(*transform, strategy))
        .map(|(transform, _)| transform)
        .unwrap_or(Transform::PlainU64)
}

/// [`choose_u64_transform`] for `i64` values: only the (bounded [`TRANSFORM_SAMPLE_SIZE`]) sample is zigzag-mapped
/// to score candidates, instead of the whole column — the full mapping happens only once, in [`encode_i64_with`],
/// for whichever transform wins.
fn choose_i64_transform(values: &[i64], strategy: CascadeStrategy) -> Transform {
    let sample = transform_sample(values);
    let sample: &[i64] = sample.as_deref().unwrap_or(values);
    let mapped: Vec<u64> = sample.iter().map(|value| zigzag(*value)).collect();
    score_u64_candidates(&mapped, values.len(), strategy)
}

fn encode_u64_with(transform: Transform, values: &[u64], out: &mut Writer) {
    match transform {
        Transform::ForBitpack => encode_for_bitpack(values, out),
        Transform::DeltaBitpack => encode_delta_bitpack(values, out),
        Transform::Rle => encode_rle(values, out),
        _ => encode_plain_u64(values, out),
    }
}

/// [`encode_u64_with`] for `i64` values: `ForBitpack`/`DeltaBitpack` fuse the zigzag mapping straight into the
/// bitpack gather ([`encode_for_bitpack_i64`], [`encode_delta_bitpack_i64`]); `Rle` and `PlainU64` write every value
/// regardless of encoding, so there is no gather to fuse into and the zigzag-mapped column is built once here.
fn encode_i64_with(transform: Transform, values: &[i64], out: &mut Writer) {
    match transform {
        Transform::ForBitpack => encode_for_bitpack_i64(values, out),
        Transform::DeltaBitpack => encode_delta_bitpack_i64(values, out),
        _ => {
            let mapped: Vec<u64> = values.iter().map(|value| zigzag(*value)).collect();
            match transform {
                Transform::Rle => encode_rle(&mapped, out),
                _ => encode_plain_u64(&mapped, out),
            }
        }
    }
}

/// Decodes an integer block body straight into the column's value type: `u64` values as stored, `i64` values with the
/// block-level zigzag mapping already undone, in a single pass over the stream.
fn decode_ints_with<T: IntValue>(transform: Transform, reader: &mut Reader<'_>) -> Result<Vec<T>, FormatError> {
    match transform {
        Transform::ForBitpack => decode_for_bitpack(reader),
        Transform::DeltaBitpack => decode_delta_bitpack(reader),
        Transform::Rle => decode_rle(reader),
        Transform::PlainU64 => decode_plain_u64(reader),
        _ => Err(FormatError::Structural {
            rule: "transform does not decode to u64",
        }),
    }
}

const ALP_POWERS: [f64; 19] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16, 1e17, 1e18,
];

/// The one-value scaling probe [`alp_probe_chunk`] must match bit for bit: `Some(scaled integer)` when `value * power`
/// rounds to an integer that divides back to exactly `value`, `None` otherwise. Kept as the plain reference the
/// kernel's parity tests compare against.
#[cfg(test)]
fn alp_try(value: f64, power: f64) -> Option<i64> {
    let scaled = value * power;
    if !scaled.is_finite() || scaled.abs() >= ALP_MAX_SCALED_MAGNITUDE {
        return None;
    }
    let rounded = scaled.round();
    let as_int = rounded as i64;
    // Exact round-trip required (the tolerance contract for storage is exactness; lossy ALP is forbidden here).
    if (as_int as f64) / power == value {
        Some(as_int)
    } else {
        None
    }
}

/// Scales one run of floats by `power` and writes, per value, its zigzag-mapped scaled integer into `zigzags` (0 where
/// the value cannot be scaled) and whether the scaling round-trips exactly into `encodable`. This is the ALP probe —
/// multiply, round, divide back, compare — run for every candidate exponent and again for every encoded vector.
///
/// Compiled once per instruction set and dispatched on the CPU features detected at runtime; the straight-line
/// multiply/round/compare body vectorizes four to eight values per instruction. Rounding is spelled as truncate plus a
/// half-away adjustment because that form both vectorizes and reproduces `f64::round` exactly (the fraction
/// `scaled - trunc(scaled)` is computed exactly for every finite float), so every clone accepts and rejects the same
/// values the scalar reference (`alp_try`) does — asserted by test. Requirement: "SIMD kernels dispatch on detected
/// CPU features at runtime".
#[multiversion(targets("x86_64+avx512f+avx512bw+avx512dq+avx512vl", "x86_64+avx2", "aarch64+neon"))]
fn alp_probe_chunk(values: &[f64], power: f64, zigzags: &mut [u64], encodable: &mut [bool]) {
    for ((value, zigzag_slot), encodable_slot) in values.iter().zip(zigzags.iter_mut()).zip(encodable.iter_mut()) {
        let scaled = value * power;
        // One comparison covers the reference's finite and magnitude gates: an infinity or NaN never satisfies it.
        let in_range = scaled.abs() < ALP_MAX_SCALED_MAGNITUDE;
        let truncated = scaled.trunc();
        let fraction = scaled - truncated;
        let adjust = if fraction.abs() >= 0.5 {
            1.0f64.copysign(scaled)
        } else {
            0.0
        };
        let rounded = truncated + adjust;
        // `rounded` is an integer-valued f64, so `rounded / power` equals the reference's `(as_int as f64) / power`.
        let ok = in_range && (rounded / power) == *value;
        let as_int = rounded as i64;
        *zigzag_slot = if ok { zigzag(as_int) } else { 0 };
        *encodable_slot = ok;
    }
}

/// Bytes an ALP exception index stream occupies stored plainly: one u32 per exception.
fn plain_exception_index_size(count: usize) -> usize {
    count * 4
}

/// Bytes the same index stream occupies under the frame-of-reference + bitpack inner encoding: the FOR base, the
/// count/width header, and whole FastLanes vectors of packed words. Measured exactly — this is the sampling input that
/// decides whether the deeper cascade level is kept.
fn cascaded_exception_index_size(indexes: &[u64]) -> usize {
    let base = indexes.iter().copied().min().unwrap_or(0);
    let max_delta = indexes.iter().map(|index| index - base).max().unwrap_or(0);
    let width = bits_needed(max_delta) as usize;
    let vectors = indexes.len().div_ceil(FASTLANES_VECTOR);
    8 + 5 + vectors * width * FASTLANES_LANES * 8
}

/// Picks the inner encoding for an ALP block's exception index stream — the cascade's second level. The deeper level
/// is kept only when it must fit under [`MAX_CASCADE_DEPTH`] and the sampled sizes prove it shrinks the stream past
/// the strategy's bar: `DecodeOptimized` demands at least a 25% saving before it accepts the extra decode step,
/// `SizeOptimized` accepts 5%.
fn choose_exception_side_stream(indexes: &[u64], strategy: CascadeStrategy) -> SideStream {
    if indexes.is_empty() || !cascade_level_fits(1) {
        return SideStream::None;
    }
    let plain = plain_exception_index_size(indexes.len());
    let cascaded = cascaded_exception_index_size(indexes);
    let threshold = match strategy {
        CascadeStrategy::DecodeOptimized | CascadeStrategy::NoTrailing => {
            plain * DECODE_OPTIMIZED_THRESHOLD_NUM / DECODE_OPTIMIZED_THRESHOLD_DEN
        }
        CascadeStrategy::SizeOptimized => plain - plain / SIZE_OPTIMIZED_SAVINGS_DIVISOR,
    };
    if cascaded < threshold {
        SideStream::ForBitpack
    } else {
        SideStream::None
    }
}

/// Whether ALP should encode a block whose sampled per-value verdicts ([`alp_probe_chunk`]'s `encodable` output under
/// the chosen power) look like this. Judged per sampled run rather than globally: a run that is mostly exceptions
/// predicts a vector the per-vector raw escape will absorb, so it does not disqualify the block — but such runs must
/// stay a minority, or the block is mostly raw payloads and the float fallbacks serve it better. A run that is not
/// pathological must itself be mostly encodable, which the per-run bound guarantees.
fn alp_viable(encodable: &[bool]) -> bool {
    if encodable.is_empty() {
        return true;
    }
    let run = (encodable.len() / TRANSFORM_SAMPLE_RUNS).max(1);
    let mut pathological = 0usize;
    let mut runs = 0usize;
    for chunk in encodable.chunks(run) {
        runs += 1;
        let exceptions = chunk.iter().filter(|ok| !**ok).count();
        if exceptions * ALP_MAX_EXCEPTION_SAMPLE_DIVISOR > chunk.len() {
            pathological += 1;
        }
    }
    pathological * 2 <= runs && pathological < runs
}

/// ALP block: exponent + FOR/bitpacked scaled integers + exact exceptions. A FastLanes vector whose own exception rate
/// exceeds the sample bound is stored raw instead — flagged by [`ALP_VECTOR_ESCAPE_SENTINEL`] in the exponent byte's
/// position — so one pathological 1024-value stretch costs its own bytes, never the whole block's eligibility. A block
/// with no escaped vector keeps the original layout byte for byte. Returns the inner encoding chosen for the exception
/// index stream (the recorded second cascade level), or `None` when ALP is not viable for this data.
fn encode_alp(values: &[f64], strategy: CascadeStrategy, out: &mut Writer) -> Option<SideStream> {
    let sample = transform_sample(values);
    let sample: &[f64] = sample.as_deref().unwrap_or(values);
    // One probe scratch pair serves the exponent search, the viability check, and every encoded vector, resized to
    // each phase's exact length so the verdict counts never read a stale tail.
    let mut zigzags = vec![0u64; sample.len()];
    let mut encodable = vec![false; sample.len()];
    let mut best: Option<(usize, usize)> = None; // (exponent index, exceptions)
    for (index, power) in ALP_POWERS.iter().enumerate() {
        alp_probe_chunk(sample, *power, &mut zigzags, &mut encodable);
        let exceptions = encodable.iter().filter(|ok| !**ok).count();
        if best.is_none_or(|(_, current)| exceptions < current) {
            best = Some((index, exceptions));
        }
        if exceptions == 0 {
            break;
        }
    }
    let (exponent_index, _) = best?;
    let power = ALP_POWERS.get(exponent_index).copied().unwrap_or(1.0);
    alp_probe_chunk(sample, power, &mut zigzags, &mut encodable);
    if !alp_viable(&encodable) {
        return None;
    }
    let vector_scratch = values.len().min(FASTLANES_VECTOR);
    zigzags.resize(vector_scratch, 0);
    encodable.resize(vector_scratch, false);
    let mut ints = Vec::with_capacity(values.len());
    let mut exceptions: Vec<(u32, u64)> = Vec::new();
    let mut escaped: Vec<u32> = Vec::new();
    for (vector_index, vector) in values.chunks(FASTLANES_VECTOR).enumerate() {
        let (Some(vector_zigzags), Some(vector_encodable)) =
            (zigzags.get_mut(..vector.len()), encodable.get_mut(..vector.len()))
        else {
            continue;
        };
        alp_probe_chunk(vector, power, vector_zigzags, vector_encodable);
        let vector_exceptions = vector_encodable.iter().filter(|ok| !**ok).count();
        if vector_exceptions * ALP_MAX_EXCEPTION_SAMPLE_DIVISOR > vector.len() {
            escaped.push(vector_index as u32);
            ints.extend(std::iter::repeat_n(0u64, vector.len()));
            continue;
        }
        let base = vector_index * FASTLANES_VECTOR;
        // The kernel already wrote zigzag codes with 0 in every exception slot, so the codes append wholesale and only
        // the exceptions walk per value.
        ints.extend_from_slice(vector_zigzags);
        for (offset, (ok, value)) in vector_encodable.iter().zip(vector).enumerate() {
            if !*ok {
                exceptions.push(((base + offset) as u32, value.to_bits()));
            }
        }
    }
    let indexes: Vec<u64> = exceptions.iter().map(|(index, _)| u64::from(*index)).collect();
    let side = choose_exception_side_stream(&indexes, strategy);
    if escaped.is_empty() {
        out.put_u8(exponent_index as u8);
    } else {
        out.put_u8(ALP_VECTOR_ESCAPE_SENTINEL);
        out.put_u8(exponent_index as u8);
        out.put_u32(escaped.len() as u32);
        for vector_index in &escaped {
            out.put_u32(*vector_index);
        }
    }
    out.put_u32(exceptions.len() as u32);
    match side {
        SideStream::None => {
            for (index, bits) in &exceptions {
                out.put_u32(*index);
                out.put_u64(*bits);
            }
        }
        SideStream::ForBitpack => {
            encode_for_bitpack(&indexes, out);
            for (_, bits) in &exceptions {
                out.put_u64(*bits);
            }
        }
        SideStream::FileScopeDictionary | SideStream::FsstDictionaryValues | SideStream::FsstValueKeys => {
            unreachable!("the exception side-stream chooser never picks a string block's side stream")
        }
    }
    let max = ints.iter().copied().max().unwrap_or(0);
    bitpack(&ints, bits_needed(max), out);
    // Raw payloads ride after the packed words, where a sequential decoder already knows the value count needed to
    // size each escaped vector.
    for vector_index in &escaped {
        let start = *vector_index as usize * FASTLANES_VECTOR;
        for value in &values[start..(start + FASTLANES_VECTOR).min(values.len())] {
            out.put_u64(value.to_bits());
        }
    }
    Some(side)
}

/// Reads the escaped-vector index list of an ALP block flagged with [`ALP_VECTOR_ESCAPE_SENTINEL`]: strictly
/// ascending, bounded by the most vectors a block can hold, so forged bytes cannot amplify allocation.
fn read_alp_escaped_vectors(reader: &mut Reader<'_>) -> Result<Vec<u32>, FormatError> {
    let count = reader.u32("alp escaped vector count")? as usize;
    if count > MAX_BLOCK_VALUES / FASTLANES_VECTOR {
        return Err(FormatError::Structural {
            rule: "alp escaped vector count beyond the block bound",
        });
    }
    let mut escaped = Vec::with_capacity(count);
    for _ in 0..count {
        let index = reader.u32("alp escaped vector index")?;
        if escaped.last().is_some_and(|last| *last >= index) {
            return Err(FormatError::Structural {
                rule: "alp escaped vector indexes must be strictly ascending",
            });
        }
        escaped.push(index);
    }
    Ok(escaped)
}

/// Overwrites each escaped vector's slots with the raw values stored after the packed words. `values` is the full
/// decoded block, so each escaped vector's extent is exact; an index past the block refuses.
fn apply_alp_escapes(reader: &mut Reader<'_>, escaped: &[u32], values: &mut [f64]) -> Result<(), FormatError> {
    for vector_index in escaped {
        let start = *vector_index as usize * FASTLANES_VECTOR;
        if start >= values.len() {
            return Err(FormatError::RefOutOfRange {
                what: "alp escaped vector index",
            });
        }
        let end = (start + FASTLANES_VECTOR).min(values.len());
        for slot in &mut values[start..end] {
            *slot = f64::from_bits(reader.u64("alp escaped value")?);
        }
    }
    Ok(())
}

/// Reads an ALP block's exception list, honouring the recorded side-stream inner encoding.
fn decode_alp_exceptions(reader: &mut Reader<'_>, side: SideStream) -> Result<Vec<(u32, u64)>, FormatError> {
    let exception_count = reader.u32("alp exception count")? as usize;
    let mut exceptions = Vec::with_capacity(reader.capacity_hint(exception_count, 12));
    match side {
        SideStream::FileScopeDictionary | SideStream::FsstDictionaryValues | SideStream::FsstValueKeys => {
            return Err(FormatError::Structural {
                rule: "alp blocks never carry a string block's side stream",
            });
        }
        SideStream::None => {
            for _ in 0..exception_count {
                let index = reader.u32("alp exception index")?;
                let bits = reader.u64("alp exception bits")?;
                exceptions.push((index, bits));
            }
        }
        SideStream::ForBitpack => {
            let indexes: Vec<u64> = decode_for_bitpack(reader)?;
            if indexes.len() != exception_count {
                return Err(FormatError::Structural {
                    rule: "alp exception index stream disagrees with the exception count",
                });
            }
            for index in indexes {
                let bits = reader.u64("alp exception bits")?;
                let index = u32::try_from(index).map_err(|_| FormatError::RefOutOfRange {
                    what: "alp exception index",
                })?;
                exceptions.push((index, bits));
            }
        }
    }
    Ok(exceptions)
}

fn decode_alp(reader: &mut Reader<'_>, side: SideStream) -> Result<Vec<f64>, FormatError> {
    let mut exponent_index = reader.u8("alp exponent")? as usize;
    let mut escaped = Vec::new();
    if exponent_index == usize::from(ALP_VECTOR_ESCAPE_SENTINEL) {
        exponent_index = reader.u8("alp exponent")? as usize;
        escaped = read_alp_escaped_vectors(reader)?;
    }
    let power = ALP_POWERS.get(exponent_index).copied().ok_or(FormatError::Structural {
        rule: "alp exponent out of range",
    })?;
    let exceptions = decode_alp_exceptions(reader, side)?;
    let stream = read_packed_stream(reader)?;
    let mut values = vec![0f64; stream.padded_count()];
    // Reconstruction — unzigzag, convert, divide — rides inside the unpack's lane writes, so the block decodes in one
    // pass. A stream narrow enough for the branch-free conversion takes the form that vectorizes; a wider (forged or
    // pathological) stream keeps the scalar conversion, whose semantics both forms share where they overlap.
    if stream.width <= ALP_EXACT_CONVERT_MAX_WIDTH {
        stream.unpack_into(&mut values, |packed| alp_narrow_int_to_f64(packed) / power);
    } else {
        stream.unpack_into(&mut values, |packed| (unzigzag(packed) as f64) / power);
    }
    values.truncate(stream.count);
    for (index, bits) in exceptions {
        let slot = values.get_mut(index as usize).ok_or(FormatError::RefOutOfRange {
            what: "alp exception index",
        })?;
        *slot = f64::from_bits(bits);
    }
    apply_alp_escapes(reader, &escaped, &mut values)?;
    Ok(values)
}

fn encode_plain_f64(values: &[f64], out: &mut Writer) {
    out.put_u32(values.len() as u32);
    // An `f64`'s bits are its `to_bits()` value by definition, so the slice casts straight to `u64` without a
    // per-value `to_bits()` call, and `put_u64_slice` writes it in one bulk `memcpy` on a little-endian target.
    out.put_u64_slice(bytemuck::cast_slice(values));
}

/// Writes `values` regrouped into 8 same-significance planes straight into `out` (exactly `values.len() * 8` bytes):
/// plane 0 holds every value's byte 0, plane 1 every value's byte 1, and so on — eight linear, streaming passes (one
/// per plane) rather than one scattered store per byte, mirroring the eight linear passes [`decode_byte_stream_split`]
/// already reassembles a value from.
fn write_byte_stream_split(values: &[f64], out: &mut [u8]) {
    if values.is_empty() {
        return;
    }
    let plane_len = values.len();
    let bits: &[u64] = bytemuck::cast_slice(values);
    for (plane_index, plane) in out.chunks_exact_mut(plane_len).enumerate() {
        let shift = (8 * plane_index) as u32;
        for (slot, value) in plane.iter_mut().zip(bits) {
            *slot = (value >> shift) as u8;
        }
    }
}

/// The bytes of `values`, regrouped into 8 same-significance planes (see [`write_byte_stream_split`]) — for a caller
/// that needs them as a standalone buffer rather than written into a [`Writer`] in place, as
/// [`choose_float_fallback`]'s sample trial does.
fn byte_stream_split_bytes(values: &[f64]) -> Vec<u8> {
    let mut out = vec![0u8; values.len() * 8];
    write_byte_stream_split(values, &mut out);
    out
}

/// Picks the transform for a float column ALP rejected: ALP-RD, byte-stream-split, or plain, whichever the sample says
/// stores smallest. Byte-stream-split regroups bytes by significance so the high-order planes (sign/exponent) form
/// long runs deflate can shrink; plain interleaves all 8 bytes of every value, which defeats deflate's small window on
/// noisy data; ALP-RD wins when the top bits cluster into a tiny dictionary while the low bits are true noise no
/// byte-level codec can shrink.
fn choose_float_fallback(values: &[f64]) -> Transform {
    let sample = transform_sample(values);
    let sample: &[f64] = sample.as_deref().unwrap_or(values);
    if sample.is_empty() {
        return Transform::PlainF64;
    }
    let plain_bytes: Vec<u8> = sample.iter().flat_map(|value| value.to_le_bytes()).collect();
    let bss_bytes = byte_stream_split_bytes(sample);
    let plain_compressed = deflate::compress(&plain_bytes).len();
    let bss_compressed = deflate::compress(&bss_bytes).len();
    let (_, _, alp_rd_estimate) = alp_rd_choose_split(sample);
    if alp_rd_estimate < plain_compressed.min(bss_compressed) {
        Transform::AlpRd
    } else if bss_compressed < plain_compressed {
        Transform::ByteStreamSplit
    } else {
        Transform::PlainF64
    }
}

/// Byte-stream-split block: a row count, then 8 same-significance byte planes, written straight into the space
/// reserved for them (see [`write_byte_stream_split`]) instead of through a temporary buffer.
fn encode_byte_stream_split(values: &[f64], out: &mut Writer) {
    out.put_u32(values.len() as u32);
    write_byte_stream_split(values, out.reserve_bytes(values.len() * 8));
}

fn decode_byte_stream_split(reader: &mut Reader<'_>) -> Result<Vec<f64>, FormatError> {
    let count = reader.u32("byte-stream-split count")? as usize;
    let mut planes: Vec<&[u8]> = Vec::with_capacity(8);
    for _ in 0..8 {
        planes.push(reader.take(count, "byte-stream-split plane")?);
    }
    // Each plane is exactly `count` bytes, so eight linear passes reassemble every value without a per-value bounds
    // check: pass `p` ORs each row's plane-`p` byte into bit position `8 * p`.
    let mut bits = vec![0u64; count];
    for (plane_index, plane) in planes.iter().enumerate() {
        let shift = (8 * plane_index) as u32;
        for (slot, byte) in bits.iter_mut().zip(plane.iter()) {
            *slot |= u64::from(*byte) << shift;
        }
    }
    // A pure bit reinterpret, not a value mapping: cast the buffer in place instead of collecting a second one.
    Ok(bytemuck::cast_vec(bits))
}

/// Picks the ALP-RD split for a sample: the right-part bit width and the left-part dictionary that minimize the
/// estimated encoded size, plus that estimate in bytes so selection can weigh ALP-RD against the other float
/// fallbacks. Deterministic: candidate widths are tried in a fixed order and ties keep the first winner.
fn alp_rd_choose_split(sample: &[f64]) -> (u32, Vec<u16>, usize) {
    let mut best: Option<(u32, Vec<u16>, usize)> = None;
    for right_width in ALP_RD_MIN_RIGHT_WIDTH..=ALP_RD_MAX_RIGHT_WIDTH {
        let mut counts: std::collections::BTreeMap<u16, usize> = std::collections::BTreeMap::new();
        for value in sample {
            *counts.entry((value.to_bits() >> right_width) as u16).or_default() += 1;
        }
        let mut ranked: Vec<(u16, usize)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked.truncate(ALP_RD_MAX_DICT_LEN);
        let covered: usize = ranked.iter().map(|(_, count)| count).sum();
        let dict: Vec<u16> = ranked.into_iter().map(|(left, _)| left).collect();
        let exceptions = sample.len() - covered;
        let code_bits = bits_needed(dict.len().saturating_sub(1) as u64) as usize;
        let estimate =
            2 + 2 * dict.len() + 4 + exceptions * 6 + (sample.len() * (code_bits + right_width as usize)).div_ceil(8);
        if best.as_ref().is_none_or(|(_, _, current)| estimate < *current) {
            best = Some((right_width, dict, estimate));
        }
    }
    best.unwrap_or((ALP_RD_MAX_RIGHT_WIDTH, Vec::new(), usize::MAX))
}

/// Splits one run of floats at `right_width`, writing each value's masked right part into `rights` and its left part's
/// dictionary code into `codes` — [`ALP_RD_CODE_MISS`] where the left part is not in the dictionary. The dictionary
/// arrives padded to [`ALP_RD_MAX_DICT_LEN`] entries with `dict_len` naming how many are real, so the lookup is a
/// fixed-length compare-select chain (one splat-compare per entry) instead of a per-value linear search.
///
/// Compiled once per instruction set and dispatched on the CPU features detected at runtime; every clone computes the
/// same shifts, masks, and compares, so the codes are identical whichever clone runs. Requirement: "SIMD kernels
/// dispatch on detected CPU features at runtime".
#[multiversion(targets("x86_64+avx512f+avx512bw+avx512dq+avx512vl", "x86_64+avx2", "aarch64+neon"))]
fn alp_rd_split_chunk(
    values: &[f64],
    right_width: u32,
    dict: &[u16; ALP_RD_MAX_DICT_LEN],
    dict_len: usize,
    codes: &mut [u64],
    rights: &mut [u64],
) {
    let right_mask = mask(right_width);
    for ((value, code_slot), right_slot) in values.iter().zip(codes.iter_mut()).zip(rights.iter_mut()) {
        let bits = value.to_bits();
        *right_slot = bits & right_mask;
        let left = (bits >> right_width) as u16;
        let mut code = ALP_RD_CODE_MISS;
        // Walked highest entry first so the last write wins for the lowest matching entry, matching a first-match
        // linear search even if the dictionary ever carried a duplicate.
        for (entry_index, entry) in dict.iter().enumerate().rev() {
            if entry_index < dict_len && *entry == left {
                code = entry_index as u64;
            }
        }
        *code_slot = code;
    }
}

/// ALP-RD block: right-part width, left-part dictionary, exact left-part exceptions, then bitpacked codes and right
/// parts. The split and dictionary come from the block's sample; a value whose left part missed the dictionary stores
/// code 0 and patches its real left part through the exception list.
fn encode_alp_rd(values: &[f64], out: &mut Writer) {
    let sample = transform_sample(values);
    let sample: &[f64] = sample.as_deref().unwrap_or(values);
    let (right_width, dict, _) = alp_rd_choose_split(sample);
    let mut padded_dict = [0u16; ALP_RD_MAX_DICT_LEN];
    for (slot, entry) in padded_dict.iter_mut().zip(&dict) {
        *slot = *entry;
    }
    let mut codes = vec![0u64; values.len()];
    let mut rights = vec![0u64; values.len()];
    alp_rd_split_chunk(values, right_width, &padded_dict, dict.len(), &mut codes, &mut rights);
    let mut exceptions: Vec<(u32, u16)> = Vec::new();
    for (index, (code, value)) in codes.iter_mut().zip(values).enumerate() {
        if *code == ALP_RD_CODE_MISS {
            *code = 0;
            exceptions.push((index as u32, (value.to_bits() >> right_width) as u16));
        }
    }
    out.put_u8(right_width as u8);
    out.put_u8(dict.len() as u8);
    for left in &dict {
        out.put_u16(*left);
    }
    out.put_u32(exceptions.len() as u32);
    for (index, left) in &exceptions {
        out.put_u32(*index);
        out.put_u16(*left);
    }
    let max_code = codes.iter().copied().max().unwrap_or(0);
    bitpack(&codes, bits_needed(max_code), out);
    let max_right = rights.iter().copied().max().unwrap_or(0);
    bitpack(&rights, bits_needed(max_right), out);
}

fn decode_alp_rd(reader: &mut Reader<'_>) -> Result<Vec<f64>, FormatError> {
    let right_width = u32::from(reader.u8("alp-rd right width")?);
    if !(ALP_RD_MIN_RIGHT_WIDTH..=ALP_RD_MAX_RIGHT_WIDTH).contains(&right_width) {
        return Err(FormatError::Structural {
            rule: "alp-rd right width out of range",
        });
    }
    let dict_len = reader.u8("alp-rd dictionary length")? as usize;
    if dict_len > ALP_RD_MAX_DICT_LEN {
        return Err(FormatError::Structural {
            rule: "alp-rd dictionary too long",
        });
    }
    // A left part carrying bits at or above the split would collide with the right part on reassembly.
    let left_limit = 1u64 << (64 - right_width);
    let mut dict = Vec::with_capacity(dict_len);
    for _ in 0..dict_len {
        let left = reader.u16("alp-rd dictionary entry")?;
        if u64::from(left) >= left_limit {
            return Err(FormatError::Structural {
                rule: "alp-rd left part exceeds its width",
            });
        }
        dict.push(left);
    }
    let exception_count = reader.u32("alp-rd exception count")? as usize;
    if exception_count > MAX_BLOCK_VALUES {
        return Err(FormatError::Structural {
            rule: "alp-rd exception count beyond the block bound",
        });
    }
    let mut exceptions = Vec::with_capacity(reader.capacity_hint(exception_count, 6));
    for _ in 0..exception_count {
        let index = reader.u32("alp-rd exception index")?;
        let left = reader.u16("alp-rd exception left part")?;
        if u64::from(left) >= left_limit {
            return Err(FormatError::Structural {
                rule: "alp-rd left part exceeds its width",
            });
        }
        exceptions.push((index, left));
    }
    let codes = bitunpack(reader)?;
    let rights = bitunpack(reader)?;
    if codes.len() != rights.len() {
        return Err(FormatError::Structural {
            rule: "alp-rd code and right-part streams disagree",
        });
    }
    let mut values = Vec::with_capacity(codes.len());
    for (code, right) in codes.iter().zip(rights.iter()) {
        if *right >> right_width != 0 {
            return Err(FormatError::Structural {
                rule: "alp-rd right part exceeds its width",
            });
        }
        let left = dict
            .get(*code as usize)
            .copied()
            .ok_or(FormatError::RefOutOfRange { what: "alp-rd code" })?;
        values.push(f64::from_bits((u64::from(left) << right_width) | *right));
    }
    for (index, left) in exceptions {
        let right = rights.get(index as usize).ok_or(FormatError::RefOutOfRange {
            what: "alp-rd exception index",
        })?;
        let slot = values.get_mut(index as usize).ok_or(FormatError::RefOutOfRange {
            what: "alp-rd exception index",
        })?;
        *slot = f64::from_bits((u64::from(left) << right_width) | *right);
    }
    Ok(values)
}

/// The stored form of one string block's null side stream. The encoder picks the smallest deterministic form for the
/// block's actual null pattern; every form decodes to the identical presence answers.
///
/// Variants are in strict alphabetical order; the wire tags (`all_present` 0, `all_absent` 1, runs 2, raw 3) are
/// written and read in [`NullStream::write`] / [`NullStream::read`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum NullStreamForm {
    /// No row carries a value — zero stored bytes beyond the header.
    AllAbsent,
    /// Every row carries a value — zero stored bytes beyond the header.
    AllPresent,
    /// The raw one-bit-per-row LSB-0 bitmap: the fallback and the equivalence reference for the compressed forms.
    Raw(Vec<u8>),
    /// Sorted, disjoint, non-adjacent `[start, end)` runs of present rows — the roaring-style run form, smallest when
    /// nulls cluster (the usual case for event data).
    Runs(Vec<(u32, u32)>),
}

/// One string block's presence-of-value stream, shared by every string transform (dictionary, FSST, raw) instead of
/// each writing its own unconditional raw bitmap. An all-present block stores zero null bytes; clustered nulls store
/// a short run list; only a scattered pattern falls back to the raw bitmap. Presence questions — is a row present,
/// how many present rows precede it — are answered directly on the stored form, never by first materializing the raw
/// bitmap. Requirement: "Presence and null bitmaps are encoded side streams".
#[derive(Debug, Clone, PartialEq, Eq)]
struct NullStream {
    form: NullStreamForm,
    row_count: usize,
}

/// Sets bits `[start, end)` in a packed little-endian bitmap, byte-filling whole bytes and masking only the two
/// partial edge bytes rather than setting one bit at a time.
fn set_bit_range(bits: &mut [u8], start: usize, end: usize) {
    if start >= end {
        return;
    }
    let start_byte = start / 8;
    let end_byte = (end - 1) / 8;
    let low = start % 8;
    let high = (end - 1) % 8;
    if start_byte == end_byte {
        bits[start_byte] |= (0xFFu8 << low) & (0xFFu8 >> (7 - high));
        return;
    }
    bits[start_byte] |= 0xFFu8 << low;
    for byte in &mut bits[start_byte + 1..end_byte] {
        *byte = 0xFF;
    }
    bits[end_byte] |= 0xFFu8 >> (7 - high);
}

impl NullStream {
    /// Derives the stream from a block's values, choosing the smallest form: `all_present` / `all_absent` at zero
    /// bytes, the run form when it undercuts the raw bitmap, the raw bitmap otherwise. Deterministic, so
    /// byte-identical encodes across nodes hold unchanged.
    fn from_values(values: &StringColumn) -> Self {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for (index, value) in values.iter().enumerate() {
            if value.is_some() {
                let index = index as u32;
                match runs.last_mut() {
                    Some(run) if run.1 == index => run.1 = index + 1,
                    _ => runs.push((index, index + 1)),
                }
            }
        }
        let row_count = values.len();
        let form = if runs.is_empty() {
            NullStreamForm::AllAbsent
        } else if runs == [(0, row_count as u32)] {
            NullStreamForm::AllPresent
        } else if 4 + runs.len() * 8 < row_count.div_ceil(8) {
            NullStreamForm::Runs(runs)
        } else {
            let mut raw = vec![0u8; row_count.div_ceil(8)];
            for &(start, end) in &runs {
                set_bit_range(&mut raw, start as usize, end as usize);
            }
            NullStreamForm::Raw(raw)
        };
        Self { form, row_count }
    }

    /// Writes the stream: the row count, the form tag, then the form's payload (nothing for the zero-byte forms).
    fn write(&self, out: &mut Writer) {
        out.put_u32(self.row_count as u32);
        match &self.form {
            NullStreamForm::AllPresent => out.put_u8(0),
            NullStreamForm::AllAbsent => out.put_u8(1),
            NullStreamForm::Runs(runs) => {
                out.put_u8(2);
                out.put_u32(runs.len() as u32);
                for &(start, end) in runs {
                    out.put_u32(start);
                    out.put_u32(end);
                }
            }
            NullStreamForm::Raw(bits) => {
                out.put_u8(3);
                out.put_slice(bits);
            }
        }
    }

    /// Reads a stream written by [`write`](Self::write), refusing malformed input: an unknown form tag, a zero-byte
    /// form whose row count is pure output amplification, or runs that are empty, out of bounds, or not sorted and
    /// non-adjacent.
    fn read(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        let row_count = reader.u32("string row count")? as usize;
        let form = match reader.u8("null stream form")? {
            0 | 1 if row_count > MAX_BLOCK_VALUES => {
                // The zero-byte forms consume no input per row, so a forged count would amplify straight into the
                // decoder's output allocation; bound it like the zero-width bitpack count.
                return Err(FormatError::Structural {
                    rule: "null stream row count beyond the block bound",
                });
            }
            0 => NullStreamForm::AllPresent,
            1 => NullStreamForm::AllAbsent,
            2 => {
                let run_count = reader.u32("null stream run count")? as usize;
                let mut runs = Vec::with_capacity(reader.capacity_hint(run_count, 8));
                for _ in 0..run_count {
                    let start = reader.u32("null stream run start")?;
                    let end = reader.u32("null stream run end")?;
                    if end <= start || end as usize > row_count {
                        return Err(FormatError::Structural {
                            rule: "null stream run must be non-empty and within the row count",
                        });
                    }
                    if runs.last().is_some_and(|&(_, previous_end)| start <= previous_end) {
                        return Err(FormatError::Structural {
                            rule: "null stream runs must be sorted and non-adjacent",
                        });
                    }
                    runs.push((start, end));
                }
                NullStreamForm::Runs(runs)
            }
            3 => NullStreamForm::Raw(reader.take(row_count.div_ceil(8), "null bitmap")?.to_vec()),
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown null stream form",
                });
            }
        };
        Ok(Self { form, row_count })
    }

    fn row_count(&self) -> usize {
        self.row_count
    }

    /// How many rows carry a value, counted on the stored form (run widths, or a popcount for the raw fallback).
    fn present_count(&self) -> usize {
        match &self.form {
            NullStreamForm::AllAbsent => 0,
            NullStreamForm::AllPresent => self.row_count,
            NullStreamForm::Raw(bits) => count_set_bits(bits, self.row_count),
            NullStreamForm::Runs(runs) => runs.iter().map(|&(start, end)| (end - start) as usize).sum(),
        }
    }

    /// Whether `row` carries a value. Rows at or past the row count are absent.
    fn is_present(&self, row: usize) -> bool {
        if row >= self.row_count {
            return false;
        }
        match &self.form {
            NullStreamForm::AllAbsent => false,
            NullStreamForm::AllPresent => true,
            NullStreamForm::Raw(bits) => bits.get(row / 8).is_some_and(|byte| byte & (1 << (row % 8)) != 0),
            NullStreamForm::Runs(runs) => {
                let idx = runs.partition_point(|&(start, _)| start as usize <= row);
                idx.checked_sub(1)
                    .and_then(|i| runs.get(i))
                    .is_some_and(|&(_, end)| row < end as usize)
            }
        }
    }

    /// How many present rows appear strictly before `row` — the rank that maps a row to its dense position among the
    /// stored values, answered on the stored form without expanding it.
    fn present_before(&self, row: usize) -> usize {
        let row = row.min(self.row_count);
        match &self.form {
            NullStreamForm::AllAbsent => 0,
            NullStreamForm::AllPresent => row,
            NullStreamForm::Raw(bits) => count_present_before(bits, row),
            NullStreamForm::Runs(runs) => runs
                .iter()
                .take_while(|&&(start, _)| (start as usize) < row)
                .map(|&(start, end)| (end as usize).min(row) - start as usize)
                .sum(),
        }
    }

    /// Calls `row` once per row, in row order, with whether that row carries a value — the linear walk the decode
    /// loops thread stored values back through.
    ///
    /// The stored form is matched once, ahead of the walk, rather than on every row: the two zero-byte forms are a
    /// straight count of one constant, the run form alternates whole present and absent spans, and only the raw
    /// bitmap is read bit by bit. Inlined into a caller, the constant spans leave nothing per row for the caller's
    /// own present/absent branch to test.
    fn each_row(&self, mut row: impl FnMut(bool) -> Result<(), FormatError>) -> Result<(), FormatError> {
        match &self.form {
            NullStreamForm::AllAbsent => {
                for _ in 0..self.row_count {
                    row(false)?;
                }
            }
            NullStreamForm::AllPresent => {
                for _ in 0..self.row_count {
                    row(true)?;
                }
            }
            NullStreamForm::Raw(bits) => {
                for index in 0..self.row_count {
                    row(bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0))?;
                }
            }
            NullStreamForm::Runs(runs) => {
                let mut cursor = 0usize;
                for &(start, end) in runs {
                    for _ in cursor..start as usize {
                        row(false)?;
                    }
                    for _ in start as usize..end as usize {
                        row(true)?;
                    }
                    cursor = end as usize;
                }
                for _ in cursor..self.row_count {
                    row(false)?;
                }
            }
        }
        Ok(())
    }

    /// Calls `row` once per row in `[start, end)`, in row order, with whether that row carries a value — the range
    /// counterpart of [`each_row`]: the stored form is matched once, and the `Runs` form walks only the runs that
    /// intersect the range instead of a `partition_point` binary search per row.
    fn each_row_range(
        &self,
        start: usize,
        end: usize,
        mut row: impl FnMut(bool) -> Result<(), FormatError>,
    ) -> Result<(), FormatError> {
        let end = end.min(self.row_count);
        if start >= end {
            return Ok(());
        }
        match &self.form {
            NullStreamForm::AllAbsent => {
                for _ in start..end {
                    row(false)?;
                }
            }
            NullStreamForm::AllPresent => {
                for _ in start..end {
                    row(true)?;
                }
            }
            NullStreamForm::Raw(bits) => {
                for index in start..end {
                    row(bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0))?;
                }
            }
            NullStreamForm::Runs(runs) => {
                let mut cursor = start;
                let first = runs.partition_point(|&(_, run_end)| (run_end as usize) <= start);
                for &(run_start, run_end) in &runs[first..] {
                    let run_start = (run_start as usize).max(start);
                    let run_end = (run_end as usize).min(end);
                    if run_start >= end {
                        break;
                    }
                    for _ in cursor..run_start {
                        row(false)?;
                    }
                    for _ in run_start..run_end {
                        row(true)?;
                    }
                    cursor = run_end;
                }
                for _ in cursor..end {
                    row(false)?;
                }
            }
        }
        Ok(())
    }
}

/// The present values of a string block, in row order — the dense value list every string transform encodes behind
/// the shared null side stream.
fn present_values(values: &StringColumn) -> Vec<&str> {
    // Sized exactly: the column knows how many of its rows carry a value, and the flattening iterator that yields
    // them cannot say, so collecting straight from it would grow one allocation through every doubling up to a
    // granule's worth of rows.
    let mut present = Vec::with_capacity(values.len() - values.null_count());
    present.extend(values.iter_present());
    present
}

fn write_offsets(lens: impl Iterator<Item = usize>, out: &mut Writer) -> Result<(), FormatError> {
    let mut offset = 0u32;
    out.put_u32(offset);
    for len in lens {
        offset = u32::try_from(len)
            .ok()
            .and_then(|len| offset.checked_add(len))
            .ok_or(FormatError::Structural {
                rule: "string arena offsets exceed the u32 range the format stores",
            })?;
        out.put_u32(offset);
    }
    Ok(())
}

fn read_offsets(reader: &mut Reader<'_>, count: usize) -> Result<Vec<usize>, FormatError> {
    Ok(decode_offsets(
        reader.take(count.saturating_add(1).saturating_mul(4), "string offset")?,
    ))
}

/// The stored offset entries a byte range covers, decoded from their `u32` form.
fn decode_offsets(bytes: &[u8]) -> Vec<usize> {
    bytes
        .chunks_exact(4)
        .map(|entry| u32::from_le_bytes(entry.try_into().unwrap_or([0; 4])) as usize)
        .collect()
}

/// `count` entries of a block's offset table, starting at entry `first`, read straight out of the stored table.
/// A range decode needs one entry per requested value plus the closing bound, so this is what keeps it off the
/// whole table — 8 bytes for a single-row read instead of one entry per row in the block.
fn offset_window(table: &[u8], first: usize, count: usize) -> Result<Vec<usize>, FormatError> {
    Ok(decode_offsets(slice(
        table,
        first.saturating_mul(4),
        count.saturating_mul(4),
        "string offset",
    )?))
}

/// Tries the FSST inner cascade level on a dictionary's distinct-value stream: trains a symbol table, compresses every
/// entry (the scratch buffer budgets the FSST worst case of two output bytes per input byte), and keeps the level only
/// when the compressed value stream — symbol table included — clears the strategy's size bar against the plain stream.
/// Entries longer than [`DICTIONARY_FSST_MAX_VALUE_LEN`] keep the stream plain. Returns what the level emits, or
/// `None` to keep the plain value stream.
fn fsst_dictionary_values(
    dictionary: &[&str],
    strategy: CascadeStrategy,
) -> Option<(fsst::Compressor, Vec<usize>, Vec<u8>)> {
    if !cascade_level_fits(1) || dictionary.len() < FSST_MIN_SAMPLE_COUNT {
        return None;
    }
    if dictionary
        .iter()
        .any(|value| value.len() > DICTIONARY_FSST_MAX_VALUE_LEN)
    {
        return None;
    }
    let corpus: Vec<&[u8]> = dictionary.iter().map(|value| value.as_bytes()).collect();
    let compressor = fsst::Compressor::train(&corpus);
    let mut scratch = Vec::new();
    let mut arena = Vec::new();
    let mut compressed_lens = Vec::with_capacity(corpus.len());
    for value in &corpus {
        compress_fsst_value(&compressor, value, &mut scratch);
        compressed_lens.push(scratch.len());
        arena.extend_from_slice(&scratch);
    }
    let plain = (dictionary.len() + 1) * 4 + corpus.iter().map(|value| value.len()).sum::<usize>();
    let compressed = 2 + 9 * compressor.symbol_table().len() + (dictionary.len() + 1) * 4 + arena.len();
    let threshold = match strategy {
        CascadeStrategy::DecodeOptimized | CascadeStrategy::NoTrailing => {
            plain * DECODE_OPTIMIZED_THRESHOLD_NUM / DECODE_OPTIMIZED_THRESHOLD_DEN
        }
        CascadeStrategy::SizeOptimized => plain - plain / SIZE_OPTIMIZED_SAVINGS_DIVISOR,
    };
    (compressed < threshold).then_some((compressor, compressed_lens, arena))
}

/// Dictionary string block: null bitmap, sorted distinct values, then each present row's code bitpacked. When every
/// distinct value is present in the column's file-scope shared alphabet, the block's code stream against the
/// alphabet (its sorted-order assignment) competes with the local dictionary form in an exact size trial, and the
/// smaller stored form wins — a block covering only a corner of a wide alphabet pays wider codes at file scope than
/// its own narrow dictionary would, so coverage alone cannot decide. The local dictionary's distinct-value stream
/// may ride the FSST inner cascade level (see [`fsst_dictionary_values`]). Codes keep their ascending sorted-order
/// assignment in every scope, so compressed-form predicates on codes never notice. Returns the side stream the block
/// records.
fn encode_dictionary_string(
    values: &StringColumn,
    present: &[&str],
    strategy: CascadeStrategy,
    shared: Option<&[String]>,
    out: &mut Writer,
) -> Result<SideStream, FormatError> {
    NullStream::from_values(values).write(out);
    // Dictionary encoding is chosen precisely when distinct values are a small fraction of the rows, so hash-dedup
    // the (row-sized) present values down to the distinct set before paying to sort — sorting only the distinct
    // set, not every row.
    let mut dictionary: Vec<&str> = AHashSet::from_iter(present.iter().copied()).into_iter().collect();
    dictionary.sort_unstable();
    // One hash map built from the (small) distinct set turns each present row's code lookup from an
    // O(log distinct) binary search into an O(1) hash lookup.
    let code_of: AHashMap<&str, u64> = dictionary
        .iter()
        .enumerate()
        .map(|(index, value)| (*value, index as u64))
        .collect();
    let codes: Vec<u64> = present
        .iter()
        .map(|value| code_of.get(value).copied().unwrap_or(0))
        .collect();
    if let Some(alphabet) = shared
        && dictionary
            .iter()
            .all(|value| alphabet.binary_search_by(|entry| entry.as_str().cmp(value)).is_ok())
    {
        // Each distinct value's alphabet index is resolved once here, then every present row's already-computed
        // local code is translated to it in O(1) instead of every row paying its own alphabet binary search.
        let alphabet_index_of: Vec<u64> = dictionary
            .iter()
            .map(|value| {
                alphabet
                    .binary_search_by(|entry| entry.as_str().cmp(value))
                    .map(|index| index as u64)
                    .unwrap_or(0)
            })
            .collect();
        let shared_codes: Vec<u64> = codes
            .iter()
            .map(|&code| alphabet_index_of.get(code as usize).copied().unwrap_or(0))
            .collect();
        let max = shared_codes.iter().copied().max().unwrap_or(0);
        let mut shared_out = Writer::new();
        bitpack(&shared_codes, bits_needed(max), &mut shared_out);
        let shared_bytes = shared_out.into_bytes();
        let mut local_out = Writer::new();
        let local_side = encode_local_dictionary(&dictionary, &codes, strategy, &mut local_out)?;
        let local_bytes = local_out.into_bytes();
        if shared_bytes.len() < local_bytes.len() {
            out.put_slice(&shared_bytes);
            return Ok(SideStream::FileScopeDictionary);
        }
        out.put_slice(&local_bytes);
        return Ok(local_side);
    }
    encode_local_dictionary(&dictionary, &codes, strategy, out)
}

/// The block-local dictionary form: distinct values (plain or through the FSST inner cascade level), then each
/// present row's already-resolved code ([`encode_dictionary_string`]'s `codes`, sorted-order assignment against
/// `dictionary`) bitpacked. One candidate in [`encode_dictionary_string`]'s scope trial, and the only form for a
/// block the column's shared alphabet does not cover.
fn encode_local_dictionary(
    dictionary: &[&str],
    codes: &[u64],
    strategy: CascadeStrategy,
    out: &mut Writer,
) -> Result<SideStream, FormatError> {
    out.put_u32(dictionary.len() as u32);
    let side = match fsst_dictionary_values(dictionary, strategy) {
        Some((compressor, compressed_lens, arena)) => {
            let symbols = compressor.symbol_table();
            let lengths = compressor.symbol_lengths();
            out.put_u16(symbols.len() as u16);
            for (symbol, length) in symbols.iter().zip(lengths.iter()) {
                out.put_u64(symbol.to_u64());
                out.put_u8(*length);
            }
            write_offsets(compressed_lens.into_iter(), out)?;
            out.put_slice(&arena);
            SideStream::FsstDictionaryValues
        }
        None => {
            write_offsets(dictionary.iter().map(|value| value.len()), out)?;
            for value in dictionary {
                out.put_slice(value.as_bytes());
            }
            SideStream::None
        }
    };
    let max = codes.iter().copied().max().unwrap_or(0);
    bitpack(codes, bits_needed(max), out);
    Ok(side)
}

/// FSST string block: null bitmap, symbol table, then every present value compressed and laid end to end behind a
/// byte-offset table. A block whose values are long enough for [`fsst_value_keys_pay`] also stores the per-value keys
/// of [`sidecar`] after the arena — a prefix key run then a fingerprint run — and records
/// [`SideStream::FsstValueKeys`] so a reader knows they are there. Compresses with `replay`'s table when one is
/// given and trains its own otherwise; either table is written into the block the same way. Returns the side stream
/// the block records, the table it trained, if it trained one, and the body offsets where the prefix keys and the
/// fingerprints begin — where a framed trailing stage ends a frame, so a filter reading the fingerprints or the arena
/// inflates neither the other run nor the prefix keys.
fn encode_fsst_string(
    values: &StringColumn,
    present: &[&str],
    replay: Option<&FsstTable>,
    out: &mut Writer,
) -> Result<(SideStream, Option<FsstTable>, Vec<usize>), FormatError> {
    NullStream::from_values(values).write(out);
    let corpus: Vec<&[u8]> = present.iter().map(|value| value.as_bytes()).collect();
    let table = match replay {
        Some(table) => Cow::Borrowed(table),
        None => Cow::Owned(FsstTable(fsst::Compressor::train(&corpus))),
    };
    let compressor = &table.0;
    let symbols = compressor.symbol_table();
    let lengths = compressor.symbol_lengths();
    out.put_u16(symbols.len() as u16);
    for (symbol, length) in symbols.iter().zip(lengths.iter()) {
        out.put_u64(symbol.to_u64());
        out.put_u8(*length);
    }
    let mut scratch = Vec::new();
    let mut arena = Vec::new();
    let mut compressed_lens = Vec::with_capacity(corpus.len());
    for value in &corpus {
        compress_fsst_value(compressor, value, &mut scratch);
        compressed_lens.push(scratch.len());
        arena.extend_from_slice(&scratch);
    }
    let trained = match table {
        Cow::Owned(table) => Some(table),
        Cow::Borrowed(_) => None,
    };
    out.put_u32(present.len() as u32);
    write_offsets(compressed_lens.into_iter(), out)?;
    out.put_slice(&arena);
    if !fsst_value_keys_pay(present) {
        return Ok((SideStream::None, trained, Vec::new()));
    }
    // Two runs rather than one interleaved record: a substring filter reads only the fingerprints and a range filter
    // only the prefix keys, so each walks a tight run of its own.
    let prefixes_start = out.len();
    for value in present {
        out.put_slice(&PrefixKey::of(value).to_stored());
    }
    let fingerprints_start = out.len();
    for value in present {
        out.put_u32(StringFingerprint::of(value.as_bytes()).to_bits());
    }
    Ok((
        SideStream::FsstValueKeys,
        trained,
        vec![prefixes_start, fingerprints_start],
    ))
}

/// Whether an FSST block's per-value keys earn their bytes.
///
/// The keys buy a filter the right to skip decompression, so they are worth storing exactly where decompression
/// hurts: they are kept only when their bytes come to at most a
/// [`FSST_VALUE_KEYS_MAX_PLAINTEXT_DIVISOR`]-th of the plaintext they let a filter leave compressed. A block of short
/// values fails that and keeps the plain layout — decompressing it whole is cheap, and its range filters go on
/// falling back to a decode.
fn fsst_value_keys_pay(present: &[&str]) -> bool {
    if !cascade_level_fits(1) {
        return false;
    }
    let keys = present
        .len()
        .saturating_mul(PREFIX_KEY_BYTES + StringFingerprint::STORED_BYTES);
    let plaintext: usize = present.iter().map(|value| value.len()).sum();
    keys.saturating_mul(FSST_VALUE_KEYS_MAX_PLAINTEXT_DIVISOR) <= plaintext
}

/// Compresses one value into `scratch`, reusing its allocation across calls instead of allocating a fresh buffer per
/// value the way `fsst::Compressor::compress` does. Produces exactly the bytes `compress` would, including the empty
/// fast path.
fn compress_fsst_value(compressor: &fsst::Compressor, plaintext: &[u8], scratch: &mut Vec<u8>) {
    scratch.clear();
    if plaintext.is_empty() {
        return;
    }
    scratch.reserve(plaintext.len() * 2);
    // SAFETY: `compress_into` requires the output buffer's capacity to cover the worst-case compressed size, which is
    // two bytes per input byte (every byte an escape) — the same bound `compress` itself allocates. The buffer was
    // just cleared, so the `reserve` above guarantees `scratch.capacity() >= plaintext.len() * 2`.
    unsafe { compressor.compress_into(plaintext, scratch) };
}

fn encode_raw_string(values: &StringColumn, present: &[&str], out: &mut Writer) -> Result<(), FormatError> {
    NullStream::from_values(values).write(out);
    out.put_u32(present.len() as u32);
    write_offsets(present.iter().map(|value| value.len()), out)?;
    for value in present {
        out.put_slice(value.as_bytes());
    }
    Ok(())
}

/// Threads a block's densely stored present values back through its null stream, landing them in one arena-backed
/// column rather than a string per row.
fn merge_nulls<T: AsRef<str>>(nulls: &NullStream, present: &[T]) -> Result<StringColumn, FormatError> {
    let text_bytes = present.iter().map(|value| value.as_ref().len()).sum();
    let mut values = StringColumn::with_capacity(nulls.row_count(), text_bytes);
    let mut next = 0usize;
    nulls.each_row(|set| {
        if set {
            let value = present.get(next).ok_or(FormatError::Structural {
                rule: "null bitmap disagrees with present count",
            })?;
            values.push(Some(value.as_ref()));
            next += 1;
        } else {
            values.push(None);
        }
        Ok(())
    })?;
    if next != present.len() {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    Ok(values)
}

/// Reads a dictionary block's distinct-value stream — plain, or FSST-compressed when the block records the
/// [`SideStream::FsstDictionaryValues`] inner level — into owned entries, in code order.
fn read_dictionary_values(
    reader: &mut Reader<'_>,
    dict_count: usize,
    side: SideStream,
) -> Result<Vec<String>, FormatError> {
    let fsst = match side {
        SideStream::FsstDictionaryValues => Some(FsstDecodeTable::read(reader)?),
        _ => None,
    };
    let offsets = read_offsets(reader, dict_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "dictionary data")?;
    let mut dictionary = Vec::with_capacity(reader.capacity_hint(dict_count, 1));
    if let Some(table) = &fsst {
        let arena = fsst_decompress_arena(
            table,
            data,
            &offsets,
            "dictionary entry",
            "dictionary offsets must be non-decreasing",
        )?;
        for index in 0..arena.len() {
            let entry = arena.value(index).ok_or(FormatError::InvalidUtf8 {
                what: "dictionary entry",
            })?;
            if entry.len() > DICTIONARY_FSST_MAX_VALUE_LEN {
                return Err(FormatError::Structural {
                    rule: "dictionary entry exceeds the FSST value cap",
                });
            }
            dictionary.push(entry.to_owned());
        }
        return Ok(dictionary);
    }
    for pair in offsets.windows(2) {
        let (start, end) = match pair {
            [start, end] => (*start, *end),
            _ => continue,
        };
        if end < start {
            return Err(FormatError::Structural {
                rule: "dictionary offsets must be non-decreasing",
            });
        }
        let bytes = slice(data, start, end - start, "dictionary entry")?;
        dictionary.push(
            simdutf8::basic::from_utf8(bytes)
                .map_err(|_| FormatError::InvalidUtf8 {
                    what: "dictionary entry",
                })?
                .to_owned(),
        );
    }
    Ok(dictionary)
}

fn decode_dictionary_string(
    reader: &mut Reader<'_>,
    side: SideStream,
    shared: Option<&[String]>,
) -> Result<StringColumn, FormatError> {
    let nulls = NullStream::read(reader)?;
    if side == SideStream::FileScopeDictionary {
        let alphabet = shared.ok_or(FormatError::Structural {
            rule: "shared-scope dictionary block without its file alphabet",
        })?;
        let codes = bitunpack(reader)?;
        let present = resolve_dictionary_codes(&codes, alphabet)?;
        return merge_nulls(&nulls, &present);
    }
    let dict_count = reader.u32("dictionary count")? as usize;
    let dictionary = read_dictionary_values(reader, dict_count, side)?;
    let codes = bitunpack(reader)?;
    let present = resolve_dictionary_codes(&codes, &dictionary)?;
    merge_nulls(&nulls, &present)
}

/// Resolves each stored code to the dictionary entry it names, borrowed rather than copied — the codes outnumber the
/// entries, so a dense column would otherwise copy the same handful of values once per row.
fn resolve_dictionary_codes<'a>(codes: &[u64], dictionary: &'a [String]) -> Result<Vec<&'a str>, FormatError> {
    codes
        .iter()
        .map(|code| {
            dictionary
                .get(*code as usize)
                .map(String::as_str)
                .ok_or(FormatError::RefOutOfRange {
                    what: "dictionary code",
                })
        })
        .collect()
}

/// fsst-rs's lossy perfect-hash-table size (`fsst::lossy_pht::HASH_TABLE_SIZE`), pinned with fsst-rs 0.5.11. A slot
/// index is this hash masked to the table size, so `SIZE - 1` is also the slot mask.
const FSST_LOSSY_PHT_SIZE: u64 = 1 << 11;

/// Reproduces the slot a length-≥3 symbol lands in inside fsst-rs's lossy perfect-hash table. `rebuild_from` inserts
/// every such symbol into that table and asserts if two collide; honest tables never collide, but a forged table can
/// be built to force the assert, which aborts the process under `panic = "abort"`. Computing the slot here lets the
/// reader reject a colliding table as a [`FormatError`] before handing it to the crate. Mirrors `fsst::builder::fsst_hash`
/// over the symbol's low three bytes, masked to the table size; the multiplier and the table size are both fixed by the
/// pinned fsst-rs 0.5.11.
fn fsst_lossy_pht_slot(symbol_word: u64) -> u64 {
    let prefix_3_bytes = symbol_word & 0xFF_FF_FF;
    let hash =
        prefix_3_bytes.wrapping_mul(FSST_LOSSY_PHT_HASH_MULTIPLIER) ^ (prefix_3_bytes >> FSST_LOSSY_PHT_HASH_SHIFT);
    hash & (FSST_LOSSY_PHT_SIZE - 1)
}

/// Reads an FSST symbol table and rebuilds the crate's compressor from it, rejecting any table `fsst` itself would
/// abort on: more than 255 symbols, a symbol length outside 1..=8, lengths out of FSST order (non-decreasing while
/// at least 2, then all 1s), or two length-≥3 symbols that collide in the crate's lossy hash table. Decoders of
/// untrusted bytes must refuse, never panic, so every violation is a [`FormatError`] here instead of an assert
/// inside the crate.
///
/// Only a scan that compresses a needle to compare against stored codes needs the compressor; a path that merely
/// decodes takes [`FsstDecodeTable::read`], which parses the same table without rebuilding the encoder.
fn read_fsst_compressor(reader: &mut Reader<'_>) -> Result<fsst::Compressor, FormatError> {
    Ok(FsstDecodeTable::read(reader)?.compressor())
}

/// A block's whole FSST arena after decompression, plus where every value landed inside it: `bounds` marks each
/// value's start and end in `text`, so it runs one longer than the value count.
struct FsstArena {
    bounds: Vec<usize>,
    text: String,
}

impl FsstArena {
    /// How many values the arena holds.
    fn len(&self) -> usize {
        self.bounds.len().saturating_sub(1)
    }

    /// The plaintext of value `index`, or `None` when the arena holds no such value or the block's offsets cut a
    /// character in half. The decompressed bytes validate as UTF-8 as a whole, but a boundary landing mid-character
    /// would not carve them into values that each do, which is what a caller hands on.
    fn value(&self, index: usize) -> Option<&str> {
        let (start, end) = (self.bounds.get(index)?, self.bounds.get(index + 1)?);
        self.text.get(*start..*end)
    }
}

/// Decompresses a whole FSST arena at once rather than one value at a time.
///
/// `offsets` names the compressed values inside `data`, one offset per value boundary. The values decompress one
/// after another into this thread's reusable buffer (see [`fsst_decompress_values`]), which is UTF-8-validated once
/// and copied out as the arena. Callers read the values straight out of that arena through [`FsstArena::value`]
/// instead of paying an allocation and a check per value. A caller that only searches the arena should use
/// [`with_fsst_arena_bytes`], which needs neither the copy nor the validation.
fn fsst_decompress_arena(
    table: &FsstDecodeTable,
    data: &[u8],
    offsets: &[usize],
    what: &'static str,
    rule: &'static str,
) -> Result<FsstArena, FormatError> {
    with_arena_buffer(|buffer| {
        let bounds = fsst_decompress_values_with_table(table, data, offsets, what, rule, buffer)?;
        let text = simdutf8::basic::from_utf8(buffer.as_slice())
            .map_err(|_| FormatError::InvalidUtf8 { what })?
            .to_owned();
        Ok(FsstArena { bounds, text })
    })
}

/// Runs `search` over an FSST block's whole plaintext arena and its value bounds, without allocating the arena.
///
/// The plaintext lands in this thread's reusable buffer instead of a fresh `String` per block, and is never
/// UTF-8-validated: a search over the bytes needs neither the allocation nor the validation pass that rebuilding the
/// arena as text would pay for every block it walks. The values were valid UTF-8 when they were written, and `search`
/// only ever reads bytes.
pub(crate) fn with_fsst_arena_bytes<R>(
    compressor: &fsst::Compressor,
    data: &[u8],
    offsets: &[usize],
    what: &'static str,
    rule: &'static str,
    search: impl FnOnce(&[u8], &[usize]) -> R,
) -> Result<R, FormatError> {
    with_arena_buffer(|buffer| {
        let bounds = fsst_decompress_values(compressor, data, offsets, what, rule, buffer)?;
        Ok(search(buffer, &bounds))
    })
}

/// Decompresses an FSST block's values one after another onto the end of `buffer`, returning where each landed: one
/// bound more than there are values, so `bounds[i]..bounds[i + 1]` spans value `i` of `buffer`.
///
/// `offsets` names the compressed values inside `data`, one offset per value boundary. One pass over the codes both
/// writes the plaintext and records each value's bound as the write reaches it, so nothing walks the codes twice. A
/// code the table never defined, an escape with no literal byte after it, or an offset that runs backwards or past
/// `data` is a format error, never an out-of-bounds read. `what` names the values in those errors and `rule` the
/// offset ordering they must keep.
fn fsst_decompress_values(
    compressor: &fsst::Compressor,
    data: &[u8],
    offsets: &[usize],
    what: &'static str,
    rule: &'static str,
    buffer: &mut Vec<u8>,
) -> Result<Vec<usize>, FormatError> {
    let table = FsstDecodeTable::new(compressor);
    fsst_decompress_values_with_table(&table, data, offsets, what, rule, buffer)
}

/// [`fsst_decompress_values`] with an already-read decode table, used by the string-view path so it does not rebuild
/// an encoder-capable compressor merely to recover this table.
fn fsst_decompress_values_with_table(
    table: &FsstDecodeTable,
    data: &[u8],
    offsets: &[usize],
    what: &'static str,
    rule: &'static str,
    buffer: &mut Vec<u8>,
) -> Result<Vec<usize>, FormatError> {
    let Some((&arena_start, ends)) = offsets.split_first() else {
        return Ok(Vec::new());
    };
    if arena_start > data.len() {
        return Err(FormatError::Truncated { what });
    }
    let base = buffer.len();
    let mut bounds = Vec::with_capacity(offsets.len());
    bounds.push(base);
    // Eight bytes per code byte the arena could hold, reserved once. The offsets are checked as the values are
    // walked, so the values decoded before any bad offset lie end to end inside `data[arena_start..]` and use at
    // most eight bytes per byte of it between them.
    buffer.reserve((data.len() - arena_start) * constant::FSST_MAX_SYMBOL_BYTES);
    let out = buffer.spare_capacity_mut().as_mut_ptr().cast::<u8>();
    let mut written = 0usize;
    let mut value_start = arena_start;
    for &value_end in ends {
        if value_end < value_start {
            return Err(FormatError::Structural { rule });
        }
        let codes = data
            .get(value_start..value_end)
            .ok_or(FormatError::Truncated { what })?;
        // SAFETY: `written` is at most eight times the code bytes decoded so far, all of them before `codes` in
        // `data[arena_start..]`, so `slot` has at least eight bytes per byte of `codes` still spare.
        let slot = unsafe { out.add(written) };
        // SAFETY: `slot` has the room `decode_fsst_codes` asks for, as just shown.
        written += unsafe { table.decode_fsst_codes(codes, slot) }?;
        bounds.push(base + written);
        value_start = value_end;
    }
    // SAFETY: the decodes initialized exactly `written` bytes at the front of the spare capacity.
    unsafe { buffer.set_len(base + written) };
    Ok(bounds)
}

/// Decompresses one FSST value onto the end of `buffer`.
fn fsst_decompress_value(table: &FsstDecodeTable, compressed: &[u8], buffer: &mut Vec<u8>) -> Result<(), FormatError> {
    buffer.reserve(compressed.len() * constant::FSST_MAX_SYMBOL_BYTES);
    let out = buffer.spare_capacity_mut().as_mut_ptr().cast::<u8>();
    // SAFETY: `out` is the front of the spare capacity, which the reserve above sized for these codes.
    let written = unsafe { table.decode_fsst_codes(compressed, out) }?;
    // SAFETY: `decode_fsst_codes` initialized exactly `written` bytes at the front of the spare capacity.
    unsafe { buffer.set_len(buffer.len() + written) };
    Ok(())
}

/// A block's FSST symbol table laid out for decoding: one slot for every code byte a block can hold, so a stored code
/// always lands inside it, and a code the table never defined has length zero — a length no real symbol has — so the
/// decoder tells a forged code apart in the load it makes anyway.
///
/// The crate's own decoder is not used: it takes a whole run of codes with no way to learn where each value's
/// plaintext ends, and calling it once per value costs more in setup than the few codes a value holds cost to decode.
struct FsstDecodeTable {
    lengths: [u8; 256],
    symbol_count: usize,
    symbols: [u64; 256],
}

impl FsstDecodeTable {
    fn new(compressor: &fsst::Compressor) -> Self {
        let mut table = Self {
            lengths: [0; 256],
            symbol_count: compressor.symbol_lengths().len(),
            symbols: [0; 256],
        };
        let slots = table.lengths.iter_mut().zip(table.symbols.iter_mut());
        let defined = compressor.symbol_table().iter().zip(compressor.symbol_lengths());
        for ((slot_length, slot_symbol), (symbol, &len)) in slots.zip(defined) {
            *slot_length = len;
            *slot_symbol = symbol.to_u64();
        }
        table
    }

    /// The crate's encoder-capable compressor over this table, for a scan that must compress a needle. Rebuilding
    /// it costs the crate's lossy hash table, so decoders never call this.
    fn compressor(&self) -> fsst::Compressor {
        let lengths: Vec<u8> = self.lengths.iter().copied().take(self.symbol_count).collect();
        let symbols: Vec<fsst::Symbol> = self
            .symbols
            .iter()
            .take(self.symbol_count)
            .map(|word| fsst::Symbol::from_slice(&word.to_le_bytes()))
            .collect();
        fsst::Compressor::rebuild_from(symbols, lengths)
    }

    /// Reads the stored symbol table straight into the decoder layout. A decode never needs the compressor's
    /// encoder-side lossy hash table, so rebuilding a full [`fsst::Compressor`] only to copy these two arrays back
    /// out wastes work on every block. This accepts and rejects exactly the table shapes [`read_fsst_compressor`]
    /// does (it is that function's parser), including the collision check that keeps forged input away from the
    /// crate's aborting rebuild path.
    fn read(reader: &mut Reader<'_>) -> Result<Self, FormatError> {
        let symbol_count = reader.u16("fsst symbol count")? as usize;
        if symbol_count > FSST_MAX_SYMBOL_COUNT {
            return Err(FormatError::Structural {
                rule: "fsst symbol count beyond 255",
            });
        }
        let mut table = Self {
            lengths: [0; 256],
            symbol_count,
            symbols: [0; 256],
        };
        let mut occupied_long_slots = [0u64; FSST_LOSSY_PHT_SIZE as usize / u64::BITS as usize];
        let mut expected = 2u8;
        for index in 0..symbol_count {
            let word = reader.u64("fsst symbol")?;
            let len = reader.u8("fsst symbol length")?;
            if !(1..=8).contains(&len) {
                return Err(FormatError::Structural {
                    rule: "fsst symbol length outside 1..=8",
                });
            }
            if expected == 1 {
                if len != 1 {
                    return Err(FormatError::Structural {
                        rule: "fsst symbol lengths out of order",
                    });
                }
            } else if len == 1 {
                expected = 1;
            } else if len < expected {
                return Err(FormatError::Structural {
                    rule: "fsst symbol lengths out of order",
                });
            } else {
                expected = len;
            }
            if len >= 3 {
                let slot = fsst_lossy_pht_slot(word) as usize;
                let word_index = slot / u64::BITS as usize;
                let bit = 1u64 << (slot % u64::BITS as usize);
                if occupied_long_slots[word_index] & bit != 0 {
                    return Err(FormatError::Structural {
                        rule: "fsst long-symbol table collides in the decoder hash",
                    });
                }
                occupied_long_slots[word_index] |= bit;
            }
            table.lengths[index] = len;
            table.symbols[index] = word;
        }
        Ok(table)
    }

    /// Decodes one value's `codes` to `out`, returning how many bytes it wrote. A code the table never defined or an
    /// escape with no literal byte after it is a format error. The bytes written are exactly those the crate's
    /// decoder writes for the same codes.
    ///
    /// Codes go eight at a time while a whole word of them is left: one test finds whether the word holds an escape,
    /// and a word without one expands with a single undefined-code check for all eight, so the checks the one-at-a-
    /// time loop paid per code are paid per word instead. A word with an escape expands up to it, takes the escaped
    /// literal, and resumes on whole words; the last few codes of a value go one at a time.
    ///
    /// # Safety
    ///
    /// `out` must have room for [`FSST_MAX_SYMBOL_BYTES`](constant::FSST_MAX_SYMBOL_BYTES) bytes per byte of `codes`:
    /// every code writes its symbol's whole eight-byte word (only the symbol's length of it counts — none of it for
    /// an undefined code, which is reported once its word is done) after consuming at least one input byte, so no
    /// write reaches past eight times the input bytes consumed through it.
    #[inline(always)]
    unsafe fn decode_fsst_codes(&self, codes: &[u8], out: *mut u8) -> Result<usize, FormatError> {
        let mut rest = codes;
        let mut written = 0usize;
        while let Some((chunk, after)) = rest.split_first_chunk::<{ size_of::<u64>() }>() {
            let word = u64::from_le_bytes(*chunk);
            let escapes = fsst_escape_bytes(word);
            if escapes == 0 {
                let mut defined = true;
                for shift in (0..u64::BITS).step_by(u8::BITS as usize) {
                    // SAFETY: as the caller guarantees; `written` is at most eight times the codes consumed so far.
                    defined &= unsafe { self.emit_fsst_symbol((word >> shift) as u8, out, &mut written) };
                }
                if !defined {
                    return Err(FormatError::RefOutOfRange { what: "fsst code" });
                }
                rest = after;
                continue;
            }
            let clean = (escapes.trailing_zeros() / u8::BITS) as usize;
            for shift in (0..clean as u32 * u8::BITS).step_by(u8::BITS as usize) {
                // SAFETY: as the caller guarantees; `written` is at most eight times the codes consumed so far.
                if !unsafe { self.emit_fsst_symbol((word >> shift) as u8, out, &mut written) } {
                    return Err(FormatError::RefOutOfRange { what: "fsst code" });
                }
            }
            // `clean` is under eight and `rest` holds at least eight, so this always finds the escape.
            let [_escape, literal, after @ ..] = rest.get(clean..).unwrap_or_default() else {
                return Err(FormatError::Truncated {
                    what: "fsst escaped byte",
                });
            };
            // SAFETY: `written` is at most eight times the codes consumed so far, inside the room the caller
            // guarantees.
            let slot = unsafe { out.add(written) };
            // SAFETY: one byte of the room the caller guarantees.
            unsafe { slot.write(*literal) };
            written = written.wrapping_add(1);
            rest = after;
        }
        while let Some((&code, after)) = rest.split_first() {
            if code == fsst::ESCAPE_CODE {
                let Some((&literal, after)) = after.split_first() else {
                    return Err(FormatError::Truncated {
                        what: "fsst escaped byte",
                    });
                };
                // SAFETY: `written` is at most eight times the codes consumed so far, inside the room the caller
                // guarantees.
                let slot = unsafe { out.add(written) };
                // SAFETY: one byte of the room the caller guarantees.
                unsafe { slot.write(literal) };
                written = written.wrapping_add(1);
                rest = after;
            } else {
                // SAFETY: as the caller guarantees; `written` is at most eight times the codes consumed so far.
                if !unsafe { self.emit_fsst_symbol(code, out, &mut written) } {
                    return Err(FormatError::RefOutOfRange { what: "fsst code" });
                }
                rest = after;
            }
        }
        Ok(written)
    }

    /// Writes `code`'s symbol at `out.add(*written)` and advances `written` by its length, answering whether the
    /// table defines the code. An undefined code writes an all-zero word and advances nothing, so a caller may fold
    /// the answers of several codes and check them once.
    ///
    /// # Safety
    ///
    /// `out.add(*written)` must have eight bytes of room. `written` never overflows: it stays within eight times a
    /// slice's length, so the wrapping add is only there to skip the overflow check.
    #[inline(always)]
    unsafe fn emit_fsst_symbol(&self, code: u8, out: *mut u8, written: &mut usize) -> bool {
        let len = self.lengths.get(usize::from(code)).copied().unwrap_or(0);
        let symbol = self.symbols.get(usize::from(code)).copied().unwrap_or(0);
        // SAFETY: the caller guarantees room at `out.add(*written)`.
        let slot = unsafe { out.add(*written) };
        // SAFETY: eight bytes of the room the caller guarantees; only `len` of them count.
        unsafe { slot.cast::<u64>().write_unaligned(symbol) };
        *written = written.wrapping_add(usize::from(len));
        len != 0
    }
}

/// The high bit of every byte of `word` that holds [`fsst::ESCAPE_CODE`], found without a test per byte: the
/// classic has-a-zero-byte test over the inverted word. It answers "any escape at all" exactly, and its lowest set
/// bit marks the first escape exactly; bytes above the first may be flagged by a borrow, so nothing else is read
/// off it.
fn fsst_escape_bytes(word: u64) -> u64 {
    const { assert!(fsst::ESCAPE_CODE == u8::MAX) };
    let inverted = !word;
    inverted.wrapping_sub(0x0101_0101_0101_0101) & !inverted & 0x8080_8080_8080_8080
}

/// Runs `read` over the plaintext of just the values `selected` names, and over where each of them landed.
///
/// A filter that has already ruled most rows out from a block's per-value keys ([`sidecar`]) wants the real text of
/// only the few left over, so this decompresses only those — into the same reusable buffer
/// [`with_fsst_arena_bytes`] uses, laid end to end with a bound per value, so the caller still reads them in one
/// pass. Selecting every value costs what decompressing the whole arena costs, which is the worst case a filter that
/// prunes nothing falls back to. `selected` names value positions in `offsets`; one past the block's values is a
/// format error.
pub(crate) fn with_fsst_selected_bytes<R>(
    compressor: &fsst::Compressor,
    data: &[u8],
    offsets: &[usize],
    selected: &[usize],
    what: &'static str,
    rule: &'static str,
    read: impl FnOnce(&[u8], &[usize]) -> R,
) -> Result<R, FormatError> {
    let table = FsstDecodeTable::new(compressor);
    with_arena_buffer(|buffer| {
        let mut bounds = Vec::with_capacity(selected.len() + 1);
        bounds.push(buffer.len());
        for index in selected {
            let (Some(&start), Some(&end)) = (offsets.get(*index), offsets.get(index + 1)) else {
                return Err(FormatError::RefOutOfRange { what });
            };
            if end < start {
                return Err(FormatError::Structural { rule });
            }
            let compressed = slice(data, start, end - start, what)?;
            fsst_decompress_value(&table, compressed, buffer)?;
            bounds.push(buffer.len());
        }
        Ok(read(buffer, &bounds))
    })
}

fn decode_fsst_string(reader: &mut Reader<'_>) -> Result<StringColumn, FormatError> {
    let nulls = NullStream::read(reader)?;
    let table = FsstDecodeTable::read(reader)?;
    let present_count = reader.u32("fsst present count")? as usize;
    let offsets = read_offsets(reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "fsst data")?;
    let arena = fsst_decompress_arena(
        &table,
        data,
        &offsets,
        "fsst value",
        "fsst offsets must be non-decreasing",
    )?;
    let mut present: Vec<&str> = Vec::with_capacity(arena.len());
    for index in 0..arena.len() {
        present.push(
            arena
                .value(index)
                .ok_or(FormatError::InvalidUtf8 { what: "fsst value" })?,
        );
    }
    merge_nulls(&nulls, &present)
}

fn decode_raw_string(reader: &mut Reader<'_>) -> Result<StringColumn, FormatError> {
    let nulls = NullStream::read(reader)?;
    let present_count = reader.u32("raw present count")? as usize;
    let offsets = read_offsets(reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "raw string data")?;
    let mut present = Vec::with_capacity(present_count);
    for pair in offsets.windows(2) {
        let (start, end) = match pair {
            [start, end] => (*start, *end),
            _ => continue,
        };
        if end < start {
            return Err(FormatError::Structural {
                rule: "raw string offsets must be non-decreasing",
            });
        }
        let bytes = slice(data, start, end - start, "raw string")?;
        present.push(simdutf8::basic::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 { what: "raw string" })?);
    }
    merge_nulls(&nulls, &present)
}

/// Decodes a string block straight into an Arrow `StringView` array: the same values and nulls `decode_block` returns
/// as `ColumnData::Strings`, but each value lands in the view builder's own buffer with no per-value `String` on the
/// way — so short strings sit inline in the finished array and equality or prefix predicates compare them without a
/// heap indirection.
///
/// Returns `Ok(None)` when the pipeline is not one of the directly view-decodable string encodings (dictionary, FSST,
/// raw); the caller then falls back to `decode_block`'s `ColumnData` path.
pub fn decode_string_block_views(pipeline: PipelineId, bytes: &[u8]) -> Result<Option<StringViewArray>, FormatError> {
    decode_string_block_views_shared(pipeline, bytes, None)
}

/// [`decode_string_block_views`] with the column's file-scope shared alphabet, for blocks recording the file
/// dictionary scope.
pub fn decode_string_block_views_shared(
    pipeline: PipelineId,
    bytes: &[u8],
    shared: Option<&[String]>,
) -> Result<Option<StringViewArray>, FormatError> {
    decode_string_block_views_shared_prepared(pipeline, bytes, shared, None)
}

/// The immutable Arrow backing buffer and one view per entry of a file-scope string dictionary. A
/// [`HefFile`](crate::layout::reader::HefFile) prepares this once when it opens and every granule then gathers its
/// codes from these views without concatenating and revalidating the same alphabet again.
#[derive(Debug, Clone)]
pub(crate) struct SharedStringViewDictionary {
    arena: Buffer,
    entry_views: Vec<u128>,
}

/// Prepares a file-scope alphabet for repeated Arrow string-view decodes.
pub(crate) fn prepare_shared_string_view_dictionary(
    shared: &[String],
) -> Result<SharedStringViewDictionary, FormatError> {
    let entries: Vec<&str> = shared.iter().map(String::as_str).collect();
    let (arena, bounds) = concatenate(&entries);
    let entry_views = views_over(&arena, &bounds)?;
    Ok(SharedStringViewDictionary {
        arena: Buffer::from(arena),
        entry_views,
    })
}

/// [`decode_string_block_views_shared`] with a prepared representation of the same file-scope alphabet. The prepared
/// value is used only by blocks that record [`SideStream::FileScopeDictionary`]; all other pipelines decode normally.
pub(crate) fn decode_string_block_views_shared_prepared(
    pipeline: PipelineId,
    bytes: &[u8],
    shared: Option<&[String]>,
    prepared: Option<&SharedStringViewDictionary>,
) -> Result<Option<StringViewArray>, FormatError> {
    if pipeline.value_kind()? != ValueKind::String {
        return Ok(None);
    }
    let transform = pipeline.transform()?;
    if !matches!(
        transform,
        Transform::DictionaryString | Transform::FsstString | Transform::RawString
    ) {
        return Ok(None);
    }
    with_trailing_removed(pipeline.compression()?, bytes, |body| {
        decode_string_block_views_with_body(pipeline, transform, body, shared, prepared)
    })
}

/// [`decode_string_block_views_shared_prepared`] over a block's decompressed body, `transform` already screened to a
/// string encoding.
fn decode_string_block_views_with_body(
    pipeline: PipelineId,
    transform: Transform,
    body: &[u8],
    shared: Option<&[String]>,
    prepared: Option<&SharedStringViewDictionary>,
) -> Result<Option<StringViewArray>, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = NullStream::read(&mut reader)?;

    // Every encoding here already holds its text in one contiguous run of bytes — the dictionary's entries, the FSST
    // arena, the raw arena — so the decode hands that run to Arrow as the array's single data buffer and describes
    // each value with a view into it. Nothing is copied per value.
    let (arena, views): (Buffer, Vec<u128>) = match transform {
        Transform::DictionaryString => {
            let side = pipeline.side_stream()?;
            if side == SideStream::FileScopeDictionary {
                let owned_dictionary;
                let dictionary = match prepared {
                    Some(dictionary) => dictionary,
                    None => {
                        let alphabet = shared.ok_or(FormatError::Structural {
                            rule: "shared-scope dictionary block without its file alphabet",
                        })?;
                        owned_dictionary = prepare_shared_string_view_dictionary(alphabet)?;
                        &owned_dictionary
                    }
                };
                let codes = bitunpack(&mut reader)?;
                (
                    dictionary.arena.clone(),
                    dictionary_views_by_row(&nulls, &dictionary.entry_views, &codes)?,
                )
            } else {
                let mut entries: Vec<&str> = Vec::new();
                let owned: Vec<String>;
                if side == SideStream::FsstDictionaryValues {
                    let dict_count = reader.u32("dictionary count")? as usize;
                    owned = read_dictionary_values(&mut reader, dict_count, side)?;
                    entries.extend(owned.iter().map(String::as_str));
                } else {
                    let dict_count = reader.u32("dictionary count")? as usize;
                    let offsets = read_offsets(&mut reader, dict_count)?;
                    let data_len = offsets.last().copied().unwrap_or(0);
                    let data = reader.take(data_len, "dictionary data")?;
                    for pair in offsets.windows(2) {
                        let (start, end) = match pair {
                            [start, end] => (*start, *end),
                            _ => continue,
                        };
                        if end < start {
                            return Err(FormatError::Structural {
                                rule: "dictionary offsets must be non-decreasing",
                            });
                        }
                        let bytes = slice(data, start, end - start, "dictionary entry")?;
                        entries.push(simdutf8::basic::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
                            what: "dictionary entry",
                        })?);
                    }
                }
                // One view per distinct value, then one gather per row: a row costs a 16-byte copy out of a table as
                // long as the dictionary, never a copy of its text.
                //
                // Every entry arrived as a `&str`, so the arena is their concatenation and each entry's view spans exactly
                // one of them — the UTF-8 proof [`StringViewArray::new_unchecked`] needs, taken over the dictionary rather
                // than over one view per row.
                let (arena, bounds) = concatenate(&entries);
                let entry_views = views_over(&arena, &bounds)?;
                let codes = bitunpack(&mut reader)?;
                (
                    Buffer::from(arena),
                    dictionary_views_by_row(&nulls, &entry_views, &codes)?,
                )
            }
        }
        Transform::FsstString => {
            let table = FsstDecodeTable::read(&mut reader)?;
            let present_count = reader.u32("fsst present count")? as usize;
            let offsets = read_offsets(&mut reader, present_count)?;
            let data_len = offsets.last().copied().unwrap_or(0);
            let data = reader.take(data_len, "fsst data")?;
            // The values decompress one after another into what becomes the array's own data buffer, each one's
            // bound read off the buffer as it grows. The decoder reserves that buffer once, at its ceiling of eight
            // bytes per stored code, so the finished arena is trimmed to the bytes written rather than decoded into a
            // scratch buffer and copied out: the allocator trims a reservation of up to 2 MiB in place, and moves a
            // larger one for about what the copy cost.
            let mut arena = Vec::new();
            let bounds = fsst_decompress_values_with_table(
                &table,
                data,
                &offsets,
                "fsst value",
                "fsst offsets must be non-decreasing",
                &mut arena,
            )?;
            let views = string_views_by_row(&nulls, &arena, &bounds, "fsst value")?;
            arena.shrink_to_fit();
            (Buffer::from(arena), views)
        }
        Transform::RawString => {
            let present_count = reader.u32("raw present count")? as usize;
            let offsets = read_offsets(&mut reader, present_count)?;
            let data_len = offsets.last().copied().unwrap_or(0);
            let data = reader.take(data_len, "raw string data")?;
            let views = string_views_by_row(&nulls, data, &offsets, "raw string value")?;
            (Buffer::from(data.to_vec()), views)
        }
        _ => unreachable!("transform was screened to a string encoding above"),
    };

    // SAFETY: `new_unchecked` needs every view to address bytes inside the data buffer and to describe valid UTF-8.
    // Both are established above, per stored value rather than per row, before any view is taken. The dictionary
    // arms' entry views come from `views_over` — a bounds-checked slice per entry of the arena that becomes the
    // buffer, over entries that arrived as `&str` — and `dictionary_views_by_row` refuses a code past those entries.
    // The FSST and raw arms validate the whole arena as UTF-8 and then `check_value_bounds` proves every value's ends
    // lie inside it on character boundaries. A row's view is a copy of one of those, or the all-zero view of an absent
    // row, which describes an empty inline value.
    let array = unsafe { StringViewArray::new_unchecked(ScalarBuffer::from(views), vec![arena], null_buffer(&nulls)) };
    Ok(Some(array))
}

/// One view per row of a block whose values lie end to end in `arena`, `bounds[i]..bounds[i + 1]` spanning stored
/// value `i`, woven through the block's presence stream. The arena is validated as UTF-8 in one pass and every bound
/// proved in one more; the views are then written straight into their row-sized vector with nothing left to check
/// per value. `what` names the values in the UTF-8 error.
fn string_views_by_row(
    nulls: &NullStream,
    arena: &[u8],
    bounds: &[usize],
    what: &'static str,
) -> Result<Vec<u128>, FormatError> {
    let text = simdutf8::basic::from_utf8(arena).map_err(|_| FormatError::InvalidUtf8 { what })?;
    check_value_bounds(text, bounds)?;
    let views = bounds.windows(2).map(|pair| {
        let &[start, end] = pair else { return 0 };
        // Every range was proved above, so none is out of the arena.
        make_view(arena.get(start..end).unwrap_or_default(), 0, start as u32)
    });
    views_by_row(nulls, views)
}

/// Proves in one pass that `bounds` carve `text` into values: non-decreasing, none past the end, each on a character
/// boundary — so every value's slice of an arena that is valid UTF-8 as a whole is valid UTF-8 on its own — and the
/// last inside the 32-bit offset a view records. Each bound is one value's end and the next one's start, so it is
/// checked once, not once for each value it borders.
fn check_value_bounds(text: &str, bounds: &[usize]) -> Result<(), FormatError> {
    let mut previous = 0;
    for &bound in bounds {
        if bound < previous {
            return Err(FormatError::Structural {
                rule: "string offsets must be non-decreasing",
            });
        }
        if bound > text.len() {
            return Err(FormatError::Truncated { what: "string value" });
        }
        if !text.is_char_boundary(bound) {
            return Err(FormatError::InvalidUtf8 { what: "string value" });
        }
        previous = bound;
    }
    if u32::try_from(previous).is_err() {
        return Err(FormatError::Structural {
            rule: "string arena larger than a view offset can address",
        });
    }
    Ok(())
}

/// One view per row of a dictionary block: the entry view each stored code names, at the rows the presence stream
/// marks present. A code past the entries is refused before any view is gathered.
fn dictionary_views_by_row(nulls: &NullStream, entries: &[u128], codes: &[u64]) -> Result<Vec<u128>, FormatError> {
    if codes.iter().any(|&code| code >= entries.len() as u64) {
        return Err(FormatError::RefOutOfRange {
            what: "dictionary code",
        });
    }
    // Every code was proved in range above.
    let views = codes
        .iter()
        .map(|&code| entries.get(code as usize).copied().unwrap_or(0));
    views_by_row(nulls, views)
}

/// Lays `values` end to end in one buffer, returning it with the boundary offsets — one longer than `values`, opening
/// with a leading `0`, the shape [`views_over`] reads.
fn concatenate(values: &[&str]) -> (Vec<u8>, Vec<usize>) {
    let mut bounds = Vec::with_capacity(values.len() + 1);
    bounds.push(0);
    let mut arena = Vec::with_capacity(values.iter().map(|value| value.len()).sum());
    for value in values {
        arena.extend_from_slice(value.as_bytes());
        bounds.push(arena.len());
    }
    (arena, bounds)
}

/// One Arrow view per value of `arena`, where `bounds[i]..bounds[i + 1]` spans value `i`.
///
/// A view is sixteen bytes: a value of twelve bytes or fewer rides inside it, and a longer one records where it sits
/// in `arena` — which becomes the array's data buffer — so no value's text is copied. The bounds are checked as a
/// whole first, so the views are then written in one exact-size pass with nothing left to check per value.
fn views_over(arena: &[u8], bounds: &[usize]) -> Result<Vec<u128>, FormatError> {
    let Some((_, ends)) = bounds.split_first() else {
        return Ok(Vec::new());
    };
    if bounds
        .windows(2)
        .any(|pair| matches!(pair, [start, end] if end < start))
    {
        return Err(FormatError::Structural {
            rule: "string offsets must be non-decreasing",
        });
    }
    // Non-decreasing, so the last bound is the highest: inside the arena means every value is, and inside a `u32`
    // means every offset a view records is.
    let last = bounds.last().copied().unwrap_or(0);
    if last > arena.len() {
        return Err(FormatError::Truncated { what: "string value" });
    }
    if u32::try_from(last).is_err() {
        return Err(FormatError::Structural {
            rule: "string arena larger than a view offset can address",
        });
    }
    let mut views = Vec::with_capacity(ends.len());
    views.extend(
        bounds
            .iter()
            .zip(ends)
            // Every range was checked above, so none is out of the arena.
            .map(|(&start, &end)| make_view(arena.get(start..end).unwrap_or_default(), 0, start as u32)),
    );
    Ok(views)
}

/// Lays one view per row of the block: the stored values' views, in stored order, at the rows the presence stream
/// marks present, and the zero view, which its null flag hides, at every other row. There must be exactly as many
/// stored values as the stream marks present.
///
/// The vector is allocated at its final length and written once, straight from the stored form: an all-present block
/// collects its views with nothing per row but the view itself, and a block with absent rows zero-fills and then
/// writes only its present rows — whole runs at a time for the run form, bit by bit only for the raw bitmap.
fn views_by_row(nulls: &NullStream, mut values: impl ExactSizeIterator<Item = u128>) -> Result<Vec<u128>, FormatError> {
    if values.len() != nulls.present_count() {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    let rows = nulls.row_count();
    // The counts agree, so a present row never runs out of values below; the `0` fallback is never taken.
    Ok(match &nulls.form {
        NullStreamForm::AllAbsent => vec![0; rows],
        NullStreamForm::AllPresent => values.collect(),
        NullStreamForm::Raw(bits) => {
            let mut views = vec![0u128; rows];
            for (row, slot) in views.iter_mut().enumerate() {
                if bits.get(row / 8).is_some_and(|byte| byte & (1 << (row % 8)) != 0) {
                    *slot = values.next().unwrap_or(0);
                }
            }
            views
        }
        NullStreamForm::Runs(runs) => {
            let mut views = vec![0u128; rows];
            for &(start, end) in runs {
                // `NullStream::read` and `from_values` keep every run inside the row count.
                for slot in views.get_mut(start as usize..end as usize).into_iter().flatten() {
                    *slot = values.next().unwrap_or(0);
                }
            }
            views
        }
    })
}

/// The block's presence stream as an Arrow null buffer, or `None` when every row carries a value.
///
/// The raw form is already the bit order Arrow reads — bit `i` of byte `i / 8`, lowest bit first — so it transfers
/// without a per-row walk.
fn null_buffer(nulls: &NullStream) -> Option<NullBuffer> {
    match &nulls.form {
        NullStreamForm::AllPresent => None,
        NullStreamForm::AllAbsent => Some(NullBuffer::new_null(nulls.row_count())),
        NullStreamForm::Raw(bits) => Some(NullBuffer::new(BooleanBuffer::new(
            Buffer::from_slice_ref(bits),
            0,
            nulls.row_count(),
        ))),
        NullStreamForm::Runs(runs) => {
            let mut builder = BooleanBufferBuilder::new(nulls.row_count());
            for &(start, end) in runs {
                let (start, end) = (start as usize, end as usize);
                if builder.len() < start {
                    builder.append_n(start - builder.len(), false);
                }
                if end > start {
                    builder.append_n(end - start, true);
                }
            }
            if builder.len() < nulls.row_count() {
                builder.append_n(nulls.row_count() - builder.len(), false);
            }
            Some(NullBuffer::new(builder.finish()))
        }
    }
}

fn choose_string_transform(present: &[&str]) -> Transform {
    if present.is_empty() {
        return Transform::RawString;
    }
    let sample = transform_sample(present);
    let sample: &[&str] = sample.as_deref().unwrap_or(present);
    let mut distinct: Vec<&str> = sample.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    // Low cardinality: dictionary. High-cardinality short strings: FSST. Long/opaque strings: raw arena + offsets.
    let average_len = sample.iter().map(|value| value.len()).sum::<usize>() / sample.len().max(1);
    if distinct.len() * DICTIONARY_MAX_CARDINALITY_DIVISOR <= sample.len() {
        Transform::DictionaryString
    } else if average_len <= FSST_MAX_AVERAGE_VALUE_LEN && sample.len() >= FSST_MIN_SAMPLE_COUNT {
        Transform::FsstString
    } else {
        Transform::RawString
    }
}

/// Smallest and largest element of `values` in one pass, matching `iter().min()` / `iter().max()` on ties (the
/// output value is the same regardless of which equal element either picks).
fn min_max<T: Copy + PartialOrd>(values: &[T]) -> Option<(T, T)> {
    let mut iter = values.iter().copied();
    let first = iter.next()?;
    Some(iter.fold((first, first), |(min, max), v| {
        (if v < min { v } else { min }, if v > max { v } else { max })
    }))
}

/// Smallest and largest of `values` in one pass, matching separate `reduce(f64::min)` / `reduce(f64::max)` calls
/// bit-for-bit, including how each treats `NaN`.
fn min_max_f64(values: &[f64]) -> Option<(f64, f64)> {
    let mut iter = values.iter().copied();
    let first = iter.next()?;
    Some(iter.fold((first, first), |(min, max), v| (f64::min(min, v), f64::max(max, v))))
}

fn stats_for(data: &ColumnData) -> BlockStats {
    let mut stats = BlockStats {
        row_count: data.row_count() as u32,
        null_count: 0,
        min_i128: None,
        max_i128: None,
        min_f64: None,
        max_f64: None,
    };
    match data {
        ColumnData::U64(values) => {
            if let Some((min, max)) = min_max(values) {
                stats.min_i128 = Some(i128::from(min));
                stats.max_i128 = Some(i128::from(max));
            }
        }
        ColumnData::I64(values) => {
            if let Some((min, max)) = min_max(values) {
                stats.min_i128 = Some(i128::from(min));
                stats.max_i128 = Some(i128::from(max));
            }
        }
        ColumnData::U128(values) => {
            // Both bounds or neither: a half-converted pair is a footer no reader will accept.
            if let Some((min, max)) = min_max(values)
                && let (Ok(min), Ok(max)) = (i128::try_from(min), i128::try_from(max))
            {
                stats.min_i128 = Some(min);
                stats.max_i128 = Some(max);
            }
        }
        ColumnData::Decimal { values, .. } => {
            if let Some((min, max)) = min_max(values) {
                stats.min_i128 = Some(min);
                stats.max_i128 = Some(max);
            }
        }
        ColumnData::F64(values) => {
            if let Some((min, max)) = min_max_f64(values) {
                stats.min_f64 = Some(min);
                stats.max_f64 = Some(max);
            }
        }
        ColumnData::Strings(values) => {
            stats.null_count = values.null_count() as u32;
        }
    }
    stats
}

/// Whole-block byte size at or below which the trailing cascade compresses the block with both codecs and keeps the
/// smaller result. Below this size the two compressions are cheap enough that sampling would not pay for itself, and
/// running both keeps the codec choice exact.
const TRAILING_FULL_TRIAL_MAX_BYTES: usize = 1 << 15;

/// Number of evenly spaced windows the large-block sampler reads to choose one trailing codec.
const TRAILING_SAMPLE_WINDOWS: usize = 8;

/// Bytes per sampler window (see [`TRAILING_SAMPLE_WINDOWS`]).
const TRAILING_SAMPLE_WINDOW_BYTES: usize = 2048;

/// Picks the trailing stage from both codecs' whole-block outputs, keeping the smaller stored form when it clears the
/// strategy threshold. Sizes are compared as stored — each codec's framing included — so neither wins on bytes it does
/// not pay for. The large-block path in [`apply_trailing`] reaches the same three outcomes from a sample instead of
/// compressing the whole block twice.
/// Picks the winner between the two trailing codecs' whole-block outputs — both already in their final stored form,
/// `zstd_framed` behind the uncompressed-length prefix [`compress_zstd_framed`] puts in front of it and `lz4` behind
/// its own — so the lengths compare directly and the winner needs no more framing before being returned.
fn pick_trailing(bytes: Vec<u8>, threshold: usize, lz4: Vec<u8>, zstd_framed: Vec<u8>) -> (Compression, Vec<u8>) {
    let lz4_wins = !lz4.is_empty() && lz4.len() < threshold;
    let zstd_wins = !zstd_framed.is_empty() && zstd_framed.len() < threshold && zstd_framed.len() < lz4.len();
    if zstd_wins {
        (Compression::Zstd1, zstd_framed)
    } else if lz4_wins {
        (Compression::Lz4, lz4)
    } else {
        (Compression::None, bytes)
    }
}

thread_local! {
    /// One [`trailing_sample`] scratch buffer per thread, reused for every large block that thread samples instead of
    /// a fresh up-to-[`TRAILING_SAMPLE_WINDOW_BYTES`]` * `[`TRAILING_SAMPLE_WINDOWS`]-byte allocation per call.
    static TRAILING_SAMPLE_SCRATCH: RefCell<Vec<u8>> = RefCell::new(Vec::new());
}

/// Builds a small representative sample of `bytes` — concatenating [`TRAILING_SAMPLE_WINDOWS`] evenly spaced windows
/// of [`TRAILING_SAMPLE_WINDOW_BYTES`] each, so the sample reflects the whole block rather than just its head — into
/// this thread's reused scratch buffer, and hands it to `f`. The window positions are fixed, so the sample — and
/// therefore the codec it chooses — is deterministic and simulation replay is unaffected.
fn with_trailing_sample<R>(bytes: &[u8], f: impl FnOnce(&[u8]) -> R) -> R {
    TRAILING_SAMPLE_SCRATCH.with(|cell| {
        let mut sample = cell.borrow_mut();
        sample.clear();
        let window = TRAILING_SAMPLE_WINDOW_BYTES.min(bytes.len());
        let span = bytes.len().saturating_sub(window);
        for i in 0..TRAILING_SAMPLE_WINDOWS {
            let start = if TRAILING_SAMPLE_WINDOWS > 1 {
                span * i / (TRAILING_SAMPLE_WINDOWS - 1)
            } else {
                0
            };
            if let Some(window_bytes) = bytes.get(start..start + window) {
                sample.extend_from_slice(window_bytes);
            }
        }
        f(&sample)
    })
}

thread_local! {
    /// One Zstandard encoding context per thread, reused for every block that thread compresses.
    ///
    /// A context allocates its match-finder tables once, and codec selection compresses each block once or twice, so
    /// a fresh context per call cost more than compressing most blocks did. The compressed bytes are identical
    /// either way, as a context carries nothing from one frame to the next.
    static ZSTD_ENCODER: RefCell<ZstdCompressor<'static>> = RefCell::new(ZstdCompressor::default());

    /// One scratch buffer per thread holding the frame [`compress_zstd_framed`] just produced, so framing a block
    /// reuses one allocation instead of taking a fresh one per block.
    static ZSTD_FRAME_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Compresses one block with Zstandard at `level` through this thread's reused encoding context, into `out` —
/// which is overwritten, its allocation kept. The bytes are those a context used for this block alone would write.
/// `out` is left empty if libzstd rejects the level or fails, since no real frame is ever empty.
fn compress_zstd_into(bytes: &[u8], level: i32, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(compress_bound(bytes.len()));
    ZSTD_ENCODER.with(|slot| {
        let mut encoder = slot.borrow_mut();
        let compressed = encoder
            .set_compression_level(level)
            .and_then(|()| encoder.compress_to_buffer(bytes, out));
        if compressed.is_err() {
            out.clear();
        }
    });
}

/// Compresses one block with Zstandard at `level`, returning a self-contained frame, or an empty `Vec` if libzstd
/// failed.
pub(crate) fn compress_zstd(bytes: &[u8], level: i32) -> Vec<u8> {
    let mut out = Vec::new();
    compress_zstd_into(bytes, level, &mut out);
    out
}

/// [`compress_zstd`], but behind the 4-byte little-endian uncompressed-length prefix every stored Zstandard block
/// opens with (see [`ZSTD_LENGTH_PREFIX_BYTES`]) — the block's final stored form, sized in one allocation. Empty if
/// libzstd failed, so callers keep the block uncompressed rather than store a bare length prefix.
fn compress_zstd_framed(bytes: &[u8], level: i32) -> Vec<u8> {
    ZSTD_FRAME_SCRATCH.with(|cell| {
        let mut frame = cell.borrow_mut();
        compress_zstd_into(bytes, level, &mut frame);
        if frame.is_empty() {
            return Vec::new();
        }
        let mut framed = Vec::with_capacity(ZSTD_LENGTH_PREFIX_BYTES + frame.len());
        framed.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        framed.extend_from_slice(&frame);
        framed
    })
}

/// The stored size a trailing stage must come in under for `strategy` to keep it over a `len`-byte body, or `None` for
/// a strategy that never keeps one.
fn trailing_threshold(strategy: CascadeStrategy, len: usize) -> Option<usize> {
    match strategy {
        CascadeStrategy::DecodeOptimized => Some(len * DECODE_OPTIMIZED_THRESHOLD_NUM / DECODE_OPTIMIZED_THRESHOLD_DEN),
        CascadeStrategy::NoTrailing => None,
        CascadeStrategy::SizeOptimized => Some(len - len / SIZE_OPTIMIZED_SAVINGS_DIVISOR),
    }
}

fn apply_trailing(bytes: Vec<u8>, strategy: CascadeStrategy) -> (Compression, Vec<u8>) {
    // LZ4 may appear only as a trailing stage after the adaptive transform (or as a measured fallback); applied only
    // when the sample proves a benefit. The savings threshold is biased by the cascade strategy: DecodeOptimized
    // requires ≥ 25%; SizeOptimized accepts ≥ 5%.
    let Some(threshold) = trailing_threshold(strategy, bytes.len()) else {
        return (Compression::None, bytes);
    };
    // Small blocks: compressing with both codecs costs little, so keep the exact choice. Only large blocks pay for the
    // double compression the sampling cascade removes.
    if bytes.len() <= TRAILING_FULL_TRIAL_MAX_BYTES {
        let lz4 = lz4_flex::compress_prepend_size(&bytes);
        let zstd_framed = compress_zstd_framed(&bytes, 1);
        return pick_trailing(bytes, threshold, lz4, zstd_framed);
    }
    // Large blocks: a small representative sample chooses which single codec runs over the whole block, so the sampler
    // pays one whole-block compression instead of two. Whether any trailing stage is kept is still decided on the chosen
    // codec's real whole-block output against the threshold, exactly as before.
    let (sample_lz4, sample_zstd) = with_trailing_sample(&bytes, |sample| {
        (
            lz4_flex::compress_prepend_size(sample).len(),
            compress_zstd(sample, 1).len(),
        )
    });
    if sample_zstd < sample_lz4 {
        let framed = compress_zstd_framed(&bytes, 1);
        if !framed.is_empty() && framed.len() < threshold {
            return (Compression::Zstd1, framed);
        }
    } else {
        let lz4 = lz4_flex::compress_prepend_size(&bytes);
        if !lz4.is_empty() && lz4.len() < threshold {
            return (Compression::Lz4, lz4);
        }
    }
    (Compression::None, bytes)
}

/// Runs exactly the trailing codec a replay capture recorded — one compression, never the two-codec trial — keeping
/// the stage only when it still clears the strategy's savings bar, so a replayed block cannot regress past its
/// uncompressed form. A captured `None` skips compression entirely, and a captured framed stage never reaches here
/// (its column is random-access, which takes the framing path before this one). Requirement: "Encoding selection may
/// capture and replay a winning pipeline".
fn apply_captured_trailing(body: Vec<u8>, captured: Compression, strategy: CascadeStrategy) -> (Compression, Vec<u8>) {
    let Some(threshold) = trailing_threshold(strategy, body.len()) else {
        return (Compression::None, body);
    };
    match captured {
        Compression::Lz4 => {
            let lz4 = lz4_flex::compress_prepend_size(&body);
            if !lz4.is_empty() && lz4.len() < threshold {
                return (Compression::Lz4, lz4);
            }
            (Compression::None, body)
        }
        Compression::Zstd1 | Compression::Zstd3 => {
            let level = if captured == Compression::Zstd3 { 3 } else { 1 };
            let framed = compress_zstd_framed(&body, level);
            if !framed.is_empty() && framed.len() < threshold {
                return (captured, framed);
            }
            (Compression::None, body)
        }
        // A random-access column's framed stage is chosen before this one, and no encoder emits deflate any more, so
        // neither can be the captured codec here; both fall back to storing the body as it stands.
        Compression::Deflate | Compression::SeekableZstd | Compression::None => (Compression::None, body),
    }
}

/// The trailing compression a random-access-required block may still gain. Whole-block LZ4/Zstandard are skipped here
/// (their compressed bytes are not addressable by row), but the seekable Zstandard family compresses in independently
/// decompressible [`seekable_zstd::FRAME_BYTES`] frames, so it alone is sampled — and only for the transforms whose
/// stored layout still tells a reader which frames one row's bytes fall in
/// ([`Transform::keeps_range_access_under_framing`]). A frame also ends at each of `frame_breaks` — the body offsets
/// where a run a filter reads on its own begins — so such a filter inflates that run's frames and no other's.
fn apply_seekable_frames(
    transform: Transform,
    body: Vec<u8>,
    strategy: CascadeStrategy,
    frame_breaks: &[usize],
) -> (Compression, Vec<u8>) {
    if !transform.keeps_range_access_under_framing() {
        return (Compression::None, body);
    }
    let Some(threshold) = trailing_threshold(strategy, body.len()) else {
        return (Compression::None, body);
    };
    let Some(candidate) = seekable_zstd::compress_with_breaks(&body, frame_breaks) else {
        return (Compression::None, body);
    };
    if candidate.len() < threshold {
        (Compression::SeekableZstd, candidate)
    } else {
        (Compression::None, body)
    }
}

/// Undoes a block's trailing compression stage through the process-wide active decompressor (the software one by
/// default; a QATZip-backed one when a node's startup probe installed it). The bytes are identical whichever engine ran
/// — the software path is the correctness oracle. An uncompressed block is returned as a borrow of the input, so the
/// hot read path never copies it.
///
/// A caller that needs more than one of a block's descriptor, a predicate filter, or a decode should call this once
/// and pass the resulting body to the matching `_with_body` function for each — [`descriptor::extract_descriptor_with_body`],
/// [`predicate::filter_string_block_shared_with_body`], [`predicate::filter_numeric_block_with_body`],
/// [`predicate::filter_float_block_with_body`], [`decode_block_shared_with_body`] — rather than letting each one repeat
/// this decompression.
pub fn remove_trailing(compression: Compression, bytes: &[u8]) -> Result<Cow<'_, [u8]>, FormatError> {
    match compression {
        Compression::None => Ok(Cow::Borrowed(bytes)),
        other => Ok(Cow::Owned(
            decompressor::active_decompressor().decompress(other, bytes)?,
        )),
    }
}

/// [`remove_trailing`] into a buffer the caller owns: an inflated body lands in `scratch`, emptied first with its
/// allocation kept, so a caller undoing block after block reuses one buffer. An uncompressed block is returned as the
/// stored bytes themselves, with `scratch` left alone.
pub fn remove_trailing_into<'a>(
    compression: Compression,
    bytes: &'a [u8],
    scratch: &'a mut Vec<u8>,
) -> Result<&'a [u8], FormatError> {
    match compression {
        Compression::None => Ok(bytes),
        other => {
            decompressor::active_decompressor().decompress_into(other, bytes, scratch)?;
            Ok(scratch)
        }
    }
}

/// [`remove_trailing`] for a caller that consumes the body at once: an inflated body lands in this thread's retained
/// buffer (see [`decompressor::with_inflate_buffer`]) rather than a fresh allocation per block, and `consume` runs over
/// it. An uncompressed block reaches `consume` as the stored bytes themselves, never copied.
pub fn with_trailing_removed<R>(
    compression: Compression,
    bytes: &[u8],
    consume: impl FnOnce(&[u8]) -> Result<R, FormatError>,
) -> Result<R, FormatError> {
    if compression == Compression::None {
        return consume(bytes);
    }
    decompressor::with_inflate_buffer(|scratch| consume(remove_trailing_into(compression, bytes, scratch)?))
}

/// Encodes one column block using the `DecodeOptimized` cascade strategy (the write path default).
///
/// For rewrite or compaction, use [`encode_block_with_strategy`] with [`CascadeStrategy::SizeOptimized`] instead.
pub fn encode_block(data: &ColumnData, random_access: bool) -> EncodedBlock {
    encode_block_with_strategy(data, random_access, CascadeStrategy::DecodeOptimized)
}

/// Encodes one column block: mandatory representation rules first (decimal128 for money), then sample-based selection
/// among the mandated candidates, then a trailing compression stage biased by `strategy` and only when the caller
/// permits it (`random_access` forbids whole-block compression on shredded scan-path columns). Requirement:
/// "Lifecycle-selected cascade strategies".
pub fn encode_block_with_strategy(data: &ColumnData, random_access: bool, strategy: CascadeStrategy) -> EncodedBlock {
    encode_block_inner(data, random_access, strategy, None, None, None, None)
}

/// [`encode_block_with_strategy`] with the column's file-scope shared alphabet: a dictionary block whose distinct
/// values all appear in the alphabet stores only its code stream and records the file scope.
pub fn encode_block_with_shared_dictionary(
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    shared: Option<&[String]>,
) -> EncodedBlock {
    encode_block_inner(data, random_access, strategy, None, None, None, shared)
}

/// A trained FSST symbol table, kept so later blocks of the same column can compress with it instead of training
/// their own. Wraps the crate's compressor to give it the `Debug` the structs carrying it derive.
#[derive(Clone)]
pub struct FsstTable(fsst::Compressor);

impl std::fmt::Debug for FsstTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsstTable")
            .field("symbols", &self.0.symbol_table().len())
            .finish()
    }
}

/// A winning transform captured from an earlier block of the same column, replayable for its later blocks so a
/// distribution-stable column stops re-running candidate selection on every block. The captured trailing codec is
/// replayed too — one compression run instead of the two-codec trial, as is an FSST head's trained symbol table —
/// one training per column instead of one per block, with the table still written into every block. The head's
/// decoded-to-raw sizes are the trip-wire reference [`encode_block_replayed`] compares each replayed block against.
/// Requirement: "Encoding selection may capture and replay a winning pipeline".
#[derive(Debug, Clone)]
pub struct ReplayCapture {
    pub compression: Compression,
    /// The head's size *before* its trailing compression stage — what its transform alone produced. The trip-wire
    /// asks whether the captured transform still fits a later block, and a trailing codec can shrink an amplifying
    /// transform's output far enough to make a bad fit look like a good one, so the ratio is taken where the
    /// transform's own work is still visible.
    pub decoded_len: u64,
    /// Only an FSST head carries one; a replayed block without it trains its own table.
    pub fsst: Option<FsstTable>,
    pub raw_len: u64,
    pub transform: Transform,
}

impl ReplayCapture {
    /// The capture a fully selected head block yields, or `None` when its transform is not a replayable selection
    /// (floats re-run ALP's combined select-and-encode; decimal128 and u128 are mandatory representations with nothing
    /// to skip). `fsst` is the table the head trained ([`EncodedBlock::fsst`]); it is kept only for an FSST head.
    pub fn from_head(
        data: &ColumnData,
        pipeline: PipelineId,
        decoded_len: u64,
        fsst: Option<FsstTable>,
    ) -> Option<Self> {
        let transform = pipeline.transform().ok()?;
        if !transform_matches_kind(transform, data) {
            return None;
        }
        Some(Self {
            compression: pipeline.compression().ok()?,
            decoded_len,
            fsst: fsst.filter(|_| transform == Transform::FsstString),
            raw_len: raw_value_bytes(data).max(1),
            transform,
        })
    }
}

/// Whether `transform` is a replay- or force-eligible selection for this block's value kind.
fn transform_matches_kind(transform: Transform, data: &ColumnData) -> bool {
    match data {
        ColumnData::U64(_) | ColumnData::I64(_) => matches!(
            transform,
            Transform::DeltaBitpack | Transform::ForBitpack | Transform::PlainU64 | Transform::Rle
        ),
        ColumnData::Strings(_) => matches!(
            transform,
            Transform::DictionaryString | Transform::FsstString | Transform::RawString
        ),
        ColumnData::Decimal { .. } | ColumnData::F64(_) | ColumnData::U128(_) => false,
    }
}

/// The raw byte size of a block's values, the denominator of the replay trip-wire's compression ratios. Deterministic
/// and cheap; strings count value bytes plus one so an all-empty block never divides by zero.
fn raw_value_bytes(data: &ColumnData) -> u64 {
    match data {
        ColumnData::U64(values) => (values.len() * 8) as u64,
        ColumnData::I64(values) => (values.len() * 8) as u64,
        ColumnData::F64(values) => (values.len() * 8) as u64,
        ColumnData::U128(values) => (values.len() * 16) as u64,
        ColumnData::Decimal { values, .. } => (values.len() * 16) as u64,
        ColumnData::Strings(values) => (values.len() + values.text_len()) as u64,
    }
}

/// Encodes one block by replaying `capture`'s transform when it fits this block — skipping candidate selection — and
/// re-arming full selection when the replayed transform's decoded-to-raw ratio drifts past the head's by more than
/// the pinned trip-wire, or when the replayed bytes grow past the block's plain form, so a distribution shift never
/// rides a stale capture and a replay can never regress a block past plain. Returns whether the replay was kept. A
/// pure function of the capture and this block's content: any node encoding the same rows makes the same choice.
///
/// The trip-wire measures the transform's own output rather than the stored bytes, because the trailing stage sits
/// between the two: a transform that amplifies a block — a per-value offset table over values shorter than their
/// offsets, say — can compress back down to something that looks like a good fit, and would then ride the rest of
/// the replay segment storing several times what selection would.
pub fn encode_block_replayed(
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    capture: Option<&ReplayCapture>,
    shared: Option<&[String]>,
) -> (EncodedBlock, bool) {
    if let Some(capture) = capture
        && transform_matches_kind(capture.transform, data)
    {
        let block = encode_block_inner(
            data,
            random_access,
            strategy,
            Some(capture.transform),
            Some(capture.compression),
            capture.fsst.as_ref(),
            shared,
        );
        let raw = raw_value_bytes(data).max(1);
        // Keep the replay while replayed_ratio <= head_ratio * TRIP_NUM/TRIP_DEN, cross-multiplied to stay integral.
        if u128::from(block.uncompressed_len) * u128::from(capture.raw_len) * u128::from(REPLAY_TRIP_DEN)
            <= u128::from(capture.decoded_len) * u128::from(raw) * u128::from(REPLAY_TRIP_NUM)
            && replay_within_plain_form(&block, data, random_access, strategy, shared)
        {
            return (block, true);
        }
    }
    (
        encode_block_inner(data, random_access, strategy, None, None, None, shared),
        false,
    )
}

/// The same values written in their plain form with no trailing stage — the size every encoded block is held
/// against, so an encoding that failed to earn its keep is never what gets stored.
fn plain_form(
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    shared: Option<&[String]>,
) -> EncodedBlock {
    let transform = match data {
        ColumnData::Strings(_) => Transform::RawString,
        _ => Transform::PlainU64,
    };
    // Naming the transform is what keeps this from recursing: a forced encode skips the acceptance gate.
    encode_chosen_form(
        data,
        random_access,
        strategy,
        Some(transform),
        Some(Compression::None),
        None,
        shared,
    )
}

/// The plain-form acceptance gate on a replayed block: kept replays never store more bytes than the block's plain
/// form would. A replayed result at or under the raw value bytes passes without further work (the plain frame can
/// only be larger); past that, the plain form is encoded and compared exactly.
fn replay_within_plain_form(
    block: &EncodedBlock,
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    shared: Option<&[String]>,
) -> bool {
    if block.bytes.len() as u64 <= raw_value_bytes(data) {
        return true;
    }
    block.bytes.len() <= plain_form(data, random_access, strategy, shared).bytes.len()
}

/// Conformance-harness-only: encodes one block with a named transform instead of adaptive selection, so the harness
/// exercises every encode/decode path deliberately. Unreachable from any production flow — no writer input selects a
/// pipeline, and forcing a transform that is not valid for the block's value kind is rejected exactly as a corrupt
/// recorded pipeline would be. Requirement: "Forced-pipeline conformance coverage".
pub fn encode_block_forced_for_conformance(
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    transform: Transform,
) -> Result<EncodedBlock, FormatError> {
    let mandatory = matches!(
        (data, transform),
        (ColumnData::Decimal { .. }, Transform::Decimal128)
            | (ColumnData::U128(_), Transform::PlainU128)
            | (
                ColumnData::F64(_),
                Transform::Alp | Transform::AlpRd | Transform::ByteStreamSplit | Transform::PlainF64
            )
    );
    if !mandatory && !transform_matches_kind(transform, data) {
        return Err(FormatError::Structural {
            rule: "forced transform is not valid for the block's value kind",
        });
    }
    Ok(encode_block_inner(
        data,
        random_access,
        strategy,
        Some(transform),
        None,
        None,
        None,
    ))
}

/// The shared encode body behind [`encode_block_with_strategy`], [`encode_block_replayed`], and the conformance
/// forcing entry: `forced` bypasses candidate selection where it names a kind-valid transform, `replay_trailing`
/// replays a capture's trailing codec instead of the two-codec trial, `replay_fsst` replays a capture's symbol table
/// instead of training one, and every other stage — side streams, the recorded pipeline — runs identically.
fn encode_block_inner(
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    forced: Option<Transform>,
    replay_trailing: Option<Compression>,
    replay_fsst: Option<&FsstTable>,
    shared: Option<&[String]>,
) -> EncodedBlock {
    let block = encode_chosen_form(
        data,
        random_access,
        strategy,
        forced,
        replay_trailing,
        replay_fsst,
        shared,
    );
    // A caller that named the transform asked for that transform, and gets it; so does a replay, which the caller
    // holds against the plain form itself. Everything else goes through the acceptance gate: a block that a
    // misleading sample encoded larger than its plain form is stored plain instead, so an unlucky sample costs the
    // wasted trial encode and never a bigger block. The comparison is a pure function of the encoded output, so two
    // nodes encoding the same rows still store the same bytes.
    if forced.is_some() || replay_trailing.is_some() || block.bytes.len() as u64 <= raw_value_bytes(data) {
        return block;
    }
    let plain = plain_form(data, random_access, strategy, shared);
    if plain.bytes.len() < block.bytes.len() {
        plain
    } else {
        block
    }
}

/// The encode itself — mandatory representation rules, candidate selection, side streams, and the trailing stage —
/// without the acceptance gate [`encode_block_inner`] wraps it in.
fn encode_chosen_form(
    data: &ColumnData,
    random_access: bool,
    strategy: CascadeStrategy,
    forced: Option<Transform>,
    replay_trailing: Option<Compression>,
    replay_fsst: Option<&FsstTable>,
    shared: Option<&[String]>,
) -> EncodedBlock {
    let forced =
        forced.filter(|transform| transform_matches_kind(*transform, data) || matches!(data, ColumnData::F64(_)));
    let stats = stats_for(data);
    // Most transforms land at or below the plain size, so sizing the buffer from it up front avoids the several
    // reallocations a `Writer::new()` would grow through as the encode fills it.
    let mut out = Writer::with_capacity(raw_value_bytes(data) as usize);
    let mut side = SideStream::None;
    let mut fsst = None;
    let mut frame_breaks = Vec::new();
    let (transform, kind) = match data {
        ColumnData::U64(values) => {
            let transform = forced.unwrap_or_else(|| choose_u64_transform(values, strategy));
            encode_u64_with(transform, values, &mut out);
            (transform, ValueKind::U64)
        }
        ColumnData::I64(values) => {
            let transform = forced.unwrap_or_else(|| choose_i64_transform(values, strategy));
            encode_i64_with(transform, values, &mut out);
            (transform, ValueKind::I64)
        }
        ColumnData::U128(values) => {
            out.put_u32(values.len() as u32);
            out.put_u128_slice(values);
            (Transform::PlainU128, ValueKind::U128)
        }
        ColumnData::F64(values) => {
            // A forced non-ALP float transform skips the ALP attempt; anything else (no forcing, or forced ALP) tries
            // ALP first and falls back exactly as adaptive selection does, so a forced ALP that cannot represent the
            // data still encodes correctly.
            let alp = match forced {
                Some(Transform::AlpRd | Transform::ByteStreamSplit | Transform::PlainF64) => None,
                _ => encode_alp(values, strategy, &mut out),
            };
            if let Some(chosen) = alp {
                side = chosen;
                (Transform::Alp, ValueKind::F64)
            } else {
                // Reuses the abandoned ALP attempt's allocation instead of discarding it for a fresh `Writer`.
                out.clear();
                let transform = match forced {
                    Some(forced @ (Transform::AlpRd | Transform::ByteStreamSplit | Transform::PlainF64)) => forced,
                    _ => choose_float_fallback(values),
                };
                match transform {
                    Transform::AlpRd => encode_alp_rd(values, &mut out),
                    Transform::ByteStreamSplit => encode_byte_stream_split(values, &mut out),
                    _ => encode_plain_f64(values, &mut out),
                }
                (transform, ValueKind::F64)
            }
        }
        ColumnData::Decimal { values, scale } => {
            // Money is fixed-scale decimal128, never float: mandatory.
            out.put_u8(*scale);
            out.put_u32(values.len() as u32);
            // `i128 as u128` is a bit-preserving reinterpret, exactly what `bytemuck::cast_slice` does over the
            // whole slice at once.
            out.put_u128_slice(bytemuck::cast_slice(values));
            (Transform::Decimal128, ValueKind::Decimal)
        }
        ColumnData::Strings(values) => {
            // Collected once here instead of once per candidate: `choose_string_transform` and whichever encoder
            // runs both need the present rows, not the whole (null-carrying) column.
            let present = present_values(values);
            let transform = forced.unwrap_or_else(|| choose_string_transform(&present));
            let encoded = match transform {
                Transform::DictionaryString => encode_dictionary_string(values, &present, strategy, shared, &mut out)
                    .map(|side| (side, None, Vec::new())),
                Transform::FsstString => encode_fsst_string(values, &present, replay_fsst, &mut out),
                _ => encode_raw_string(values, &present, &mut out).map(|()| (SideStream::None, None, Vec::new())),
            };
            (side, fsst, frame_breaks) =
                encoded.expect("string-block arena offsets fit u32: the writer bounds a granule far below that");
            (transform, ValueKind::String)
        }
    };
    let body = out.into_bytes();
    // The decoded length is the block content before the trailing compression stage — captured now, since `body` is
    // consumed by the compression call below.
    let uncompressed_len = body.len() as u64;
    let depth_so_far = 1 + usize::from(side != SideStream::None);
    let (compression, bytes) = if random_access {
        // Random access preserved: per-value offsets stay addressable, so only the seekable Zstandard family (which
        // preserves it too) may still apply; every other trailing stage is skipped.
        apply_seekable_frames(transform, body, strategy, &frame_breaks)
    } else if !cascade_level_fits(depth_so_far) {
        // A trailing stage would exceed the declared maximum cascade depth, so it is not added — however well it
        // would have sampled.
        (Compression::None, body)
    } else if let Some(captured) = replay_trailing {
        apply_captured_trailing(body, captured, strategy)
    } else {
        apply_trailing(body, strategy)
    };
    EncodedBlock {
        pipeline: PipelineId::new(transform, compression, kind).with_side_stream(side),
        bytes,
        fsst,
        stats,
        uncompressed_len,
    }
}

/// One value of a FastLanes transposed stream, read straight from the packed words that hold it.
///
/// The layout places value `index` at a position derived by arithmetic alone — which sub-block it falls in, which row
/// of that sub-block, which of the sixteen lanes — so reaching a handful of scattered values costs a shift and a mask
/// each rather than a whole vector's unpack per value. `index` counts from the first value of `packed`, so a caller
/// that seeked to a vector boundary subtracts that boundary first. A value whose words run past `packed` reads as
/// zero, matching what the whole-vector kernel leaves in an unfilled slot.
fn fastlanes_value_at(packed: &[u8], width: u32, index: usize) -> u64 {
    if width == 0 {
        return 0;
    }
    let bits = width as usize;
    let bytes_per_vector = FASTLANES_LANES * fastlanes_words_per_lane(bits) * 8;
    let within = index % FASTLANES_VECTOR;
    let sub_block = within / (FASTLANES_ROWS * FASTLANES_LANES);
    let row = (within / FASTLANES_LANES) % FASTLANES_ROWS;
    let lane = within % FASTLANES_LANES;
    let vector_start = (index / FASTLANES_VECTOR) * bytes_per_vector;
    let Some(packed_vector) = packed.get(vector_start..vector_start + bytes_per_vector) else {
        return 0;
    };
    // The sub-block order is self-inverse, so the same table maps a logical sub-block back to its storage slot.
    let vl_block = FASTLANES_ORDER.get(sub_block).copied().unwrap_or(0);
    let bit = (vl_block * FASTLANES_ROWS + row) * bits;
    let (word, offset) = (bit / 64, (bit % 64) as u32);
    let value_mask = mask(width);
    let Some(low) = lane_word(packed_vector, word, lane) else {
        return 0;
    };
    if offset + width <= 64 {
        return (low >> offset) & value_mask;
    }
    match lane_word(packed_vector, word + 1, lane) {
        Some(high) => ((low >> offset) | (high << (64 - offset))) & value_mask,
        None => 0,
    }
}

/// The 64-bit word lane `lane` holds at word index `word` of one packed vector.
fn lane_word(packed_vector: &[u8], word: usize, lane: usize) -> Option<u64> {
    let start = (word * FASTLANES_LANES + lane) * 8;
    let bytes = packed_vector.get(start..start + 8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

/// Unpacks the FastLanes vectors covering rows `[start, end)` into `values`, and borrows back exactly those rows, as
/// stored — before any zigzag or frame-of-reference reversal.
///
/// `packed` holds whole vectors, beginning with the one that contains `start`. Unpacking a whole vector at a time runs
/// the same sixteen-lane shift-and-mask kernel a whole-block decode runs; addressing one row at a time instead
/// re-derives the lane geometry, and re-reads the packed word, for every single value.
///
/// `values` is the caller's buffer — a scan passes the same one through
/// [`with_unpack_buffer`](scratch::with_unpack_buffer) for every page, so decoding a column costs one allocation
/// rather than one per page.
fn unpack_fastlanes_range_into<'a>(
    packed: &[u8],
    width: u32,
    start: usize,
    end: usize,
    values: &'a mut Vec<u64>,
) -> &'a [u64] {
    let vector_start = start / FASTLANES_VECTOR;
    // A point read asks for one value and would otherwise unpack the 1024 around it. Below the width where the
    // sixteen-lane kernel starts paying for itself, each value is read from its own packed words instead.
    if end.saturating_sub(start) <= FASTLANES_SPARSE_MAX_VALUES {
        let first_row = vector_start * FASTLANES_VECTOR;
        values.clear();
        values.extend((start..end).map(|index| fastlanes_value_at(packed, width, index - first_row)));
        return values.as_slice();
    }
    let vectors = end.div_ceil(FASTLANES_VECTOR).saturating_sub(vector_start);
    let stream = PackedStream {
        count: vectors * FASTLANES_VECTOR,
        packed,
        width,
    };
    values.clear();
    values.resize(stream.padded_count(), 0);
    stream.unpack_into(values, |packed| packed);
    // The unpacked vectors begin at `first_row`, so the requested rows are the window inside them — returned as a
    // borrow rather than shifted down to index 0, which would move every value a second time.
    let first_row = vector_start * FASTLANES_VECTOR;
    let tail = end.saturating_sub(first_row).min(values.len());
    let head = start.saturating_sub(first_row).min(tail);
    values.get(head..tail).unwrap_or(&[])
}

/// Decodes rows `[start, end)` from an ALP-encoded float block, reading only the FastLanes vectors that cover that row
/// range rather than the full stream. The exception streams are small side streams read in full; the packed value
/// stream — the bulk of the block — is vector-skipped.
fn decode_alp_range(
    reader: &mut Reader<'_>,
    side: SideStream,
    start: usize,
    end: usize,
) -> Result<Vec<f64>, FormatError> {
    let mut exponent_index = reader.u8("alp exponent")? as usize;
    let mut escaped = Vec::new();
    if exponent_index == usize::from(ALP_VECTOR_ESCAPE_SENTINEL) {
        exponent_index = reader.u8("alp exponent")? as usize;
        escaped = read_alp_escaped_vectors(reader)?;
    }
    let power = ALP_POWERS.get(exponent_index).copied().ok_or(FormatError::Structural {
        rule: "alp exponent out of range",
    })?;
    let exceptions: Vec<(usize, u64)> = decode_alp_exceptions(reader, side)?
        .into_iter()
        .filter_map(|(index, bits)| {
            let index = index as usize;
            (index >= start && index < end).then(|| (index - start, bits))
        })
        .collect();
    let count = reader.u32("bitpack count")? as usize;
    let width = u32::from(reader.u8("bitpack width")?);
    if width > 64 {
        return Err(FormatError::Structural {
            rule: "bit width beyond 64",
        });
    }
    let effective_end = end.min(count);
    let effective_len = effective_end.saturating_sub(start);
    if effective_len == 0 {
        return Ok(Vec::new());
    }
    // Words consumed from the packed stream so far, so the escape patch below can seek to the raw payloads behind it.
    let mut consumed_words = 0usize;
    let unscale = |packed: u64| (unzigzag(packed) as f64) / power;
    let mut values: Vec<f64> = if width == 0 {
        // A zero-width stream stores no words, so the count is pure output amplification; fail closed beyond the block
        // bound (allocation-light decoding of untrusted bytes), matching `bitunpack`.
        if count > MAX_BLOCK_VALUES {
            return Err(FormatError::Structural {
                rule: "zero-width bitpack count beyond the block bound",
            });
        }
        vec![unscale(0); effective_len]
    } else {
        let bits_per_val = width as usize;
        let words_per_lane = fastlanes_words_per_lane(bits_per_val);
        let vector_start = start / FASTLANES_VECTOR;
        let vector_end = effective_end.div_ceil(FASTLANES_VECTOR);
        let words_per_vector = FASTLANES_LANES * words_per_lane;
        // Seek past vectors before vector_start without loading them.
        let skip_words = vector_start * words_per_vector;
        reader.take(skip_words.saturating_mul(8), "bitpack skip word")?;
        // Load only the words covering [vector_start, vector_end).
        let needed_words = (vector_end - vector_start) * words_per_vector;
        let packed_bytes = reader.take(needed_words.saturating_mul(8), "bitpack word")?;
        consumed_words = skip_words + needed_words;
        with_unpack_buffer(|buffer| {
            unpack_fastlanes_range_into(packed_bytes, width, start, effective_end, buffer)
                .iter()
                .map(|packed| unscale(*packed))
                .collect()
        })
    };
    for (rel_idx, bits) in exceptions {
        if let Some(slot) = values.get_mut(rel_idx) {
            *slot = f64::from_bits(bits);
        }
    }
    if !escaped.is_empty() {
        // Seek past the packed words this range decode left unread, landing on the raw escape payloads, then patch the
        // escaped vectors that overlap [start, effective_end).
        let total_words = if width == 0 {
            0
        } else {
            count.div_ceil(FASTLANES_VECTOR) * FASTLANES_LANES * fastlanes_words_per_lane(width as usize)
        };
        reader.take(
            total_words.saturating_sub(consumed_words).saturating_mul(8),
            "bitpack skip word",
        )?;
        for vector_index in &escaped {
            let vector_start = *vector_index as usize * FASTLANES_VECTOR;
            if vector_start >= count {
                return Err(FormatError::RefOutOfRange {
                    what: "alp escaped vector index",
                });
            }
            let vector_end = (vector_start + FASTLANES_VECTOR).min(count);
            let raw = reader.take((vector_end - vector_start) * 8, "alp escaped value")?;
            for row in vector_start.max(start)..vector_end.min(effective_end) {
                let word = raw
                    .get((row - vector_start) * 8..(row - vector_start) * 8 + 8)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map(u64::from_le_bytes)
                    .unwrap_or(0);
                if let Some(slot) = values.get_mut(row - start) {
                    *slot = f64::from_bits(word);
                }
            }
        }
    }
    Ok(values)
}

/// Decodes logical rows `[start, end)` from a dictionary string block, extracting only the needed codes from the
/// FastLanes vectors that cover them and materializing only the dictionary entries those codes name. When the block
/// records the FSST inner level, the whole (small) dictionary is materialized first — that is the level's contract:
/// only dictionary-value materialization pays the FSST decode.
fn decode_dictionary_string_range(
    body: &[u8],
    side: SideStream,
    shared: Option<&[String]>,
    start: usize,
    end: usize,
) -> Result<StringColumn, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = NullStream::read(&mut reader)?;
    let effective_end = end.min(nulls.row_count());
    let effective_len = effective_end.saturating_sub(start);
    if effective_len == 0 {
        return Ok(StringColumn::new());
    }
    // Map logical row boundaries to positions in the ordered present-value list — codes exist only for present rows.
    let present_start = nulls.present_before(start);
    let present_end = nulls.present_before(effective_end);
    // Borrow the entries once instead of copying one per row: the codes outnumber the entries, so a dense range would
    // otherwise allocate the same handful of values over and over.
    let mut entries: Vec<&str> = Vec::new();
    let owned: Vec<String>;
    match side {
        SideStream::FileScopeDictionary => {
            let alphabet = shared.ok_or(FormatError::Structural {
                rule: "shared-scope dictionary block without its file alphabet",
            })?;
            entries.extend(alphabet.iter().map(String::as_str));
        }
        SideStream::FsstDictionaryValues => {
            let dict_count = reader.u32("dictionary count")? as usize;
            owned = read_dictionary_values(&mut reader, dict_count, side)?;
            entries.extend(owned.iter().map(String::as_str));
        }
        _ => {
            let dict_count = reader.u32("dictionary count")? as usize;
            let offsets = read_offsets(&mut reader, dict_count)?;
            let data_len = offsets.last().copied().unwrap_or(0);
            let data = reader.take(data_len, "dictionary data")?;
            for pair in offsets.windows(2) {
                let (lo, hi) = match pair {
                    [lo, hi] => (*lo, *hi),
                    _ => continue,
                };
                if hi < lo {
                    return Err(FormatError::Structural {
                        rule: "dictionary offsets must be non-decreasing",
                    });
                }
                let bytes = slice(data, lo, hi - lo, "dictionary entry")?;
                entries.push(simdutf8::basic::from_utf8(bytes).map_err(|_| FormatError::InvalidUtf8 {
                    what: "dictionary entry",
                })?);
            }
        }
    }
    let count = reader.u32("bitpack count")? as usize;
    let width = u32::from(reader.u8("bitpack width")?);
    if width > 64 {
        return Err(FormatError::Structural {
            rule: "bit width beyond 64",
        });
    }
    if present_end > count {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    // Materialize only the entries the extracted codes name, weaving them back through the null stream.
    let materialize = |codes: &[u64]| {
        let mut resolved = Vec::with_capacity(codes.len());
        let mut text_bytes = 0usize;
        for &code in codes {
            let entry = entries.get(code as usize).copied().ok_or(FormatError::RefOutOfRange {
                what: "dictionary code",
            })?;
            text_bytes += entry.len();
            resolved.push(entry);
        }
        let mut result = StringColumn::with_capacity(effective_len, text_bytes);
        let mut resolved_iter = resolved.into_iter();
        nulls.each_row_range(start, effective_end, |present| {
            if present {
                let entry = resolved_iter.next().ok_or(FormatError::Structural {
                    rule: "null bitmap disagrees with present count",
                })?;
                result.push(Some(entry));
            } else {
                result.push(None);
            }
            Ok(())
        })?;
        Ok(result)
    };
    if width == 0 {
        let zero_codes = vec![0u64; present_end - present_start];
        return materialize(&zero_codes);
    }
    let bits = width as usize;
    let words_per_lane = fastlanes_words_per_lane(bits);
    let vector_start = present_start / FASTLANES_VECTOR;
    let vector_end = present_end.div_ceil(FASTLANES_VECTOR);
    let words_per_vector = FASTLANES_LANES * words_per_lane;
    // Seek past vectors before vector_start without loading them, then load only the covering words.
    reader.take((vector_start * words_per_vector).saturating_mul(8), "bitpack skip word")?;
    let packed_bytes = reader.take(
        ((vector_end - vector_start) * words_per_vector).saturating_mul(8),
        "bitpack word",
    )?;
    with_unpack_buffer(|buffer| {
        materialize(unpack_fastlanes_range_into(
            packed_bytes,
            width,
            present_start,
            present_end,
            buffer,
        ))
    })
}

/// What every string block writes before its values: the presence stream, the FSST symbol table when the block
/// carries one, how many present values follow, and where the offset table over them begins.
struct StringHeader {
    fsst: Option<FsstDecodeTable>,
    nulls: NullStream,
    offsets_start: usize,
    present_count: usize,
}

impl StringHeader {
    /// The block's FSST symbol table, which every FSST block carries and no other string block does.
    fn fsst(&self) -> Result<&FsstDecodeTable, FormatError> {
        self.fsst.as_ref().ok_or(FormatError::Structural {
            rule: "fsst block without its symbol table",
        })
    }

    /// The block's symbol table rebuilt as the crate's compressor, for a scan that compresses a needle. A decode
    /// takes [`Self::fsst`] instead and skips the rebuild.
    fn compressor(&self) -> Result<fsst::Compressor, FormatError> {
        Ok(self.fsst()?.compressor())
    }
}

/// Reads the header of a string block stored under `transform`, leaving `reader` positioned at its offset table.
fn read_string_header(transform: Transform, reader: &mut Reader<'_>) -> Result<StringHeader, FormatError> {
    let nulls = NullStream::read(reader)?;
    let fsst = match transform {
        Transform::FsstString => Some(FsstDecodeTable::read(reader)?),
        _ => None,
    };
    let present_count = reader.u32(match transform {
        Transform::FsstString => "fsst present count",
        _ => "raw present count",
    })? as usize;
    Ok(StringHeader {
        fsst,
        nulls,
        offsets_start: reader.position(),
        present_count,
    })
}

/// Threads a decoded run of present values back through the block's presence stream, for rows `[start, end)` only —
/// the range counterpart of [`merge_nulls`].
fn merge_nulls_range<T: AsRef<str>>(
    nulls: &NullStream,
    present: &[T],
    start: usize,
    end: usize,
) -> Result<StringColumn, FormatError> {
    let text_bytes = present.iter().map(|value| value.as_ref().len()).sum();
    let mut result = StringColumn::with_capacity(end.saturating_sub(start), text_bytes);
    let mut next = 0usize;
    nulls.each_row_range(start, end, |row_present| {
        if row_present {
            let value = present.get(next).ok_or(FormatError::Structural {
                rule: "null bitmap disagrees with present count",
            })?;
            result.push(Some(value.as_ref()));
            next += 1;
        } else {
            result.push(None);
        }
        Ok(())
    })?;
    Ok(result)
}

/// The plaintext of the values `[present_start, present_end)` name, and the offsets bounding them rebased onto that
/// slice — read from a block body through the two offset entries that bound the run, never through the whole table.
fn value_bytes_for_range<'a>(
    body: &'a [u8],
    header: &StringHeader,
    present_start: usize,
    present_end: usize,
    what: &'static str,
) -> Result<(&'a [u8], Vec<usize>), FormatError> {
    let table = slice(
        body,
        header.offsets_start,
        header.present_count.saturating_add(1).saturating_mul(4),
        "string offset",
    )?;
    let offsets = offset_window(table, present_start, present_end - present_start + 1)?;
    // The table's closing entry is the arena's length: read it too, so a body that stops short of the values it
    // declares is refused here exactly as a whole-block decode refuses it.
    let arena_len = offset_window(table, header.present_count, 1)?
        .first()
        .copied()
        .unwrap_or(0);
    let data = slice(body, header.offsets_start + table.len(), arena_len, what)?;
    Ok((data, offsets))
}

/// Decodes logical rows `[start, end)` from an FSST string block, using the per-value offset table to decompress only
/// the needed compressed values.
fn decode_fsst_string_range(body: &[u8], start: usize, end: usize) -> Result<StringColumn, FormatError> {
    let header = read_string_header(Transform::FsstString, &mut Reader::new(body))?;
    let effective_end = end.min(header.nulls.row_count());
    if effective_end <= start {
        return Ok(StringColumn::new());
    }
    // Map logical row boundaries to positions in the ordered present-value list.
    let present_start = header.nulls.present_before(start);
    let present_end = header.nulls.present_before(effective_end);
    let (data, offsets) = value_bytes_for_range(body, &header, present_start, present_end, "fsst data")?;
    // Decompress only the present values in [present_start, present_end), using the offset table to seek directly to
    // the needed bytes.
    let arena = fsst_decompress_arena(
        header.fsst()?,
        data,
        &offsets,
        "fsst value",
        "fsst offsets must be non-decreasing",
    )?;
    merge_nulls_range(&header.nulls, &fsst_arena_values(&arena)?, start, effective_end)
}

/// Every value of a decompressed FSST arena, in order.
fn fsst_arena_values(arena: &FsstArena) -> Result<Vec<&str>, FormatError> {
    (0..arena.len())
        .map(|index| {
            arena
                .value(index)
                .ok_or(FormatError::InvalidUtf8 { what: "fsst value" })
        })
        .collect()
}

/// Decodes logical rows `[start, end)` from a raw string block, slicing the requested values straight out of the
/// stored byte arena through its offset table — the same per-value addressing FSST blocks get, minus the symbol
/// table. This is what keeps a point read on long free text off a whole-granule decode.
fn decode_raw_string_range(body: &[u8], start: usize, end: usize) -> Result<StringColumn, FormatError> {
    let header = read_string_header(Transform::RawString, &mut Reader::new(body))?;
    let effective_end = end.min(header.nulls.row_count());
    if effective_end <= start {
        return Ok(StringColumn::new());
    }
    let present_start = header.nulls.present_before(start);
    let present_end = header.nulls.present_before(effective_end);
    let (data, offsets) = value_bytes_for_range(body, &header, present_start, present_end, "raw string data")?;
    merge_nulls_range(&header.nulls, &raw_string_values(data, &offsets)?, start, effective_end)
}

/// The values `offsets` bounds inside a raw string block's byte arena, checked for UTF-8 as they are sliced.
fn raw_string_values<'a>(data: &'a [u8], offsets: &[usize]) -> Result<Vec<&'a str>, FormatError> {
    offsets
        .windows(2)
        .map(|pair| {
            let (start, end) = (pair.first().copied().unwrap_or(0), pair.last().copied().unwrap_or(0));
            if end < start {
                return Err(FormatError::Structural {
                    rule: "raw string offsets must be non-decreasing",
                });
            }
            simdutf8::basic::from_utf8(slice(data, start, end - start, "raw string")?)
                .map_err(|_| FormatError::InvalidUtf8 { what: "raw string" })
        })
        .collect()
}

/// Reads a framed string block's header by decompressing only the frames it spans. The header sits at the front of
/// the body and fits inside the first frame for any ordinary block, so that frame alone is inflated — and borrowed,
/// not copied; a block wide enough that its presence stream alone outruns a frame doubles the prefix until the header
/// parses.
fn read_windowed_string_header(
    transform: Transform,
    window: &mut seekable_zstd::Window<'_>,
) -> Result<StringHeader, FormatError> {
    let mut probe = window.first_frame_len().min(window.plain_len());
    loop {
        let head = window.read_forward(0, probe)?;
        match read_string_header(transform, &mut Reader::new(&head)) {
            Err(FormatError::Truncated { .. }) if probe < window.plain_len() => {
                probe = probe.saturating_mul(2).min(window.plain_len());
            }
            header => return header,
        }
    }
}

/// Decodes logical rows `[start, end)` from an FSST or raw string block whose trailing stage is the seekable Zstandard
/// family, decompressing only the frames holding the block's header, the two offset entries that bound the requested
/// values, and those values' own bytes. Everything else in the page stays compressed, so a single-row read costs one
/// frame instead of the whole block — the mechanism behind "Single-frame random access on a compressed page", carried
/// over to the string layouts by their offset table.
fn decode_string_framed_range(
    transform: Transform,
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<StringColumn, FormatError> {
    let mut window = seekable_zstd::Window::open(bytes)?;
    let header = read_windowed_string_header(transform, &mut window)?;
    let effective_end = end.min(header.nulls.row_count());
    if effective_end <= start {
        return Ok(StringColumn::new());
    }
    let present_start = header.nulls.present_before(start);
    let present_end = header.nulls.present_before(effective_end);
    let offsets = decode_offsets(&window.read(
        header.offsets_start + present_start * 4,
        (present_end - present_start + 1) * 4,
    )?);
    let (&first, &last) = match (offsets.first(), offsets.last()) {
        (Some(first), Some(last)) if first <= last => (first, last),
        _ => {
            return Err(FormatError::Structural {
                rule: "string offsets must be non-decreasing",
            });
        }
    };
    let arena_start = header.offsets_start + (header.present_count + 1) * 4;
    let data = window.read(arena_start + first, last - first)?;
    // The values were read out of the middle of the arena, so their offsets are rebased onto what was read.
    let rebased: Vec<usize> = offsets.iter().map(|offset| offset.saturating_sub(first)).collect();
    match transform {
        Transform::FsstString => {
            let arena = fsst_decompress_arena(
                header.fsst()?,
                &data,
                &rebased,
                "fsst value",
                "fsst offsets must be non-decreasing",
            )?;
            merge_nulls_range(&header.nulls, &fsst_arena_values(&arena)?, start, effective_end)
        }
        _ => merge_nulls_range(
            &header.nulls,
            &raw_string_values(&data, &rebased)?,
            start,
            effective_end,
        ),
    }
}

/// Decodes rows `[start, end)` from a framed plain u64/i64/f64 block by decompressing only the frames whose fixed
/// 8-bytes-per-row body range covers those rows — the mechanism behind "Single-frame random access on a compressed
/// page". Row `i` of the plain body sits at byte offset `4 + i * 8` (the `4` is the leading row-count header).
fn decode_plain_framed_range(
    kind: ValueKind,
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<ColumnData, FormatError> {
    // The seek table is read once here; every read below resolves its byte range through it.
    let mut window = seekable_zstd::Window::open(bytes)?;
    // The authoritative row count is the u32 header at the front of the decompressed body — exactly what a full-block
    // decode reads — not the raw decompressed length, which trailing bytes could inflate past the real count. Reject
    // any block whose body length disagrees with that header, so a range read can never surface phantom rows a full
    // decode would silently drop.
    let count = {
        let header = window.read(0, 4)?;
        u32::from_le_bytes(header.as_slice().try_into().unwrap_or([0; 4])) as usize
    };
    let expected_plain_len = count.checked_mul(8).and_then(|body| body.checked_add(4));
    if expected_plain_len != Some(window.plain_len()) {
        return Err(FormatError::Structural {
            rule: "framed plain body length disagrees with its row-count header",
        });
    }
    let effective_end = end.min(count);
    if effective_end <= start {
        return Ok(plain_rows(kind, &[]));
    }
    let byte_start = 4 + start * 8;
    let row_bytes = window.read(byte_start, (effective_end - start) * 8)?;
    Ok(plain_rows(kind, &row_bytes))
}

/// Decodes rows `[start, end)` from a plain u64/i64/f64 block stored as a deflate page by inflating only the granules
/// whose bytes hold those rows, located through the page's recorded per-granule offsets: the mechanism behind
/// "Single-granule random access on a deflate page". Row `i` of the plain body sits at byte offset `4 + i * 8`.
///
/// The row count comes from the page's declared plain length, so a range that starts past the first granule never
/// inflates it just to read the body's leading count header; when the range does include the first granule, the
/// header is checked against that count.
fn decode_plain_deflate_range(
    kind: ValueKind,
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<ColumnData, FormatError> {
    let page = deflate::Page::open(bytes)?;
    let count = page
        .plain_len()
        .checked_sub(4)
        .filter(|body| body % 8 == 0)
        .map(|body| body / 8)
        .ok_or(FormatError::Structural {
            rule: "deflate plain body is not a row-count header plus whole rows",
        })?;
    let effective_end = end.min(count);
    if effective_end <= start {
        return Ok(plain_rows(kind, &[]));
    }
    let byte_start = 4 + start * 8;
    let byte_end = 4 + effective_end * 8;
    let first = byte_start / deflate::GRANULE_BYTES;
    let plain = page.decompress_granules(first, (byte_end - 1) / deflate::GRANULE_BYTES)?;
    if first == 0
        && plain
            .get(..4)
            .map(|header| u32::from_le_bytes(header.try_into().unwrap_or([0; 4])) as usize)
            != Some(count)
    {
        return Err(FormatError::Structural {
            rule: "deflate plain body length disagrees with its row-count header",
        });
    }
    let offset = first * deflate::GRANULE_BYTES;
    let row_bytes = slice(&plain, byte_start - offset, byte_end - byte_start, "deflate row range")?;
    Ok(plain_rows(kind, row_bytes))
}

/// Turns plain-body row bytes (8 little-endian bytes per row) into column values of `kind`.
fn plain_rows(kind: ValueKind, row_bytes: &[u8]) -> ColumnData {
    let values: Vec<u64> = row_bytes
        .chunks_exact(8)
        .map(|word| u64::from_le_bytes(word.try_into().unwrap_or([0; 8])))
        .collect();
    match kind {
        ValueKind::F64 => ColumnData::F64(values.into_iter().map(f64::from_bits).collect()),
        // I64 values ride the u64 transforms zigzag-mapped, exactly as in `decode_block`; undo it here too.
        ValueKind::I64 => ColumnData::I64(values.into_iter().map(unzigzag).collect()),
        _ => ColumnData::U64(values),
    }
}

/// Decodes rows `[start, end)` from a framed byte-stream-split block by decompressing, for each of the 8 byte planes,
/// only the frames covering that plane's `[start, end)` byte range — the same single-frame random-access mechanism as
/// [`decode_plain_framed_range`], generalized to 8 contiguous planes instead of one row-major stream. Plane `p`'s byte
/// for row `i` sits at offset `4 + p * count + i` (the `4` is the row-count header).
fn decode_byte_stream_split_framed_range(bytes: &[u8], start: usize, end: usize) -> Result<ColumnData, FormatError> {
    // One window for all 8 plane reads: a frame two planes share is decompressed once.
    let mut window = seekable_zstd::Window::open(bytes)?;
    let count = {
        let header = window.read(0, 4)?;
        u32::from_le_bytes(header.as_slice().try_into().unwrap_or([0; 4])) as usize
    };
    let expected_plain_len = count.checked_mul(8).and_then(|body| body.checked_add(4));
    if expected_plain_len != Some(window.plain_len()) {
        return Err(FormatError::Structural {
            rule: "framed byte-stream-split body length disagrees with its row-count header",
        });
    }
    let effective_end = end.min(count);
    if effective_end <= start {
        return Ok(ColumnData::F64(Vec::new()));
    }
    let row_span = effective_end - start;
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(8);
    for plane in 0..8 {
        planes.push(window.read(4 + plane * count + start, row_span)?);
    }
    let mut values = Vec::with_capacity(row_span);
    for index in 0..row_span {
        let mut le = [0u8; 8];
        for (slot, plane) in le.iter_mut().zip(planes.iter()) {
            *slot = *plane.get(index).ok_or(FormatError::Truncated {
                what: "byte-stream-split plane",
            })?;
        }
        values.push(f64::from_le_bytes(le));
    }
    Ok(ColumnData::F64(values))
}

/// Narrows an owned value vector to rows `[start, end)` in place — dropping the tail, then shifting the kept rows down
/// over the head — so a range read reuses the decode's own allocation instead of copying the range into a new one.
fn retain_range<T>(mut values: Vec<T>, start: usize, end: usize) -> Vec<T> {
    values.truncate(end.min(values.len()));
    values.drain(..start.min(values.len()));
    values
}

/// Extracts rows `[start, end)` from already-decoded column data.
fn slice_column_data(data: ColumnData, start: usize, end: usize) -> ColumnData {
    match data {
        ColumnData::Decimal { values, scale } => ColumnData::Decimal {
            scale,
            values: retain_range(values, start, end),
        },
        ColumnData::F64(values) => ColumnData::F64(retain_range(values, start, end)),
        ColumnData::I64(values) => ColumnData::I64(retain_range(values, start, end)),
        ColumnData::Strings(values) => ColumnData::Strings(values.slice(start, end)),
        ColumnData::U128(values) => ColumnData::U128(retain_range(values, start, end)),
        ColumnData::U64(values) => ColumnData::U64(retain_range(values, start, end)),
    }
}

/// Decodes a row range `[start, end)` from an encoded column block.
///
/// For ALP float blocks and FSST or raw string blocks this loads only the bytes that cover the requested rows: the
/// FastLanes vectors that span `[start, end)` for ALP, and the value slice identified by the offset table for the two
/// string arenas. Under the seekable Zstandard family those blocks go further and decompress only the frames that slice
/// falls in, leaving the rest of the page compressed. All other encodings fall back to a full decode followed by a
/// slice, since their formats do not carry per-row offsets.
///
/// The result equals `decode_block(pipeline, bytes)[start..end]` in every case.
pub fn decode_block_range(
    pipeline: PipelineId,
    bytes: &[u8],
    start: usize,
    end: usize,
) -> Result<ColumnData, FormatError> {
    decode_block_range_shared(pipeline, bytes, None, start, end)
}

/// [`decode_block_range`] with the column's file-scope shared alphabet, for blocks recording the file dictionary
/// scope.
pub fn decode_block_range_shared(
    pipeline: PipelineId,
    bytes: &[u8],
    shared: Option<&[String]>,
    start: usize,
    end: usize,
) -> Result<ColumnData, FormatError> {
    if start >= end {
        return Ok(match pipeline.value_kind()? {
            ValueKind::Decimal => ColumnData::Decimal {
                scale: 0,
                values: Vec::new(),
            },
            ValueKind::F64 => ColumnData::F64(Vec::new()),
            ValueKind::I64 => ColumnData::I64(Vec::new()),
            ValueKind::String => ColumnData::Strings(StringColumn::new()),
            ValueKind::U128 => ColumnData::U128(Vec::new()),
            ValueKind::U64 => ColumnData::U64(Vec::new()),
        });
    }
    let transform = pipeline.transform()?;
    if transform.keeps_range_access_under_framing() && pipeline.compression()? == Compression::SeekableZstd {
        return match transform {
            Transform::ByteStreamSplit => decode_byte_stream_split_framed_range(bytes, start, end),
            Transform::FsstString | Transform::RawString => Ok(ColumnData::Strings(decode_string_framed_range(
                transform, bytes, start, end,
            )?)),
            _ => decode_plain_framed_range(pipeline.value_kind()?, bytes, start, end),
        };
    }
    if matches!(transform, Transform::PlainU64 | Transform::PlainF64) && pipeline.compression()? == Compression::Deflate
    {
        return decode_plain_deflate_range(pipeline.value_kind()?, bytes, start, end);
    }
    let body = remove_trailing(pipeline.compression()?, bytes)?;
    match (pipeline.value_kind()?, transform) {
        (ValueKind::F64, Transform::Alp) => {
            let mut reader = Reader::new(&body);
            Ok(ColumnData::F64(decode_alp_range(
                &mut reader,
                pipeline.side_stream()?,
                start,
                end,
            )?))
        }
        (ValueKind::String, Transform::DictionaryString) => Ok(ColumnData::Strings(decode_dictionary_string_range(
            &body,
            pipeline.side_stream()?,
            shared,
            start,
            end,
        )?)),
        (ValueKind::String, Transform::FsstString) => {
            Ok(ColumnData::Strings(decode_fsst_string_range(&body, start, end)?))
        }
        (ValueKind::String, Transform::RawString) => {
            Ok(ColumnData::Strings(decode_raw_string_range(&body, start, end)?))
        }
        _ => {
            debug_assert!(
                !transform.is_per_value_addressable(),
                "transform {transform:?} claims byte-range extraction support but decode_block_range has no fast path for it"
            );
            let full = decode_block_shared(pipeline, bytes, shared)?;
            Ok(slice_column_data(full, start, end))
        }
    }
}

/// Decodes one column block by its recorded pipeline id. A block recording the file-scope dictionary side stream
/// cannot decode without its alphabet — use [`decode_block_shared`] with the footer's shared dictionary for the
/// column.
pub fn decode_block(pipeline: PipelineId, bytes: &[u8]) -> Result<ColumnData, FormatError> {
    decode_block_shared(pipeline, bytes, None)
}

/// [`decode_block`] with the column's file-scope shared alphabet, for blocks recording the file dictionary scope.
pub fn decode_block_shared(
    pipeline: PipelineId,
    bytes: &[u8],
    shared: Option<&[String]>,
) -> Result<ColumnData, FormatError> {
    with_trailing_removed(pipeline.compression()?, bytes, |body| {
        decode_block_shared_with_body(pipeline, body, shared)
    })
}

/// [`decode_block_shared`] for a caller that already holds this block's decompressed body — reused, for instance,
/// across a descriptor or filter call already made on the same block — so decoding does not decompress it again.
pub fn decode_block_shared_with_body(
    pipeline: PipelineId,
    body: &[u8],
    shared: Option<&[String]>,
) -> Result<ColumnData, FormatError> {
    let mut reader = Reader::new(body);
    let transform = pipeline.transform()?;
    match pipeline.value_kind()? {
        ValueKind::U64 => Ok(ColumnData::U64(decode_ints_with(transform, &mut reader)?)),
        // I64 values ride the u64 transforms zigzag-mapped; the decode undoes that as it writes, so no second pass.
        ValueKind::I64 => Ok(ColumnData::I64(decode_ints_with(transform, &mut reader)?)),
        ValueKind::U128 => {
            if transform != Transform::PlainU128 {
                return Err(FormatError::Structural {
                    rule: "u128 columns use the plain transform",
                });
            }
            let count = reader.u32("u128 count")? as usize;
            let bytes = reader.take(count.saturating_mul(16), "u128 value")?;
            Ok(ColumnData::U128(
                bytes
                    .chunks_exact(16)
                    .map(|word| u128::from_le_bytes(word.try_into().unwrap_or([0; 16])))
                    .collect(),
            ))
        }
        ValueKind::F64 => match transform {
            Transform::Alp => Ok(ColumnData::F64(decode_alp(&mut reader, pipeline.side_stream()?)?)),
            Transform::PlainF64 => {
                let count = reader.u32("f64 count")? as usize;
                let bytes = reader.take(count.saturating_mul(8), "f64 bits")?;
                Ok(ColumnData::F64(
                    bytes
                        .chunks_exact(8)
                        .map(|word| f64::from_bits(u64::from_le_bytes(word.try_into().unwrap_or([0; 8]))))
                        .collect(),
                ))
            }
            Transform::AlpRd => Ok(ColumnData::F64(decode_alp_rd(&mut reader)?)),
            Transform::ByteStreamSplit => Ok(ColumnData::F64(decode_byte_stream_split(&mut reader)?)),
            _ => Err(FormatError::Structural {
                rule: "f64 columns use alp, alp-rd, plain, or byte-stream-split transforms",
            }),
        },
        ValueKind::Decimal => {
            if transform != Transform::Decimal128 {
                return Err(FormatError::Structural {
                    rule: "money columns use the decimal128 transform",
                });
            }
            let scale = reader.u8("decimal scale")?;
            let count = reader.u32("decimal count")? as usize;
            let bytes = reader.take(count.saturating_mul(16), "decimal value")?;
            Ok(ColumnData::Decimal {
                values: bytes
                    .chunks_exact(16)
                    .map(|word| u128::from_le_bytes(word.try_into().unwrap_or([0; 16])) as i128)
                    .collect(),
                scale,
            })
        }
        ValueKind::String => match transform {
            Transform::DictionaryString => Ok(ColumnData::Strings(decode_dictionary_string(
                &mut reader,
                pipeline.side_stream()?,
                shared,
            )?)),
            Transform::FsstString => Ok(ColumnData::Strings(decode_fsst_string(&mut reader)?)),
            Transform::RawString => Ok(ColumnData::Strings(decode_raw_string(&mut reader)?)),
            _ => Err(FormatError::Structural {
                rule: "string columns use dictionary, fsst, or raw transforms",
            }),
        },
    }
}

#[cfg(test)]
#[path = "test/mod.rs"]
mod tests;
