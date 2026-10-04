//! Answers common filters straight off compressed column bytes, so predicates like `event_id = '…'` or `amount >= 100`
//! never have to decode every row first.
//!
//! **String predicates** (`filter_string_block`): dictionary blocks store distinct values in sorted order so both
//! equality and range reduce to comparing small integer codes; FSST blocks answer equality-class predicates by
//! compressing the search term with the block's own symbol table, and — where the block stores the per-value keys of
//! [`super::sidecar`] — answer range filters by comparing prefix keys and prune substring candidates by fingerprint,
//! decompressing only the values whose keys cannot decide.
//!
//! **Numeric predicates** (`filter_numeric_block`, `filter_float_block`): FOR bit-packed integer blocks translate the
//! predicate into the packed-lane domain once (the bounds rescaled against the block's base) and compare the lanes
//! directly, so each FastLanes vector unpacks straight into verdicts through the runtime-dispatched SIMD kernel;
//! DELTA blocks rebuild the running prefix sum and compare without building the full decoded Vec; decimal128 blocks
//! compare the stored integer mantissa after rescaling the comparison value; ALP float blocks answer entirely in the
//! scaled-integer domain — the float bounds become exact integer ranges — so no rejected float is ever reconstructed.
//!
//! Every kernel is an optimisation, never a correctness dependency: when it cannot answer a filter it returns `None`
//! and the caller falls back to `decode_block` + plain filter. The fast answer is exact and equals the slow path row
//! for row. Requirements: "Compressed-data string predicates" and "Compressed-data numeric predicates".

use super::constant::{PARALLEL_SCAN_MIN_GRANULE_BYTES, PARALLEL_SCAN_MIN_GRANULES, PREFIX_KEY_BYTES};
use super::seekable_zstd::Window;
use super::sidecar::{PrefixKey, StringFingerprint};
use super::{
    ALP_POWERS, ColumnData, Compression, FASTLANES_VECTOR, PackedStream, PipelineId, SideStream, StringColumn,
    Transform, ValueKind, decode_block, decode_block_shared_with_body, mask, read_fsst_compressor, read_packed_stream,
    unzigzag, zigzag,
};
use crate::error::FormatError;
use crate::file::bytes::{Reader, slice};
use aho_corasick::{AhoCorasick, MatchKind};
use memchr::memmem::Finder;
use std::cmp::Ordering;
use std::sync::Arc;

/// One end of a range filter: the value to compare against, and whether a row equal to that value is itself a match
/// (`>=`/`<=` vs `>`/`<`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringBound {
    pub inclusive: bool,
    pub value: String,
}

/// Tests whether text contains every one of a set of substrings, in a single pass, optionally ignoring ASCII case.
///
/// Built once for a query and reused for every block it is applied to: the search automaton is the expensive part,
/// and a scan that rebuilt it per block would pay for it once per page. A matcher with no needles matches every
/// present value, which is what an empty search term means.
///
/// See: query-execution/spec.md
#[derive(Clone, Debug)]
pub struct ContainsMatcher {
    case_insensitive: bool,
    /// Every byte bucket the needles together need, so a block's stored fingerprints rule a value out before it is
    /// decompressed. Computed once here rather than per block, as the automaton is.
    fingerprint: StringFingerprint,
    needles: Vec<String>,
    search: ContainsSearch,
}

/// The cheapest exact searcher for the query shape. Most full-text scans carry one case-sensitive needle, where
/// `memmem`'s runtime-selected SIMD finder is materially cheaper than a general multi-pattern automaton. Empty and
/// multi-needle/case-insensitive queries retain their exact existing semantics.
#[derive(Clone, Debug)]
enum ContainsSearch {
    MatchAll,
    Multiple(Arc<AhoCorasick>),
    Single(Finder<'static>),
}

impl ContainsMatcher {
    /// Builds a matcher for text that must contain every string in `needles`. With `case_insensitive` set, ASCII
    /// letters match in either case — the case folding non-ASCII text needs is not a byte-for-byte substitution, so it
    /// is not offered here rather than being offered wrongly.
    pub fn new(needles: Vec<String>, case_insensitive: bool) -> Self {
        let search = match needles.as_slice() {
            [] => ContainsSearch::MatchAll,
            [needle] if needle.is_empty() => ContainsSearch::MatchAll,
            [needle] if !case_insensitive => ContainsSearch::Single(Finder::new(needle).into_owned()),
            _ => {
                // `Standard` is what makes overlapping search available, and overlapping is what a conjunction needs:
                // with leftmost-first semantics a needle that starts inside another needle's match is never reported,
                // and the value would be judged not to contain it.
                #[allow(clippy::expect_used)]
                let automaton = AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .ascii_case_insensitive(case_insensitive)
                    .build(&needles)
                    .expect("an Aho-Corasick automaton over plain string needles always builds");
                ContainsSearch::Multiple(Arc::new(automaton))
            }
        };
        let fingerprint = needles.iter().fold(StringFingerprint::default(), |bits, needle| {
            bits.union(StringFingerprint::of(needle.as_bytes()))
        });
        Self {
            case_insensitive,
            fingerprint,
            needles,
            search,
        }
    }

    /// Whether ASCII letters match in either case.
    pub fn case_insensitive(&self) -> bool {
        self.case_insensitive
    }

    /// The substrings a value has to contain, all of them, to match.
    pub fn needles(&self) -> &[String] {
        &self.needles
    }

    /// The byte buckets a value must have for any needle to occur in it. Case folding needs no special handling: a
    /// bucket holds an ASCII letter in both cases.
    fn fingerprint(&self) -> StringFingerprint {
        self.fingerprint
    }

    /// Whether one value contains every needle. An empty needle set matches anything.
    pub fn matches(&self, value: &str) -> bool {
        self.matches_bytes(value.as_bytes())
    }

    fn matches_bytes(&self, value: &[u8]) -> bool {
        match &self.search {
            ContainsSearch::MatchAll => return true,
            ContainsSearch::Single(finder) => return finder.find(value).is_some(),
            ContainsSearch::Multiple(automaton) => self.matches_automaton(automaton, value),
        }
    }

    fn matches_automaton(&self, automaton: &AhoCorasick, value: &[u8]) -> bool {
        let mut seen = vec![0u64; self.mark_words()];
        for hit in automaton.find_overlapping_iter(value) {
            set_bit(&mut seen, hit.pattern().as_usize());
        }
        all_bits_set(&seen, self.needles.len())
    }

    /// How many 64-bit words hold one value's "which needles were seen" mark.
    fn mark_words(&self) -> usize {
        self.needles.len().div_ceil(64)
    }

    /// Marks, in one pass over a whole value arena, which stored values contain every needle. `bounds` holds the
    /// arena offset each value starts at plus a final end offset, so value `i` spans `bounds[i]..bounds[i + 1]`.
    ///
    /// The pass costs the arena's size rather than its row count: one automaton walk answers every value and every
    /// needle at once, instead of one search per value or one pass per needle. A hit that starts inside one value but
    /// runs past its end straddles two neighbours that are only adjacent because they share a buffer, and is no match.
    fn mark_arena(&self, text: &[u8], bounds: &[usize]) -> Vec<bool> {
        match &self.search {
            ContainsSearch::MatchAll => return vec![true; bounds.len().saturating_sub(1)],
            ContainsSearch::Single(finder) => {
                let mut marks = vec![false; bounds.len().saturating_sub(1)];
                visit_single_matches(finder, text, bounds, |value| marks[value] = true);
                return marks;
            }
            ContainsSearch::Multiple(automaton) => self.mark_arena_automaton(automaton, text, bounds),
        }
    }

    /// Counts matching values without allocating the Boolean mask needed by row-producing callers.
    fn count_arena(&self, text: &[u8], bounds: &[usize]) -> usize {
        match &self.search {
            ContainsSearch::MatchAll => bounds.len().saturating_sub(1),
            ContainsSearch::Single(finder) => {
                let mut count = 0;
                visit_single_matches(finder, text, bounds, |_| count += 1);
                count
            }
            ContainsSearch::Multiple(automaton) => self
                .mark_arena_automaton(automaton, text, bounds)
                .into_iter()
                .filter(|matched| *matched)
                .count(),
        }
    }

    fn mark_arena_automaton(&self, automaton: &AhoCorasick, text: &[u8], bounds: &[usize]) -> Vec<bool> {
        let count = bounds.len().saturating_sub(1);
        let words = self.mark_words();
        if words == 0 {
            return vec![true; count];
        }
        let mut seen = vec![0u64; count * words];
        for hit in automaton.find_overlapping_iter(text) {
            // The value covering this hit: the last one that starts at or before it. Binary search rather than a
            // moving cursor, because overlapping matches are reported in order of where they end, not where they
            // start.
            let Some(value) = bounds.partition_point(|start| *start <= hit.start()).checked_sub(1) else {
                continue;
            };
            let (Some(start), Some(end)) = (bounds.get(value), bounds.get(value + 1)) else {
                continue;
            };
            if hit.start() >= *start
                && hit.end() <= *end
                && let Some(mark) = seen.get_mut(value * words..(value + 1) * words)
            {
                set_bit(mark, hit.pattern().as_usize());
            }
        }
        seen.chunks_exact(words)
            .map(|mark| all_bits_set(mark, self.needles.len()))
            .collect()
    }
}

/// Visits each value containing a single needle exactly once. A whole-arena search keeps the SIMD kernel at full
/// stride. When its first hit crosses a value boundary, restarting at that boundary (rather than after the hit) is
/// essential: a valid overlapping occurrence may begin there. Once a value matches, the rest of it is skipped.
fn visit_single_matches(finder: &Finder<'_>, text: &[u8], bounds: &[usize], mut visit: impl FnMut(usize)) {
    let mut search_from = 0;
    while search_from <= text.len() {
        let Some(relative) = finder.find(&text[search_from..]) else {
            break;
        };
        let start = search_from + relative;
        let Some(value) = bounds.partition_point(|bound| *bound <= start).checked_sub(1) else {
            break;
        };
        let Some(&end) = bounds.get(value + 1) else {
            break;
        };
        if start + finder.needle().len() <= end {
            visit(value);
        }
        // `Finder` reports non-overlapping hits. Restarting at the value boundary both skips duplicate hits after a
        // successful value and preserves an occurrence overlapping a rejected cross-boundary hit.
        search_from = end;
    }
}

/// Sets bit `at` of a little-endian bitmask held in 64-bit words.
fn set_bit(mark: &mut [u64], at: usize) {
    if let Some(word) = mark.get_mut(at / 64) {
        *word |= 1u64 << (at % 64);
    }
}

/// Whether the lowest `count` bits of a bitmask are all set.
fn all_bits_set(mark: &[u64], count: usize) -> bool {
    mark.iter().enumerate().all(|(index, word)| {
        let bits = (count - index * 64).min(64);
        let full = if bits == 64 { u64::MAX } else { (1u64 << bits) - 1 };
        *word & full == full
    })
}

impl PartialEq for ContainsMatcher {
    fn eq(&self, other: &Self) -> bool {
        self.case_insensitive == other.case_insensitive && self.needles == other.needles
    }
}

impl Eq for ContainsMatcher {}

/// A filter over a string column that the compressed-data kernels try to answer without rebuilding every row's text.
///
/// `Range` compares with plain byte order (the same order the dictionary is sorted in); there is no locale collation.
/// `Contains` asks for every one of its needles, and is case-sensitive unless its matcher was built to ignore ASCII
/// case. A null row never satisfies any of these — comparing with an unknown value yields no match, matching SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StringPredicate {
    Contains(ContainsMatcher),
    Equals(String),
    InSet(Vec<String>),
    NotEquals(String),
    Range {
        lower: Option<StringBound>,
        upper: Option<StringBound>,
    },
}

