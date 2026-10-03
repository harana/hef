//! Per-block column flags that enable metadata-only answers.
//!
//! Before reading any column bytes, a scan can consult two lightweight flags recorded in the page or granule metadata:
//!
//! - **`all_null`**: every value in the block is NULL.  Any predicate that
//! requires a non-NULL value is immediately falsified without touching the block.
//! - **`constant_value`**: the block holds exactly one distinct non-NULL value
//! and no NULLs.  An equality predicate on this column becomes a flag comparison: `AllRowsMatch` if the constant equals
//! the filter target, `NoRowsMatch` otherwise.  A projection of the column is also filled from the constant without
//! decoding the block.
//!
//! Both flags are exact: a block marked `all_null` truly contains no non-NULL values, and a `constant_value` truly
//! equals every row's value.  No residual filter is needed when either flag answers the predicate.

/// What the metadata flags conclude about one predicate for a single block, without reading or decoding the column
/// bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataAnswer {
    /// Every row in the block satisfies the predicate.  No block bytes are read; the result can be filled from the
    /// constant marker alone.
    AllRowsMatch,
    /// The flags cannot determine the outcome; the column block must be decoded and the predicate applied row by row.
    MustDecode,
    /// The predicate can never be satisfied by any row in this block.  No block bytes are fetched.
    NoRowsMatch,
}

/// Lightweight per-block column flags recorded in page or granule metadata.
///
/// When `all_null` is `true` the block holds only NULLs and `constant_value` must be `None`.  When `constant_value` is
/// `Some(v)` the block holds exactly the single non-NULL value `v` for every row, with no NULLs; `all_null` is then
/// `false`.  Both `false`/`None` means the block is mixed and must be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnFlags {
    /// Every value in this block is NULL.
    pub all_null: bool,
    /// The one non-NULL value shared by every row in this block, or `None` when the block is not constant or is
    /// all-null. Held as `i128`, the domain every stats-bearing kind maps into: integers directly, decimals as their
    /// fixed-scale mantissa, timestamps as their nanosecond count.
    pub constant_value: Option<i128>,
}

impl ColumnFlags {
    /// Flags for a block that is neither all-null nor constant — normal mixed-value block that must be decoded.
    pub fn mixed() -> Self {
        Self {
            all_null: false,
            constant_value: None,
        }
    }

    /// Flags for a block where every row is NULL.
    pub fn all_null() -> Self {
        Self {
            all_null: true,
            constant_value: None,
        }
    }

    /// Flags for a block where every row holds exactly `value` (no NULLs).
    pub fn constant(value: i128) -> Self {
        Self {
            all_null: false,
            constant_value: Some(value),
        }
    }

    /// Derives the flags from the exact per-block statistics the footer already stores: every stored row null means
    /// all-null; equal integer bounds with no stored null mean every row holds that one value. Anything else is mixed.
    /// Sound because the bounds are exact: they come from the same single pass that counted the rows.
    pub fn from_stats(row_count: u32, null_count: u32, min_i128: Option<i128>, max_i128: Option<i128>) -> Self {
        if row_count > 0 && null_count == row_count {
            return Self::all_null();
        }
        if let (0, Some(min), Some(max)) = (null_count, min_i128, max_i128)
            && min == max
            && row_count > 0
        {
            return Self::constant(min);
        }
        Self::mixed()
    }
}

/// Attempts to answer an equality predicate (`column = target`) from block metadata flags alone, without fetching or
/// decoding the column block.
///
/// - All-null block: no row holds any non-NULL value, so no row can equal
/// `target` → `NoRowsMatch`.
/// - Constant block whose value equals `target`: every row matches → `AllRowsMatch`.
/// - Constant block whose value differs from `target`: no row matches → `NoRowsMatch`.
/// - Mixed block: flags cannot answer → `MustDecode`.
pub fn answer_equality_from_flags(flags: &ColumnFlags, target: i128) -> MetadataAnswer {
    if flags.all_null {
        return MetadataAnswer::NoRowsMatch;
    }
    match flags.constant_value {
        Some(v) if v == target => MetadataAnswer::AllRowsMatch,
        Some(_) => MetadataAnswer::NoRowsMatch,
        None => MetadataAnswer::MustDecode,
    }
}

/// Attempts to answer any predicate that requires at least one non-NULL value (e.g. `amount > 0`, `currency IS NOT
/// NULL`) from block metadata flags alone.
///
/// An all-null block can never satisfy such a predicate → `NoRowsMatch`. All other blocks (constant or mixed) cannot be
/// ruled out by this flag alone → `MustDecode`.
pub fn answer_presence_from_flags(flags: &ColumnFlags) -> MetadataAnswer {
    if flags.all_null {
        MetadataAnswer::NoRowsMatch
    } else {
        MetadataAnswer::MustDecode
    }
}

#[cfg(test)]
#[path = "test/constant_flags.rs"]
mod tests;
