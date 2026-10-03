//! Compresses a byte arena into a series of independently decompressible Zstandard frames, so reading one value back
//! inflates one frame instead of the whole arena.
//!
//! A whole-arena Zstandard block has to be inflated end to end before its first byte is readable, which makes a point
//! read pay for every row the arena holds. The frames written here follow the Zstandard Seekable Format: each frame
//! compresses [`FRAME_BYTES`] of plaintext on its own, and a seek table — a skippable frame appended after the last
//! one — records where every frame starts in both the compressed and the decompressed stream. A reader resolves the
//! byte range it wants to the frames covering it and decompresses only those.
//!
//! The stored bytes stay an ordinary Zstandard stream: any conforming decoder concatenating the frames recovers the
//! original arena, and the seek table is skippable, so a decoder unaware of the format ignores it.
//!
//! See: hef-encodings-and-compression/spec.md

use crate::error::FormatError;
use std::borrow::Cow;
use std::cell::RefCell;
use zeekstd::{BytesWrapper, EncodeOptions, FrameSizePolicy, SeekTable};
use zstd::bulk::Decompressor;

/// Plaintext bytes one frame holds. Big enough that a frame's own zstd frame header and its seek-table entry stay
/// noise against the compressed body, and that back-references still find matches inside the frame; small enough that
/// a point read inflates a fraction of the arena rather than all of it.
pub const FRAME_BYTES: u32 = 64 * 1024;

/// Compression level every frame is compressed at — the same level the whole-arena cold path uses, so framing changes
/// where the boundaries fall and nothing else.
const LEVEL: i32 = 3;

/// Decode-bomb guard: the most plaintext bytes one [`decompress_range`] call, or one frame, may be asked for. Both
/// allocate the full length up front, so a forged seek table claiming a huge frame would otherwise force an
/// arbitrarily large allocation. Far above any arena this format's writer produces.
const MAX_RANGE_BYTES: u64 = 1 << 28;

thread_local! {
    /// One Zstandard decoding context per thread, reused for every frame that thread inflates. A context allocates
    /// its tables once; the streaming decoder used before built a fresh one per call, which cost more than inflating
    /// most frames did. The bytes are identical either way, as a context carries nothing from one frame to the next.
    static DECODER: RefCell<Decompressor<'static>> = RefCell::new(Decompressor::default());

    /// The frame buffer the last [`Window`] on this thread let go of, so the next window on the thread inflates its
    /// first frame into it instead of allocating.
    static SPARE_FRAME: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };

    /// Frames inflated one at a time on this thread since a test last took the count, so a test can see what a read
    /// left compressed.
    #[cfg(test)]
    static FRAMES_INFLATED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The number of frames inflated one at a time on this thread since the last call, for tests that check what a read
/// left compressed.
#[cfg(test)]
pub(crate) fn take_frames_inflated() -> usize {
    FRAMES_INFLATED.with(|count| count.replace(0))
}

fn failed(rule: &'static str) -> FormatError {
    FormatError::Structural { rule }
}

/// Inflates `compressed` — one frame, or a whole stored form — into `plain`, emptied first and sized for exactly the
/// `expected` plaintext bytes the seek table promises. A single call decodes every frame in the input in turn and
/// skips the seek table's own skippable frame, so nothing is copied through a streaming buffer on the way. Output that
/// is not exactly `expected` bytes long is a forged or truncated stream, and refuses.
fn inflate_exact(compressed: &[u8], expected: u64, plain: &mut Vec<u8>) -> Result<(), FormatError> {
    if expected > MAX_RANGE_BYTES {
        return Err(failed(
            "seekable zstd range is inverted or exceeds the per-call ceiling",
        ));
    }
    plain.clear();
    plain.reserve_exact(expected as usize);
    let written = DECODER
        .with(|slot| slot.borrow_mut().decompress_to_buffer(compressed, plain))
        .map_err(|_| failed("seekable zstd frame failed to decompress"))?;
    if written as u64 != expected {
        return Err(failed(
            "seekable zstd range decompressed short of its seek table's length",
        ));
    }
    Ok(())
}

