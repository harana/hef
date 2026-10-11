//! Constants the write path shares across its builders.

use crate::error::FormatError;

/// Bloom bits spent per distinct reference in a granule's reference filter: about a 1% false-positive rate, the same
/// budget the identity-hash point filters use.
pub const REFERENCE_FILTER_BITS_PER_KEY: u32 = 10;

/// The error every path that would replace an audit file returns.
pub(crate) const AUDIT_REWRITE_REFUSED: FormatError = FormatError::Structural {
    rule: "an audit file is never rewritten, spliced or compacted",
};

/// Why a compaction publish refuses an audit input or output.
pub(crate) const AUDIT_COMPACTION_REFUSED: &str = "an audit file is never compacted";
