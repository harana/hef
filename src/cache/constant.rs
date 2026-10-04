//! Fixed values shared by the cache tiers.

/// Bytes of bookkeeping charged against the memory tier's capacity for one held entry, used only to derive how many
/// entries the tier will hold at once.
///
/// An entry costs more than its payload: its key is copied into both index maps, each map carries a tree node, and the
/// slot holds an `Arc` and its access tick. Accounting only the payload lets a flood of one-byte entries stay under
/// `capacity_bytes` while resident memory grows without bound, so the entry count is bounded too. The figure is a
/// deliberate over-estimate, so the overhead a full tier can carry stays on the order of its configured capacity.
pub const ENTRY_OVERHEAD_BYTES: u64 = 256;

/// Numerator of the fraction an evicted block must compress to before it is kept in the cold segment: at most 3/4 of
/// its original size, a saving of at least 25%. A block that misses the bar is dropped, since keeping it would cost
/// nearly its full size and still charge a decompression on the next read.
pub const COLD_COMPRESSION_BAR_NUM: u64 = 3;

/// Denominator of the [`COLD_COMPRESSION_BAR_NUM`] fraction.
pub const COLD_COMPRESSION_BAR_DEN: u64 = 4;

/// Fewest entries the memory tier will hold however small its capacity, so a tier configured for a handful of tiny
/// blocks still works.
pub const MIN_ENTRIES: usize = 8;

/// Sub-directory of the disk tier's root that holds its block files. Only this directory is cleared when a disk tier
/// opens, so a root shared with other files never loses them.
pub const DISK_TIER_DIR: &str = "hef-block-cache";
