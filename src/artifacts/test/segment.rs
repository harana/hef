use super::super::batch::tests::sample_frame;
use super::*;
use crate::invariants::sim::SimJournalStorage;

/// The smallest sample frame spanning more than two 4 KiB blocks (the next allowed normal size is 16 KiB).
fn multi_block_frame(epoch: u64, first_sequence: u64) -> Vec<u8> {
    for count in 2..200 {
        let frame_bytes = sample_frame(count, epoch, first_sequence);
        if frame_bytes.len() > 8192 {
            return frame_bytes;
        }
    }
    panic!("sample events never filled a multi-block frame");
}

#[test]
fn a_torn_multi_block_frame_is_a_clean_tail_not_trailing_nonzero() {
    // Frame A lands durably; a multi-block frame B is torn so only its first two 4 KiB blocks reach media. B's own
    // partial payload blocks sit past offset + 4096, inside its declared extent — the scan must not read them as
    // trailing non-zero bit rot.
    let frame_a = sample_frame(1, 1, 1);
    let frame_b = multi_block_frame(1, 2);
    assert!(
        frame_b[4096..8192].iter().any(|byte| *byte != 0),
        "the torn frame's second block must be non-zero for the scenario to be meaningful"
    );
    let mut storage = SimJournalStorage::new();
    storage.append(ShardId(0), &frame_a).unwrap();
    let offset_b = storage.append(ShardId(0), &frame_b[..8192]).unwrap();
    storage.sync(ShardId(0)).unwrap();

    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1);
    assert_eq!(
        replay.tail,
        ReplayTail::Truncated {
            offset: offset_b,
            trailing_nonzero: false,
        },
        "an ordinary torn multi-block frame is a clean torn tail, not bit rot"
    );
}

#[test]
fn a_corrupt_frame_with_real_data_beyond_it_still_reports_trailing_nonzero() {
    // Bit rot inside a fully written multi-block frame B, with a later frame C on media: replay truncates at B, and
    // the non-zero bytes past B's declared extent (frame C) must still be flagged.
    let frame_a = sample_frame(1, 1, 1);
    let frame_b = multi_block_frame(1, 2);
    let frame_c = sample_frame(1, 1, 60);
    let mut storage = SimJournalStorage::new();
    storage.append(ShardId(0), &frame_a).unwrap();
    let offset_b = storage.append(ShardId(0), &frame_b).unwrap();
    storage.append(ShardId(0), &frame_c).unwrap();
    storage.sync(ShardId(0)).unwrap();
    // Flip one payload byte in B's second block: the header CRC precheck still passes, the authoritative BLAKE3 fails.
    storage.corrupt_durable_byte(ShardId(0), offset_b + 5000);

    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 1);
    assert_eq!(
        replay.tail,
        ReplayTail::Truncated {
            offset: offset_b,
            trailing_nonzero: true,
        },
        "non-zero data past the failed frame's declared extent is still suspicious"
    );
}

