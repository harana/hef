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
