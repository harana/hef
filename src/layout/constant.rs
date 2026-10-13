//! Constants shared across this module's files, in one place.

use std::sync::LazyLock;

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

/// The `prev_row_hash` the first row of a brand-new audit trail chains from: thirty-two zero bytes. Every later audit
/// file starts from the previous file's last row hash instead.
pub const AUDIT_GENESIS: [u8; 32] = [0; 32];

/// The BLAKE3 key-derivation context that turns a row's `prev_row_hash` and the digest of its contents into the row's
/// own hash, keeping audit row hashes apart from every other BLAKE3 value the format stores.
pub const AUDIT_ROW_HASH_CONTEXT: &str = "pulse HEF v1 audit row hash";

/// A BLAKE3 hasher already keyed by [`AUDIT_ROW_HASH_CONTEXT`]. Hashing a row clones it rather than deriving the
/// context key again for every row.
pub static AUDIT_ROW_HASHER: LazyLock<blake3::Hasher> =
    LazyLock::new(|| blake3::Hasher::new_derive_key(AUDIT_ROW_HASH_CONTEXT));

/// Bytes reserved up front for the encoding of one audit row that its digest hashes, enough for a typical row's
/// envelope and a small payload so the buffer rarely grows.
pub const ROW_DIGEST_INPUT_CAPACITY: usize = 512;

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

/// How many edge kinds a graph edge block keeps per event: `prev` and `auth`, so each row owns two offset slots.
pub const GRAPH_EDGE_KINDS: usize = 2;

/// The longest subject name a graph-profile file records, in bytes; its length is stored in one byte.
pub const MAX_GRAPH_SUBJECT_BYTES: usize = 255;

/// The fewest bytes one entry of a graph file's unresolved-parent list takes: the child `u64`, the kind byte, and the
/// reference length `u16`, before the reference itself.
pub const UNRESOLVED_ENTRY_MIN_BYTES: usize = 11;

/// The bytes one chain's entry takes in a graph file's chain-cover summary: `chain_id`, `min_seq` and `max_seq`, each a
/// `u32`.
pub const CHAIN_RANGE_BYTES: usize = 12;

/// A remote graph read bridges a gap of up to this many bytes between two ranges it needs, fetching the gap rather
/// than paying for a second request.
pub const GRAPH_FETCH_GAP_BYTES: u64 = 1024 * 1024;

/// A remote graph read never grows one coalesced request past this many bytes by joining ranges; a single range
/// larger than this is still fetched whole.
pub const GRAPH_FETCH_WINDOW_BYTES: u64 = 8 * 1024 * 1024;

/// How many recent request latencies a remote graph keeps to estimate its 95th percentile for hedging.
pub const GRAPH_HEDGE_SAMPLES: usize = 128;

/// Below this many timed requests a remote graph hedges at the caller's floor alone, because the percentile is not
/// yet meaningful.
pub const GRAPH_HEDGE_MIN_SAMPLES: usize = 20;

/// The fit error, in rows, of a snapshot file's learned key model: a lookup's predicted window spans this many rows
/// either side of the estimate (plus one, see `LearnedPositionIndex::build`), plus the longest run of keys sharing a
/// model key. Small next to a granule, so the window almost always falls inside one.
pub const SNAPSHOT_KEY_ERROR_BOUND: u64 = 32;
