//! A token-membership filter: answers "could this block contain the token T?" without ever wrongly saying "no".
//!
//! For a schema-declared free-text or searchable string field, the writer tokenizes the field values, hashes each
//! token, and builds a [`TextTokenIndex`] over the token hashes. At query time, the planner hashes the query token the
//! same way and probes the filter; a `false` answer guarantees the block holds no row whose field contains the token; a
//! `true` answer is a candidate — the block *might* contain it, and the exact check over the materialized text is kept
//! above the scan.
//!
//! The on-disk representation is a [`SplitBlockBloomFilter`] over the token hashes, with a narrow n-gram extension:
//! when the field is declared for substring matching, the writer also stores hashes of every n-gram of the declared
//! length, so the filter can prune `CONTAINS` predicates that would otherwise require a full-text scan. The exactness
//! is always [`Exactness::InexactNoFalseNegative`] — a token that is absent can produce a false positive, but a token
//! that is present is never missed.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::indexes::probabilistic::SplitBlockBloomFilter;
use crate::indexes::{Exactness, stable_hash, stable_hash_ascii_lowercase};
use hashbrown::HashSet;
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::VecDeque;

/// Format tag at the front of an encoded [`TextTokenIndex`].
const TEXT_TOKEN_MAGIC: u32 = 0x5454_4b31; // "TTK1"

/// Bits allocated per token hash when sizing the backing Bloom filter. Eight bits per token gives roughly a 2%
/// false-positive rate, which is tight enough to prune well while keeping the per-granule metadata small.
const BITS_PER_TOKEN: u32 = 8;

/// The encoded size of an index's fixed header: the magic tag and the flags word, each a `u32`.
const TEXT_TOKEN_HEADER_BYTES: usize = 8;

/// The n-gram length used when the field is declared for substring matching. Trigrams are the practical minimum: they
/// resolve most substrings while keeping the token count manageable.
const NGRAM_LEN: usize = 3;

/// Bits in the code of one case-folded ASCII trigram: three seven-bit bytes, most recent byte lowest.
const TRIGRAM_CODE_BITS: u32 = 21;

/// Keeps a rolling trigram code to its three most recent bytes.
const TRIGRAM_CODE_MASK: usize = (1 << TRIGRAM_CODE_BITS) - 1;

/// Words in the bitmap that holds one bit per trigram code: 2^21 bits, 256 KiB.
const TRIGRAM_BITMAP_WORDS: usize = 1 << (TRIGRAM_CODE_BITS - 6);

/// A build whose token-hash set grew past this many slots (8 MiB of table) hands the set back to the allocator instead
/// of keeping it for the thread's next block, so one block of unusually many distinct tokens does not pin that memory
/// for the life of the worker.
const MAX_RETAINED_TOKEN_HASHES: usize = 1 << 20;

thread_local! {
    /// This thread's index-build scratch, absent while a build holds it.
    static BUILD_SCRATCH: RefCell<Option<BuildScratch>> = const { RefCell::new(None) };
}

/// A compact membership filter over the tokens that appear in a block of free-text or searchable string field values.
///
/// Built by tokenizing (splitting on whitespace and punctuation boundaries) the field values across all rows in a
/// granule or page and storing their hashes in a split-block Bloom filter. An optional n-gram layer extends the filter
/// to support `CONTAINS` predicates: when `has_ngrams` is set, hashes of every trigram of every raw (case-folded)
/// field value were also inserted — not just trigrams within a single token — so a substring lookup can prune blocks
/// that cannot contain the trigrams of the search string, including substrings that span a token boundary.
///
/// Both layers share the same backing filter so the on-disk cost is one Bloom filter per granule plus one bit of
/// metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTokenIndex {
    filter: SplitBlockBloomFilter,
    /// Whether trigram hashes were also inserted, enabling `CONTAINS` pruning.
    has_ngrams: bool,
}

