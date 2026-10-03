//! Decides whether this build can safely read a given file, by checking its version and the features it declares.
//!
//! The rule is to refuse: a file that requires a feature this reader does not understand is rejected outright rather
//! than read partially or wrong. Optional features the reader does not know are simply ignored; the ones it does know
//! are validated (their checksums) and used.
//!
//! Newer fleets sometimes ship a brand-new encoding in an *optional* block before every reader has the native code for
//! it. The escape hatch in this module is the one narrow way an older reader may still read such a block: only when the
//! file vouches for a portable decoder the reader's fleet trusts, and only after the block's checksum verifies —
//! otherwise the reader skips the block and falls back to a plain scan. Required features never get this relaxation;
//! they always refuse.

pub mod api;
pub mod availability;
pub mod fixture_corpus;
pub mod model;
pub mod sim;

use super::error::FormatError;
use super::layout::{optional_features, required_features};
pub use api::{plan_optional_block, read_optional_block_via_escape_hatch};
pub use availability::{AvailabilityWindow, validate_pipeline_window, validate_pipeline_window_pinned};
pub use model::{EscapeHatch, OptionalBlockOutcome, OptionalBlockPlan, PortableDecoder, PortableDecoderFleet};
pub use sim::{SimulatedDecoder, SimulatedDecoderFleet, encode_demo_block, native_reference_decode};

/// The format major version this reader implements.
pub const SUPPORTED_FORMAT_MAJOR: u16 = 1;

/// The reader/fleet software version this build runs at. An optional block's escape hatch is usable only when this
/// meets the block's `min_reader_version`.
pub const SUPPORTED_READER_VERSION: u32 = 1;

/// Applies the reader rule to a file's declared feature flags.
///
/// Returns the subset of optional features this reader may use natively; optional bits beyond
/// `optional_features::KNOWN` are ignorable extension blocks (a newer fleet may still read them through the escape
/// hatch — see [`plan_optional_block`]). A required bit this reader does not understand fails the file — a refusal,
/// never partial or incorrect data.
pub fn check_features(required: u64, optional: u64) -> Result<u64, FormatError> {
    let unknown_required = required & !required_features::KNOWN;
    if unknown_required != 0 {
        return Err(FormatError::UnknownRequiredFeature { bits: unknown_required });
    }
    Ok(optional & optional_features::KNOWN)
}

/// Footer version gate: the major version must be implemented; minor revisions are forward-compatible (new optional
/// sections/flags only).
pub fn check_format_version(major: u16, minor: u16) -> Result<(), FormatError> {
    if major != SUPPORTED_FORMAT_MAJOR {
        return Err(FormatError::UnsupportedVersion {
            field: "footer.format_version.major",
            found: u32::from(major) << 16 | u32::from(minor),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "test/mod.rs"]
mod tests;
