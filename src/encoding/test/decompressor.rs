use super::*;
use crate::encoding::Compression;
use std::sync::Arc;
use structured_zstd::encoding::CompressionLevel;

/// A decompressor that delegates to the software one but reports a different backend label — stands in for a
/// hardware-backed decompressor in tests without needing a device. Because it returns exactly the software bytes,
/// installing it process-wide is harmless even while other tests run.
#[derive(Debug, Clone, Copy, Default)]
struct DelegatingDecompressor;

impl Decompressor for DelegatingDecompressor {
    fn decompress(&self, compression: Compression, bytes: &[u8]) -> Result<Vec<u8>, crate::error::FormatError> {
        SoftwareDecompressor.decompress(compression, bytes)
    }

    fn backend(&self) -> DecompressorBackend {
        DecompressorBackend::Qatzip
    }
}

fn lz4_block(raw: &[u8]) -> Vec<u8> {
    lz4_flex::compress_prepend_size(raw)
}

fn zstd_block(raw: &[u8]) -> Vec<u8> {
    // Mirror the HEF zstd framing: a little-endian u32 uncompressed length, then the raw zstd body.
    let body = structured_zstd::encoding::compress_slice_to_vec(raw, CompressionLevel::Fastest);
    let mut framed = (raw.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&body);
    framed
}

#[test]
fn software_round_trips_every_compression() {
    let raw = b"the quick brown fox jumps over the lazy dog, repeatedly and repeatedly".repeat(8);
    let sw = SoftwareDecompressor;

    assert_eq!(sw.decompress(Compression::None, &raw).unwrap(), raw);
    assert_eq!(sw.decompress(Compression::Lz4, &lz4_block(&raw)).unwrap(), raw);
    assert_eq!(sw.decompress(Compression::Zstd1, &zstd_block(&raw)).unwrap(), raw);
    assert_eq!(sw.decompress(Compression::Zstd3, &zstd_block(&raw)).unwrap(), raw);
    assert_eq!(
        sw.decompress(
            Compression::SeekableZstd,
            &crate::encoding::seekable_zstd::compress(&raw).expect("the block compresses")
        )
        .unwrap(),
        raw
    );
    assert_eq!(sw.backend(), DecompressorBackend::Software);
}

/// The thread's Zstandard context is reused across blocks, so a block must never see anything the block before it
/// left behind: differing sizes, contents, and levels, interleaved with the other codecs, all decode to their own
/// bytes.
#[test]
fn a_reused_zstd_context_decodes_every_block_independently() {
    let sw = SoftwareDecompressor;
    let blocks: Vec<Vec<u8>> = (0..32)
        .map(|i| {
            let len = 1 + i * 37 % 4096;
            (0..len).map(|byte| ((byte * 7 + i) % 251) as u8).collect()
        })
        .collect();

    for round in 0..3 {
        for (index, raw) in blocks.iter().enumerate() {
            let level = if index.is_multiple_of(2) { 1 } else { 3 };
            let body = crate::encoding::compress_zstd(raw, level);
            let mut framed = (raw.len() as u32).to_le_bytes().to_vec();
            framed.extend_from_slice(&body);
            let compression = if level == 1 {
                Compression::Zstd1
            } else {
                Compression::Zstd3
            };
            assert_eq!(
                &sw.decompress(compression, &framed).unwrap(),
                raw,
                "block {index} in round {round}"
            );
            // Another codec between two zstd blocks must not disturb the context either.
            assert_eq!(&sw.decompress(Compression::Lz4, &lz4_block(raw)).unwrap(), raw);
        }
    }
}

/// The reused encoding context must produce the bytes a fresh one would — including when the level changes from one
/// block to the next, which is the only state a shared context carries between calls. A file's bytes are hashed and
/// compared across rewrites, so a context that quietly encoded differently would break reproducibility.
#[test]
fn a_reused_zstd_context_encodes_exactly_as_a_fresh_one() {
    let blocks: Vec<Vec<u8>> = (0..16)
        .map(|i| {
            let len = 1 + i * 211 % 8192;
            (0..len).map(|byte| ((byte / (1 + i)) % 251) as u8).collect()
        })
        .collect();

    // Levels alternate so every call changes the shared context's level, the case a per-call context never meets.
    for (index, raw) in blocks.iter().enumerate() {
        for level in [1, 3, 1] {
            assert_eq!(
                crate::encoding::compress_zstd(raw, level),
                structured_zstd::encoding::compress_slice_to_vec(raw, CompressionLevel::from_level(level)),
                "block {index} at level {level}"
            );
        }
    }
}