impl TextTokenIndex {
    /// Builds a token index over `token_hashes` (one `u64` per token).
    ///
    /// Pass `has_ngrams = true` when the field is declared for substring matching and you have already mixed the n-gram
    /// hashes into `token_hashes`; pass `false` for whole-token equality/membership lookups only. The result is a
    /// deterministic function of the hash multiset and the flag, so any node building over the same rows produces
    /// byte-identical bits.
    pub fn build(token_hashes: &[u64], has_ngrams: bool) -> Self {
        TextTokenIndex {
            filter: SplitBlockBloomFilter::build(token_hashes, BITS_PER_TOKEN),
            has_ngrams,
        }
    }

    /// Builds a token index directly from text values, tokenizing and hashing each one. Pass `include_ngrams = true` to
    /// also insert trigram hashes for `CONTAINS` support.
    ///
    /// The hashes are deduplicated before the filter is sized: membership is a set question, so a token repeated
    /// across many rows sets the same bits either way, and sizing by occurrences would only inflate the stored filter
    /// without lowering its false-positive rate.
    pub fn build_from_values(values: &[&str], include_ngrams: bool) -> Self {
        Self::build_from_iter(values.iter().copied(), include_ngrams)
    }

    /// Iterator form of [`TextTokenIndex::build_from_values`]. It lets a column encoder hand present values straight
    /// to the index without first allocating a parallel `Vec<&str>`.
    ///
    /// The keys are deduplicated in this thread's [`BuildScratch`] before anything is hashed for the filter, so a
    /// trigram that a granule of free text repeats thousands of times is hashed once, and the scratch is kept for the
    /// next block the thread builds rather than allocated afresh each time.
    pub fn build_from_iter<'a>(values: impl IntoIterator<Item = &'a str>, include_ngrams: bool) -> Self {
        let mut scratch = BUILD_SCRATCH
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_else(BuildScratch::new);
        for text in values {
            scratch.insert_value(text, include_ngrams);
        }
        let filter = scratch.build_filter();
        if scratch.other_hashes.capacity() <= MAX_RETAINED_TOKEN_HASHES {
            BUILD_SCRATCH.with(|slot| *slot.borrow_mut() = Some(scratch));
        }
        TextTokenIndex {
            filter,
            has_ngrams: include_ngrams,
        }
    }

    /// Reports whether this block might contain `token`. A `false` answer is certain — no row whose field contains this
    /// token is present. A `true` answer may be a false positive; the caller must confirm with an exact text predicate
    /// over the materialized values.
    pub fn might_contain_token(&self, token: &str) -> bool {
        // Case-fold the query the same way the index folds tokens at build time, so lookup is case-insensitive.
        self.filter.contains(hash_folded_token(token))
    }

    /// Reports whether this block might contain the token whose hash is `token_hash` (the caller has already folded
    /// and hashed it with [`hash_folded_token`]). Lets a caller that probes the same fixed query token against many
    /// blocks — one per granule or page — fold and hash it once instead of on every probe.
    pub fn might_contain_token_hash(&self, token_hash: u64) -> bool {
        self.filter.contains(token_hash)
    }

    /// Reports whether this block might contain the substring `needle`. Returns `false` (certain miss) only when
    /// `has_ngrams` is set and at least one trigram of `needle` is absent from the filter. When `has_ngrams` is `false`
    /// the index has no substring information, so this conservatively returns `true` (keep the block for exact
    /// checking).
    pub fn might_contain_substring(&self, needle: &str) -> bool {
        if !self.has_ngrams {
            return true;
        }
        if needle.is_ascii() {
            return needle
                .as_bytes()
                .windows(NGRAM_LEN)
                .all(|ngram| self.filter.contains(stable_hash_ascii_lowercase(ngram)));
        }
        // Case-fold the needle so its trigrams hash identically to the ones inserted at build time.
        let needle = fold_for_ngrams(needle);
        for ngram in ngrams(&needle, NGRAM_LEN) {
            if !self.filter.contains(hash_token(ngram)) {
                return false;
            }
        }
        true
    }

    /// Whether trigram hashes were inserted alongside whole-token hashes, enabling `CONTAINS` / substring pruning.
    pub fn has_ngrams(&self) -> bool {
        self.has_ngrams
    }

    /// How trustworthy this index is for pruning: always [`Exactness::InexactNoFalseNegative`] — it never drops a block
    /// that contains the queried token, but may keep blocks that don't.
    pub fn exactness(&self) -> Exactness {
        Exactness::InexactNoFalseNegative
    }

    /// Serializes the index to bytes (magic, flags, then the Bloom filter bytes). Pairs with
    /// [`TextTokenIndex::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let filter_bytes = self.filter.encode();
        let mut out = Writer::with_capacity(TEXT_TOKEN_HEADER_BYTES + filter_bytes.len());
        out.put_u32(TEXT_TOKEN_MAGIC);
        out.put_u32(if self.has_ngrams { 1 } else { 0 });
        out.put_slice(&filter_bytes);
        out.into_bytes()
    }

    /// Rebuilds an index from [`TextTokenIndex::encode`] output. Refuses on a wrong tag or malformed Bloom bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("text token magic")? != TEXT_TOKEN_MAGIC {
            return Err(FormatError::Structural {
                rule: "text token index bad magic",
            });
        }
        let flags = reader.u32("text token flags")?;
        if flags & !1 != 0 {
            return Err(FormatError::ReservedNotZero {
                field: "TextTokenIndex.flags",
            });
        }
        let has_ngrams = flags & 1 != 0;
        let remaining = reader.remaining();
        let rest = reader.take(remaining, "text token filter bytes")?;
        let filter = SplitBlockBloomFilter::decode(rest)?;
        Ok(TextTokenIndex { filter, has_ngrams })
    }
}

