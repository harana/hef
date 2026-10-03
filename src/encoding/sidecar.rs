//! Tiny keys stored beside a block's compressed string values so a filter can rule most rows in or out without
//! decompressing them.
//!
//! An FSST-compressed value tells a reader nothing about the text it stands for: the codes do not sort like the
//! strings, and a byte of a search term need not appear among them at all. These two keys put back just enough
//! plaintext to decide most rows anyway — the first few bytes of the value, which order it against a comparison
//! value, and a one-word summary of which bytes it holds, which rules a substring out outright. Each is exact when it
//! answers and simply declines when it cannot, so the real text is fetched only for the rows that genuinely tie.
//!
//! See: hef-encodings-and-compression/spec.md

use super::constant::{PREFIX_KEY_BYTES, PREFIX_KEY_PREFIX_LEN};
use std::cmp::Ordering;

/// The first few bytes of a string value, kept in the clear so a filter can order that value against a comparison
/// value without decompressing it.
///
/// Seven prefix bytes plus a length byte, eight in all, so a block's keys are one flat run a filter walks straight
/// through. Comparing two keys gives the same verdict as comparing the values themselves whenever it gives one at
/// all; it declines only for two values that agree on all seven bytes and both run longer, and the caller settles
/// those from the real text.
///
/// See: hef-encodings-and-compression/spec.md
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct PrefixKey {
    bytes: [u8; PREFIX_KEY_PREFIX_LEN],
    /// The value's length, capped at the prefix width — so a length below that width means the key holds the whole
    /// value, which is what lets two keys that agree on their bytes still be ordered.
    len: u8,
}

const _: () = assert!(
    size_of::<PrefixKey>() == PREFIX_KEY_BYTES,
    "a prefix key must be exactly its stored width, so a block's keys stay a flat run with nothing between them"
);

impl PrefixKey {
    /// The key for a value.
    pub fn of(value: &str) -> Self {
        let mut bytes = [0u8; PREFIX_KEY_PREFIX_LEN];
        for (slot, byte) in bytes.iter_mut().zip(value.bytes()) {
            *slot = byte;
        }
        Self {
            bytes,
            len: value.len().min(PREFIX_KEY_PREFIX_LEN) as u8,
        }
    }

    /// The key read back from the bytes a block stores, or `None` when the length byte runs past the prefix width —
    /// a key no writer produces, so a block claiming one is refused rather than trusted.
    pub fn from_stored(stored: [u8; PREFIX_KEY_BYTES]) -> Option<Self> {
        let (&len, head) = stored.split_last()?;
        if usize::from(len) > PREFIX_KEY_PREFIX_LEN {
            return None;
        }
        Some(Self {
            bytes: head.try_into().ok()?,
            len,
        })
    }

    /// The key's eight stored bytes.
    pub fn to_stored(self) -> [u8; PREFIX_KEY_BYTES] {
        let mut stored = [0u8; PREFIX_KEY_BYTES];
        for (slot, byte) in stored.iter_mut().zip(self.bytes) {
            *slot = byte;
        }
        if let Some(last) = stored.last_mut() {
            *last = self.len;
        }
        stored
    }

    /// How this key's value orders against `other`'s, or `None` when the keys cannot tell.
    ///
    /// The prefix bytes are padded with zeroes, which sort below every real byte, so any difference between two
    /// padded prefixes is a difference the values themselves have in the same direction. Prefixes that match leave
    /// the shorter value first, since it is then a prefix of the longer one. Only two values that both fill the key
    /// and agree on every byte of it are undecidable — the caller compares their real text.
    pub fn compare(self, other: Self) -> Option<Ordering> {
        match (self.bytes, self.len).cmp(&(other.bytes, other.len)) {
            Ordering::Equal if usize::from(self.len) == PREFIX_KEY_PREFIX_LEN => None,
            ordering => Some(ordering),
        }
    }
}

/// Which byte values a string holds, folded into 32 buckets — a one-word summary that rules a substring out.
///
/// Every byte of the string sets the bit its low five bits name, so the summary carries a bit for each bucket the
/// string's bytes fall in. A substring can only occur inside a string whose summary has every bit the substring's
/// summary has, so a single `AND` rejects a candidate outright; one that passes still has to be checked for real.
/// Folding on the low five bits also makes the test agree with an ASCII-case-insensitive search for free: `a` and `A`
/// differ by exactly 32, so they share a bucket.
///
/// See: hef-encodings-and-compression/spec.md
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StringFingerprint(u32);

impl StringFingerprint {
    /// Bytes one fingerprint occupies in a block.
    pub const STORED_BYTES: usize = 4;

    /// The fingerprint of some text.
    pub fn of(text: &[u8]) -> Self {
        Self(text.iter().fold(0u32, |bits, byte| bits | 1u32 << (byte & 31)))
    }

    /// The fingerprint read back from a block.
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    /// Whether text summarised by this fingerprint could hold text summarised by `needle`. `false` is proof it cannot;
    /// `true` means the text still has to be searched.
    pub fn might_contain(self, needle: Self) -> bool {
        self.0 & needle.0 == needle.0
    }

    /// The fingerprint's stored bits.
    pub fn to_bits(self) -> u32 {
        self.0
    }

    /// Every bucket either fingerprint has — what a search for several substrings at once needs, since a match has to
    /// hold all of them.
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

#[cfg(test)]
#[path = "test/sidecar.rs"]
mod tests;