impl StringPredicate {
    /// Whether one value satisfies this filter. A missing (null) value never matches, so callers get the same answer
    /// whether a row was stored as null or simply failed the comparison.
    pub fn matches_value(&self, value: Option<&str>) -> bool {
        let Some(value) = value else { return false };
        match self {
            StringPredicate::Contains(matcher) => matcher.matches(value),
            StringPredicate::Equals(needle) => value == needle.as_str(),
            StringPredicate::InSet(set) => set.iter().any(|needle| needle.as_str() == value),
            StringPredicate::NotEquals(needle) => value != needle.as_str(),
            StringPredicate::Range { lower, upper } => {
                let above_lower = match lower {
                    Some(bound) if bound.inclusive => value >= bound.value.as_str(),
                    Some(bound) => value > bound.value.as_str(),
                    None => true,
                };
                let below_upper = match upper {
                    Some(bound) if bound.inclusive => value <= bound.value.as_str(),
                    Some(bound) => value < bound.value.as_str(),
                    None => true,
                };
                above_lower && below_upper
            }
        }
    }

    /// Filters already-decoded values, one bool per row. This is the plain reference path the compressed-data kernels
    /// must agree with; it is also what a caller falls back to when no kernel applies.
    pub fn filter_decoded(&self, values: &StringColumn) -> Vec<bool> {
        values.iter().map(|value| self.matches_value(value)).collect()
    }
}

/// Tries to answer `predicate` straight from an encoded string block, without decoding every row to text.
///
/// Returns `Some(mask)` — one bool per row, in stored order, with null rows always `false` — when the block's encoding
/// can decide the filter from its compressed form: any predicate on a dictionary block, an equality-class predicate
/// (`=`, `!=`, `IN`) on any FSST block, and a range predicate on an FSST block that stores per-value prefix keys.
/// Returns `None` when no kernel applies (a raw/uncompressed string block, or a range filter on an FSST block without
/// those keys, whose codes are not order-preserving), signalling the caller to fall back to [`decode_block`] and
/// [`StringPredicate::filter_decoded`]. The mask is exact: every selected row truly satisfies the predicate.
///
/// `pipeline` and `bytes` are exactly what [`decode_block`] takes, and the returned mask lines up one-to-one with the
/// rows [`decode_block`] would produce.
pub fn filter_string_block(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &StringPredicate,
) -> Result<Option<Vec<bool>>, FormatError> {
    filter_string_block_shared(pipeline, bytes, predicate, None)
}

/// A predicate translated once against a column's file-scope shared alphabet, reusable across every shared-scope
/// block of the file — no per-block dictionary work. Build it with [`translate_for_shared_dictionary`].
pub struct SharedDictionaryPredicate {
    test: CodeTest,
}

/// Translates `predicate` into code space against a file-scope shared alphabet, once per file; hand the result to
/// [`filter_string_block_shared`] for every block of the column.
pub fn translate_for_shared_dictionary(predicate: &StringPredicate, alphabet: &[String]) -> SharedDictionaryPredicate {
    SharedDictionaryPredicate {
        test: dictionary_code_test(predicate, alphabet),
    }
}

/// [`filter_string_block`] with the column's once-per-file shared-dictionary translation, for blocks recording the
/// file dictionary scope. A shared-scope block without a translation declines (returns `None`) so the caller falls
/// back to a decode it can feed the alphabet to.
pub fn filter_string_block_shared(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &StringPredicate,
    shared: Option<&SharedDictionaryPredicate>,
) -> Result<Option<Vec<bool>>, FormatError> {
    if !string_block_has_fast_path(pipeline, predicate)? {
        return Ok(None);
    }
    let body = super::remove_trailing(pipeline.compression()?, bytes)?;
    filter_string_block_shared_with_body(pipeline, &body, predicate, shared)
}

/// Counts rows satisfying `predicate` straight from an encoded string block when a compressed-data kernel applies.
///
/// This is the count-only companion to [`filter_string_block_shared`]. In particular, FSST and raw substring scans
/// count matches while walking the arena and never allocate a row-sized Boolean mask. Other encodings retain their
/// existing exact kernel and reduce its result, and an unsupported pipeline still returns `None` for decode fallback.
pub fn count_string_block_shared(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &StringPredicate,
    shared: Option<&SharedDictionaryPredicate>,
) -> Result<Option<usize>, FormatError> {
    if !string_block_has_fast_path(pipeline, predicate)? {
        return Ok(None);
    }
    let transform = pipeline.transform()?;
    let side = pipeline.side_stream()?;
    let compression = pipeline.compression()?;
    if transform == Transform::FsstString
        && compression == Compression::SeekableZstd
        && let StringPredicate::Contains(matcher) = predicate
    {
        return Ok(Some(count_fsst_framed_contains(bytes, side, matcher)?));
    }
    let body = super::remove_trailing(compression, bytes)?;
    match (transform, predicate) {
        (Transform::FsstString, StringPredicate::Contains(matcher)) => {
            Ok(Some(count_fsst_contains(&body, side, matcher)?))
        }
        (Transform::RawString, StringPredicate::Contains(matcher)) => Ok(Some(count_raw_contains(&body, matcher)?)),
        _ => Ok(
            filter_string_block_shared_with_body(pipeline, &body, predicate, shared)?
                .map(|mask| mask.into_iter().filter(|matched| *matched).count()),
        ),
    }
}

/// Whether a scan over `granules` surviving granules, each decoding to about `bytes_per_granule` bytes of the scanned
/// column, should run its granules on the thread pool rather than one after another on the calling thread.
///
/// Fanning out pays only when there is more than one granule and each carries more work than handing it to a worker
/// costs, so a scan whose pruning left one survivor, or a few small ones, keeps them inline. Callers such as a
/// full-text search over the granules a token filter could not rule out take this decision from here rather than
/// each keeping a threshold of their own.
pub fn scan_in_parallel(granules: usize, bytes_per_granule: u64) -> bool {
    granules >= PARALLEL_SCAN_MIN_GRANULES && bytes_per_granule >= PARALLEL_SCAN_MIN_GRANULE_BYTES
}

/// [`filter_string_block_shared`] for a caller that already holds this block's decompressed body — reused, for
/// instance, across a descriptor call or a decode fallback on the same block — so the filter does not decompress it
/// again.
pub fn filter_string_block_shared_with_body(
    pipeline: PipelineId,
    body: &[u8],
    predicate: &StringPredicate,
    shared: Option<&SharedDictionaryPredicate>,
) -> Result<Option<Vec<bool>>, FormatError> {
    if !string_block_has_fast_path(pipeline, predicate)? {
        return Ok(None);
    }
    let transform = pipeline.transform()?;
    let side = pipeline.side_stream()?;
    if matches!(transform, Transform::DictionaryString) && side == SideStream::FileScopeDictionary {
        let Some(translated) = shared else {
            return Ok(None);
        };
        return Ok(Some(filter_shared_dictionary(body, translated)?));
    }
    let mask = match (transform, predicate) {
        (Transform::FsstString, StringPredicate::Contains(matcher)) => filter_fsst_contains(body, side, matcher)?,
        (Transform::RawString, StringPredicate::Contains(matcher)) => filter_raw_contains(body, matcher)?,
        (Transform::DictionaryString, _) => filter_dictionary(body, side, predicate)?,
        (Transform::FsstString, StringPredicate::Range { .. }) => filter_fsst_range(body, predicate)?,
        (Transform::FsstString, _) => filter_fsst(body, predicate)?,
        _ => return Ok(None),
    };
    Ok(Some(mask))
}

/// Whether a string block's transform could possibly answer `predicate` from its compressed bytes — cheap enough to
/// check before paying to strip trailing compression. `false` always means "decompressing would not have helped";
/// a `true` here is not a promise of an answer, only that it is worth trying.
fn string_block_has_fast_path(pipeline: PipelineId, predicate: &StringPredicate) -> Result<bool, FormatError> {
    if pipeline.value_kind()? != ValueKind::String {
        return Ok(false);
    }
    let transform = pipeline.transform()?;
    // A substring test is answered over the block's own value arena, raw blocks included: one search across the whole
    // arena beats rebuilding every row's text and searching each one on its own.
    if matches!(predicate, StringPredicate::Contains(_)) {
        return Ok(matches!(
            transform,
            Transform::DictionaryString | Transform::FsstString | Transform::RawString
        ));
    }
    // A range filter on FSST is decidable only from the block's per-value prefix keys: the codes themselves are not
    // order-preserving, so a block that stores no keys has nothing in it that orders the values.
    if matches!(transform, Transform::FsstString) && matches!(predicate, StringPredicate::Range { .. }) {
        return Ok(pipeline.side_stream()? == SideStream::FsstValueKeys);
    }
    Ok(!matches!(transform, Transform::RawString))
}

