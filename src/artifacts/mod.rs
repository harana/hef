//! The on-disk pieces of the durable event log and how they turn back into queryable rows after a restart.
//!
//! Events are first written as fixed-size aligned frames in an append-only journal (called HEJ). This module owns those
//! frame and batch formats, the per-segment hash chain and crash replay that recover them intact, the deterministic
//! conversion of a recovered frame into an in-memory row batch the query side can serve, and the watermarks that track
//! how far durable and visible coverage has advanced.

pub mod batch;
pub mod external;
pub mod frame;
pub mod overlay;
pub mod segment;
pub mod watermark;

/// All HEJ frames are aligned to 4096 bytes.
pub const FRAME_ALIGN: u32 = 4096;

/// A normal HEJ frame length must be exactly one of these.
pub const NORMAL_FRAME_SIZES: [u32; 5] = [4096, 8192, 16384, 32768, 65536];

/// The default all-round flush target.
pub const DEFAULT_FLUSH_TARGET: u32 = 16384;

/// The latency-critical and low-load force-commit target.
pub const FORCE_COMMIT_TARGET: u32 = 4096;

/// The maximum normal HEJ frame size.
pub const MAX_NORMAL_FRAME: u32 = 65536;

/// The maximum large-event HEJ frame size.
pub const MAX_LARGE_FRAME: u32 = 1_048_576;

/// HEJ v1 has exactly one valid payload encoding.
pub const PAYLOAD_ENCODING_COMPACT_BATCH_V1: u32 = 1;

/// `HEJFrameHeaderV1` is exactly 192 bytes.
pub const FRAME_HEADER_LEN: u32 = 192;

/// `HEJCompactBatchHeaderV1` is exactly 128 bytes.
pub const BATCH_HEADER_LEN: u32 = 128;

/// `EventFixedRecordV1` is exactly 128 bytes.
pub const FIXED_RECORD_LEN: u32 = 128;

/// `EventVariableRecordV1` is exactly 64 bytes.
pub const VARIABLE_RECORD_LEN: u32 = 64;

/// `EventProvenanceRecordV1` is exactly 144 bytes.
pub const PROVENANCE_RECORD_LEN: u32 = 144;

/// String/dictionary ref value meaning "absent".
pub const REF_ABSENT: u32 = 0xFFFF_FFFF;

/// A BLAKE3 digest is exactly 32 bytes, whatever it hashes: a frame's own `frame_blake3`, or a segment's chained
/// `segment_chain_blake3`.
pub const BLAKE3_LEN: usize = 32;

/// Every HEJ magic tag (`HEJ1`, `HCB1`) is exactly 4 ASCII bytes.
pub const MAGIC_LEN: usize = 4;

/// Whether every byte of `bytes` is zero, checked eight bytes at a time instead of one at a time — a frame's padding
/// and a recycled block are the common callers, both wanting this over ranges up to tens of KiB.
pub(crate) fn is_all_zero(bytes: &[u8]) -> bool {
    let mut chunks = bytes.chunks_exact(8);
    (&mut chunks).all(|chunk| u64::from_ne_bytes(chunk.try_into().unwrap_or_default()) == 0)
        && chunks.remainder().iter().all(|&byte| byte == 0)
}