/// Every HEF file written before the block stages moved to the pure-Rust codec carries frames the C Zstandard
/// library produced. The stored format is Zstandard itself, not one library's dialect of it, so those blocks must
/// still decode to their exact bytes.
#[test]
fn blocks_written_by_the_c_zstd_library_still_decode() {
    let raw = b"the quick brown fox jumps over the lazy dog, repeatedly and repeatedly".repeat(64);
    for (level, compression) in [(1, Compression::Zstd1), (3, Compression::Zstd3)] {
        let body = zstd::bulk::compress(&raw, level).unwrap();
        let mut framed = (raw.len() as u32).to_le_bytes().to_vec();
        framed.extend_from_slice(&body);
        assert_eq!(
            SoftwareDecompressor.decompress(compression, &framed).unwrap(),
            raw,
            "a level-{level} block from the C library must still decode"
        );
    }
}

/// A forged `uncompressed_len` far past [`MAX_ZSTD_UNCOMPRESSED_BYTES`] is rejected before it ever reaches
/// the decoder, so a tiny compressed body can't force a multi-gigabyte allocation.
#[test]
fn software_rejects_forged_zstd_uncompressed_length() {
    let body = structured_zstd::encoding::compress_slice_to_vec(b"tiny", CompressionLevel::Fastest);
    let mut framed = u32::MAX.to_le_bytes().to_vec();
    framed.extend_from_slice(&body);

    let err = SoftwareDecompressor
        .decompress(Compression::Zstd1, &framed)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
}

/// A forged size prefix far past [`MAX_LZ4_UNCOMPRESSED_BYTES`] is rejected before `lz4_flex` allocates
/// a buffer for it, so a tiny compressed body can't force a multi-gigabyte allocation.
#[test]
fn software_rejects_forged_lz4_uncompressed_length() {
    let body = lz4_flex::compress_prepend_size(b"tiny");
    // Replace the first 4 bytes (lz4 size prefix) with u32::MAX while keeping the body.
    let mut forged = u32::MAX.to_le_bytes().to_vec();
    forged.extend_from_slice(&body[4..]);

    let err = SoftwareDecompressor.decompress(Compression::Lz4, &forged).unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
}

/// An lz4 size prefix that overstates the real decompressed length (while staying under the allocation ceiling) must
/// be rejected, not served as the real bytes silently padded with zeros.
#[test]
fn software_rejects_an_lz4_block_that_inflates_short_of_its_size_prefix() {
    let raw = b"short but genuine payload".repeat(4);
    let block = lz4_flex::compress_prepend_size(&raw);
    // Overstate the prefix by 10 bytes; the compressed body still inflates to only raw.len() bytes.
    let mut forged = ((raw.len() + 10) as u32).to_le_bytes().to_vec();
    forged.extend_from_slice(&block[4..]);

    let err = SoftwareDecompressor.decompress(Compression::Lz4, &forged).unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
}

/// The active decompressor defaults to software, can be swapped to another backend at runtime, and routes reads through
/// the installed one with identical bytes. This is one test on purpose: it mutates the process-wide active
/// decompressor, so splitting the default-is-software check into a separate test would let this install race that
/// assertion under the parallel test harness. Other store tests that decode during the install window are unaffected —
/// the stand-in delegates to software, so the bytes are identical regardless.
#[test]
fn active_decompressor_defaults_to_software_and_swaps_when_installed() {
    // Before any install, the default is the software decompressor.
    assert_eq!(active_decompressor().backend(), DecompressorBackend::Software);

    let raw = b"cold block bytes that an accelerator would decompress".repeat(4);
    let framed = zstd_block(&raw);

    // What the software path yields — the correctness oracle.
    let oracle = SoftwareDecompressor.decompress(Compression::Zstd1, &framed).unwrap();

    install_decompressor(Arc::new(DelegatingDecompressor));
    let active = active_decompressor();
    assert_eq!(active.backend(), DecompressorBackend::Qatzip);
    assert_eq!(active.decompress(Compression::Zstd1, &framed).unwrap(), oracle);
    // A backend that leaves `decompress_into` to its default still lands the software bytes in the caller's buffer.
    let mut plain = Vec::new();
    active.decompress_into(Compression::Zstd1, &framed, &mut plain).unwrap();
    assert_eq!(plain, oracle);

    // Restore the default so the global never outlives this test as anything other than the software decompressor.
    install_decompressor(Arc::new(SoftwareDecompressor));
    assert_eq!(active_decompressor().backend(), DecompressorBackend::Software);
}