/// Decides a pre-translated predicate over a shared-scope dictionary block: the body carries only the null side
/// stream and the code stream, and each code is tested against the once-per-file translation.
fn filter_shared_dictionary(body: &[u8], translated: &SharedDictionaryPredicate) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let stream = read_packed_stream(&mut reader)?;
    let mut present = vec![false; stream.padded_count()];
    stream.unpack_into(&mut present, |code| translated.test.matches(code));
    present.truncate(stream.count);
    weave_present(&nulls, &present)
}

/// Answers `predicate` over an encoded string block, decoding the block only when no compressed-data kernel applies.
///
/// This is the mandatory fallback wired up for callers: it tries [`filter_string_block`] first and, when that declines,
/// decodes the block and filters the plain strings — so a caller always gets an answer and never depends on the fast
/// path existing for a given encoding.
pub fn filter_string_block_or_decode(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &StringPredicate,
) -> Result<Vec<bool>, FormatError> {
    // Decompressed once and reused for both the filter attempt and the decode fallback: `filter_string_block`
    // declining after already having decompressed (a file-scope dictionary block with no shared translation) would
    // otherwise leave `decode_block` paying to decompress the same bytes again.
    let body = super::remove_trailing(pipeline.compression()?, bytes)?;
    if let Some(mask) = filter_string_block_shared_with_body(pipeline, &body, predicate, None)? {
        return Ok(mask);
    }
    match decode_block_shared_with_body(pipeline, &body, None)? {
        ColumnData::Strings(values) => Ok(predicate.filter_decoded(&values)),
        _ => Err(FormatError::Structural {
            rule: "string predicate applied to a non-string column",
        }),
    }
}

/// Which dictionary codes a predicate accepts, resolved once against the sorted dictionary so each row is then a
/// constant-time check on its code.
enum CodeTest {
    All,
    None,
    NotCode(u64),
    Range(usize, usize),
    Set(Vec<u64>),
}

impl CodeTest {
    fn matches(&self, code: u64) -> bool {
        match self {
            CodeTest::All => true,
            CodeTest::None => false,
            CodeTest::NotCode(excluded) => code != *excluded,
            CodeTest::Range(lo, hi) => code >= *lo as u64 && code < *hi as u64,
            CodeTest::Set(codes) => codes.binary_search(&code).is_ok(),
        }
    }
}

/// Translates a predicate into the set of codes that satisfy it, exploiting that `dictionary` is sorted so codes share
/// the values' order. Generic over the entry type so a caller already holding `&[String]` (the shared alphabet) needs
/// no `Vec<&str>` copy just to binary-search it.
fn dictionary_code_test<T: AsRef<str>>(predicate: &StringPredicate, dictionary: &[T]) -> CodeTest {
    let search = |needle: &str| dictionary.binary_search_by(|entry| entry.as_ref().cmp(needle));
    match predicate {
        // Substring order does not follow the dictionary's, so every entry is tested — but the entries are the block's
        // distinct values, so a column of a million rows costs one search per distinct value rather than per row.
        StringPredicate::Contains(matcher) => CodeTest::Set(
            dictionary
                .iter()
                .enumerate()
                .filter(|(_, entry)| matcher.matches(entry.as_ref()))
                .map(|(index, _)| index as u64)
                .collect(),
        ),
        StringPredicate::Equals(needle) => match search(needle) {
            Ok(index) => CodeTest::Range(index, index + 1),
            Err(_) => CodeTest::None,
        },
        StringPredicate::NotEquals(needle) => match search(needle) {
            Ok(index) => CodeTest::NotCode(index as u64),
            Err(_) => CodeTest::All,
        },
        StringPredicate::InSet(needles) => {
            let mut codes: Vec<u64> = needles
                .iter()
                .filter_map(|needle| search(needle).ok())
                .map(|index| index as u64)
                .collect();
            codes.sort_unstable();
            codes.dedup();
            CodeTest::Set(codes)
        }
        StringPredicate::Range { lower, upper } => {
            let lo = match lower {
                Some(bound) if bound.inclusive => {
                    dictionary.partition_point(|entry| entry.as_ref() < bound.value.as_str())
                }
                Some(bound) => dictionary.partition_point(|entry| entry.as_ref() <= bound.value.as_str()),
                None => 0,
            };
            let hi = match upper {
                Some(bound) if bound.inclusive => {
                    dictionary.partition_point(|entry| entry.as_ref() <= bound.value.as_str())
                }
                Some(bound) => dictionary.partition_point(|entry| entry.as_ref() < bound.value.as_str()),
                None => dictionary.len(),
            };
            if lo >= hi {
                CodeTest::None
            } else {
                CodeTest::Range(lo, hi)
            }
        }
    }
}

/// Decides a predicate over a dictionary block by testing each row's code against the dictionary, never materialising
/// the row strings. A block carrying the FSST inner level decompresses the (small) dictionary once to translate the
/// predicate into codes; the code stream is then tested exactly as on a plain dictionary block.
fn filter_dictionary(body: &[u8], side: SideStream, predicate: &StringPredicate) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let dict_count = reader.u32("dictionary count")? as usize;
    let owned: Vec<String>;
    let mut dictionary: Vec<&str> = Vec::with_capacity(reader.capacity_hint(dict_count, 1));
    if side == SideStream::FsstDictionaryValues {
        owned = super::read_dictionary_values(&mut reader, dict_count, side)?;
        dictionary.extend(owned.iter().map(String::as_str));
    } else {
        let offsets = super::read_offsets(&mut reader, dict_count)?;
        let data_len = offsets.last().copied().unwrap_or(0);
        let data = reader.take(data_len, "dictionary data")?;
        for pair in offsets.windows(2) {
            let [start, end] = pair else { continue };
            if end < start {
                return Err(FormatError::Structural {
                    rule: "dictionary offsets must be non-decreasing",
                });
            }
            let entry = slice(data, *start, end - start, "dictionary entry")?;
            dictionary.push(simdutf8::basic::from_utf8(entry).map_err(|_| FormatError::InvalidUtf8 {
                what: "dictionary entry",
            })?);
        }
    }
    let test = dictionary_code_test(predicate, &dictionary);
    let stream = read_packed_stream(&mut reader)?;
    let mut present = vec![false; stream.padded_count()];
    stream.unpack_into(&mut present, |code| test.matches(code));
    present.truncate(stream.count);
    weave_present(&nulls, &present)
}

/// Which compressed search terms an equality-class predicate accepts. The terms are pre-compressed under the block's
/// symbol table so each row is a byte compare against its stored compressed slice.
enum FsstTest {
    Equals(Vec<u8>),
    InSet(Vec<Vec<u8>>),
    NotEquals(Vec<u8>),
}

impl FsstTest {
    fn matches(&self, compressed: &[u8]) -> bool {
        match self {
            FsstTest::Equals(term) => compressed == term.as_slice(),
            FsstTest::InSet(terms) => terms.iter().any(|term| term.as_slice() == compressed),
            FsstTest::NotEquals(term) => compressed != term.as_slice(),
        }
    }
}

/// Decides an equality-class predicate over an FSST block by compressing the search term(s) with the block's own symbol
/// table and byte-comparing against each stored compressed value — no value is ever decompressed.
fn filter_fsst(body: &[u8], predicate: &StringPredicate) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let compressor = read_fsst_compressor(&mut reader)?;
    let test = match predicate {
        StringPredicate::Equals(needle) => FsstTest::Equals(compressor.compress(needle.as_bytes())),
        StringPredicate::NotEquals(needle) => FsstTest::NotEquals(compressor.compress(needle.as_bytes())),
        StringPredicate::InSet(needles) => FsstTest::InSet(
            needles
                .iter()
                .map(|needle| compressor.compress(needle.as_bytes()))
                .collect(),
        ),
        StringPredicate::Contains(_) => {
            return Err(FormatError::Structural {
                rule: "FSST substring predicates are answered by the arena kernel, not the equality kernel",
            });
        }
        StringPredicate::Range { .. } => {
            return Err(FormatError::Structural {
                rule: "FSST blocks cannot answer range predicates from compressed bytes",
            });
        }
    };
    let present_count = reader.u32("fsst present count")? as usize;
    let offsets = super::read_offsets(&mut reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "fsst data")?;
    let mut present = Vec::with_capacity(reader.capacity_hint(present_count, 1));
    for pair in offsets.windows(2) {
        let [start, end] = pair else { continue };
        if end < start {
            return Err(FormatError::Structural {
                rule: "fsst offsets must be non-decreasing",
            });
        }
        let compressed = slice(data, *start, end - start, "fsst value")?;
        present.push(test.matches(compressed));
    }
    weave_present(&nulls, &present)
}

/// The per-value keys an FSST block stores after its arena when it records [`SideStream::FsstValueKeys`]: a run of
/// [`PrefixKey`]s, then a run of [`StringFingerprint`]s, one of each per present value in stored order.
struct FsstValueKeys<'a> {
    fingerprints: &'a [u8],
    prefixes: &'a [u8],
}

impl FsstValueKeys<'_> {
    /// The stored prefix key of every present value, in order. A key whose length byte runs past the prefix width is
    /// a forged block, and refuses.
    fn prefixes(&self) -> impl Iterator<Item = Result<PrefixKey, FormatError>> + '_ {
        self.prefixes.chunks_exact(PREFIX_KEY_BYTES).map(|stored| {
            <[u8; PREFIX_KEY_BYTES]>::try_from(stored)
                .ok()
                .and_then(PrefixKey::from_stored)
                .ok_or(FormatError::Structural {
                    rule: "fsst prefix key length byte runs past the prefix",
                })
        })
    }

    /// The positions of the values that could hold text summarised by `needles` — every value whose stored
    /// fingerprint has all of the needles' bits. Everything left out provably contains no needle.
    fn contains_candidates(&self, needles: StringFingerprint) -> impl Iterator<Item = usize> + '_ {
        self.fingerprints
            .chunks_exact(StringFingerprint::STORED_BYTES)
            .enumerate()
            .filter(move |(_, stored)| {
                let bits = u32::from_le_bytes((*stored).try_into().unwrap_or([0; 4]));
                StringFingerprint::from_bits(bits).might_contain(needles)
            })
            .map(|(index, _)| index)
    }
}

