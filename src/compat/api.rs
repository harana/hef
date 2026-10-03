//! The front door for reading a file a newer fleet wrote: decide what to do with an optional block this reader may not
//! understand, and — only when it is safe — read it through a portable decoder the fleet vouches for.
//!
//! Both entry points enforce the same rule the spec demands: a reader takes exactly one of two paths for an unknown
//! optional block — read it through a trusted decoder after its checksum verifies, or skip it and scan — and never
//! surfaces unverified or misdecoded bytes. Neither path applies to required features; those are gated, refuse, by
//! [`super::check_features`] before a file is ever opened.

use super::model::{EscapeHatch, OptionalBlockOutcome, OptionalBlockPlan, PortableDecoder, PortableDecoderFleet};
use crate::error::FormatError;
use crate::layout::optional_features;

/// Decides how to read an optional block this file declares.
///
/// A block whose encoding this reader knows natively is read natively. For one it does not know, the only way to read
/// it is through the escape hatch: the hatch must describe this very block (its `optional_feature_bit` must equal
/// `feature_bit`), the reader must meet the hatch's `min_reader_version`, the fleet must resolve its
/// `portable_decoder_ref`, and that decoder must be conformance-passing for the block's version. If any of those is
/// missing — including a hatch that describes a different block — the block is skipped and the query is answered from a
/// scan instead, costing performance, never correctness.
pub fn plan_optional_block(
    feature_bit: u64,
    reader_version: u32,
    hatch: Option<&EscapeHatch>,
    fleet: &dyn PortableDecoderFleet,
) -> OptionalBlockPlan {
    if optional_features::KNOWN & feature_bit != 0 {
        return OptionalBlockPlan::Native;
    }
    match hatch.and_then(|hatch| usable_decoder(feature_bit, reader_version, hatch, fleet)) {
        Some(_) => OptionalBlockPlan::PortableDecode,
        None => OptionalBlockPlan::Skip,
    }
}

/// Reads an optional block this reader cannot decode natively, taking exactly one of the two allowed paths and never
/// surfacing unverified or misdecoded bytes.
///
/// The hatch is honoured only when it describes this very block — its `optional_feature_bit` must equal `feature_bit` —
/// so a hatch written for one optional block can never authorize a portable decode of a different one. When a trusted
/// decoder is available, the block's checksum is validated *before* anything decodes it; only then is it decoded, and
/// the returned rows are byte-for-byte what a native decoder would produce (that is what the decoder's conformance and
/// software-parity gate guarantees). Otherwise the block is skipped — the caller falls back to a scan path or another
/// block that still returns correct, complete results. A checksum mismatch is a hard error, never a silent skip, so
/// corruption can never masquerade as a missing block.
pub fn read_optional_block_via_escape_hatch(
    block: &[u8],
    expected_checksum: &[u8; blake3::OUT_LEN],
    feature_bit: u64,
    reader_version: u32,
    hatch: Option<&EscapeHatch>,
    fleet: &dyn PortableDecoderFleet,
) -> Result<OptionalBlockOutcome, FormatError> {
    let Some(decoder) = hatch.and_then(|hatch| usable_decoder(feature_bit, reader_version, hatch, fleet)) else {
        return Ok(OptionalBlockOutcome::Skip);
    };
    // Validate the block checksum before decoding — never decode, let alone surface, bytes that have not been verified
    // against the footer.
    if crate::file::integrity::hash_tree(block).as_bytes() != expected_checksum {
        return Err(FormatError::Blake3Mismatch {
            scope: "optional block",
        });
    }
    Ok(OptionalBlockOutcome::Decoded(decoder.decode(block)?))
}

/// The portable decoder the reader may actually use for one escape hatch, or `None` when it must fall back to a scan. A
/// decoder qualifies only when the hatch describes this block (`optional_feature_bit == feature_bit`), the reader meets
/// the hatch's minimum version, the fleet resolves the named decoder, and that decoder is conformance-passing at the
/// hatch's version — the single place both the feature-bit binding and the conformance gate are enforced.
fn usable_decoder<'fleet>(
    feature_bit: u64,
    reader_version: u32,
    hatch: &EscapeHatch,
    fleet: &'fleet dyn PortableDecoderFleet,
) -> Option<&'fleet dyn PortableDecoder> {
    if hatch.optional_feature_bit != feature_bit {
        return None;
    }
    if reader_version < hatch.min_reader_version {
        return None;
    }
    let decoder = fleet.resolve(&hatch.portable_decoder_ref)?;
    decoder.is_conformant_at(hatch.min_reader_version).then_some(decoder)
}

#[cfg(test)]
#[path = "test/api.rs"]
mod tests;
