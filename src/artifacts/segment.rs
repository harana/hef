//! Recovers a journal file after a restart, frame by frame, stopping cleanly at the first torn or corrupt frame.
//!
//! A segment is one append-only journal file. This module tracks its small descriptor, chains every frame's hash into
//! the next so tampering or reordering is detectable, and replays the file from the start over the storage interface.
//! Replay is a pure read: running it again over the same bytes gives the identical result, so crash recovery is
//! repeatable. Startup goes one step further through [`recover_segment`], which cuts the file back to the last complete
//! frame so later appends stay reachable.

use super::frame::{self, HejFrameHeaderV1};
use super::{BLAKE3_LEN, FRAME_ALIGN, MAX_LARGE_FRAME, MAX_NORMAL_FRAME};
use crate::error::StorageError;
use crate::invariants::{JournalStorage, ShardId};

/// Segment lifecycle states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentState {
    Active,
    Recyclable,
    Retired,
    Sealed,
}

/// One segment's descriptor: the only persistent allocator metadata replay requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentDescriptor {
    pub first_frame_blake3: Option<[u8; BLAKE3_LEN]>,
    pub first_frame_sequence: Option<u64>,
    pub generation: u64,
    pub last_frame_blake3: Option<[u8; BLAKE3_LEN]>,
    pub last_frame_sequence: Option<u64>,
    pub segment_chain_blake3: Option<[u8; BLAKE3_LEN]>,
    pub segment_id: u64,
    pub state: SegmentState,
    pub write_cursor: u64,
}

impl SegmentDescriptor {
    /// A fresh descriptor for a brand-new, empty segment: active, with no frames and no recorded hashes yet.
    pub fn new(segment_id: u64, generation: u64) -> Self {
        Self {
            segment_id,
            generation,
            state: SegmentState::Active,
            write_cursor: 0,
            first_frame_sequence: None,
            last_frame_sequence: None,
            first_frame_blake3: None,
            last_frame_blake3: None,
            segment_chain_blake3: None,
        }
    }
}

/// A sealed segment becomes recyclable only when every condition holds: published coverage (or retained replicated
/// state), the recovery safety window elapsed, no reader-local LiveOverlay rebuild can require it, and the chain anchor
/// recorded in retention metadata. A recycled segment increments its generation and zeroes its header before reuse.
pub fn recycling_eligible(
    state: SegmentState,
    coverage_published_or_replicated: bool,
    safety_window_elapsed: bool,
    overlay_rebuild_may_require: bool,
    chain_anchor_recorded: bool,
) -> bool {
    state == SegmentState::Sealed
        && coverage_published_or_replicated
        && safety_window_elapsed
        && !overlay_rebuild_may_require
        && chain_anchor_recorded
}

/// `segment_chain_blake3[0] = BLAKE3(segment_id || generation || frame_blake3[0])`.
pub fn chain_init(segment_id: u64, generation: u64, frame_blake3: &[u8; BLAKE3_LEN]) -> [u8; BLAKE3_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&segment_id.to_le_bytes());
    hasher.update(&generation.to_le_bytes());
    hasher.update(frame_blake3);
    *hasher.finalize().as_bytes()
}

/// `segment_chain_blake3[n] = BLAKE3(segment_chain_blake3[n-1] || frame_blake3[n])`.
pub fn chain_next(previous: &[u8; BLAKE3_LEN], frame_blake3: &[u8; BLAKE3_LEN]) -> [u8; BLAKE3_LEN] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(previous);
    hasher.update(frame_blake3);
    *hasher.finalize().as_bytes()
}

/// One frame recovered by replay.
#[derive(Debug, Clone)]
pub struct ReplayedFrame {
    /// The whole aligned frame (header + payload + padding), so callers can re-decode the batch without another
    /// storage read.
    pub frame_bytes: Vec<u8>,
    pub header: HejFrameHeaderV1,
    pub offset: u64,
}

/// How the replay scan ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayTail {
    /// The scan consumed the written extent cleanly.
    Clean,
    /// A torn or corrupt frame was found: replay truncates here. When `trailing_nonzero` is set, non-zero bytes exist
    /// beyond the truncation point — bit rot rather than a clean torn tail — and the operator surface should flag the
    /// segment.
    Truncated { offset: u64, trailing_nonzero: bool },
}

/// The outcome of replaying one segment. Replay is a pure read: re-running it over the same durable bytes yields an
/// identical outcome (idempotent recovery).
#[derive(Debug)]
pub struct ReplayOutcome {
    pub frames: Vec<ReplayedFrame>,
    /// The verified hash-chain anchor over the recovered frames.
    pub segment_chain_blake3: Option<[u8; BLAKE3_LEN]>,
    pub tail: ReplayTail,
}