/// Splits `text` into tokens at whitespace and ASCII punctuation boundaries, yielding each non-empty piece lowercased.
/// Case folding uses full-Unicode `to_lowercase`, which is deterministic and locale-independent, so indexing "Foo" and
/// querying "foo" match and every node that indexes the same bytes produces the same token set. A token that is
/// already folded is handed back as it lies, which is what keeps indexing a file of ordinary lowercase text free of a
/// copy per token.
pub fn tokenize(text: &str) -> impl Iterator<Item = Cow<'_, str>> + '_ {
    split_tokens(text).map(|token| match is_already_folded(token) {
        true => Cow::Borrowed(token),
        false => Cow::Owned(token.to_lowercase()),
    })
}

/// Splits `text` at the same boundaries as [`tokenize`] but hands back borrowed, still-cased pieces, so a caller that
/// only needs each token's hash never pays for a lowercased copy even when the token does need folding.
fn split_tokens(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || (c.is_ascii_punctuation() && c != '\''))
        .filter(|s| !s.is_empty())
}

/// [`split_tokens`] for a value already known to be ASCII, where every character is one byte and the separator test
/// needs no character decoding.
fn split_ascii_tokens(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    text.split(|&byte| is_ascii_token_separator(byte))
        .filter(|token| !token.is_empty())
}

/// Whether an ASCII byte ends a token: exactly the bytes [`split_tokens`]' character predicate accepts — the six
/// ASCII characters with the Unicode white-space property, and ASCII punctuation other than the apostrophe.
fn is_ascii_token_separator(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') || (byte.is_ascii_punctuation() && byte != b'\'')
}

/// The keys one index build has seen so far, deduplicated before they are hashed into the filter.
///
/// Nearly every key is a three-byte window of ASCII text, and a granule of free text repeats each such window many
/// times over. Those keys are tracked as one bit each in a bitmap over the case-folded seven-bit trigram space, so
/// seeing a window costs a shift and an OR and only the distinct trigrams are ever hashed. Every other key — a token
/// of some other length, or a trigram of a value that left ASCII — is hashed and kept in a set, as before.
///
/// A three-byte ASCII token is the same key as the trigram of the same bytes: both hash through
/// [`stable_hash_ascii_lowercase`], so the previous set-of-hashes build counted them once. They share the bitmap so the
/// count still comes out the same. The filter the two builds produce is identical unless two distinct keys share a
/// 64-bit hash: within the bitmap that cannot happen (the three-byte polynomial fold is injective and the finalizer a
/// bijection), and across the bitmap and the set it has the odds of any other 64-bit collision.
///
/// See: hef-query-metadata-and-indexes/spec.md
struct BuildScratch {
    /// Hashes of every key that is not a three-byte ASCII key.
    other_hashes: HashSet<u64>,
    /// One bit per case-folded ASCII trigram code, set once the trigram has been seen; drained and cleared by
    /// [`BuildScratch::build_filter`].
    trigrams: Vec<u64>,
}

