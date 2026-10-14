//! A compact, reusable per-mini-block directory: how many fixed-size plaintext chunks a compressed stream holds, how
//! many compressed bytes each one occupies, and the total plaintext length they inflate to — the one lookup a codec
//! needs to seek straight to a single mini-block's compressed bytes without touching its neighbours.
//!
//! On-disk shape: `block_count:u32`, `plain_len:u32`, one `compressed_len:u32` per block, then the concatenated
//! compressed blocks follow (written by the caller). [`crate::encoding::deflate`] is this shape's first user,
//! one entry per independently inflatable granule.
//!
//! Not every random-access codec needs this directory. It earns its keep only when a codec's mini-blocks are
//! fixed-size before compression and variable-size after, so a reader cannot compute a block's byte range by
//! arithmetic alone. ALP's FastLanes vectors share one bit width for the whole stream, so a vector's byte range is
//! pure arithmetic (see `decode_alp_range` in `crate::encoding`); FSST's compressed values are addressed by
//! their own per-value offset table — one entry per value, not per fixed-size block (see
//! `decode_fsst_string_range`). Reach for this directory when a future codec needs the shape deflate already has;
//! don't force it onto a codec whose own addressing is already exact and cheaper.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};

/// A decoded mini-block directory: where the compressed blocks begin, how long each one runs, and the total
/// plaintext length they inflate to.
pub struct MiniBlockDirectory {
    /// Absolute offset of each block's first compressed byte; a block's length is the gap to the next start (or to
    /// `body_end` for the last block), so any block's range is one indexed lookup.
    block_starts: Vec<usize>,
    /// Absolute offset one past the last block's compressed bytes; equals `body_start` when there are no blocks.
    body_end: usize,
    body_start: usize,
    plain_capacity_hint: usize,
    plain_len: usize,
}

impl MiniBlockDirectory {
    /// How many mini-blocks the directory covers.
    pub fn block_count(&self) -> usize {
        self.block_starts.len()
    }

    /// The byte offset, within the buffer [`decode`] was called on, where the first block's compressed bytes begin.
    pub fn body_start(&self) -> usize {
        self.body_start
    }

    /// A capacity hint for allocating the full plaintext buffer, bounded by the bytes [`decode`] actually saw.
    pub fn plain_capacity_hint(&self) -> usize {
        self.plain_capacity_hint
    }

    /// The total plaintext length every block inflates to, combined.
    pub fn plain_len(&self) -> usize {
        self.plain_len
    }

    /// The absolute byte range of block `index`'s compressed bytes within the buffer [`decode`] was called on.
    pub fn byte_range(&self, index: usize) -> Result<(usize, usize), FormatError> {
        let start = *self.block_starts.get(index).ok_or(FormatError::RefOutOfRange {
            what: "mini-block directory index",
        })?;
        let end = self.block_starts.get(index + 1).copied().unwrap_or(self.body_end);
        Ok((start, end - start))
    }

    /// Every block's absolute byte range, in block order: the linear-time path for a full sequential decode.
    pub fn ranges(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.ranges_from(0)
    }

    /// The absolute byte ranges of blocks `first..`, in block order, without walking the blocks before `first`.
    /// Yields nothing when `first` is at or past [`Self::block_count`].
    pub fn ranges_from(&self, first: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
        let starts = self.block_starts.get(first..).unwrap_or(&[]);
        let ends = starts.iter().skip(1).copied().chain(std::iter::once(self.body_end));
        starts.iter().zip(ends).map(|(&start, end)| (start, end - start))
    }
}

/// Encodes a mini-block directory: `block_count:u32`, `plain_len:u32`, one `compressed_len:u32` per block. The
/// caller appends the concatenated compressed blocks after this header.
pub fn encode(plain_len: usize, block_lens: &[usize]) -> Vec<u8> {
    let mut out = Writer::with_capacity(8 + block_lens.len() * 4);
    out.put_u32(block_lens.len() as u32);
    out.put_u32(plain_len as u32);
    for len in block_lens {
        out.put_u32(*len as u32);
    }
    out.into_bytes()
}

/// Decodes a mini-block directory written by [`encode`] from the front of `bytes`.
pub fn decode(bytes: &[u8]) -> Result<MiniBlockDirectory, FormatError> {
    let mut reader = Reader::new(bytes);
    let block_count = reader.u32("mini-block count")? as usize;
    let plain_len = reader.u32("mini-block plain length")? as usize;
    let plain_capacity_hint = reader.capacity_hint(plain_len, 1);
    let mut block_starts = Vec::with_capacity(reader.capacity_hint(block_count, 4));
    let len_table = reader.take(block_count.saturating_mul(4), "mini-block compressed length")?;
    let body_start = reader.position();
    let mut offset = body_start;
    for len in len_table.chunks_exact(4) {
        block_starts.push(offset);
        let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
        offset = offset.checked_add(len).ok_or(FormatError::Structural {
            rule: "mini-block compressed lengths overflow the address space",
        })?;
    }
    Ok(MiniBlockDirectory {
        block_starts,
        body_end: offset,
        body_start,
        plain_capacity_hint,
        plain_len,
    })
}

#[cfg(test)]
#[path = "test/mini_block_directory.rs"]
mod tests;