/// Reads the per-value key runs sitting after an FSST block's arena. The reader must already have consumed the arena.
fn read_fsst_value_keys<'a>(reader: &mut Reader<'a>, present_count: usize) -> Result<FsstValueKeys<'a>, FormatError> {
    let prefixes = reader.take(present_count.saturating_mul(PREFIX_KEY_BYTES), "fsst prefix key")?;
    let fingerprints = reader.take(
        present_count.saturating_mul(StringFingerprint::STORED_BYTES),
        "fsst fingerprint",
    )?;
    Ok(FsstValueKeys { fingerprints, prefixes })
}

/// The compressed codes for one FSST value, validating the same monotonic-offset commitment as the decompression
/// path before the count-only scanner reads them.
fn fsst_value_codes<'a>(data: &'a [u8], offsets: &[usize], index: usize) -> Result<&'a [u8], FormatError> {
    let (Some(&start), Some(&end)) = (offsets.get(index), offsets.get(index + 1)) else {
        return Err(FormatError::RefOutOfRange { what: "fsst value" });
    };
    if end < start {
        return Err(FormatError::Structural {
            rule: "fsst offsets must be non-decreasing",
        });
    }
    Ok(slice(data, start, end - start, "fsst value")?)
}

/// Rows whose value contains every needle `matcher` holds, decided over an FSST block's own value arena.
///
/// A block that stores per-value fingerprints answers most rows from them alone: one `AND` per value rules out
/// everything whose bytes cannot spell a needle, so only the survivors are decompressed and searched — and a needle
/// absent from the whole block costs no decompression at all. A block without them decompresses once into the scan's
/// reusable buffer and the search runs across that whole buffer in a single pass instead of once per row, so the
/// substring scan reaches full stride rather than paying its setup on every short value, no row's text is ever
/// rebuilt as a separate string, and a scan of many blocks neither allocates nor UTF-8-validates one arena per block.
/// Either way the candidates are searched in one pass, and the verdict is the same as searching every decoded row.
fn filter_fsst_contains(body: &[u8], side: SideStream, matcher: &ContainsMatcher) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let compressor = read_fsst_compressor(&mut reader)?;
    let present_count = reader.u32("fsst present count")? as usize;
    let offsets = super::read_offsets(&mut reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "fsst data")?;
    if side == SideStream::FsstValueKeys {
        let keys = read_fsst_value_keys(&mut reader, present_count)?;
        let candidates: Vec<usize> = keys.contains_candidates(matcher.fingerprint()).collect();
        let verdicts = super::with_fsst_selected_bytes(
            &compressor,
            data,
            &offsets,
            &candidates,
            "fsst value",
            "fsst offsets must be non-decreasing",
            |text, bounds| matcher.mark_arena(text, bounds),
        )?;
        return weave_present(&nulls, &scatter(present_count, &candidates, verdicts));
    }
    let present = super::with_fsst_arena_bytes(
        &compressor,
        data,
        &offsets,
        "fsst value",
        "fsst offsets must be non-decreasing",
        |text, bounds| matcher.mark_arena(text, bounds),
    )?;
    weave_present(&nulls, &present)
}

/// Count-only form of [`filter_fsst_contains`]. It performs all the same structural validation and candidate pruning,
/// but arena matches accumulate into a scalar rather than being scattered and woven into a row mask.
fn count_fsst_contains(body: &[u8], side: SideStream, matcher: &ContainsMatcher) -> Result<usize, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let compressor = read_fsst_compressor(&mut reader)?;
    let present_count = reader.u32("fsst present count")? as usize;
    if nulls.present_count() != present_count {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    let offsets = super::read_offsets(&mut reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "fsst data")?;
    if side == SideStream::FsstValueKeys {
        let keys = read_fsst_value_keys(&mut reader, present_count)?;
        return count_fsst_candidates(
            &compressor,
            data,
            &offsets,
            keys.contains_candidates(matcher.fingerprint()),
            matcher,
        );
    }
    count_fsst_candidates(&compressor, data, &offsets, 0..present_count, matcher)
}

/// Decompresses and searches count-only candidates one at a time in the reusable arena buffer. Unlike the mask path,
/// this needs neither a candidate vector nor a second bounds vector, and never retains the whole selected plaintext.
fn count_fsst_candidates(
    compressor: &fsst::Compressor,
    data: &[u8],
    offsets: &[usize],
    candidates: impl Iterator<Item = usize>,
    matcher: &ContainsMatcher,
) -> Result<usize, FormatError> {
    let table = super::FsstDecodeTable::new(compressor);
    super::with_arena_buffer(|buffer| {
        let mut count = 0;
        for index in candidates {
            let compressed = fsst_value_codes(data, offsets, index)?;
            buffer.clear();
            super::fsst_decompress_value(&table, compressed, buffer)?;
            count += usize::from(matcher.matches_bytes(buffer));
        }
        Ok(count)
    })
}

/// Count-only substring scan of an FSST block whose trailing stage is the seekable Zstandard family, inflating one
/// frame at a time instead of the whole block.
///
/// The header, the offset table and — when the block stores them — the per-value fingerprints are read out of the
/// frames they sit in, and the values' codes are then walked front to back through the window, which lets each frame
/// go as the walk passes it, so the scan never holds the inflated block; a value straddling two frames is gathered
/// from both. Fingerprints prune candidates exactly as [`count_fsst_contains`] does, and every structural check it
/// makes is made here too, so a malformed block is refused the same way.
fn count_fsst_framed_contains(
    stored: &[u8],
    side: SideStream,
    matcher: &ContainsMatcher,
) -> Result<usize, FormatError> {
    let mut window = Window::open(stored)?;
    let header = super::read_windowed_string_header(Transform::FsstString, &mut window)?;
    let present_count = header.present_count;
    if header.nulls.present_count() != present_count {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    let offsets_len = present_count.saturating_add(1).saturating_mul(4);
    let offsets = super::decode_offsets(&window.read(header.offsets_start, offsets_len)?);
    let data_start = header.offsets_start.saturating_add(offsets_len);
    let data_len = offsets.last().copied().unwrap_or(0);
    if data_start.saturating_add(data_len) > window.plain_len() {
        return Err(FormatError::Truncated { what: "fsst data" });
    }
    let fingerprints = match side {
        SideStream::FsstValueKeys => Some(
            window.read(
                data_start
                    .saturating_add(data_len)
                    .saturating_add(present_count.saturating_mul(PREFIX_KEY_BYTES)),
                present_count.saturating_mul(StringFingerprint::STORED_BYTES),
            )?,
        ),
        _ => None,
    };
    let compressor = header.compressor()?;
    let mut arena = FramedArena {
        data_len,
        data_start,
        offsets: &offsets,
        window: &mut window,
    };
    match &fingerprints {
        Some(fingerprints) => {
            let keys = FsstValueKeys {
                fingerprints,
                prefixes: &[],
            };
            count_framed_candidates(
                &mut arena,
                &compressor,
                keys.contains_candidates(matcher.fingerprint()),
                matcher,
            )
        }
        None => count_framed_candidates(&mut arena, &compressor, 0..present_count, matcher),
    }
}

/// An FSST block's value arena as the frame-by-frame scan sees it: the data region's place in the block, its offset
/// table, and the window the codes are read through.
struct FramedArena<'a, 'b> {
    data_len: usize,
    data_start: usize,
    offsets: &'a [usize],
    window: &'a mut Window<'b>,
}

/// [`count_fsst_candidates`] over a framed arena: each candidate's codes come out of the window's current frame,
/// decompress into the reusable arena buffer, and are searched there.
fn count_framed_candidates(
    arena: &mut FramedArena<'_, '_>,
    compressor: &fsst::Compressor,
    candidates: impl Iterator<Item = usize>,
    matcher: &ContainsMatcher,
) -> Result<usize, FormatError> {
    let table = super::FsstDecodeTable::new(compressor);
    super::with_arena_buffer(|buffer| {
        let mut count = 0;
        for index in candidates {
            let (Some(&start), Some(&end)) = (arena.offsets.get(index), arena.offsets.get(index + 1)) else {
                return Err(FormatError::RefOutOfRange { what: "fsst value" });
            };
            if end < start {
                return Err(FormatError::Structural {
                    rule: "fsst offsets must be non-decreasing",
                });
            }
            if end > arena.data_len {
                return Err(FormatError::Truncated { what: "fsst value" });
            }
            let compressed = arena
                .window
                .read_forward(arena.data_start.saturating_add(start), end - start)?;
            buffer.clear();
            super::fsst_decompress_value(&table, &compressed, buffer)?;
            count += usize::from(matcher.matches_bytes(buffer));
        }
        Ok(count)
    })
}

/// Rows inside a range, decided over an FSST block's per-value prefix keys.
///
/// Comparing a value's stored prefix against each bound's settles every row whose prefix differs from the bound's,
/// which is what makes a range answerable on data whose codes carry no order. Only the rows tying with a bound on all
/// seven prefix bytes are left undecided, and just those values are decompressed and compared for real — so a bound
/// that separates the block's values costs no decompression at all, and one that separates none costs what the decode
/// fallback costs. Requires the block to carry the keys; [`string_block_has_fast_path`] declines a range on one that
/// does not.
fn filter_fsst_range(body: &[u8], predicate: &StringPredicate) -> Result<Vec<bool>, FormatError> {
    let StringPredicate::Range { lower, upper } = predicate else {
        return Err(FormatError::Structural {
            rule: "the FSST prefix-key kernel answers only range predicates",
        });
    };
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let compressor = read_fsst_compressor(&mut reader)?;
    let present_count = reader.u32("fsst present count")? as usize;
    let offsets = super::read_offsets(&mut reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "fsst data")?;
    let keys = read_fsst_value_keys(&mut reader, present_count)?;

    let lower_key = lower.as_ref().map(|bound| PrefixKey::of(&bound.value));
    let upper_key = upper.as_ref().map(|bound| PrefixKey::of(&bound.value));
    let mut present = vec![false; present_count];
    let mut ambiguous = Vec::new();
    for (index, key) in keys.prefixes().enumerate() {
        let key = key?;
        let above = clears_bound(key, lower.as_ref(), lower_key, Ordering::Greater);
        let below = clears_bound(key, upper.as_ref(), upper_key, Ordering::Less);
        match (above, below) {
            (Some(false), _) | (_, Some(false)) => {}
            (Some(true), Some(true)) => {
                if let Some(slot) = present.get_mut(index) {
                    *slot = true;
                }
            }
            _ => ambiguous.push(index),
        }
    }
    if !ambiguous.is_empty() {
        let verdicts = super::with_fsst_selected_bytes(
            &compressor,
            data,
            &offsets,
            &ambiguous,
            "fsst value",
            "fsst offsets must be non-decreasing",
            |text, bounds| range_verdicts(predicate, text, bounds),
        )??;
        for (index, verdict) in ambiguous.iter().zip(verdicts) {
            if let Some(slot) = present.get_mut(*index) {
                *slot = verdict;
            }
        }
    }
    weave_present(&nulls, &present)
}

/// Whether a value with prefix key `key` clears one end of a range: `Some(true)` when it satisfies the bound,
/// `Some(false)` when it cannot, and `None` when the two prefixes tie and only the real text can say. `past` is the
/// order a value must be in against the bound to satisfy it — `Greater` for a lower bound, `Less` for an upper one —
/// and a value equal to the bound satisfies it exactly when the bound is inclusive. A missing bound is always
/// cleared.
fn clears_bound(
    key: PrefixKey,
    bound: Option<&StringBound>,
    bound_key: Option<PrefixKey>,
    past: Ordering,
) -> Option<bool> {
    let (Some(bound), Some(bound_key)) = (bound, bound_key) else {
        return Some(true);
    };
    match key.compare(bound_key)? {
        Ordering::Equal => Some(bound.inclusive),
        ordering => Some(ordering == past),
    }
}

/// Applies `predicate` to values laid end to end in `text`, where value `i` spans `bounds[i]..bounds[i + 1]` — the
/// exact answer for the rows a prefix key could not decide.
fn range_verdicts(predicate: &StringPredicate, text: &[u8], bounds: &[usize]) -> Result<Vec<bool>, FormatError> {
    bounds
        .windows(2)
        .filter_map(|pair| match pair {
            [start, end] => Some((*start, *end)),
            _ => None,
        })
        .map(|(start, end)| {
            let value = text
                .get(start..end)
                .and_then(|bytes| simdutf8::basic::from_utf8(bytes).ok())
                .ok_or(FormatError::InvalidUtf8 { what: "fsst value" })?;
            Ok(predicate.matches_value(Some(value)))
        })
        .collect()
}

/// Spreads the verdicts of a selected subset back over every present value, with everything left out judged `false`.
fn scatter(present_count: usize, selected: &[usize], verdicts: Vec<bool>) -> Vec<bool> {
    let mut present = vec![false; present_count];
    for (index, verdict) in selected.iter().zip(verdicts) {
        if let Some(slot) = present.get_mut(*index) {
            *slot = verdict;
        }
    }
    present
}

/// Rows whose value contains every needle `matcher` holds, decided over a raw string block's stored arena — the bytes are already
/// plaintext, so the search runs straight over them and nothing is decoded at all.
fn filter_raw_contains(body: &[u8], matcher: &ContainsMatcher) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let present_count = reader.u32("raw present count")? as usize;
    let offsets = super::read_offsets(&mut reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "raw string data")?;
    weave_present(&nulls, &matcher.mark_arena(data, &offsets))
}

