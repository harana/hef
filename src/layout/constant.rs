//! Constants shared across this module's files, in one place.

/// The fixed trailer that ends a HEF content tail: an 8-byte little-endian footer-blob length followed by
/// [`HEF_MAGIC`]. Mirrors [`crate::file::constant::TREE_TRAILER_LEN`], which plays the same role for the sibling
/// outboard-tree trailer.
pub const FOOTER_TAIL_TRAILER_LEN: usize = 12;

/// The format's magic bytes: the first four bytes of the fixed header, and the last four bytes of every footer tail.
/// Mirrors [`crate::file::constant::TREE_TRAILER_MAGIC`], which plays the same role for the sibling outboard-tree
/// trailer.
pub const HEF_MAGIC: [u8; 4] = *b"HEF1";

/// The most decoded data a projected scan ([`super::reader::HefFile::scan_projected`]) holds at once, in estimated
/// bytes. Consecutive granules are gathered into a window of at most this much, the window's blocks decode together,
/// and the next window does not start until the caller has taken this one's batches — so a scan of every column of
/// a large file never holds `columns × granules` decoded blocks at once. A block's estimate is its mark's decoded
/// body length plus sixteen bytes per row (the inflate buffer, then the widest fixed-width value or one string view),
/// so the bound tracks the real decode to within a small factor whatever the column kinds. Per-worker codec scratch
/// sits on top of it, once per pool thread, bounded on its own by the `encoding` module's retained-scratch caps. At
/// 32 MiB the default benchmark file (100,000 rows, 56 MiB estimated over every column) scans as two windows with a
/// measured 22 MB peak of live decoded blocks, while a day-scale file streams through in fixed-size steps.
pub const SCAN_WINDOW_BYTES: u64 = 32 * 1024 * 1024;

/// How many doubling buckets hold the verified ranges a remote reader has fetched: bucket `b` holds `2^b` ranges, so
/// this bounds one reader at `2^48 - 1` fetches, far past anything a single file can need.
pub const HELD_RANGE_BUCKETS: usize = 48;

/// The magic bytes of a log object ([`super::log`]): its first four bytes and, once sealed, its last four. Different
/// from [`HEF_MAGIC`] at both ends, so a log object never opens as a HEF file and a HEF file never opens as a log.
pub const LOG_MAGIC: [u8; 4] = *b"HEFL";

/// The log object format version stored in its header. A reader refuses any other value.
pub const LOG_FORMAT_VERSION: u8 = 1;

/// A log object's header: [`LOG_MAGIC`], the format version byte, and the profile code byte.
pub const LOG_HEADER_LEN: usize = 6;

/// The bytes one record adds around its payload: a 4-byte little-endian length before it and an 8-byte CRC-64/NVME
/// after it.
pub const LOG_RECORD_FRAME_LEN: usize = 12;

/// The record length that marks the trailer instead of a record, so no record payload may be this long.
pub const LOG_TRAILER_SENTINEL: u32 = u32::MAX;

/// A sealed log object's trailer: [`LOG_TRAILER_SENTINEL`], the 4-byte little-endian record count, and [`LOG_MAGIC`].
pub const LOG_TRAILER_LEN: usize = 12;
