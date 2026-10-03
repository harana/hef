//! Turns a compressed column block back into its raw bytes — and lets a faster hardware decompressor stand in for the
//! built-in one without any caller noticing.
//!
//! Every column block in a HEF file may carry a trailing compression stage (LZ4 or Zstandard). Reading the block back
//! starts by undoing that stage. This module names the one place that work happens — the [`Decompressor`] interface —
//! and ships the built-in [`SoftwareDecompressor`] that always works on every host. A deployment that has a
//! QuickAssist (QAT) device can install a QATZip-backed decompressor at startup with [`install_decompressor`]; the read
//! path then routes cold-block decompression through it and falls back to the software one whenever the device is
//! absent, unhealthy, or disagrees. The bytes a reader sees are identical either way — the software path is the
//! correctness oracle.
//!
//! A scan undoes one trailing stage per block, so the output buffer is retained too: [`with_inflate_buffer`] lends
//! out one per thread, and [`Decompressor::decompress_into`] fills it in place, so a scan over many blocks allocates
//! once rather than once per block.

use super::{Compression, constant::MAX_RETAINED_INFLATE_BYTES};
use crate::error::FormatError;
use crate::file::bytes::Reader;
use std::cell::RefCell;
use std::sync::{Arc, OnceLock, RwLock};
use zstd::bulk::Decompressor as ZstdContext;

thread_local! {
    /// This thread's trailing-stage output buffer, absent while a decode holds it. See [`with_inflate_buffer`].
    static INFLATE_BUFFER: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };

    /// One Zstandard decoding context per thread, reused for every block that thread decompresses.
    ///
    /// A context allocates its window and entropy tables once, and this format's metadata blocks are tens of bytes
    /// — a marks page's columns hold one or two values each — so a fresh context per call made a stripe's marks
    /// decode many times the cost of its own payload. The decoded bytes are identical either way, as a context
    /// carries nothing from one block to the next.
    static ZSTD_CONTEXT: RefCell<ZstdContext<'static>> = RefCell::new(ZstdContext::default());
}

/// Runs `decode` with this thread's reusable trailing-stage output buffer and keeps the buffer for the next block.
///
/// The buffer is taken out of the thread's slot for the duration of the call, so a decode that undoes another block's
/// stage inside `decode` gets a buffer of its own rather than colliding with the outer one. A buffer that grew past
/// [`MAX_RETAINED_INFLATE_BYTES`] — one unusually large block in a scan of ordinary ones — is dropped instead of held
/// for the life of the thread.
pub(crate) fn with_inflate_buffer<R>(decode: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    let mut buffer = INFLATE_BUFFER.with(|slot| slot.borrow_mut().take()).unwrap_or_default();
    let decoded = decode(&mut buffer);
    if buffer.capacity() <= MAX_RETAINED_INFLATE_BYTES {
        INFLATE_BUFFER.with(|slot| *slot.borrow_mut() = Some(buffer));
    }
    decoded
}

/// Undoes one Zstandard block through this thread's reused decoding context into `plain`, emptied first and sized for
/// exactly the `expected` bytes the block's length prefix claims. The context writes straight into the buffer's spare
/// capacity — no byte is zeroed only to be overwritten — and the buffer's length becomes what the context reports
/// written, never more. Output that is not exactly `expected` bytes long is a forged or truncated block, and refuses:
/// a block is never served zero-padded or cut short.
fn zstd_decompress_into(body: &[u8], expected: usize, plain: &mut Vec<u8>) -> Result<(), FormatError> {
    plain.clear();
    plain.reserve_exact(expected);
    let written = ZSTD_CONTEXT
        .with(|slot| slot.borrow_mut().decompress_to_buffer(body, plain))
        .map_err(|_| FormatError::Structural {
            rule: "zstd stage failed to decompress",
        })?;
    if written != expected {
        return Err(FormatError::Structural {
            rule: "zstd stage decompressed to a length other than its length prefix",
        });
    }
    Ok(())
}

/// Decode-bomb guard: the most bytes a Zstandard block's stored `uncompressed_len` may claim before the read path
/// refuses to preallocate for it. The decode reserves a buffer of exactly this claimed size up front, so an
/// unbounded claim lets a tiny compressed body force an arbitrarily large allocation. Far above any block this
/// format's writer produces, so it rejects only forged lengths.
const MAX_ZSTD_UNCOMPRESSED_BYTES: usize = 1 << 28;

/// Decode-bomb guard: the most bytes an LZ4 block's size prefix may claim before the read path rejects it.
/// `lz4_flex::decompress_size_prepended` allocates a buffer of exactly this claimed size, so an attacker-controlled
/// prefix lets a tiny body force an arbitrarily large allocation. Far above any block this format's writer
/// produces, so it rejects only forged sizes.
const MAX_LZ4_UNCOMPRESSED_BYTES: usize = 1 << 28;

/// Which engine undid a block's trailing compression. A diagnostics label only, never a correctness input: the bytes
/// are the same whichever engine ran.
///
/// Variants are in strict alphabetical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecompressorBackend {
    /// An Intel QuickAssist (QAT) device, reached through `libqatzip`.
    Qatzip,
    /// The built-in decompressor (`lz4_flex` / the bundled libzstd).
    Software,
}

impl DecompressorBackend {
    /// A short, stable name for this backend, suitable for a metric label.
    pub fn label(self) -> &'static str {
        match self {
            DecompressorBackend::Qatzip => "intel_qat_qatzip",
            DecompressorBackend::Software => "software",
        }
    }
}