/// Replays one segment from offset 0 of the shard: validates every frame (header CRC-64/NVME precheck, structural
/// rules, authoritative frame BLAKE3) and the segment hash chain, truncating at the first torn or corrupt frame. Void
/// records are recovered like event frames; they carry zero rows but participate in the chain and in sequence coverage.
pub fn replay_segment(
    storage: &dyn JournalStorage,
    shard: ShardId,
    segment_id: u64,
    generation: u64,
) -> Result<ReplayOutcome, StorageError> {
    let extent = storage.extent(shard)?;
    let mut offset = 0u64;
    let mut frames = Vec::new();
    let mut chain: Option<[u8; BLAKE3_LEN]> = None;
    let mut recycled = false;
    let mut tail = ReplayTail::Clean;

    while offset + u64::from(FRAME_ALIGN) <= extent {
        let head_block = storage.read(shard, offset, FRAME_ALIGN)?;
        if super::is_all_zero(&head_block) {
            // Zeroed (recycled) space: the segment ends here.
            recycled = true;
            break;
        }
        let Ok(precheck) = frame::precheck_header(&head_block) else {
            tail = truncated(storage, shard, offset, offset + u64::from(FRAME_ALIGN), extent)?;
            break;
        };
        let frame_len = u64::from(precheck.frame_len);
        // The format's size limit is applied before the read, not after: the header CRC only proves the length is the
        // one that was written, so a crafted header could otherwise have replay request — and allocate — a frame far
        // larger than any frame the writer can produce.
        let max_frame_len = if precheck.is_large_event_frame() {
            MAX_LARGE_FRAME
        } else {
            MAX_NORMAL_FRAME
        };
        if frame_len == 0 || frame_len % u64::from(FRAME_ALIGN) != 0 || precheck.frame_len > max_frame_len {
            // The declared length cannot be trusted, so only the header block is known to belong to the failed frame.
            tail = truncated(storage, shard, offset, offset + u64::from(FRAME_ALIGN), extent)?;
            break;
        }
        if offset + frame_len > extent {
            // A torn tail: the frame's later blocks never all reached media. Everything inside its declared extent is
            // the frame's own partial write, not trailing data.
            tail = truncated(storage, shard, offset, offset + frame_len, extent)?;
            break;
        }
        let frame_bytes = if frame_len == u64::from(FRAME_ALIGN) {
            head_block
        } else {
            // The first 4096 bytes are already in hand as `head_block`; read only the remainder instead of
            // re-fetching the whole frame (which would read its first block a second time).
            let mut frame_bytes = head_block;
            frame_bytes.extend_from_slice(&storage.read(
                shard,
                offset + u64::from(FRAME_ALIGN),
                precheck.frame_len - FRAME_ALIGN,
            )?);
            frame_bytes
        };
        let Ok((header, _payload)) = frame::decode_frame(&frame_bytes) else {
            // Torn or corrupt inside its declared extent: the frame's own blocks are not trailing data, so the scan
            // starts past its declared end.
            tail = truncated(storage, shard, offset, offset + frame_len, extent)?;
            break;
        };
        chain = Some(match &chain {
            None => chain_init(segment_id, generation, &header.frame_blake3),
            Some(previous) => chain_next(previous, &header.frame_blake3),
        });
        frames.push(ReplayedFrame {
            offset,
            header,
            frame_bytes,
        });
        offset += frame_len;
    }

    // Fewer than 4096 bytes are left over: the extent ends mid-block, so those bytes cannot be a frame. That is a torn
    // tail — a short write or a crash partway through an append — and replay has to report it, or the segment reads as
    // cleanly ended while a partial frame still sits on media.
    if matches!(tail, ReplayTail::Clean) && !recycled && offset < extent {
        // The leftover bytes are the failed frame's own partial write, so the trailing-data scan starts past them.
        tail = truncated(storage, shard, offset, extent, extent)?;
    }

    Ok(ReplayOutcome {
        frames,
        tail,
        segment_chain_blake3: chain,
    })
}

/// Recovers one segment at startup: replays it, then cuts the shard back to the last complete frame when replay found
/// a torn or corrupt one. Returns the same outcome [`replay_segment`] does.
///
/// Replay on its own only reports where the damage starts. Without this cut the next append lands *after* the bad
/// frame, and because replay stops at the first frame it cannot read, everything written from then on is unreachable
/// forever. The storage layer cannot make the cut on its own — it sees 4096-byte blocks and a frame torn on a block
/// boundary looks perfectly aligned to it — so the offset has to come from replay, which decodes frames.
///
/// Running this twice over the same bytes is safe: the second run replays the already-shortened extent cleanly and
/// truncates nothing.
pub fn recover_segment(
    storage: &mut dyn JournalStorage,
    shard: ShardId,
    segment_id: u64,
    generation: u64,
) -> Result<ReplayOutcome, StorageError> {
    let outcome = replay_segment(&*storage, shard, segment_id, generation)?;
    if let ReplayTail::Truncated { offset, .. } = outcome.tail {
        storage.truncate(shard, offset)?;
    }
    Ok(outcome)
}

/// Classifies a truncation point: clean torn tail (only zeros beyond the failed frame) versus suspicious trailing
/// data. `scan_from` is the first offset that is not part of the failed frame itself — its declared end when the
/// header was readable, otherwise the block after `offset` — so a torn multi-block frame's own partial blocks are
/// never misread as bit rot.
fn truncated(
    storage: &dyn JournalStorage,
    shard: ShardId,
    offset: u64,
    scan_from: u64,
    extent: u64,
) -> Result<ReplayTail, StorageError> {
    let mut scan = scan_from;
    let mut trailing_nonzero = false;
    while scan + u64::from(FRAME_ALIGN) <= extent {
        let block = storage.read(shard, scan, FRAME_ALIGN)?;
        if !super::is_all_zero(&block) {
            trailing_nonzero = true;
            break;
        }
        scan += u64::from(FRAME_ALIGN);
    }
    Ok(ReplayTail::Truncated {
        offset,
        trailing_nonzero,
    })
}

/// Verifies a recovered chain anchor against a descriptor's recorded anchor. Replay must reject a segment whose
/// descriptor generation disagrees with the on-segment generation; with file-backed shards the generation lives in the
/// descriptor, so the anchor comparison is the enforcement point.
pub fn verify_chain_anchor(descriptor: &SegmentDescriptor, recovered: Option<&[u8; BLAKE3_LEN]>) -> bool {
    match (&descriptor.segment_chain_blake3, recovered) {
        (Some(recorded), Some(recovered)) => recorded == recovered,
        (None, None) => true,
        _ => false,
    }
}

#[cfg(test)]
#[path = "test/segment.rs"]
mod tests;
