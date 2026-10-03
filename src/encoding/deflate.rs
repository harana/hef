//! A trailing compression family built from small, independently-inflatable RFC-1951 deflate granules, so a reader can
//! decode one granule of a page without touching the rest of it.
//!
//! Every granule is deflated on its own, with no shared dictionary carried over from its neighbours. That is what
//! bounds each granule's back-references to its own [`GRANULE_BYTES`] window and makes it a complete, portable deflate
//! stream in its own right — decodable by the pure-Rust `miniz_oxide` inflater below whether or not Intel IAA produced
//! or ever reads it.
//!
//! The granule index is a [`mini_block_directory`] — deflate is that shape's first user, one directory entry per
//! independently inflatable granule.

use super::mini_block_directory;
use crate::error::FormatError;
use crate::file::bytes::slice;
use miniz_oxide::deflate::core::{
    CompressorOxide, TDEFLFlush, TDEFLStatus, compress_to_output, create_comp_flags_from_zip_params,
};
use miniz_oxide::inflate::TINFLStatus;
use miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
use miniz_oxide::inflate::core::{DecompressorOxide, decompress as inflate_granule};
use std::cell::RefCell;

/// The most plaintext bytes one granule may hold — the IAA-compatible history window. Compressing each granule with
/// no dictionary shared with its neighbours keeps every back-reference inside this bound.
pub const GRANULE_BYTES: usize = 4096;

/// A fixed, moderate effort level: this is a correctness-preserving trailing stage picked by measured size, not a
/// tunable operator setting.
const DEFLATE_LEVEL: u8 = 6;

/// The most bytes a single deflate granule's decompressor may emit before the decompress path aborts.
/// This bounds the output of any stored deflate block, even a crafted one, to at most GRANULE_BYTES — far
/// above any granule a well-formed writer produces, so it rejects only forged input.
const MAX_GRANULE_UNCOMPRESSED_BYTES: usize = GRANULE_BYTES + 65536;

/// The compressor's raw `tdefl` flags for [`DEFLATE_LEVEL`], raw (no zlib header) format — computed once so every
/// thread's reused [`CompressorOxide`] is built with exactly the flags `compress_to_vec(_, DEFLATE_LEVEL)` used.
const DEFLATE_FLAGS: u32 = create_comp_flags_from_zip_params(DEFLATE_LEVEL as i32, 0, 0);

thread_local! {
    /// One deflate compressor per thread, reused for every granule that thread compresses.
    ///
    /// A fresh `CompressorOxide` carries ~170 KB of boxed, zeroed hash-chain and Huffman state — 40x a granule's own
    /// size — so building one per [`GRANULE_BYTES`] granule made compression's own bookkeeping dwarf the bytes it was
    /// compressing. [`CompressorOxide::reset`] clears that state without reallocating it, and carries nothing from one
    /// granule to the next, so the compressed bytes are identical either way.
    static COMPRESSOR: RefCell<CompressorOxide> = RefCell::new(CompressorOxide::new(DEFLATE_FLAGS));
    /// One deflate decompressor per thread, reused for every granule that thread inflates. Mirrors [`COMPRESSOR`]:
    /// [`DecompressorOxide::init`] resets it to a fresh stream's starting state without reallocating its ~11 KB of
    /// Huffman tables.
    static DECOMPRESSOR: RefCell<DecompressorOxide> = RefCell::new(DecompressorOxide::new());
}

/// Splits `bytes` into `<= GRANULE_BYTES` granules and deflates each one independently, through one reused
/// [`CompressorOxide`] — no fresh ~170 KB compressor, and no per-granule `Vec<u8>`, for every granule.
///
/// On-disk shape: a [`mini_block_directory`] header, then the concatenated compressed granules.
pub fn compress(bytes: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(bytes.len());
    let mut granule_lens = Vec::with_capacity(bytes.len().div_ceil(GRANULE_BYTES));

    COMPRESSOR.with(|cell| {
        let mut compressor = cell.borrow_mut();
        for granule in bytes.chunks(GRANULE_BYTES) {
            compressor.reset();
            let before = body.len();
            deflate_granule(&mut compressor, granule, &mut body);
            granule_lens.push(body.len() - before);
        }
    });

    let mut out = mini_block_directory::encode(bytes.len(), &granule_lens);
    out.extend_from_slice(&body);
    out
}