/// Undoes a column block's trailing compression stage.
///
/// One method takes the block's compression kind and its stored bytes and hands back the raw bytes underneath.
/// Implementations are swappable: the built-in [`SoftwareDecompressor`] runs everywhere, and an optional
/// hardware-backed one can be installed to offload the work — but an accelerated implementation must return
/// byte-for-byte what the software one would, or fall back to it.
pub trait Decompressor: Send + Sync {
    /// Returns the raw bytes of a block stored under `compression`. For [`Compression::None`] this is just the input;
    /// for LZ4/Zstandard it is the decompressed body. The result is identical to the software path.
    fn decompress(&self, compression: Compression, bytes: &[u8]) -> Result<Vec<u8>, FormatError>;

    /// [`Self::decompress`] into `plain`, which is emptied first and whose allocation is kept, so a scan that undoes
    /// block after block into one retained buffer allocates once rather than once per block. The bytes left in
    /// `plain` are exactly what [`Self::decompress`] returns. The default replaces `plain` with a fresh decompression,
    /// so an implementation that gains nothing from the caller's buffer need not override it.
    fn decompress_into(&self, compression: Compression, bytes: &[u8], plain: &mut Vec<u8>) -> Result<(), FormatError> {
        *plain = self.decompress(compression, bytes)?;
        Ok(())
    }

    /// Which engine this decompressor uses, for diagnostics/metrics.
    fn backend(&self) -> DecompressorBackend;
}

/// The built-in decompressor: LZ4 through `lz4_flex` and Zstandard through the bundled libzstd, available on every
/// host with no accelerator. This is the correctness oracle every hardware decompressor must match.
#[derive(Debug, Clone, Copy, Default)]
pub struct SoftwareDecompressor;

impl Decompressor for SoftwareDecompressor {
    fn decompress(&self, compression: Compression, bytes: &[u8]) -> Result<Vec<u8>, FormatError> {
        let mut plain = Vec::new();
        self.decompress_into(compression, bytes, &mut plain)?;
        Ok(plain)
    }

    fn decompress_into(&self, compression: Compression, bytes: &[u8], plain: &mut Vec<u8>) -> Result<(), FormatError> {
        match compression {
            Compression::None => {
                plain.clear();
                plain.extend_from_slice(bytes);
            }
            Compression::Deflate => *plain = super::deflate::decompress(bytes)?,
            Compression::SeekableZstd => super::seekable_zstd::decompress_all_into(bytes, plain)?,
            Compression::Lz4 => {
                let mut reader = Reader::new(bytes);
                let uncompressed_len = reader.u32("lz4 uncompressed length")? as usize;
                if uncompressed_len > MAX_LZ4_UNCOMPRESSED_BYTES {
                    return Err(FormatError::Structural {
                        rule: "lz4 uncompressed length exceeds the per-block ceiling",
                    });
                }
                let body = reader.take(reader.remaining(), "lz4 body")?;
                plain.clear();
                plain.resize(uncompressed_len, 0);
                let written = lz4_flex::decompress_into(body, plain).map_err(|_| FormatError::Structural {
                    rule: "lz4 stage failed to decompress",
                })?;
                // A block whose real output falls short of its size prefix would otherwise be served zero-padded.
                if written != uncompressed_len {
                    return Err(FormatError::Structural {
                        rule: "lz4 stage decompressed to a length other than its size prefix",
                    });
                }
            }
            Compression::Zstd1 | Compression::Zstd3 => {
                let mut reader = Reader::new(bytes);
                let uncompressed_len = reader.u32("zstd uncompressed length")? as usize;
                if uncompressed_len > MAX_ZSTD_UNCOMPRESSED_BYTES {
                    return Err(FormatError::Structural {
                        rule: "zstd uncompressed length exceeds the per-block ceiling",
                    });
                }
                let body = reader.take(reader.remaining(), "zstd body")?;
                zstd_decompress_into(body, uncompressed_len, plain)?;
            }
        }
        Ok(())
    }

    fn backend(&self) -> DecompressorBackend {
        DecompressorBackend::Software
    }
}

/// The process-wide active decompressor. Starts as the software one and can be replaced once at startup by
/// [`install_decompressor`] when a faster device is probed and selected.
fn slot() -> &'static RwLock<Arc<dyn Decompressor>> {
    static ACTIVE: OnceLock<RwLock<Arc<dyn Decompressor>>> = OnceLock::new();
    ACTIVE.get_or_init(|| RwLock::new(Arc::new(SoftwareDecompressor)))
}

/// The decompressor the read path uses right now. Defaults to the software one, so callers always get a working
/// decompressor even before any startup probe.
pub fn active_decompressor() -> Arc<dyn Decompressor> {
    slot()
        .read()
        .map(|guard| Arc::clone(&guard))
        .unwrap_or_else(|_| Arc::new(SoftwareDecompressor))
}

/// Installs `decompressor` as the active one for the whole process. A node's startup capability probe calls this once
/// when it has selected a hardware backend; with no probe, the software decompressor stays active. Installing a
/// poisoned-lock no-op rather than panicking keeps the read path alive.
pub fn install_decompressor(decompressor: Arc<dyn Decompressor>) {
    if let Ok(mut guard) = slot().write() {
        *guard = decompressor;
    }
}

#[cfg(test)]
#[path = "test/decompressor.rs"]
mod tests;
