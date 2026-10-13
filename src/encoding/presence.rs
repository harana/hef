//! Counts and locates the rows that carry a value, over a column block's packed presence bitmap.
//!
//! A column block stores one value per present row and a bitmap saying which rows those are (bit `n` of byte `n / 8`,
//! lowest bit first). Reading a row back means asking two questions of that bitmap: how many rows before this one
//! carry a value — which is where the value sits in the dense stream — and whether this row carries one at all. Every
//! answer here is exact: identical to testing the bits one at a time, only counted a machine word at a time instead.
//!
//! This is the one place packed presence bits are counted. The writer, the block decoder, and the file reader all
//! come through here, so a bitmap means the same thing on both sides of a file.
//!
//! See: hef-encodings-and-compression/spec.md

/// Counts the set bits in the first `len` bit positions of a packed bitmap.
///
/// Whole 64-bit words are popcounted at a time, then the leftover bytes, then the partial tail byte masked to the
/// bits `len` actually names — so bits past `len` never count, whatever the byte holds. A `bitmap` shorter than `len`
/// bits treats every missing bit as unset.
pub(crate) fn count_set_bits(bitmap: &[u8], len: usize) -> usize {
    let full_bytes = (len / 8).min(bitmap.len());
    let mut words = bitmap.get(..full_bytes).unwrap_or(bitmap).chunks_exact(8);
    let mut count: usize = words
        .by_ref()
        .map(|word| u64::from_le_bytes(word.try_into().unwrap_or([0; 8])).count_ones() as usize)
        .sum();
    count += words
        .remainder()
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum::<usize>();
    let tail_bits = len % 8;
    if tail_bits > 0
        && let Some(&byte) = bitmap.get(full_bytes)
    {
        let mask = (1u8 << tail_bits) - 1;
        count += (byte & mask).count_ones() as usize;
    }
    count
}

/// Counts how many non-null rows appear before `row_idx` in the null bitmap. A `row_idx` past the bitmap counts every
/// row it holds.
pub(crate) fn count_present_before(bitmap: &[u8], row_idx: usize) -> usize {
    count_set_bits(bitmap, row_idx)
}

/// The dense position of `row` among the present rows, or `None` when the row carries no value. One pass over the
/// prefix answers presence and position together, where two prefix counts walk it twice.
pub(crate) fn present_position(bitmap: &[u8], row: usize) -> Option<usize> {
    let byte = *bitmap.get(row / 8)?;
    let bit = 1u8 << (row % 8);
    (byte & bit != 0).then(|| count_present_before(bitmap, row))
}

/// Answers "does this row carry a value, and where does that value sit" for a presence bitmap probed many times over.
///
/// Built in one pass, it holds the running present-count at each 64-bit word of the bitmap, so a probe costs one table
/// read plus a popcount of the bits ahead of the row in its own word. Counting the prefix per probe instead costs the
/// whole prefix each time, which over a granule's rows is quadratic in the row count.
///
/// See: hef-encodings-and-compression/spec.md
#[derive(Debug)]
pub(crate) struct PresenceRank {
    /// One entry per eight bitmap bytes; the last word may be partial.
    present_before_word: Vec<u32>,
}

impl PresenceRank {
    /// Indexes `bitmap`, which every later probe must pass back in.
    pub(crate) fn new(bitmap: &[u8]) -> Self {
        let mut present_before_word = Vec::with_capacity(bitmap.len().div_ceil(8));
        let mut running = 0u32;
        for word in bitmap.chunks(8) {
            present_before_word.push(running);
            running += load_word(word).count_ones();
        }
        Self { present_before_word }
    }

    /// The dense position of `row` among the present rows of `bitmap` — the bitmap this rank was built over — or
    /// `None` when the row carries no value.
    pub(crate) fn position(&self, bitmap: &[u8], row: usize) -> Option<usize> {
        let byte = *bitmap.get(row / 8)?;
        if byte & (1u8 << (row % 8)) == 0 {
            return None;
        }
        let word_index = row / 64;
        let before_word = *self.present_before_word.get(word_index)? as usize;
        let start = word_index * 8;
        let word = load_word(bitmap.get(start..(start + 8).min(bitmap.len()))?);
        let below_row = (1u64 << (row % 64)) - 1;
        Some(before_word + (word & below_row).count_ones() as usize)
    }
}

/// Reads up to eight bitmap bytes as one little-endian word, so bit `n` of the word is bit `n % 8` of byte `n / 8`; a
/// short final chunk is padded with unset bits.
fn load_word(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    word[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(word)
}

#[cfg(test)]
#[path = "test/presence.rs"]
mod tests;