/// Deflates one granule through `compressor` (already [`CompressorOxide::reset`]), appending its compressed bytes
/// straight onto `out`.
fn deflate_granule(compressor: &mut CompressorOxide, granule: &[u8], out: &mut Vec<u8>) {
    let mut remaining = granule;
    loop {
        let (status, bytes_in) = compress_to_output(compressor, remaining, TDEFLFlush::Finish, |chunk| {
            out.extend_from_slice(chunk);
            true
        });
        remaining = remaining.get(bytes_in..).unwrap_or(&[]);
        match status {
            TDEFLStatus::Done => break,
            TDEFLStatus::Okay => continue,
            _ => unreachable!("miniz_oxide deflate failed to compress well-formed input"),
        }
    }
}

/// The plaintext length a page [`compress`] produced will inflate to, read from the header alone.
pub fn plain_len(bytes: &[u8]) -> Result<usize, FormatError> {
    Ok(mini_block_directory::decode(bytes)?.plain_len())
}

/// The absolute byte range of granule `granule`'s compressed bytes within an encoded page — the per-granule
/// mini-block offsets a reader uses to fetch or decode just that slice.
pub fn granule_byte_range(bytes: &[u8], granule: usize) -> Result<(usize, usize), FormatError> {
    mini_block_directory::decode(bytes)?.byte_range(granule)
}

/// Inflates every granule and concatenates them back into the original bytes.
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>, FormatError> {
    let directory = mini_block_directory::decode(bytes)?;
    let plain_len = directory.plain_len();
    // Each granule inflates to at most MAX_GRANULE_UNCOMPRESSED_BYTES, so a declared length beyond what the page's
    // granules could hold is a forged header — reject it before trusting it as the accumulation bound below.
    if plain_len > directory.block_count().saturating_mul(MAX_GRANULE_UNCOMPRESSED_BYTES) {
        return Err(FormatError::Structural {
            rule: "deflate plain length exceeds what its granule count can hold",
        });
    }
    let mut plain = Vec::with_capacity(directory.plain_capacity_hint());
    for (start, len) in directory.ranges() {
        inflate_into(
            slice(bytes, start, len, "deflate granule")?,
            GRANULE_BYTES,
            MAX_GRANULE_UNCOMPRESSED_BYTES,
            &mut plain,
        )?;
        if plain.len() > plain_len {
            return Err(FormatError::Structural {
                rule: "deflate granule decompressed to more bytes than the directory declares",
            });
        }
    }
    Ok(plain)
}

/// Inflates just granule `granule`, reading only that granule's compressed bytes — the mechanism behind
/// "Single-granule random access on a deflate page".
pub fn decompress_granule(bytes: &[u8], granule: usize) -> Result<Vec<u8>, FormatError> {
    let (start, len) = granule_byte_range(bytes, granule)?;
    let mut plain = Vec::new();
    inflate_into(
        slice(bytes, start, len, "deflate granule")?,
        GRANULE_BYTES,
        MAX_GRANULE_UNCOMPRESSED_BYTES,
        &mut plain,
    )?;
    Ok(plain)
}

/// A deflate page opened for range reads: the mini-block directory is decoded once up front, so reading several
/// granules never re-parses the header or recomputes offsets per granule.
pub struct Page<'a> {
    bytes: &'a [u8],
    directory: mini_block_directory::MiniBlockDirectory,
}