impl BuildScratch {
    fn new() -> Self {
        BuildScratch {
            other_hashes: HashSet::new(),
            trigrams: vec![0; TRIGRAM_BITMAP_WORDS],
        }
    }

    /// Records every token of `text`, and with `include_ngrams` every trigram of its case-folded form.
    fn insert_value(&mut self, text: &str, include_ngrams: bool) {
        if text.is_ascii() {
            for token in split_ascii_tokens(text.as_bytes()) {
                self.insert_ascii_token(token);
            }
            if include_ngrams {
                self.mark_ascii_trigrams(text.as_bytes());
            }
            return;
        }
        for token in split_tokens(text) {
            if token.is_ascii() {
                self.insert_ascii_token(token.as_bytes());
            } else {
                // Full Unicode lowercasing, for the same context-sensitive folds `hash_folded_token` relies on.
                self.insert_folded_key(token.to_lowercase().as_bytes());
            }
        }
        if include_ngrams {
            // Trigrams are taken over the whole case-folded value, not per token, so a substring query that
            // straddles a token boundary (e.g. "lo wo" in "hello world") still finds every trigram it needs.
            let folded = fold_for_ngrams(text);
            for ngram in ngrams(&folded, NGRAM_LEN) {
                self.insert_folded_key(ngram.as_bytes());
            }
        }
    }

    /// Records one still-cased ASCII token, folding case as it goes.
    fn insert_ascii_token(&mut self, token: &[u8]) {
        if let &[a, b, c] = token {
            self.mark_trigram(trigram_code([a, b, c]));
        } else {
            self.other_hashes.insert(stable_hash_ascii_lowercase(token));
        }
    }

    /// Records one key that is already case-folded (a lowercased token, or a trigram of a folded value), whatever
    /// its bytes: a three-byte ASCII key takes the bitmap, everything else the set.
    fn insert_folded_key(&mut self, key: &[u8]) {
        if let &[a, b, c] = key
            && key.is_ascii()
        {
            self.mark_trigram(trigram_code([a, b, c]));
        } else {
            self.other_hashes.insert(stable_hash(key));
        }
    }

    /// Marks every three-byte window of ASCII `text`. The code rolls one byte at a time — shift the previous code up
    /// seven bits, bring in the folded byte, mask — so each window costs no more than the byte that completes it.
    fn mark_ascii_trigrams(&mut self, text: &[u8]) {
        let Some((&[first, second], rest)) = text.split_first_chunk::<2>() else {
            return;
        };
        let mut code = (usize::from(first.to_ascii_lowercase()) << 7) | usize::from(second.to_ascii_lowercase());
        for &byte in rest {
            code = ((code << 7) | usize::from(byte.to_ascii_lowercase())) & TRIGRAM_CODE_MASK;
            self.mark_trigram(code);
        }
    }

    fn mark_trigram(&mut self, code: usize) {
        if let Some(word) = self.trigrams.get_mut(code >> 6) {
            *word |= 1u64 << (code & 63);
        }
    }

    /// Hashes every distinct key seen into a filter sized for exactly that many keys, leaving the scratch empty for the
    /// next build. The bitmap is cleared word by word as it is drained, so emptying it costs nothing beyond the walk
    /// that finds its set bits.
    fn build_filter(&mut self) -> SplitBlockBloomFilter {
        let key_count = self.other_hashes.len()
            + self
                .trigrams
                .iter()
                .map(|word| word.count_ones() as usize)
                .sum::<usize>();
        let trigram_hashes = self.trigrams.iter_mut().enumerate().flat_map(|(word_index, word)| {
            let mut bits = std::mem::take(word);
            std::iter::from_fn(move || {
                if bits == 0 {
                    return None;
                }
                let code = (word_index << 6) | bits.trailing_zeros() as usize;
                bits &= bits - 1;
                Some(trigram_hash(code))
            })
        });
        let filter = SplitBlockBloomFilter::build_from_iter(
            self.other_hashes.iter().copied().chain(trigram_hashes),
            key_count,
            BITS_PER_TOKEN,
        );
        self.other_hashes.clear();
        filter
    }
}

