//! Constants the write path shares across its builders.

/// Bloom bits spent per distinct reference in a granule's reference filter: about a 1% false-positive rate, the same
/// budget the identity-hash point filters use.
pub const REFERENCE_FILTER_BITS_PER_KEY: u32 = 10;
