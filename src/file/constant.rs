//! The fixed sizes and magic bytes the shared file layer is built around, gathered in one place so every consumer reads
//! the same numbers.

/// Every durable block frame is written aligned to this many bytes. Appends of any other length are rejected.
pub const FRAME_ALIGNMENT: usize = 4096;

/// The largest a single large-event frame may grow to before the payload must be stored by reference instead.
pub const MAX_LARGE_FRAME: usize = 1 << 20; // 1 MiB

/// Each leaf of an outboard verified-streaming tree covers this many bytes of content, so verifying any byte range
/// over-reads at most one chunk group. It is a power-of-two multiple of BLAKE3's 1024-byte chunk and a build-time
/// choice, never stored in the file.
pub const CHUNK_GROUP_BYTES: usize = 1 << 20; // 1 MiB

/// The trailing 4 bytes of an object that carries an outboard tree, written after the content and the tree length.
pub const TREE_TRAILER_MAGIC: [u8; 4] = *b"HEFT";

/// The fixed trailer that ends a tree-carrying object: an 8-byte little-endian tree length followed by
/// [`TREE_TRAILER_MAGIC`].
pub const TREE_TRAILER_LEN: usize = 12;