#[test]
fn recovery_truncates_an_aligned_torn_frame_so_later_appends_stay_reachable() {
    // A multi-block frame B torn exactly on a 4 KiB boundary: the storage layer's own reopen check only drops tails
    // that are *not* block-aligned, so B survives untouched. Replay alone reports the damage and nothing acts on it;
    // the next append then lands past B and every later replay stops at B, stranding it (issue #11044).
    let frame_a = sample_frame(1, 1, 1);
    let frame_b = multi_block_frame(1, 2);
    let frame_c = sample_frame(1, 1, 60);
    let mut storage = SimJournalStorage::new();
    storage.append(ShardId(0), &frame_a).unwrap();
    let offset_b = storage.append(ShardId(0), &frame_b[..8192]).unwrap();
    storage.sync(ShardId(0)).unwrap();

    let recovered = recover_segment(&mut storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(recovered.frames.len(), 1, "only the intact frame A is recovered");
    assert_eq!(
        recovered.tail,
        ReplayTail::Truncated {
            offset: offset_b,
            trailing_nonzero: false,
        }
    );
    assert_eq!(
        storage.extent(ShardId(0)).unwrap(),
        offset_b,
        "recovery cuts the shard back to the last complete frame"
    );

    // The next append now takes B's offset rather than landing behind it, so replay reaches it.
    assert_eq!(storage.append(ShardId(0), &frame_c).unwrap(), offset_b);
    storage.sync(ShardId(0)).unwrap();
    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(replay.frames.len(), 2, "frames A and C both replay");
    assert_eq!(replay.tail, ReplayTail::Clean);
}

#[test]
fn recovering_twice_truncates_once_and_leaves_a_clean_segment() {
    let frame_a = sample_frame(1, 1, 1);
    let frame_b = multi_block_frame(1, 2);
    let mut storage = SimJournalStorage::new();
    storage.append(ShardId(0), &frame_a).unwrap();
    let offset_b = storage.append(ShardId(0), &frame_b[..8192]).unwrap();
    storage.sync(ShardId(0)).unwrap();

    recover_segment(&mut storage, ShardId(0), 1, 1).unwrap();
    let again = recover_segment(&mut storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(again.tail, ReplayTail::Clean, "the shortened extent replays cleanly");
    assert_eq!(again.frames.len(), 1);
    assert_eq!(storage.extent(ShardId(0)).unwrap(), offset_b);
}

#[test]
fn recovery_leaves_an_undamaged_segment_untouched() {
    let frame_a = sample_frame(1, 1, 1);
    let frame_b = sample_frame(1, 1, 60);
    let mut storage = SimJournalStorage::new();
    storage.append(ShardId(0), &frame_a).unwrap();
    storage.append(ShardId(0), &frame_b).unwrap();
    storage.sync(ShardId(0)).unwrap();
    let extent = storage.extent(ShardId(0)).unwrap();

    let recovered = recover_segment(&mut storage, ShardId(0), 1, 1).unwrap();
    assert_eq!(recovered.tail, ReplayTail::Clean);
    assert_eq!(recovered.frames.len(), 2);
    assert_eq!(storage.extent(ShardId(0)).unwrap(), extent);
}

#[test]
fn chain_init_and_chain_next_are_deterministic_and_order_sensitive() {
    let a = [1u8; 32];
    let b = [2u8; 32];
    // The same inputs always give the same anchor.
    assert_eq!(chain_init(1, 0, &a), chain_init(1, 0, &a));
    // Segment identity, generation, and the frame hash all feed the anchor.
    assert_ne!(chain_init(1, 0, &a), chain_init(2, 0, &a));
    assert_ne!(chain_init(1, 0, &a), chain_init(1, 1, &a));
    assert_ne!(chain_init(1, 0, &a), chain_init(1, 0, &b));

    let first = chain_init(1, 0, &a);
    assert_eq!(chain_next(&first, &b), chain_next(&first, &b));
    // Chaining folds order in: the two frames in the other order yield a different anchor.
    assert_ne!(chain_next(&first, &b), chain_next(&chain_init(1, 0, &b), &a));
}

#[test]
fn verify_chain_anchor_requires_both_present_and_equal() {
    let anchor = chain_init(1, 0, &[9u8; 32]);
    let mut descriptor = SegmentDescriptor::new(1, 0);

    // A brand-new segment with no recorded and no recovered anchor verifies.
    assert!(verify_chain_anchor(&descriptor, None));

    // Recorded-but-not-recovered and recovered-but-not-recorded both fail.
    descriptor.segment_chain_blake3 = Some(anchor);
    assert!(!verify_chain_anchor(&descriptor, None));
    assert!(!verify_chain_anchor(&SegmentDescriptor::new(1, 0), Some(&anchor)));

    // Both present: equal verifies, different fails.
    assert!(verify_chain_anchor(&descriptor, Some(&anchor)));
    assert!(!verify_chain_anchor(&descriptor, Some(&[0u8; 32])));
}

#[test]
fn recycling_eligible_requires_every_condition() {
    // Sealed with every guard satisfied: eligible.
    assert!(recycling_eligible(SegmentState::Sealed, true, true, false, true));
    // Any single unmet condition blocks recycling.
    assert!(!recycling_eligible(SegmentState::Active, true, true, false, true));
    assert!(!recycling_eligible(SegmentState::Sealed, false, true, false, true));
    assert!(!recycling_eligible(SegmentState::Sealed, true, false, false, true));
    assert!(!recycling_eligible(SegmentState::Sealed, true, true, true, true));
    assert!(!recycling_eligible(SegmentState::Sealed, true, true, false, false));
}

/// A storage wrapper that remembers the largest read length replay ever asked for.
struct ReadSizeRecorder {
    inner: SimJournalStorage,
    largest_read: std::cell::Cell<u32>,
}

impl JournalStorage for ReadSizeRecorder {
    fn append(&mut self, shard: ShardId, frame: &[u8]) -> Result<u64, crate::error::StorageError> {
        self.inner.append(shard, frame)
    }

    fn sync(&mut self, shard: ShardId) -> Result<(), crate::error::StorageError> {
        self.inner.sync(shard)
    }

    fn truncate(&mut self, shard: ShardId, len: u64) -> Result<(), crate::error::StorageError> {
        self.inner.truncate(shard, len)
    }

    fn read(&self, shard: ShardId, offset: u64, len: u32) -> Result<Vec<u8>, crate::error::StorageError> {
        self.largest_read.set(self.largest_read.get().max(len));
        self.inner.read(shard, offset, len)
    }

    fn extent(&self, shard: ShardId) -> Result<u64, crate::error::StorageError> {
        self.inner.extent(shard)
    }

    fn atomicity(&self, shard: ShardId) -> Result<crate::invariants::ShardAtomicity, crate::error::StorageError> {
        self.inner.atomicity(shard)
    }
}

#[test]
fn a_frame_length_past_the_format_limit_is_rejected_before_the_read() {
    // A header CRC only proves the declared length is the one that was written. Replay used to request that whole
    // length from storage and apply the format's 1 MiB limit afterwards, in `decode_frame`, so a crafted header could
    // have replay allocate far more than any frame the writer can produce (issue #8924).
    let mut frame = sample_frame(1, 1, 1);
    assert_eq!(frame.len(), 4096);
    let forged_len: u32 = 2 * 1024 * 1024;
    frame[8..12].copy_from_slice(&forged_len.to_le_bytes());
    // Re-stamp the header CRC over the header with the CRC and BLAKE3 fields zeroed, exactly as `build_frame` does, so
    // the precheck accepts the forged length.
    frame[128..168].fill(0);
    let crc = crate::file::integrity::crc64_nvme(&frame[..192]);
    frame[128..136].copy_from_slice(&crc.to_le_bytes());

    let mut storage = ReadSizeRecorder {
        inner: SimJournalStorage::new(),
        largest_read: std::cell::Cell::new(0),
    };
    storage.append(ShardId(0), &frame).unwrap();
    // Enough real extent behind the frame that the declared length fits inside it: without the limit check the read is
    // attempted rather than refused for running past the end.
    // One append per maximum-sized frame: the block layer refuses anything larger, so the padding is written in
    // MAX_LARGE_FRAME chunks rather than a single oversized one.
    let chunk = MAX_LARGE_FRAME as usize;
    for _ in 0..(forged_len as usize).div_ceil(chunk) {
        storage.append(ShardId(0), &vec![1u8; chunk]).unwrap();
    }
    storage.sync(ShardId(0)).unwrap();

    let replay = replay_segment(&storage, ShardId(0), 1, 1).unwrap();
    assert!(replay.frames.is_empty(), "the forged frame is not recovered");
    assert_eq!(
        replay.tail,
        ReplayTail::Truncated {
            offset: 0,
            trailing_nonzero: true,
        }
    );
    assert!(
        storage.largest_read.get() <= MAX_LARGE_FRAME,
        "no read may exceed the format's maximum frame length, asked for {}",
        storage.largest_read.get()
    );
}