/// Inflates frame `index` of a stored form into `plain`, emptied first, checking it holds exactly the plaintext its
/// seek table entry promises.
fn decompress_frame(stored: &[u8], table: &SeekTable, index: u32, plain: &mut Vec<u8>) -> Result<(), FormatError> {
    let outside = || failed("seekable zstd frame index is outside the seek table");
    let start = table.frame_start_comp(index).map_err(|_| outside())?;
    let end = table.frame_end_comp(index).map_err(|_| outside())?;
    let expected = table.frame_size_decomp(index).map_err(|_| outside())?;
    let compressed = usize::try_from(start)
        .ok()
        .zip(usize::try_from(end).ok())
        .and_then(|(start, end)| stored.get(start..end))
        .ok_or_else(|| failed("seekable zstd frame runs past the stored bytes"))?;
    #[cfg(test)]
    FRAMES_INFLATED.with(|count| count.set(count.get() + 1));
    inflate_exact(compressed, expected, plain)
}

/// Compresses `bytes` into seekable frames, returning the stored form — the frames followed by their seek table.
///
/// Returns `None` if the encoder is unavailable or fails, leaving the caller to store the bytes some other way; it
/// never returns a partially written stream.
pub fn compress(bytes: &[u8]) -> Option<Vec<u8>> {
    compress_with_breaks(bytes, &[])
}

/// [`compress`] with a frame ending at each plaintext offset in `breaks` as well as every [`FRAME_BYTES`], so a
/// section of the arena that starts at a break sits in frames of its own and a reader wanting only the bytes before
/// it, or only the section, inflates nothing of the other. `breaks` are taken in ascending order; one at either end
/// of the arena, out of order, or repeated is ignored. The stored form is an ordinary seekable stream either way — a
/// reader resolves ranges through the seek table and needs no notice of where the frames fall.
pub fn compress_with_breaks(bytes: &[u8], breaks: &[usize]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(bytes.len());
    let options = EncodeOptions::try_new()?
        .compression_level(LEVEL)
        .frame_size_policy(FrameSizePolicy::Uncompressed(FRAME_BYTES));
    let mut encoder = options.into_encoder(&mut out).ok()?;
    let mut consumed = 0;
    for end in breaks.iter().copied().chain([bytes.len()]) {
        if end <= consumed || end > bytes.len() {
            continue;
        }
        while consumed < end {
            match encoder.compress(bytes.get(consumed..end)?) {
                // A compress call that consumes nothing would spin forever; treat it as a failed encode.
                Ok(0) => return None,
                Ok(progress) => consumed += progress,
                Err(_) => return None,
            }
        }
        // `finish` ends the last frame itself; ending it here too would log an empty frame behind it.
        if end < bytes.len() {
            encoder.end_frame().ok()?;
        }
    }
    encoder.finish().ok()?;
    Some(out)
}

/// Reads the seek table of a stored form [`compress`] produced. Callers hold on to the result: it answers which frame
/// covers a decompressed offset, and every read needs that answer.
pub fn seek_table(stored: &[u8]) -> Result<SeekTable, FormatError> {
    SeekTable::from_seekable(&mut BytesWrapper::new(stored))
        .map_err(|_| failed("seekable zstd seek table is unreadable"))
}