/// The bitmap code of three ASCII bytes, case-folded: seven bits each, first byte highest.
fn trigram_code(bytes: [u8; 3]) -> usize {
    bytes
        .iter()
        .fold(0, |code, byte| (code << 7) | usize::from(byte.to_ascii_lowercase()))
}

/// The hash the three folded bytes behind a bitmap code would have reached the filter with: the same
/// [`stable_hash_ascii_lowercase`] every three-byte ASCII window and token is hashed by.
fn trigram_hash(code: usize) -> u64 {
    let bytes = [(code >> 14) as u8, (code >> 7 & 0x7f) as u8, (code & 0x7f) as u8];
    stable_hash_ascii_lowercase(&bytes)
}

/// Whether case folding would leave `text` exactly as it is. True for text that is ASCII with no uppercase letter:
/// every such byte lowercases to itself, and the folds that expand a character or depend on its neighbours all live
/// outside ASCII.
fn is_already_folded(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii() && !byte.is_ascii_uppercase())
}

/// Hashes `token` to the value it would have had if it were lowercased first, which is all the build and probe sides
/// ever need — the lowercased text itself is never kept.
///
/// A token of pure ASCII, which is nearly every token, folds case byte by byte straight into the hash and allocates
/// nothing even when it carries uppercase, where [`tokenize`] still has to build the folded copy it hands back.
/// Anything else falls back to `to_lowercase`, whose allocation buys the context-sensitive folding that a
/// per-character fold would get wrong: a Greek capital sigma lowers to `ς` at the end of a token but `σ` inside one,
/// and both sides of the filter have to agree on which.
pub fn hash_folded_token(token: &str) -> u64 {
    if token.is_ascii() {
        stable_hash_ascii_lowercase(token.as_bytes())
    } else {
        hash_token(&token.to_lowercase())
    }
}

/// Case-folds text for the n-gram layer one character at a time, so a window of characters folds the same whether it
/// sits inside a stored value or arrives alone as a probe needle. `str::to_lowercase` is context-sensitive — a Greek
/// capital sigma lowers to `ς` at the end of a word but `σ` elsewhere — so folding a needle in isolation could
/// produce trigrams the build side never inserted and wrongly prune a block that contains the substring. Final sigma
/// is folded to the medial `σ` for the same reason: the last character of a needle is rarely word-final in the value
/// it matches.
fn fold_for_ngrams(text: &str) -> Cow<'_, str> {
    if is_already_folded(text) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(
        text.chars()
            .flat_map(char::to_lowercase)
            .map(|c| if c == 'ς' { 'σ' } else { c })
            .collect(),
    )
}

/// Produces all n-grams of length `n` (character n-grams) from `text`. For `n = 3` (trigrams) this yields every
/// 3-character window, which is the standard index for substring / `CONTAINS` predicates.
///
/// Slides a window of `n + 1` char-boundary byte offsets (the sentinel `text.len()` standing in for the boundary past
/// the last char) across `text` one position at a time, rather than first collecting every char's `(position, char)`
/// pair into a `Vec` the size of the whole value — the window is the only state carried between steps.
pub fn ngrams(text: &str, n: usize) -> impl Iterator<Item = &str> {
    let mut boundaries = text
        .char_indices()
        .map(|(pos, _)| pos)
        .chain(std::iter::once(text.len()));
    let mut window: VecDeque<usize> = (&mut boundaries).take(n + 1).collect();
    std::iter::from_fn(move || {
        if window.len() <= n {
            return None;
        }
        let start = *window.front()?;
        let stop = *window.get(n)?;
        let ngram = text.get(start..stop);
        window.pop_front();
        if let Some(next) = boundaries.next() {
            window.push_back(next);
        }
        ngram
    })
}

/// Hashes a token to a `u64` for insertion into / lookup in the backing filter. Uses the SplitMix64 finalizer seeded
/// with a stable polynomial rolling hash of the token bytes, so different strings always produce different hashes with
/// overwhelming probability and the result is the same on every machine.
pub fn hash_token(token: &str) -> u64 {
    stable_hash(token.as_bytes())
}

#[cfg(test)]
#[path = "test/text_token.rs"]
mod tests;
