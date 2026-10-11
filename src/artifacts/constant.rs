//! Fixed numbers of the state chunk format: its framing, entry widths, and where the chunker cuts.
//!
//! See: hef-physical-artifacts/spec.md

/// The four bytes every state chunk's plaintext starts with.
pub const STATE_CHUNK_MAGIC: [u8; 4] = *b"HSC1";

/// A state chunk's header: the magic, the level byte, three zero bytes, and the `u32` entry count.
pub const STATE_CHUNK_HEADER_LEN: usize = 12;

/// The CRC-64/NVME that closes every state chunk's plaintext.
pub const STATE_CHUNK_CRC_LEN: usize = 8;

/// One leaf entry: type id `u32`, state key id `u32`, event sequence number `u64`.
pub const STATE_LEAF_ENTRY_LEN: usize = 16;

/// One interior entry: the child's first type id `u32` and state key id `u32`, then its 32-byte address.
pub const STATE_INTERIOR_ENTRY_LEN: usize = 40;

/// Bytes of entries a chunk holds before a boundary may end it. Only the last chunk of a level is smaller.
pub const STATE_CHUNK_MIN_BYTES: usize = 4 * 1024;

/// Bytes of entries no chunk passes: the entry that would pass it starts the next chunk, whatever the boundary hash
/// says.
pub const STATE_CHUNK_MAX_BYTES: usize = 16 * 1024;

/// Past the minimum, each entry ends its chunk with probability (entry length / this), so a chunk averages about
/// minimum + this many bytes of entries: 8 KiB.
pub const STATE_CHUNK_BOUNDARY_SPAN_BYTES: usize = 4 * 1024;