/// The decompressed byte range `[start, end)` of a stored form, decompressing only the frames that cover it.
pub fn decompress_range(stored: &[u8], table: &SeekTable, start: u64, end: u64) -> Result<Vec<u8>, FormatError> {
    let len = end
        .checked_sub(start)
        .filter(|len| *len <= MAX_RANGE_BYTES)
        .ok_or_else(|| failed("seekable zstd range is inverted or exceeds the per-call ceiling"))?;
    if end > table.size_decomp() {
        return Err(failed("seekable zstd range is out of the seek table's bounds"));
    }
    let mut plain = Vec::with_capacity(len as usize);
    if len == 0 {
        return Ok(plain);
    }
    let mut frame = Vec::new();
    for index in table.frame_index_decomp(start)..=table.frame_index_decomp(end - 1) {
        let frame_start = table
            .frame_start_decomp(index)
            .map_err(|_| failed("seekable zstd frame index is outside the seek table"))?;
        decompress_frame(stored, table, index, &mut frame)?;
        let from = start.saturating_sub(frame_start) as usize;
        let to = ((end - frame_start) as usize).min(frame.len());
        plain.extend_from_slice(
            frame
                .get(from..to.max(from))
                .ok_or_else(|| failed("seekable zstd frame is shorter than its seek table entry"))?,
        );
    }
    // A stream that stops short of the range its seek table promised would otherwise be served zero-padded.
    if plain.len() != len as usize {
        return Err(failed(
            "seekable zstd range decompressed short of its seek table's length",
        ));
    }
    Ok(plain)
}

/// The whole decompressed arena of a stored form, for a caller that wants all of it rather than one range: one decode
/// call straight into a buffer of exactly the size the seek table reports.
pub fn decompress_all(stored: &[u8]) -> Result<Vec<u8>, FormatError> {
    let mut plain = Vec::new();
    decompress_all_into(stored, &mut plain)?;
    Ok(plain)
}

/// [`decompress_all`] into a buffer the caller keeps: `plain` is emptied, given exactly the room the seek table
/// reports when it has less, and filled by one decode call. A scan that inflates block after block hands the same
/// buffer in each time and allocates only when a block is wider than any before it. Refuses, like every read here, a
/// stream that inflates to anything other than what its seek table promises.
pub fn decompress_all_into(stored: &[u8], plain: &mut Vec<u8>) -> Result<(), FormatError> {
    let table = seek_table(stored)?;
    inflate_exact(stored, table.size_decomp(), plain)
}

/// A stored form read one range at a time: only the frames a requested byte range falls in are decompressed, and each
/// one at most once, so reaching a single value leaves the rest of the arena compressed. A caller walking the form
/// front to back through [`Window::read_forward`] keeps only one frame inflated at a time.
pub struct Window<'a> {
    frames: Vec<Option<Vec<u8>>>,
    /// Frames below this index were passed by a forward walk and let go.
    released: u32,
    /// A frame buffer let go of and kept for the next frame inflated, so a walk reuses one allocation.
    spare: Vec<u8>,
    stored: &'a [u8],
    table: SeekTable,
}

impl<'a> Window<'a> {
    /// Opens the stored form, reading its seek table but decompressing nothing.
    pub fn open(stored: &'a [u8]) -> Result<Self, FormatError> {
        let table = seek_table(stored)?;
        let frames = vec![None; table.num_frames() as usize];
        let spare = SPARE_FRAME.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
        Ok(Self {
            frames,
            released: 0,
            spare,
            stored,
            table,
        })
    }

    /// How much plaintext the whole stored form holds, from the seek table alone.
    pub fn plain_len(&self) -> usize {
        self.table.size_decomp() as usize
    }

    /// How much plaintext the first frame holds — the most a reader of the form's head can take without inflating a
    /// second frame. Zero for a form with no frames.
    pub fn first_frame_len(&self) -> usize {
        self.table.frame_end_decomp(0).map_or(0, |end| end as usize)
    }

    /// How many of the stored form's frames are inflated right now — the measure of what a read left alone, and of
    /// what a forward walk let go.
    #[cfg(test)]
    pub(crate) fn decompressed_frames(&self) -> usize {
        self.frames.iter().filter(|frame| frame.is_some()).count()
    }

