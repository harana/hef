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

use multiversion::multiversion;

/// Counts the set bits in the first `len` bit positions of a packed bitmap.
///
/// The whole bytes are counted in bulk (see [`count_ones`]), then the partial tail byte is masked to the bits `len`
/// actually names, so bits past `len` never count, whatever the byte holds. A `bitmap` shorter than `len` bits treats
/// every missing bit as unset.
pub(crate) fn count_set_bits(bitmap: &[u8], len: usize) -> usize {
    let full_bytes = (len / 8).min(bitmap.len());
    let mut count = count_ones(bitmap.get(..full_bytes).unwrap_or(bitmap));
    let tail_bits = len % 8;
    if tail_bits > 0
        && let Some(&byte) = bitmap.get(full_bytes)
    {
        let mask = (1u8 << tail_bits) - 1;
        count += (byte & mask).count_ones() as usize;
    }
    count
}

/// Counts every set bit in `bytes`.
///
/// Takes 32 bytes at a time as four independent 64-bit word counts, which lets the compiler turn the loop into vector
/// popcounts. It is compiled once per instruction set and the best one for the running CPU is picked at run time; the
/// portable build is the plain word loop. Leftover whole words, then leftover bytes, finish the count.
#[multiversion(targets(
    "x86_64+avx512f+avx512bw+avx512dq+avx512vl+avx512vpopcntdq",
    "x86_64+avx512f+avx512bw+avx512dq+avx512vl",
    "x86_64+avx2",
    "aarch64+neon"
))]
fn count_ones(bytes: &[u8]) -> usize {
    let mut blocks = bytes.chunks_exact(32);
    let mut lanes = [0u64; 4];
    for block in blocks.by_ref() {
        for (lane, word) in lanes.iter_mut().zip(block.chunks_exact(8)) {
            *lane += u64::from(u64::from_le_bytes(word.try_into().unwrap_or([0; 8])).count_ones());
        }
    }
    let mut words = blocks.remainder().chunks_exact(8);
    let mut count: u64 = lanes.iter().sum();
    count += words
        .by_ref()
        .map(|word| u64::from(u64::from_le_bytes(word.try_into().unwrap_or([0; 8])).count_ones()))
        .sum::<u64>();
    count += words
        .remainder()
        .iter()
        .map(|byte| u64::from(byte.count_ones()))
        .sum::<u64>();
    count as usize
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
        Self {
            present_before_word: present_before_each_word(bitmap),
        }
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

/// The running present-count before each 64-bit word of `bitmap`, one entry per eight bytes with a partial last word
/// getting an entry too. Built per instruction set like [`count_ones`], so each word costs a hardware popcount.
#[multiversion(targets(
    "x86_64+avx512f+avx512bw+avx512dq+avx512vl+avx512vpopcntdq",
    "x86_64+avx512f+avx512bw+avx512dq+avx512vl",
    "x86_64+avx2",
    "aarch64+neon"
))]
fn present_before_each_word(bitmap: &[u8]) -> Vec<u32> {
    let mut words = bitmap.chunks_exact(8);
    let mut running = 0u32;
    let mut present_before_word: Vec<u32> = words
        .by_ref()
        .map(|word| {
            let before = running;
            running += u64::from_le_bytes(word.try_into().unwrap_or([0; 8])).count_ones();
            before
        })
        .collect();
    if !words.remainder().is_empty() {
        present_before_word.push(running);
    }
    present_before_word
}

/// Reads up to eight bitmap bytes as one little-endian word, so bit `n` of the word is bit `n % 8` of byte `n / 8`; a
/// short final chunk is padded with unset bits.
fn load_word(bytes: &[u8]) -> u64 {
    let mut word = [0u8; 8];
    for (slot, byte) in word.iter_mut().zip(bytes) {
        *slot = *byte;
    }
    u64::from_le_bytes(word)
}

#[cfg(test)]
#[path = "test/presence.rs"]
mod tests;