/// Count-only raw-arena substring kernel: the bytes are searched in place and no row mask is built.
fn count_raw_contains(body: &[u8], matcher: &ContainsMatcher) -> Result<usize, FormatError> {
    let mut reader = Reader::new(body);
    let nulls = super::NullStream::read(&mut reader)?;
    let present_count = reader.u32("raw present count")? as usize;
    if nulls.present_count() != present_count {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    let offsets = super::read_offsets(&mut reader, present_count)?;
    let data_len = offsets.last().copied().unwrap_or(0);
    let data = reader.take(data_len, "raw string data")?;
    Ok(matcher.count_arena(data, &offsets))
}

// =============================================================================

/// Expands a per-present-value verdict back to one bool per row, marking absent (null) rows `false`. Mirrors how the
/// decoders thread the stored values back through the null side stream.
fn weave_present(nulls: &super::NullStream, present: &[bool]) -> Result<Vec<bool>, FormatError> {
    let mut mask = Vec::with_capacity(nulls.row_count());
    let mut verdicts = present.iter();
    nulls.each_row(|set| {
        if set {
            let verdict = verdicts.next().ok_or(FormatError::Structural {
                rule: "null bitmap disagrees with present count",
            })?;
            mask.push(*verdict);
        } else {
            mask.push(false);
        }
        Ok(())
    })?;
    if verdicts.next().is_some() {
        return Err(FormatError::Structural {
            rule: "null bitmap disagrees with present count",
        });
    }
    Ok(mask)
}

// ─── Numeric predicates ──────────────────────────────────────────────────────

/// One end of a numeric range filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumericBound {
    pub inclusive: bool,
    pub value: i128,
}

/// A filter over an integer or decimal column. Comparison values are i128; for Decimal128 columns they should be in the
/// same fixed scale as the stored mantissa so the comparison is exact.
///
/// Null rows never satisfy any of these — consistent with SQL semantics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NumericPredicate {
    Equals(i128),
    InSet(Vec<i128>),
    NotEquals(i128),
    Range {
        lower: Option<NumericBound>,
        upper: Option<NumericBound>,
    },
}

impl NumericPredicate {
    fn matches_i128(&self, v: i128) -> bool {
        match self {
            NumericPredicate::Equals(n) => v == *n,
            NumericPredicate::InSet(set) => set.contains(&v),
            NumericPredicate::NotEquals(n) => v != *n,
            NumericPredicate::Range { lower, upper } => {
                let above = match lower {
                    Some(b) if b.inclusive => v >= b.value,
                    Some(b) => v > b.value,
                    None => true,
                };
                let below = match upper {
                    Some(b) if b.inclusive => v <= b.value,
                    Some(b) => v < b.value,
                    None => true,
                };
                above && below
            }
        }
    }

    /// Whether a value strictly greater than `i128::MAX` — a `u128` that does not fit in `i128` — satisfies this
    /// predicate. Such a value behaves like +∞ against any i128 comparison: it equals no target, clears every lower
    /// bound, and overshoots every upper bound. So it matches `!=` and a lower-only range, and fails `=`, `IN`, and any
    /// range that carries an upper bound. Collapsing it to `i128::MAX` instead would drop it from a lower-only range
    /// (a false negative under an exact pushdown) and let it slip past a `= i128::MAX` probe.
    fn matches_overflowing_u128(&self) -> bool {
        match self {
            NumericPredicate::Equals(_) | NumericPredicate::InSet(_) => false,
            NumericPredicate::NotEquals(_) => true,
            NumericPredicate::Range { upper, .. } => upper.is_none(),
        }
    }

    /// Filters already-decoded column data. This is the reference path the compressed-data kernels must agree with.
    ///
    /// The predicate is translated into merged, sorted ranges once ([`NativeRangeTest::translate`]) rather than
    /// dispatched on the predicate's own enum — and, for an `IN` set, binary-searched instead of scanned — per row.
    pub fn filter_decoded(&self, data: &ColumnData) -> Vec<bool> {
        let test = NativeRangeTest::translate(self);
        match data {
            ColumnData::U64(values) => values.iter().map(|v| test.matches(*v as i128)).collect(),
            ColumnData::I64(values) => values.iter().map(|v| test.matches(*v as i128)).collect(),
            ColumnData::Decimal { values, .. } => values.iter().map(|v| test.matches(*v)).collect(),
            ColumnData::U128(values) => values
                .iter()
                .map(|v| match i128::try_from(*v) {
                    Ok(v) => test.matches(v),
                    Err(_) => self.matches_overflowing_u128(),
                })
                .collect(),
            ColumnData::F64(_) | ColumnData::Strings(_) => vec![false; data.row_count()],
        }
    }
}

/// One end of a float range filter.
#[derive(Clone, Debug, PartialEq)]
pub struct FloatBound {
    pub inclusive: bool,
    pub value: f64,
}

/// A filter over an f64 column (ALP-encoded or plain).
///
/// Null rows never satisfy any of these.
#[derive(Clone, Debug, PartialEq)]
pub enum FloatPredicate {
    Equals(f64),
    InSet(Vec<f64>),
    NotEquals(f64),
    Range {
        lower: Option<FloatBound>,
        upper: Option<FloatBound>,
    },
}

impl FloatPredicate {
    fn matches_f64(&self, v: f64) -> bool {
        match self {
            FloatPredicate::Equals(n) => v == *n,
            FloatPredicate::InSet(set) => set.contains(&v),
            FloatPredicate::NotEquals(n) => v != *n,
            FloatPredicate::Range { lower, upper } => {
                let above = match lower {
                    Some(b) if b.inclusive => v >= b.value,
                    Some(b) => v > b.value,
                    None => true,
                };
                let below = match upper {
                    Some(b) if b.inclusive => v <= b.value,
                    Some(b) => v < b.value,
                    None => true,
                };
                above && below
            }
        }
    }

    /// Filters already-decoded f64 values — the reference path.
    pub fn filter_decoded(&self, values: &[f64]) -> Vec<bool> {
        values.iter().map(|v| self.matches_f64(*v)).collect()
    }
}

/// One inclusive interval of matching packed lanes, in the domain its compare runs in.
///
/// Variants are in strict alphabetical order.
enum PackedLaneRange {
    /// The packed delta itself lies in `[lo, hi]` — the block's base is already folded into the bounds, so the lane is
    /// compared as unpacked.
    Delta { hi: u64, lo: u64 },
    /// The zigzag code `base + delta` lies in the even interval (a non-negative value) or the odd interval (a negative
    /// value), whichever matches the code's parity — the two halves a signed range splits into under the zigzag
    /// mapping, which interleaves the signs.
    Zigzag { even: (u64, u64), odd: (u64, u64) },
}

/// Whether `value` falls in any of `intervals` — sorted ascending by `lo` and merged so no two overlap or touch,
/// which turns membership into one binary search instead of a linear scan.
fn interval_contains(intervals: &[(u64, u64)], value: u64) -> bool {
    let idx = intervals.partition_point(|&(lo, _)| lo <= value);
    idx.checked_sub(1)
        .and_then(|i| intervals.get(i))
        .is_some_and(|&(_, hi)| value <= hi)
}