    /// Plaintext bytes `[start, start + len)`, decompressing only the frames they fall in.
    pub fn read(&mut self, start: usize, len: usize) -> Result<Vec<u8>, FormatError> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let (first, last, end) = self.span(start, len)?;
        let mut plain = Vec::with_capacity(len);
        for index in first..=last {
            let frame_start = self.frame_start(index)?;
            let frame = self.frame(index)?;
            let from = start.saturating_sub(frame_start);
            let to = (end - frame_start).min(frame.len());
            plain.extend_from_slice(
                frame
                    .get(from..to.max(from))
                    .ok_or_else(|| failed("seekable zstd frame is shorter than its seek table entry"))?,
            );
        }
        if plain.len() != len {
            return Err(failed("seekable zstd window range decompressed short of its length"));
        }
        Ok(plain)
    }

    /// Plaintext bytes `[start, start + len)` for a caller walking the stored form front to back. Every frame before
    /// the one holding `start` is let go, so the walk keeps one frame inflated at a time; a range inside one frame is
    /// borrowed from it, and one straddling a frame boundary is gathered into a copy, which inflates the next frame
    /// early. Reading behind the walk afterwards still works, at the cost of inflating those frames again.
    pub fn read_forward(&mut self, start: usize, len: usize) -> Result<Cow<'_, [u8]>, FormatError> {
        if len == 0 {
            return Ok(Cow::Borrowed(&[]));
        }
        let (first, last, end) = self.span(start, len)?;
        while self.released < first {
            self.release(self.released);
            self.released += 1;
        }
        if first != last {
            return self.read(start, len).map(Cow::Owned);
        }
        let frame_start = self.frame_start(first)?;
        let frame = self.frame(first)?;
        frame
            .get(start - frame_start..end - frame_start)
            .map(Cow::Borrowed)
            .ok_or_else(|| failed("seekable zstd frame is shorter than its seek table entry"))
    }

    /// The first and last frame a range falls in, and the range's end, once the range is known to lie inside the
    /// stored form's plaintext.
    fn span(&self, start: usize, len: usize) -> Result<(u32, u32, usize), FormatError> {
        let end = start
            .checked_add(len)
            .filter(|end| *end <= self.plain_len())
            .ok_or(FormatError::Truncated {
                what: "seekable zstd window range",
            })?;
        let first = self.table.frame_index_decomp(start as u64);
        let last = self.table.frame_index_decomp(end as u64 - 1);
        Ok((first, last, end))
    }

    fn frame_start(&self, index: u32) -> Result<usize, FormatError> {
        self.table
            .frame_start_decomp(index)
            .map(|start| start as usize)
            .map_err(|_| failed("seekable zstd frame index is outside the seek table"))
    }

    /// Frame `index`'s plaintext, inflated into the spare buffer on first use and kept until let go.
    fn frame(&mut self, index: u32) -> Result<&[u8], FormatError> {
        let outside = || failed("seekable zstd frame index is outside the seek table");
        let slot = usize::try_from(index).unwrap_or(usize::MAX);
        if self.frames.get(slot).is_none_or(Option::is_none) {
            let mut buffer = std::mem::take(&mut self.spare);
            decompress_frame(self.stored, &self.table, index, &mut buffer)?;
            *self.frames.get_mut(slot).ok_or_else(outside)? = Some(buffer);
        }
        self.frames.get(slot).and_then(Option::as_deref).ok_or_else(outside)
    }

    /// Lets frame `index`'s plaintext go, keeping its buffer as the spare when there is none.
    fn release(&mut self, index: u32) {
        let slot = usize::try_from(index).unwrap_or(usize::MAX);
        if let Some(buffer) = self.frames.get_mut(slot).and_then(Option::take)
            && self.spare.capacity() == 0
        {
            self.spare = buffer;
        }
    }
}

impl Drop for Window<'_> {
    /// Hands one frame buffer back to the thread for the next window, unless it grew past a frame's size.
    fn drop(&mut self) {
        let spare = std::mem::take(&mut self.spare);
        let buffer = if spare.capacity() > 0 {
            spare
        } else {
            self.frames.iter_mut().find_map(Option::take).unwrap_or_default()
        };
        if buffer.capacity() <= FRAME_BYTES as usize {
            SPARE_FRAME.with(|slot| *slot.borrow_mut() = buffer);
        }
    }
}

#[cfg(test)]
#[path = "test/seekable_zstd.rs"]
mod tests;