impl<'a> Page<'a> {
    /// Decodes the page's mini-block directory once; granule reads then resolve their byte ranges from it.
    pub fn open(bytes: &'a [u8]) -> Result<Self, FormatError> {
        Ok(Self {
            bytes,
            directory: mini_block_directory::decode(bytes)?,
        })
    }

    /// The plaintext length the whole page inflates to, from the directory alone.
    pub fn plain_len(&self) -> usize {
        self.directory.plain_len()
    }

    /// How many independently inflatable granules the page holds.
    pub fn granule_count(&self) -> usize {
        self.directory.block_count()
    }

    /// Inflates granules `first..=last` and concatenates them in order. Every granule must inflate to exactly the
    /// length its position implies — [`GRANULE_BYTES`] for all but the page's final granule, the declared remainder
    /// for the final one — so a caller's fixed-stride offset arithmetic over the result is sound. Any other length
    /// is a format error, never silently mis-aligned rows.
    pub fn decompress_granules(&self, first: usize, last: usize) -> Result<Vec<u8>, FormatError> {
        if first > last || last >= self.directory.block_count() {
            return Err(FormatError::RefOutOfRange {
                what: "mini-block directory index",
            });
        }
        let plain_len = self.directory.plain_len();
        let capacity = ((last - first + 1) * GRANULE_BYTES).min(self.directory.plain_capacity_hint());
        let mut plain = Vec::with_capacity(capacity);
        for (granule, (start, len)) in self.directory.ranges().enumerate().skip(first).take(last - first + 1) {
            let expected = GRANULE_BYTES.min(plain_len.saturating_sub(granule * GRANULE_BYTES));
            let before = plain.len();
            inflate_into(
                slice(self.bytes, start, len, "deflate granule")?,
                expected.max(1),
                MAX_GRANULE_UNCOMPRESSED_BYTES,
                &mut plain,
            )?;
            if plain.len() - before != expected {
                return Err(FormatError::Structural {
                    rule: "deflate granule length disagrees with the directory's plain length",
                });
            }
        }
        Ok(plain)
    }
}

/// Inflates one granule's compressed bytes through this thread's reused [`DECOMPRESSOR`], appending the plaintext
/// straight onto `out` — no per-granule `Vec<u8>` that is only copied into `out` right after.
///
/// `initial_guess` sizes the first attempt (callers know it exactly for a well-formed granule); if the granule
/// decompresses to more than that — never true for bytes [`compress`] produced, only possible for forged input — the
/// buffer doubles up to `hard_limit` and decompression resumes where it left off.
fn inflate_into(
    compressed: &[u8],
    initial_guess: usize,
    hard_limit: usize,
    out: &mut Vec<u8>,
) -> Result<(), FormatError> {
    let start = out.len();
    let mut cap = initial_guess.clamp(1, hard_limit);
    out.resize(start + cap, 0);
    let mut remaining = compressed;

    let result = DECOMPRESSOR.with(|cell| -> Result<usize, FormatError> {
        let mut decompressor = cell.borrow_mut();
        decompressor.init();
        let mut out_pos = start;
        loop {
            let (status, in_consumed, out_consumed) = inflate_granule(
                &mut decompressor,
                remaining,
                out,
                out_pos,
                TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
            );
            out_pos += out_consumed;
            match status {
                TINFLStatus::Done => return Ok(out_pos),
                TINFLStatus::HasMoreOutput if cap < hard_limit => {
                    remaining = remaining.get(in_consumed..).unwrap_or(&[]);
                    cap = cap.saturating_mul(2).min(hard_limit);
                    out.resize(start + cap, 0);
                }
                _ => {
                    return Err(FormatError::Structural {
                        rule: "deflate granule failed to decompress",
                    });
                }
            }
        }
    });

    match result {
        Ok(out_pos) => {
            out.truncate(out_pos);
            Ok(())
        }
        Err(err) => {
            out.truncate(start);
            Err(err)
        }
    }
}

#[cfg(test)]
#[path = "test/deflate.rs"]
mod tests;