/// Sorts `intervals` by `lo` and merges every overlapping or adjacent pair into one, so a later membership test walks
/// a minimal, non-overlapping, ascending set instead of the original (possibly overlapping) list.
fn merge_intervals(mut intervals: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    intervals.sort_unstable_by_key(|&(lo, _)| lo);
    let mut merged: Vec<(u64, u64)> = Vec::with_capacity(intervals.len());
    for (lo, hi) in intervals {
        match merged.last_mut() {
            Some((_, last_hi)) if lo <= last_hi.saturating_add(1) => {
                if hi > *last_hi {
                    *last_hi = hi;
                }
            }
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// A [`PackedLaneTest`]'s ranges, merged once at translate time so a lane's verdict is a couple of binary searches
/// instead of unpacking the stream once per range and OR-ing the results (what an `IN` list with many values used to
/// cost: one full pass and one scratch allocation per value).
///
/// `delta` compares directly against the unpacked lane delta — the domain [`PackedLaneRange::Delta`] ranges over.
/// `even`/`odd` compare against `base + delta` split by parity — the domain [`PackedLaneRange::Zigzag`] ranges over.
/// Both can be non-empty at once ([`PackedLaneTest::translate`] can mix single-point `Delta` ranges with one wider
/// `Zigzag` range for the same predicate), so a lane matches when either domain's search hits.
///
/// Fields are in strict alphabetical order.
struct MergedLaneRanges {
    delta: Vec<(u64, u64)>,
    even: Vec<(u64, u64)>,
    odd: Vec<(u64, u64)>,
}

impl MergedLaneRanges {
    fn from_ranges(ranges: Vec<PackedLaneRange>) -> Self {
        let mut delta = Vec::new();
        let mut even = Vec::new();
        let mut odd = Vec::new();
        for range in ranges {
            match range {
                PackedLaneRange::Delta { hi, lo } => delta.push((lo, hi)),
                PackedLaneRange::Zigzag { even: e, odd: o } => {
                    // `(1, 0)` marks an empty half (see `zigzag_lane_range`); skip it rather than merge it in, or a
                    // real interval later sorted next to it could get silently widened to start at its bogus `lo`.
                    if e.0 <= e.1 {
                        even.push(e);
                    }
                    if o.0 <= o.1 {
                        odd.push(o);
                    }
                }
            }
        }
        Self {
            delta: merge_intervals(delta),
            even: merge_intervals(even),
            odd: merge_intervals(odd),
        }
    }

    /// Whether one already-unpacked lane value falls in these ranges — the scalar sibling of the per-lane closure
    /// [`PackedLaneTest::apply`] runs, reused where there is no packed stream to unpack lane-by-lane (a DELTA block's
    /// running prefix sum, tested one reconstructed value at a time).
    fn contains(&self, base: u64, delta: u64) -> bool {
        if interval_contains(&self.delta, delta) {
            return true;
        }
        let code = base.wrapping_add(delta);
        if code & 1 == 0 {
            interval_contains(&self.even, code)
        } else {
            interval_contains(&self.odd, code)
        }
    }
}

/// A numeric predicate translated once into the packed-lane domain of a FOR block, so answering it is a compare per
/// lane — never a per-row reconstruction and predicate dispatch. Requirement: "Compressed-data numeric predicates" —
/// range and equality are answered "by comparing the packed lanes against the block's base ... without fully unpacking
/// the block".
///
/// Fields are in strict alphabetical order.
struct PackedLaneTest {
    /// Flips the verdicts after the ranges are OR'd: a `!=` is the negated `=`, including when the compared value is
    /// unrepresentable in the column (nothing equals it, so everything differs).
    negate: bool,
    ranges: MergedLaneRanges,
}

impl PackedLaneTest {
    /// Translates `predicate` against the block's `base`: equality and membership become lane points, a range becomes
    /// one lane interval (u64 column) or a parity-split pair of zigzag-code intervals (i64 column) — merged and sorted
    /// once here, so answering a lane is a couple of binary searches rather than testing every range in turn. Valid
    /// only for a block whose `base + delta` cannot overflow — the caller routes a base without that headroom to the
    /// checked scalar walk instead.
    fn translate(predicate: &NumericPredicate, kind: ValueKind, base: u64) -> Self {
        if kind == ValueKind::I64 {
            let (value_ranges, negate) = i64_value_ranges(predicate);
            let ranges = value_ranges
                .into_iter()
                .filter_map(|range| zigzag_lane_range(base, range))
                .collect();
            Self {
                negate,
                ranges: MergedLaneRanges::from_ranges(ranges),
            }
        } else {
            let (value_ranges, negate) = u64_value_ranges(predicate);
            let ranges = value_ranges
                .into_iter()
                .filter_map(|range| delta_lane_range(base, range))
                .collect();
            Self {
                negate,
                ranges: MergedLaneRanges::from_ranges(ranges),
            }
        }
    }

    /// Fills `out` (the stream's padded length) with one verdict per lane: whether the lane satisfies the translated
    /// predicate, in one pass over the stream regardless of how many ranges (e.g. `IN` values) translated it.
    fn apply(&self, stream: &PackedStream<'_>, base: u64, out: &mut [bool]) {
        stream.unpack_into(out, |delta| self.ranges.contains(base, delta));
        if self.negate {
            for slot in out.iter_mut() {
                *slot = !*slot;
            }
        }
    }

    /// Whether one already-reconstructed value satisfies the translated predicate — the scalar sibling of [`apply`],
    /// used to test a value a caller already has in hand (a DELTA block's running prefix sum) rather than a whole
    /// packed stream.
    fn matches(&self, base: u64, value: u64) -> bool {
        self.ranges.contains(base, value) != self.negate
    }
}

/// Inclusive u64 value intervals satisfying `predicate` — the value domain of a u64 FOR block — plus whether the
/// verdict is negated afterwards. A comparison value no u64 can equal yields no interval, which negation then turns
/// into "every value", exactly `matches_i128`'s answer for `!=`.
fn u64_value_ranges(predicate: &NumericPredicate) -> (Vec<(u64, u64)>, bool) {
    fn point(n: i128) -> Option<(u64, u64)> {
        u64::try_from(n).ok().map(|value| (value, value))
    }
    match predicate {
        NumericPredicate::Equals(n) => (point(*n).into_iter().collect(), false),
        NumericPredicate::InSet(set) => (set.iter().copied().filter_map(point).collect(), false),
        NumericPredicate::NotEquals(n) => (point(*n).into_iter().collect(), true),
        NumericPredicate::Range { lower, upper } => {
            let lo = match lower {
                None => Some(0),
                Some(bound) => {
                    let first = if bound.inclusive {
                        bound.value
                    } else {
                        bound.value.saturating_add(1)
                    };
                    if first > i128::from(u64::MAX) {
                        None
                    } else {
                        Some(first.max(0) as u64)
                    }
                }
            };
            let hi = match upper {
                None => Some(u64::MAX),
                Some(bound) => {
                    let last = if bound.inclusive {
                        bound.value
                    } else {
                        bound.value.saturating_sub(1)
                    };
                    if last < 0 {
                        None
                    } else {
                        Some(last.min(i128::from(u64::MAX)) as u64)
                    }
                }
            };
            match (lo, hi) {
                (Some(lo), Some(hi)) if lo <= hi => (vec![(lo, hi)], false),
                _ => (Vec::new(), false),
            }
        }
    }
}

/// Inclusive i64 value intervals satisfying `predicate` — the value domain of an i64 FOR block — plus whether the
/// verdict is negated afterwards, mirroring [`u64_value_ranges`].
fn i64_value_ranges(predicate: &NumericPredicate) -> (Vec<(i64, i64)>, bool) {
    fn point(n: i128) -> Option<(i64, i64)> {
        i64::try_from(n).ok().map(|value| (value, value))
    }
    match predicate {
        NumericPredicate::Equals(n) => (point(*n).into_iter().collect(), false),
        NumericPredicate::InSet(set) => (set.iter().copied().filter_map(point).collect(), false),
        NumericPredicate::NotEquals(n) => (point(*n).into_iter().collect(), true),
        NumericPredicate::Range { lower, upper } => {
            let lo = match lower {
                None => Some(i64::MIN),
                Some(bound) => {
                    let first = if bound.inclusive {
                        bound.value
                    } else {
                        bound.value.saturating_add(1)
                    };
                    if first > i128::from(i64::MAX) {
                        None
                    } else {
                        Some(first.max(i128::from(i64::MIN)) as i64)
                    }
                }
            };
            let hi = match upper {
                None => Some(i64::MAX),
                Some(bound) => {
                    let last = if bound.inclusive {
                        bound.value
                    } else {
                        bound.value.saturating_sub(1)
                    };
                    if last < i128::from(i64::MIN) {
                        None
                    } else {
                        Some(last.min(i128::from(i64::MAX)) as i64)
                    }
                }
            };
            match (lo, hi) {
                (Some(lo), Some(hi)) if lo <= hi => (vec![(lo, hi)], false),
                _ => (Vec::new(), false),
            }
        }
    }
}

/// Rebases one u64 value interval onto the packed deltas (`delta = value - base`); `None` when the whole interval sits
/// below the base, so no lane can reach it.
fn delta_lane_range(base: u64, (lo, hi): (u64, u64)) -> Option<PackedLaneRange> {
    if hi < base {
        return None;
    }
    Some(PackedLaneRange::Delta {
        hi: hi - base,
        lo: lo.saturating_sub(base),
    })
}

/// Turns one i64 value interval into its packed-lane form for a zigzag-mapped column: a single value pins one zigzag
/// code (rebased onto the deltas), while a wider interval splits into the even codes of its non-negative half and the
/// odd codes of its negative half. An empty half is encoded as the never-matching interval `(1, 0)`.
fn zigzag_lane_range(base: u64, (lo, hi): (i64, i64)) -> Option<PackedLaneRange> {
    if lo == hi {
        let delta = zigzag(lo).checked_sub(base)?;
        return Some(PackedLaneRange::Delta { hi: delta, lo: delta });
    }
    let even = if hi >= 0 {
        ((lo.max(0) as u64) * 2, (hi as u64) * 2)
    } else {
        (1, 0)
    };
    let odd = if lo < 0 {
        let nearest = hi.min(-1);
        ((-2 * i128::from(nearest) - 1) as u64, (-2 * i128::from(lo) - 1) as u64)
    } else {
        (1, 0)
    };
    Some(PackedLaneRange::Zigzag { even, odd })
}

/// Answers `predicate` over a FOR bit-packed integer block by comparing the packed lanes against the block's base and
/// the rescaled bounds: the predicate is translated into the packed domain once, then every FastLanes vector unpacks
/// straight into one verdict per row through the runtime-dispatched SIMD kernel — the decoded values are never
/// materialised. `kind` says how the lanes map to the comparison domain: identity for a `u64` column, zigzag codes for
/// an `i64` column.
fn filter_for_block(body: &[u8], predicate: &NumericPredicate, kind: ValueKind) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let base = reader.u64("for base")?;
    let stream = read_packed_stream(&mut reader)?;
    if stream.width == 0 {
        // Every lane is zero, so the whole block carries one value: the base.
        let value = if kind == ValueKind::I64 {
            i128::from(unzigzag(base))
        } else {
            i128::from(base)
        };
        return Ok(vec![predicate.matches_i128(value); stream.count]);
    }
    if base.checked_add(mask(stream.width)).is_none() {
        // A base this close to u64::MAX leaves room for a packed delta to carry it past the top. Only these (forged)
        // blocks pay the per-value checked walk, mirroring `decode_for_bitpack`; every other base makes the overflow
        // unreachable, which is what keeps the translated fast path free of it.
        let to_i128: fn(u64) -> i128 = if kind == ValueKind::I64 {
            |value| i128::from(unzigzag(value))
        } else {
            i128::from
        };
        let mut deltas = vec![0u64; stream.padded_count()];
        stream.unpack_into(&mut deltas, |packed| packed);
        deltas.truncate(stream.count);
        return deltas
            .into_iter()
            .map(|delta| {
                base.checked_add(delta)
                    .map(|value| predicate.matches_i128(to_i128(value)))
                    .ok_or(FormatError::Structural {
                        rule: "frame-of-reference overflow",
                    })
            })
            .collect();
    }
    let test = PackedLaneTest::translate(predicate, kind, base);
    let mut verdicts = vec![false; stream.padded_count()];
    test.apply(&stream, base, &mut verdicts);
    verdicts.truncate(stream.count);
    Ok(verdicts)
}

/// Reads a DELTA bit-packed integer block and applies `predicate` to each value, rebuilding the running prefix sum
/// from `first` plus the zigzag deltas and comparing it against `predicate` translated once into the same domain
/// ([`PackedLaneTest::translate`], with no base to rebase against — the running sum already lands in that domain) —
/// a native compare per row, never a per-row `i128` widening or a predicate re-dispatch.
fn filter_delta_block(body: &[u8], predicate: &NumericPredicate, kind: ValueKind) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let first = reader.u64("delta first")?;
    let deltas = super::bitunpack(&mut reader)?;
    let test = PackedLaneTest::translate(predicate, kind, 0);
    let mut result = Vec::with_capacity(deltas.len() + 1);
    let mut current = first;
    result.push(test.matches(0, current));
    for delta in deltas {
        current = current.wrapping_add(unzigzag(delta) as u64);
        result.push(test.matches(0, current));
    }
    Ok(result)
}

/// Reads a Decimal128 block and applies `predicate` to each stored mantissa directly, never converting to a decimal
/// or float — comparing against `predicate` translated once into native `i128` intervals ([`NativeRangeTest`])
/// instead of re-dispatching the predicate's enum per row.
fn filter_decimal128_block(body: &[u8], predicate: &NumericPredicate) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let _scale = reader.u8("decimal scale")?;
    let count = reader.u32("decimal count")? as usize;
    let test = NativeRangeTest::translate(predicate);
    // One bulk take plus a chunked walk over the contiguous fixed-width run, rather than a bounds-checked reader call
    // per mantissa.
    let bytes = reader.take(count.saturating_mul(16), "decimal mantissa")?;
    Ok(bytes
        .chunks_exact(16)
        .map(|chunk| test.matches(i128::from_le_bytes(chunk.try_into().unwrap_or([0; 16]))))
        .collect())
}

