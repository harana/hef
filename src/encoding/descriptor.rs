//! Per-page encoding descriptor: the parameters a consumer needs to evaluate predicates and estimate decode cost
//! directly on the encoded representation, without a full decode.
//!
//! Each page's pipeline id already names the transform family; this module adds the numeric parameters inside that
//! family — the base value and bit-width for frame-of-reference pages, the sorted dictionary for dictionary pages, the
//! exponent index for ALP pages, and so on — so a consumer can translate a predicate literal into the encoded domain
//! once and then compare codes, count or skip runs, or range-compare against the frame-of-reference base.
//!
//! Extracting a descriptor does not decode any value row. The mandatory software decode path in [`super::decode_block`]
//! remains authoritative and produces identical values whether or not a consumer used the descriptor first.

use super::{PipelineId, Transform};
use crate::error::FormatError;
use crate::file::bytes::Reader;

/// The encoding parameters extracted from one page's header, sufficient to evaluate predicates and estimate decode cost
/// without decoding all values.
///
/// Each variant corresponds to the transform stage recorded in the page's pipeline id. The parameters let a consumer
/// translate a literal into the encoded domain once and then compare codes, count or skip runs, or range-compare
/// against the frame-of-reference base — without materialising any row's decoded value.
#[derive(Clone, Debug)]
pub enum PageDescriptor {
    /// ALP-encoded floating-point: exponent index into the ALP scale table, and the number of exact exceptions that
    /// bypass the scaled-integer path.
    Alp { exception_count: u32, exponent_index: u8 },
    /// Delta-of-values + FastLanes bitpacking: the first (un-differenced) value and the bit-width of the packed zigzag
    /// deltas.
    DeltaBitpack { first_value: u64, width: u32 },
    /// Dictionary-encoded strings: the sorted entries that map each value to a compact integer code. Because the
    /// dictionary is sorted, codes share the values' byte order, so any predicate that resolves to a code range can be
    /// answered by comparing codes without decoding rows.
    Dictionary { entries: Vec<String> },
    /// Frame-of-reference + FastLanes bitpacking: the base value subtracted before packing, and the bit-width of the
    /// packed residuals. A range predicate `v ∈ [lo, hi]` becomes `code ∈ [lo − base, hi − base]`.
    ForBitpack { base: u64, width: u32 },
    /// FSST-compressed strings: number of entries in the shared symbol table, which governs per-character decode work.
    Fsst { symbol_count: u16 },
    /// Plain or raw encoding: no special parameters beyond the pipeline id.
    Plain,
    /// Run-length encoded: how many (value, count) runs the block contains. Fewer runs means the predicate engine can
    /// skip large stretches cheaply.
    Rle { run_count: u32 },
}

impl PageDescriptor {
    /// Relative cost per row to decode this page, as a unit-free estimate for ordering predicates cheapest-first. A
    /// higher value means more work per row; the absolute values carry no meaning beyond relative ordering.
    pub fn decode_cost_per_row(&self) -> u32 {
        match self {
            PageDescriptor::Plain => 1,
            PageDescriptor::Rle { .. } => 1,
            PageDescriptor::ForBitpack { width, .. } => 1 + width / 8,
            PageDescriptor::DeltaBitpack { width, .. } => 2 + width / 8,
            PageDescriptor::Alp { exception_count, .. } => 3 + (*exception_count / 64).min(4),
            PageDescriptor::Dictionary { entries } => 2 + (entries.len().min(255) as u32) / 32,
            PageDescriptor::Fsst { symbol_count } => 4 + (*symbol_count as u32) / 64,
        }
    }
}

/// Reads the encoding parameters from a page's bytes, returning a descriptor that exposes the parameters needed to
/// evaluate predicates or estimate decode cost without decoding every value.
///
/// Only the page header is consumed — the bulk of the packed data is left unread. The returned descriptor does not
/// decode any row; decoded values are identical whether or not a consumer uses the descriptor before calling
/// [`super::decode_block`].
///
/// This still has to undo the page's trailing compression stage first, since the header lives inside it — a caller
/// that will also filter or decode this same block should decompress once with [`super::remove_trailing`] and reuse
/// [`extract_descriptor_with_body`] for all three instead of paying this decompression again per call.
pub fn extract_descriptor(pipeline: PipelineId, bytes: &[u8]) -> Result<PageDescriptor, FormatError> {
    let body = super::remove_trailing(pipeline.compression()?, bytes)?;
    extract_descriptor_with_body(pipeline, &body)
}

/// [`extract_descriptor`] for a caller that already holds this block's decompressed body — reused, for instance,
/// across a filter and a decode call on the same block — so the page header is read without decompressing it again.
pub fn extract_descriptor_with_body(pipeline: PipelineId, body: &[u8]) -> Result<PageDescriptor, FormatError> {
    let transform = pipeline.transform()?;
    let mut reader = Reader::new(body);
    match transform {
        Transform::ForBitpack => {
            let base = reader.u64("for base")?;
            let _count = reader.u32("bitpack count")?;
            let width = u32::from(reader.u8("bitpack width")?);
            Ok(PageDescriptor::ForBitpack { base, width })
        }
        Transform::DeltaBitpack => {
            let first_value = reader.u64("delta first")?;
            let _count = reader.u32("bitpack count")?;
            let width = u32::from(reader.u8("bitpack width")?);
            Ok(PageDescriptor::DeltaBitpack { first_value, width })
        }
        Transform::Rle => {
            let run_count = reader.u32("rle run count")?;
            Ok(PageDescriptor::Rle { run_count })
        }
        Transform::Alp => {
            let mut exponent_index = reader.u8("alp exponent")?;
            if exponent_index == super::ALP_VECTOR_ESCAPE_SENTINEL {
                exponent_index = reader.u8("alp exponent")?;
                let escaped = reader.u32("alp escaped vector count")? as usize;
                reader.take(escaped.saturating_mul(4), "alp escaped vector index")?;
            }
            let exception_count = reader.u32("alp exception count")?;
            Ok(PageDescriptor::Alp {
                exception_count,
                exponent_index,
            })
        }
        Transform::DictionaryString => {
            let side = pipeline.side_stream()?;
            if side == super::SideStream::FileScopeDictionary {
                return Err(FormatError::Structural {
                    rule: "shared-scope dictionary block without its file alphabet",
                });
            }
            let _ = super::NullStream::read(&mut reader)?;
            let dict_count = reader.u32("dictionary count")? as usize;
            let entries = super::read_dictionary_values(&mut reader, dict_count, side)?;
            Ok(PageDescriptor::Dictionary { entries })
        }
        Transform::FsstString => {
            let _ = super::NullStream::read(&mut reader)?;
            let symbol_count = reader.u16("fsst symbol count")?;
            Ok(PageDescriptor::Fsst { symbol_count })
        }
        _ => Ok(PageDescriptor::Plain),
    }
}

#[cfg(test)]
#[path = "test/descriptor.rs"]
mod tests;