/// `decompress_into` leaves exactly the bytes `decompress` returns and keeps the caller's allocation, so a scan
/// undoing block after block into one buffer allocates once. Stale bytes from a larger earlier block never survive
/// into a smaller later one, whichever codec follows.
#[test]
fn decompress_into_reuses_the_callers_buffer_and_matches_decompress() {
    let sw = SoftwareDecompressor;
    let large = b"a larger block whose bytes must not survive into the next decode".repeat(64);
    let small = b"small".repeat(3);
    let mut plain = Vec::new();
    sw.decompress_into(Compression::Zstd3, &zstd_block(&large), &mut plain)
        .unwrap();
    assert_eq!(plain, large);
    let capacity = plain.capacity();
    let pointer = plain.as_ptr();
    for (compression, stored) in [
        (Compression::Zstd1, zstd_block(&small)),
        (Compression::Lz4, lz4_block(&small)),
        (Compression::None, small.clone()),
        (Compression::Zstd3, zstd_block(&small)),
    ] {
        sw.decompress_into(compression, &stored, &mut plain).unwrap();
        assert_eq!(plain, small, "{compression:?}");
        assert_eq!(plain, sw.decompress(compression, &stored).unwrap());
        assert_eq!(plain.as_ptr(), pointer, "{compression:?} must reuse the buffer");
        assert_eq!(plain.capacity(), capacity, "{compression:?} must keep the allocation");
    }
}

/// A zstd length prefix that overstates the frame's real output — while staying under the allocation ceiling — is
/// refused rather than served short or zero-padded, on both decode entry points.
#[test]
fn software_rejects_a_zstd_block_that_inflates_short_of_its_length_prefix() {
    let raw = b"short but genuine payload".repeat(4);
    let block = zstd_block(&raw);
    let mut forged = ((raw.len() + 10) as u32).to_le_bytes().to_vec();
    forged.extend_from_slice(&block[4..]);

    let err = SoftwareDecompressor
        .decompress(Compression::Zstd3, &forged)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
    let mut plain = Vec::with_capacity(1 << 16);
    let err = SoftwareDecompressor
        .decompress_into(Compression::Zstd3, &forged, &mut plain)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
}

/// A zstd length prefix that understates the frame's real output is refused too — including into a reused buffer
/// with room to spare past the claim, where the context would otherwise write the whole frame and carry on.
#[test]
fn software_rejects_a_zstd_block_that_inflates_past_its_length_prefix() {
    let raw = b"a genuine payload longer than its forged prefix claims".repeat(4);
    let block = zstd_block(&raw);
    let mut forged = ((raw.len() - 10) as u32).to_le_bytes().to_vec();
    forged.extend_from_slice(&block[4..]);

    let err = SoftwareDecompressor
        .decompress(Compression::Zstd3, &forged)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
    let mut plain = Vec::with_capacity(1 << 16);
    let err = SoftwareDecompressor
        .decompress_into(Compression::Zstd3, &forged, &mut plain)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
}

/// A stored zstd body cut short is refused on the buffer-reusing path exactly as on the allocating one.
#[test]
fn software_rejects_a_truncated_zstd_body_on_both_paths() {
    let raw = b"a payload long enough that half its frame is not a frame".repeat(16);
    let block = zstd_block(&raw);
    let truncated = &block[..block.len() / 2];

    let err = SoftwareDecompressor
        .decompress(Compression::Zstd1, truncated)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
    let mut plain = Vec::with_capacity(raw.len());
    let err = SoftwareDecompressor
        .decompress_into(Compression::Zstd1, truncated, &mut plain)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
}

/// A forged length past [`MAX_ZSTD_UNCOMPRESSED_BYTES`] is refused before the reused buffer grows at all, so a decode
/// bomb cannot pin a huge allocation to the thread that met it.
#[test]
fn a_forged_zstd_length_never_grows_the_reused_buffer() {
    let body = structured_zstd::encoding::compress_slice_to_vec(b"tiny", CompressionLevel::Fastest);
    let mut framed = u32::MAX.to_le_bytes().to_vec();
    framed.extend_from_slice(&body);

    let mut plain = Vec::with_capacity(64);
    let capacity = plain.capacity();
    let err = SoftwareDecompressor
        .decompress_into(Compression::Zstd1, &framed, &mut plain)
        .unwrap_err();
    assert!(matches!(err, crate::error::FormatError::Structural { .. }));
    assert_eq!(plain.capacity(), capacity);
}

/// The per-thread inflate buffer comes back for the next block — and a buffer that outgrew the retained bound is let
/// go rather than pinned to the thread.
#[test]
fn the_inflate_buffer_is_retained_up_to_its_bound() {
    let pointer = with_inflate_buffer(|buffer| {
        buffer.clear();
        buffer.reserve_exact(1024);
        buffer.as_ptr()
    });
    assert_eq!(with_inflate_buffer(|buffer| buffer.as_ptr()), pointer);
    with_inflate_buffer(|buffer| buffer.reserve_exact(MAX_RETAINED_INFLATE_BYTES + 1));
    assert!(with_inflate_buffer(|buffer| buffer.capacity()) <= MAX_RETAINED_INFLATE_BYTES);
}