/// Sorts `intervals` by `lo` and merges every overlapping or adjacent pair into one — the `i128` domain's counterpart
/// to [`merge_intervals`].
fn merge_intervals_i128(mut intervals: Vec<(i128, i128)>) -> Vec<(i128, i128)> {
    intervals.sort_unstable_by_key(|&(lo, _)| lo);
    let mut merged: Vec<(i128, i128)> = Vec::with_capacity(intervals.len());
    for (lo, hi) in intervals {
        match merged.last_mut() {
            Some((_, last_hi)) if lo <= last_hi.saturating_add(1) => {
                if hi > *last_hi {
                    *last_hi = hi;
                }
            }
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

/// Whether `value` falls in any of `intervals` — the `i128` domain's counterpart to [`interval_contains`].
fn interval_contains_i128(intervals: &[(i128, i128)], value: i128) -> bool {
    let idx = intervals.partition_point(|&(lo, _)| lo <= value);
    idx.checked_sub(1)
        .and_then(|i| intervals.get(i))
        .is_some_and(|&(_, hi)| value <= hi)
}

/// A numeric predicate translated once into merged, sorted inclusive `i128` intervals — for a domain that needs no
/// zigzag or frame-of-reference rebasing before comparing, unlike [`PackedLaneTest`] (a Decimal128 mantissa is
/// already the native comparison domain). Mirrors [`NumericPredicate::matches_i128`], but pays the enum dispatch and
/// bound arithmetic once instead of on every row, and answers an `IN` set with a binary search instead of a scan.
struct NativeRangeTest {
    negate: bool,
    ranges: Vec<(i128, i128)>,
}

impl NativeRangeTest {
    fn translate(predicate: &NumericPredicate) -> Self {
        match predicate {
            NumericPredicate::Equals(n) => Self {
                negate: false,
                ranges: vec![(*n, *n)],
            },
            NumericPredicate::NotEquals(n) => Self {
                negate: true,
                ranges: vec![(*n, *n)],
            },
            NumericPredicate::InSet(set) => Self {
                negate: false,
                ranges: merge_intervals_i128(set.iter().map(|n| (*n, *n)).collect()),
            },
            NumericPredicate::Range { lower, upper } => {
                // `checked_add`/`checked_sub`, not `saturating_*`: an exclusive bound sitting on `i128::MIN`/`MAX`
                // can never be satisfied (no value is less than `MIN` or greater than `MAX`), and only `checked_*`
                // turning that into `None` — an always-empty range — keeps that edge exact instead of saturating
                // into a bound that admits the boundary value itself.
                let lo = match lower {
                    None => Some(i128::MIN),
                    Some(bound) if bound.inclusive => Some(bound.value),
                    Some(bound) => bound.value.checked_add(1),
                };
                let hi = match upper {
                    None => Some(i128::MAX),
                    Some(bound) if bound.inclusive => Some(bound.value),
                    Some(bound) => bound.value.checked_sub(1),
                };
                Self {
                    negate: false,
                    ranges: match (lo, hi) {
                        (Some(lo), Some(hi)) if lo <= hi => vec![(lo, hi)],
                        _ => Vec::new(),
                    },
                }
            }
        }
    }

    fn matches(&self, value: i128) -> bool {
        let hit = interval_contains_i128(&self.ranges, value);
        hit != self.negate
    }
}

/// Tries to answer `predicate` straight from an encoded integer or decimal column block, without decoding every row
/// first.
///
/// Returns `Some(mask)` — one bool per row — when the block's encoding supports a compressed-data fast path:
/// - `ForBitpack` U64/I64 blocks: translates the predicate against the block's base once and compares the packed
///   lanes, never materialising the decoded values.
/// - `DeltaBitpack` U64/I64 blocks: rebuilds the running prefix sum from the deltas and compares each value.
/// - `Decimal128` blocks: compares the stored integer mantissa directly.
///
/// Returns `None` when no fast path applies (e.g. RLE, PLAIN, a `u128`, or a float column), signalling the caller to
/// fall back to [`decode_block`] + [`NumericPredicate::filter_decoded`].
pub fn filter_numeric_block(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &NumericPredicate,
) -> Result<Option<Vec<bool>>, FormatError> {
    if !numeric_block_has_fast_path(pipeline)? {
        return Ok(None);
    }
    let body = super::remove_trailing(pipeline.compression()?, bytes)?;
    filter_numeric_block_with_body(pipeline, &body, predicate)
}

/// [`filter_numeric_block`] for a caller that already holds this block's decompressed body — reused, for instance,
/// across a descriptor call or a decode fallback on the same block — so the filter does not decompress it again.
pub fn filter_numeric_block_with_body(
    pipeline: PipelineId,
    body: &[u8],
    predicate: &NumericPredicate,
) -> Result<Option<Vec<bool>>, FormatError> {
    if !numeric_block_has_fast_path(pipeline)? {
        return Ok(None);
    }
    let kind = pipeline.value_kind()?;
    let transform = pipeline.transform()?;
    if transform == Transform::Decimal128 {
        return Ok(Some(filter_decimal128_block(body, predicate)?));
    }
    let mask = match transform {
        Transform::DeltaBitpack => filter_delta_block(body, predicate, kind)?,
        _ => filter_for_block(body, predicate, kind)?,
    };
    Ok(Some(mask))
}

/// Whether a numeric block's transform could possibly answer a predicate from its compressed bytes — cheap enough to
/// check before paying to strip trailing compression.
fn numeric_block_has_fast_path(pipeline: PipelineId) -> Result<bool, FormatError> {
    let kind = pipeline.value_kind()?;
    let transform = pipeline.transform()?;
    if matches!(kind, ValueKind::F64 | ValueKind::String) {
        return Ok(false);
    }
    Ok(transform == Transform::Decimal128
        || (matches!(transform, Transform::ForBitpack | Transform::DeltaBitpack)
            && matches!(kind, ValueKind::U64 | ValueKind::I64)))
}

/// Answers a numeric predicate, falling back to a full decode when no compressed-data fast path applies.
pub fn filter_numeric_block_or_decode(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &NumericPredicate,
) -> Result<Vec<bool>, FormatError> {
    if let Some(mask) = filter_numeric_block(pipeline, bytes, predicate)? {
        return Ok(mask);
    }
    let data = decode_block(pipeline, bytes)?;
    Ok(predicate.filter_decoded(&data))
}

/// The float a non-exception ALP row with scaled integer `int` decodes to — the exact reconstruction the boundary
/// searches probe.
fn alp_value(int: i64, power: f64) -> f64 {
    (int as f64) / power
}

/// Smallest scaled integer whose reconstruction satisfies `satisfied`, or `None` when none does. `satisfied` must be
/// false-then-true along the integers, which every "at least this float" comparison is: the integer→float conversion
/// and the division by a positive power both preserve order, so the reconstruction is monotone.
fn alp_first_int(power: f64, satisfied: impl Fn(f64) -> bool) -> Option<i64> {
    if !satisfied(alp_value(i64::MAX, power)) {
        return None;
    }
    if satisfied(alp_value(i64::MIN, power)) {
        return Some(i64::MIN);
    }
    let mut below = i64::MIN; // not satisfied
    let mut at = i64::MAX; // satisfied
    while i128::from(at) - i128::from(below) > 1 {
        let mid = ((i128::from(at) + i128::from(below)) / 2) as i64;
        if satisfied(alp_value(mid, power)) {
            at = mid;
        } else {
            below = mid;
        }
    }
    Some(at)
}

/// Largest scaled integer whose reconstruction satisfies `satisfied`, or `None` when none does — the mirror of
/// [`alp_first_int`] for the true-then-false ("at most this float") comparisons.
fn alp_last_int(power: f64, satisfied: impl Fn(f64) -> bool) -> Option<i64> {
    if !satisfied(alp_value(i64::MIN, power)) {
        return None;
    }
    if satisfied(alp_value(i64::MAX, power)) {
        return Some(i64::MAX);
    }
    let mut at = i64::MIN; // satisfied
    let mut above = i64::MAX; // not satisfied
    while i128::from(above) - i128::from(at) > 1 {
        let mid = ((i128::from(above) + i128::from(at)) / 2) as i64;
        if satisfied(alp_value(mid, power)) {
            at = mid;
        } else {
            above = mid;
        }
    }
    Some(at)
}

/// The inclusive interval of scaled integers reconstructing to exactly `n`, or `None` when no integer does (including
/// `n` = NaN, which nothing equals). Usually a single integer; a flat stretch of the reconstruction (integers wide
/// enough to collapse in the conversion) yields the whole stretch.
fn alp_equal_range(n: f64, power: f64) -> Option<(i64, i64)> {
    let lo = alp_first_int(power, |value| value >= n)?;
    let hi = alp_last_int(power, |value| value <= n)?;
    (lo <= hi).then_some((lo, hi))
}

/// A float predicate rescaled once into an ALP block's scaled-integer domain: inclusive integer intervals whose union
/// (negated for `!=`) selects exactly the rows whose reconstructed float satisfies the predicate. The boundaries come
/// from monotone searches over the exact reconstruction, so the per-lane integer compare is the exact answer — not a
/// conservative prefilter needing a confirm pass.
///
/// Fields are in strict alphabetical order.
struct AlpIntTest {
    negate: bool,
    ranges: Vec<(i64, i64)>,
}

impl AlpIntTest {
    /// Rescales `predicate`'s float bounds into scaled-integer intervals against `power`.
    fn translate(predicate: &FloatPredicate, power: f64) -> Self {
        match predicate {
            FloatPredicate::Equals(n) => Self {
                negate: false,
                ranges: alp_equal_range(*n, power).into_iter().collect(),
            },
            FloatPredicate::InSet(set) => Self {
                negate: false,
                ranges: set.iter().filter_map(|n| alp_equal_range(*n, power)).collect(),
            },
            FloatPredicate::NotEquals(n) => Self {
                negate: true,
                ranges: alp_equal_range(*n, power).into_iter().collect(),
            },
            FloatPredicate::Range { lower, upper } => {
                let lo = match lower {
                    None => Some(i64::MIN),
                    Some(bound) if bound.inclusive => alp_first_int(power, |value| value >= bound.value),
                    Some(bound) => alp_first_int(power, |value| value > bound.value),
                };
                let hi = match upper {
                    None => Some(i64::MAX),
                    Some(bound) if bound.inclusive => alp_last_int(power, |value| value <= bound.value),
                    Some(bound) => alp_last_int(power, |value| value < bound.value),
                };
                let ranges = match (lo, hi) {
                    (Some(lo), Some(hi)) if lo <= hi => vec![(lo, hi)],
                    _ => Vec::new(),
                };
                Self { negate: false, ranges }
            }
        }
    }

    /// Fills `out` (the stream's padded length) with one verdict per lane: whether the lane's scaled integer falls in
    /// the translated intervals. The unzigzag and compare ride inside the unpack's lane writes, through the
    /// runtime-dispatched SIMD kernel.
    fn apply(&self, stream: &PackedStream<'_>, out: &mut [bool]) {
        match self.ranges.as_slice() {
            [] => out.fill(false),
            [(first_lo, first_hi), rest @ ..] => {
                let (first_lo, first_hi) = (*first_lo, *first_hi);
                stream.unpack_into(out, |packed| {
                    let int = unzigzag(packed);
                    first_lo <= int && int <= first_hi
                });
                if !rest.is_empty() {
                    let mut scratch = vec![false; out.len()];
                    for &(lo, hi) in rest {
                        stream.unpack_into(&mut scratch, |packed| {
                            let int = unzigzag(packed);
                            lo <= int && int <= hi
                        });
                        for (slot, extra) in out.iter_mut().zip(&scratch) {
                            *slot |= *extra;
                        }
                    }
                }
            }
        }
        if self.negate {
            for slot in out.iter_mut() {
                *slot = !*slot;
            }
        }
    }
}

/// Applies `predicate` to an ALP-encoded float block entirely in the scaled-integer domain: the float bounds are
/// rescaled once into exact integer intervals, so every packed lane is answered with an unzigzag and a compare and no
/// rejected float — in fact no float at all — is ever reconstructed. Exceptions are patched by index like
/// `decode_alp`, each judged on its stored bits; escaped vectors are judged on their raw payloads.
fn filter_alp_block(body: &[u8], side: SideStream, predicate: &FloatPredicate) -> Result<Vec<bool>, FormatError> {
    let mut reader = Reader::new(body);
    let mut exponent_index = reader.u8("alp exponent")? as usize;
    let mut escaped = Vec::new();
    if exponent_index == usize::from(super::ALP_VECTOR_ESCAPE_SENTINEL) {
        exponent_index = reader.u8("alp exponent")? as usize;
        escaped = super::read_alp_escaped_vectors(&mut reader)?;
    }
    let power = ALP_POWERS.get(exponent_index).copied().ok_or(FormatError::Structural {
        rule: "alp exponent out of range",
    })?;
    let exceptions = super::decode_alp_exceptions(&mut reader, side)?;
    let test = AlpIntTest::translate(predicate, power);
    let stream = read_packed_stream(&mut reader)?;
    let mut verdicts = vec![false; stream.padded_count()];
    test.apply(&stream, &mut verdicts);
    verdicts.truncate(stream.count);
    for (index, bits) in exceptions {
        let slot = verdicts.get_mut(index as usize).ok_or(FormatError::RefOutOfRange {
            what: "alp exception index",
        })?;
        *slot = predicate.matches_f64(f64::from_bits(bits));
    }
    // Escaped vectors store their raw floats after the packed words; judge each one directly, walking the payloads in
    // the same order `apply_alp_escapes` does.
    for vector_index in &escaped {
        let start = *vector_index as usize * FASTLANES_VECTOR;
        if start >= verdicts.len() {
            return Err(FormatError::RefOutOfRange {
                what: "alp escaped vector index",
            });
        }
        let end = (start + FASTLANES_VECTOR).min(verdicts.len());
        for slot in verdicts.iter_mut().take(end).skip(start) {
            *slot = predicate.matches_f64(f64::from_bits(reader.u64("alp escaped value")?));
        }
    }
    Ok(verdicts)
}

/// Tries to answer `predicate` from an ALP-encoded float block without decoding every row.
///
/// Returns `Some(mask)` for ALP-encoded F64 blocks. Returns `None` for plain F64 or any non-float block, signalling the
/// caller to fall back to [`decode_block`] + [`FloatPredicate::filter_decoded`].
pub fn filter_float_block(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &FloatPredicate,
) -> Result<Option<Vec<bool>>, FormatError> {
    if !float_block_has_fast_path(pipeline)? {
        return Ok(None);
    }
    let body = super::remove_trailing(pipeline.compression()?, bytes)?;
    filter_float_block_with_body(pipeline, &body, predicate)
}

/// [`filter_float_block`] for a caller that already holds this block's decompressed body — reused, for instance,
/// across a descriptor call or a decode fallback on the same block — so the filter does not decompress it again.
pub fn filter_float_block_with_body(
    pipeline: PipelineId,
    body: &[u8],
    predicate: &FloatPredicate,
) -> Result<Option<Vec<bool>>, FormatError> {
    if !float_block_has_fast_path(pipeline)? {
        return Ok(None);
    }
    Ok(Some(filter_alp_block(body, pipeline.side_stream()?, predicate)?))
}

/// Whether a float block's transform could possibly answer a predicate from its compressed bytes — cheap enough to
/// check before paying to strip trailing compression.
fn float_block_has_fast_path(pipeline: PipelineId) -> Result<bool, FormatError> {
    Ok(pipeline.value_kind()? == ValueKind::F64 && pipeline.transform()? == Transform::Alp)
}

/// Answers a float predicate, falling back to a full decode when no compressed-data fast path applies.
pub fn filter_float_block_or_decode(
    pipeline: PipelineId,
    bytes: &[u8],
    predicate: &FloatPredicate,
) -> Result<Vec<bool>, FormatError> {
    if let Some(mask) = filter_float_block(pipeline, bytes, predicate)? {
        return Ok(mask);
    }
    match decode_block(pipeline, bytes)? {
        ColumnData::F64(values) => Ok(predicate.filter_decoded(&values)),
        _ => Err(FormatError::Structural {
            rule: "float predicate applied to a non-float column",
        }),
    }
}

#[cfg(test)]
#[path = "test/predicate.rs"]
mod tests;
