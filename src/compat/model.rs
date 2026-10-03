//! The data and contracts behind reading files a newer fleet wrote: the footer note that points an older reader at a
//! trustworthy decoder, the two small interfaces a fleet supplies to resolve and vouch for those decoders, and the ways
//! reading one such block can turn out.
//!
//! None of these bytes are executed by the reader. A decoder is identified by a name the fleet resolves to software it
//! ships out of band; the file only names that decoder, it never dictates code to run. The reader still validates the
//! block's checksum and only trusts a decoder the fleet certifies, so a forged reference can never make it surface
//! wrong bytes.

use crate::error::FormatError;

/// A file's note, alongside one optional block, telling an older reader how it may still read that block: the minimum
/// reader version required and the name of a portable decoder the fleet can resolve.
///
/// It applies to optional blocks only and is advice, not a command — a reader that does not meet the version, cannot
/// resolve the decoder, or cannot confirm the decoder is trustworthy simply skips the block and scans instead.
///
/// Fields are alphabetical; on disk the footer writes the bytes in the spec's order — `min_reader_version`, then
/// `portable_decoder_ref`, then the block's optional feature flag — and covers them with the checksum directory like
/// any other footer field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscapeHatch {
    pub min_reader_version: u32,
    /// Which optional feature flag bit (in `optional_feature_flags`) this hatch describes how to read.
    pub optional_feature_bit: u64,
    /// A versioned, fleet-resolvable decoder name — never bytes to execute.
    pub portable_decoder_ref: String,
}

/// What a reader decides to do with an optional block: read it with its own native code, read it through a
/// fleet-supplied portable decoder, or skip it and answer from a plain scan instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionalBlockPlan {
    /// The reader understands this block's encoding itself.
    Native,
    /// Read the block through a fleet-resolved, trusted portable decoder.
    PortableDecode,
    /// Leave the block unread and fall back to a correct scan.
    Skip,
}

/// How an attempt to read an optional block through the escape hatch turned out: either the trusted decoder's rows, or
/// a decision to skip the block.
///
/// There is no "raw bytes" variant on purpose — a reader never hands back bytes it could not verify and decode through
/// a trusted decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionalBlockOutcome {
    /// The block's rows, byte-for-byte what a native decoder would produce.
    Decoded(Vec<u8>),
    /// No usable decoder; the block was skipped for a scan fallback.
    Skip,
}

/// One portable decoder the fleet ships for a particular optional encoding.
///
/// A decoder is trusted only when the fleet has put it through the HEF conformance suite and the software-parity check,
/// which together guarantee its rows match a native decoder's exactly. The reader asks
/// [`PortableDecoder::is_conformant_at`] before ever calling [`PortableDecoder::decode`].
pub trait PortableDecoder {
    /// Whether this decoder has passed the conformance suite and software-parity check for its encoding at or above
    /// `min_reader_version`, so its output is byte-for-byte what a native decoder would produce.
    fn is_conformant_at(&self, min_reader_version: u32) -> bool;

    /// Turns one verified optional block into its rows. Callers MUST validate the block's checksum and confirm
    /// [`PortableDecoder::is_conformant_at`] first; a malformed block refuses with a [`FormatError`] rather than
    /// returning misdecoded bytes.
    fn decode(&self, block: &[u8]) -> Result<Vec<u8>, FormatError>;
}

/// The reader's fleet: the thing that turns a decoder name from a file into the portable decoder the fleet actually
/// ships, if it ships one at all.
///
/// Resolving a name does not mean trusting it — the reader still checks the resolved decoder with
/// [`PortableDecoder::is_conformant_at`] before using it.
pub trait PortableDecoderFleet {
    /// Returns the portable decoder the fleet ships for `decoder_ref`, or `None` when the fleet does not resolve that
    /// name.
    fn resolve(&self, decoder_ref: &str) -> Option<&dyn PortableDecoder>;
}
