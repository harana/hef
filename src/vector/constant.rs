//! Fixed numbers for vector columns and vector segment files.

/// Rows per granule: one sealed block per column per granule.
pub const GRANULE_ROWS: usize = 4096;
/// Every vector block starts on this file offset boundary, so SIMD kernels and Arrow buffers see aligned rows.
pub const BLOCK_ALIGNMENT: usize = 64;
pub const SEGMENT_MAGIC: &[u8; 4] = b"HEFV";
pub const SEGMENT_VERSION: u16 = 1;
pub const KEY_LEN: usize = 32;
pub const NONCE_PREFIX_LEN: usize = 16;
pub const WRAP_NONCE_LEN: usize = 24;
pub const TAG_LEN: usize = 16;
/// Magic, version, reserved, nonce prefix, wrap nonce, wrapped data key; padded to [`HEADER_LEN`].
pub const HEADER_USED_LEN: usize = 4 + 2 + 2 + NONCE_PREFIX_LEN + WRAP_NONCE_LEN + KEY_LEN + TAG_LEN;
/// The clear header, padded so the first block starts aligned.
pub const HEADER_LEN: usize = 128;
/// Footer offset, sealed footer length, magic.
pub const TRAILER_LEN: usize = 8 + 8 + 4;
/// The nonce counter the sealed footer uses; blocks count up from zero and never reach it.
pub const FOOTER_BLOCK_INDEX: u64 = u64::MAX;
/// int8 rows end in `scale f32 | offset f32`; each element decodes as `q * scale + offset`.
pub const I8_TRAILER_BYTES: usize = 8;
/// 1-bit rows end in `norm f32 | alignment f32`, the RaBitQ correction terms (see [`super::u1_estimated_dot`]).
pub const U1_TRAILER_BYTES: usize = 8;
/// How far a cosine vector's length may stray from 1 before the writer refuses it.
pub const NORMALIZATION_TOLERANCE: f32 = 1e-3;
/// Candidates the coarse 1-bit scan keeps per wanted result before re-scoring.
pub const DEFAULT_OVERSAMPLE: usize = 8;
/// The logical embedding column every stored representation links to.
pub const EMBEDDING_COLUMN_ID: u32 = crate::columns::column_ids::EMBEDDING_BASE;
pub const EVENT_ID_COLUMN: &str = "event_id";
pub const ROOM_ID_COLUMN: &str = "room_id";
pub const SENDER_COLUMN: &str = "sender";
pub const TIMESTAMP_COLUMN: &str = "timestamp";
