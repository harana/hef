//! Heavy skip indexes packaged as their own sealed objects, so a data file gains and loses indexes without a single
//! byte of it changing.
//!
//! An index artifact wraps one column's index (a path-presence filter, a membership filter, a range filter, a learned
//! position index, or a value bitmap) in a self-describing object: a header naming what it accelerates, a coverage
//! record naming exactly which rows of which file it speaks for, a directory-then-pages layout so a reader can load
//! the directory first and fetch only the pages a query touches, and a trailing BLAKE3 seal. The manifest references
//! artifacts by identity and hash; dropping one is omitting it from the next generation, and pruning falls back to
//! the footer-tier statistics with identical results.
//!
//! See: hef-query-metadata-and-indexes/spec.md

use super::{
    Exactness,
    bitmap::BitmapIndex,
    learned_position::LearnedPositionIndex,
    path_presence::{PathPresenceIndex, hash_path},
    probabilistic::{BinaryFuseFilter, SplitBlockBloomFilter},
    pruning::{FilterClause, PredicateKind, PruningTerm, Statistic},
    range_filter::RangeFilter,
};
use crate::error::FormatError;
use crate::events::variant::VariantValue;
use crate::file::bytes::{Reader, Writer, slice};
use crate::layout::reader::PayloadRead;
use hashbrown::HashSet;
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Magic the artifact object starts with.
pub const ARTIFACT_MAGIC: [u8; 4] = *b"HIA1";

/// Bit budget per key when a membership artifact falls back from binary fuse to split-block bloom.
const BLOOM_FALLBACK_BITS_PER_KEY: u32 = 16;

/// The false-positive rate recorded for the split-block bloom fallback, conservative for its 16-bit-per-key budget.
const BLOOM_FALLBACK_FALSE_POSITIVE_PPM: u32 = 1_000;

/// The index families an artifact may carry, pinned in pruning-value order by their discriminants: path presence
/// prunes the most queries per byte, the value bitmap the fewest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    BinaryFuse = 1,
    Bitmap = 5,
    LearnedPosition = 4,
    PathPresence = 0,
    RangeFilter = 3,
    SplitBlockBloom = 2,
}

impl ArtifactKind {
    fn from_u8(value: u8) -> Result<Self, FormatError> {
        Ok(match value {
            0 => ArtifactKind::PathPresence,
            1 => ArtifactKind::BinaryFuse,
            2 => ArtifactKind::SplitBlockBloom,
            3 => ArtifactKind::RangeFilter,
            4 => ArtifactKind::LearnedPosition,
            5 => ArtifactKind::Bitmap,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown index artifact kind",
                });
            }
        })
    }
}

/// What one artifact accelerates: the index family, the column it summarises, and how trustworthy its answers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactHeader {
    pub column_id: u32,
    /// True when the index answers with no false positives either, so its verdicts need no confirming read.
    pub exact: bool,
    /// Expected false-positive rate in parts per million; zero for an exact index.
    pub false_positive_ppm: u32,
    pub kind: ArtifactKind,
    pub projection_id: u32,
}

/// Exactly which rows of which file the artifact speaks for. A reader must refuse an artifact whose coverage does not
/// match what it is scanning — see [`IndexArtifact::usable_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactCoverage {
    /// The deletion-vector generation the index was built against. An exact index is invalidated by a newer
    /// generation (rows it points at may be gone); an inexact no-false-negative index survives, since deletions only
    /// remove rows.
    pub deletion_vector_generation: u64,
    pub file_id: u128,
    /// Inclusive granule ranges the artifact covers. Granules outside stay on the footer-tier statistics.
    pub granule_ranges: Vec<(u32, u32)>,
    pub schema_fingerprint: [u8; 32],
}

impl ArtifactCoverage {
    /// Whether `granule_id` is inside the covered ranges.
    pub fn covers(&self, granule_id: u32) -> bool {
        self.granule_ranges
            .iter()
            .any(|(first, last)| (*first..=*last).contains(&granule_id))
    }
}

/// One progressive-load page: the index bytes for an inclusive granule range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactPage {
    pub bytes: Vec<u8>,
    pub first_granule: u32,
    pub last_granule: u32,
}

/// One directory entry of an encoded artifact: where a page's bytes sit inside the pages area, decodable without
/// fetching any page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactPageEntry {
    pub first_granule: u32,
    pub last_granule: u32,
    pub len: u64,
    pub offset: u64,
}

/// One skip index sealed as its own object outside the data file: header, coverage, and per-granule-range pages.
#[derive(Debug)]
pub struct IndexArtifact {
    pub coverage: ArtifactCoverage,
    /// One decode slot per entry of `pages`, filled on first probe and reused afterwards, so probing one page for many
    /// granules and many clauses decodes it once. An accelerator only — `pages` stays the source of truth. Each slot is
    /// its own `OnceLock`, so concurrent probes landing on different pages never contend with each other — unlike a
    /// single mutex-guarded map, which would serialize every probe against every other regardless of which page it
    /// touched. Holds `None` for a page whose bytes fail to decode, so a corrupt page is not retried on every
    /// subsequent probe either.
    decoded_pages: Vec<OnceLock<Option<DecodedPage>>>,
    pub header: ArtifactHeader,
    /// Every page as first granule, last granule, and its position in `pages`, sorted by first granule so the page
    /// covering a granule is found by binary search. Built once with the artifact; scanning `pages` per probe instead
    /// costs a pass over every page, which with one page per granule is quadratic across a scan's granules.
    page_by_granule: Vec<(u32, u32, usize)>,
    pub pages: Vec<ArtifactPage>,
}

impl Clone for IndexArtifact {
    /// Copies the artifact's data. The clone starts with an empty decoded-page cache: the cache only saves repeated
    /// decodes, and the copy pays for exactly the pages it probes itself.
    fn clone(&self) -> Self {
        Self::new(self.coverage.clone(), self.header, self.pages.clone())
    }
}

impl PartialEq for IndexArtifact {
    /// Compares the artifact's data only — the decoded-page cache is derived from `pages`, so which pages happen to
    /// have been probed never makes two artifacts differ.
    fn eq(&self, other: &Self) -> bool {
        self.coverage == other.coverage && self.header == other.header && self.pages == other.pages
    }
}

/// One artifact page's index after decoding, held by the artifact so every later probe that lands on the same page
/// reuses it. A learned position index has no variant here: it can never falsify a clause, so it is never decoded for
/// pruning.
#[derive(Debug)]
enum DecodedPage {
    BinaryFuse(BinaryFuseFilter),
    Bitmap(BitmapIndex),
    PathPresence(PathPresenceIndex),
    RangeFilter(RangeFilter),
    SplitBlockBloom(SplitBlockBloomFilter),
}

impl DecodedPage {
    /// What this page's index concludes about one clause, or `None` when the clause's shape is one this index family
    /// cannot answer.
    fn statistic(&self, clause: &FilterClause) -> Option<Statistic> {
        Some(match self {
            DecodedPage::BinaryFuse(filter) => {
                let key = equality_key(clause)?;
                Statistic::PointMembership {
                    column_id: clause.column_id,
                    members: if filter.contains(key) {
                        vec![clause.value_lo]
                    } else {
                        Vec::new()
                    },
                }
            }
            DecodedPage::Bitmap(index) => {
                let key = equality_key(clause)?;
                let present = index.bitmap(key).is_some_and(|rows| !rows.is_empty());
                Statistic::PointMembership {
                    column_id: clause.column_id,
                    members: if present { vec![clause.value_lo] } else { Vec::new() },
                }
            }
            DecodedPage::PathPresence(index) => {
                if clause.predicate != PredicateKind::Presence {
                    return None;
                }
                let hash = u64::try_from(clause.value_lo).ok()?;
                Statistic::PathPresence {
                    column_id: clause.column_id,
                    present: index.might_contain_hash(hash),
                }
            }
            DecodedPage::RangeFilter(filter) => {
                let lo = u64::try_from(clause.value_lo.max(0)).ok()?;
                let hi = u64::try_from(clause.value_hi.max(0)).ok()?;
                Statistic::RangeEmptiness {
                    column_id: clause.column_id,
                    non_empty_spans: if filter.range_nonempty(lo, hi) {
                        vec![(clause.value_lo, clause.value_hi)]
                    } else {
                        Vec::new()
                    },
                }
            }
            DecodedPage::SplitBlockBloom(filter) => {
                let key = equality_key(clause)?;
                Statistic::PointMembership {
                    column_id: clause.column_id,
                    members: if filter.contains(key) {
                        vec![clause.value_lo]
                    } else {
                        Vec::new()
                    },
                }
            }
        })
    }
}

impl IndexArtifact {
    /// Assembles an artifact from its header, coverage, and pages, ready to encode or probe.
    pub fn new(coverage: ArtifactCoverage, header: ArtifactHeader, pages: Vec<ArtifactPage>) -> Self {
        let mut page_by_granule: Vec<(u32, u32, usize)> = pages
            .iter()
            .enumerate()
            .map(|(index, page)| (page.first_granule, page.last_granule, index))
            .collect();
        page_by_granule.sort_unstable();
        let decoded_pages = pages.iter().map(|_| OnceLock::new()).collect();
        Self {
            coverage,
            decoded_pages,
            header,
            page_by_granule,
            pages,
        }
    }

    /// Serializes the artifact: magic, header, coverage, page directory, pages area, and a trailing BLAKE3 seal over
    /// everything before it.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::new();
        out.put_slice(&ARTIFACT_MAGIC);
        out.put_u8(self.header.kind as u8);
        out.put_u32(self.header.column_id);
        out.put_u32(self.header.projection_id);
        out.put_u8(u8::from(self.header.exact));
        out.put_u32(self.header.false_positive_ppm);
        out.put_u128(self.coverage.file_id);
        out.put_u64(self.coverage.deletion_vector_generation);
        out.put_slice(&self.coverage.schema_fingerprint);
        out.put_u32(self.coverage.granule_ranges.len() as u32);
        for (first, last) in &self.coverage.granule_ranges {
            out.put_u32(*first);
            out.put_u32(*last);
        }
        out.put_u32(self.pages.len() as u32);
        let mut offset = 0u64;
        for page in &self.pages {
            out.put_u32(page.first_granule);
            out.put_u32(page.last_granule);
            out.put_u64(offset);
            out.put_u64(page.bytes.len() as u64);
            offset += page.bytes.len() as u64;
        }
        for page in &self.pages {
            out.put_slice(&page.bytes);
        }
        let mut bytes = out.into_bytes();
        let seal = crate::file::integrity::hash_tree(&bytes);
        bytes.extend_from_slice(seal.as_bytes());
        bytes
    }

    /// Deserializes and verifies a whole artifact, refusing on a broken seal or malformed structure.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() < 32 {
            return Err(FormatError::Truncated {
                what: "index artifact seal",
            });
        }
        let (sealed, seal) = bytes.split_at(bytes.len() - 32);
        if crate::file::integrity::hash_tree(sealed).as_bytes() != seal {
            return Err(FormatError::Structural {
                rule: "index artifact seal does not match its bytes",
            });
        }
        let (header, coverage, directory, pages_area) = decode_artifact_prefix(sealed)?;
        let mut pages = Vec::with_capacity(directory.len());
        for entry in directory {
            let page = slice(pages_area, entry.offset as usize, entry.len as usize, "artifact page")?;
            pages.push(ArtifactPage {
                bytes: page.to_vec(),
                first_granule: entry.first_granule,
                last_granule: entry.last_granule,
            });
        }
        Ok(Self::new(coverage, header, pages))
    }

    /// Whether this artifact may serve a scan of `file_id` under `schema_fingerprint` at `deletion_vector_generation`.
    /// A fingerprint or file mismatch is never usable; an exact index dies with a superseded deletion-vector
    /// generation, while an inexact no-false-negative index survives it (deletions only remove rows, so it still
    /// never reports a false negative).
    pub fn usable_for(&self, file_id: u128, schema_fingerprint: &[u8; 32], deletion_vector_generation: u64) -> bool {
        if self.coverage.file_id != file_id || self.coverage.schema_fingerprint != *schema_fingerprint {
            return false;
        }
        if self.header.exact {
            self.coverage.deletion_vector_generation == deletion_vector_generation
        } else {
            self.coverage.deletion_vector_generation <= deletion_vector_generation
        }
    }

    /// The page covering `granule_id`, when the artifact covers it at all.
    pub fn page_for(&self, granule_id: u32) -> Option<&ArtifactPage> {
        self.pages.get(self.page_index_for(granule_id)?)
    }

    /// The position in `pages` of the page covering `granule_id` — what the decoded-page cache is keyed by. Found by
    /// binary search over `page_by_granule`: the last page starting at or before the granule is the only one that can
    /// cover it, since pages partition the covered granules.
    fn page_index_for(&self, granule_id: u32) -> Option<usize> {
        let after = self.page_by_granule.partition_point(|(first, ..)| *first <= granule_id);
        let (_, last_granule, index) = *self.page_by_granule.get(after.checked_sub(1)?)?;
        (granule_id <= last_granule).then_some(index)
    }

    /// Builds the pruning term this artifact contributes for one clause against one granule, or `None` when the
    /// artifact does not cover the granule, does not falsify this clause shape, or its page fails to decode
    /// (an accelerator never turns into an error — the caller just keeps the footer-tier terms).
    ///
    /// The page backing the granule is decoded on its first probe and reused afterwards, so probing one page across
    /// many granules and many clauses costs one decode, not one per probe.
    pub fn term_for(&self, granule_id: u32, clause: &FilterClause) -> Option<PruningTerm> {
        if clause.column_id != self.header.column_id || !self.coverage.covers(granule_id) {
            return None;
        }
        let page_index = self.page_index_for(granule_id)?;
        let statistic = self.decoded_page(page_index)?.statistic(clause)?;
        let exactness = if self.header.exact {
            Exactness::Exact
        } else {
            Exactness::InexactNoFalseNegative
        };
        Some(PruningTerm {
            clause: *clause,
            exactness,
            statistic,
        })
    }

    /// The decoded index for one page, decoding it on first use and serving the held copy thereafter. `None` when the
    /// page's bytes fail to decode, or when the artifact is a learned position index — which accelerates seeks inside
    /// a granule and can never prove a value absent, so it contributes no falsification term.
    fn decoded_page(&self, page_index: usize) -> Option<&DecodedPage> {
        let slot = self.decoded_pages.get(page_index)?;
        let pages = &self.pages;
        let kind = self.header.kind;
        slot.get_or_init(move || {
            let bytes = &pages.get(page_index)?.bytes;
            Some(match kind {
                ArtifactKind::BinaryFuse => DecodedPage::BinaryFuse(BinaryFuseFilter::decode(bytes).ok()?),
                ArtifactKind::Bitmap => DecodedPage::Bitmap(BitmapIndex::decode(bytes).ok()?),
                ArtifactKind::LearnedPosition => return None,
                ArtifactKind::PathPresence => DecodedPage::PathPresence(PathPresenceIndex::decode(bytes).ok()?),
                ArtifactKind::RangeFilter => DecodedPage::RangeFilter(RangeFilter::decode(bytes).ok()?),
                ArtifactKind::SplitBlockBloom => {
                    DecodedPage::SplitBlockBloom(SplitBlockBloomFilter::decode(bytes).ok()?)
                }
            })
        })
        .as_ref()
    }

    /// How many of this artifact's pages are decoded and held. Never feeds a probe result; it lets tests observe that
    /// the probes landing on one page decode it once.
    pub fn decoded_page_count(&self) -> usize {
        self.decoded_pages
            .iter()
            .filter(|slot| matches!(slot.get(), Some(Some(_))))
            .count()
    }
}

/// The clause's single probe key, when it has one a membership filter can test: an equality (or degenerate
/// single-value membership) whose value fits the filters' u64 key space.
fn equality_key(clause: &FilterClause) -> Option<u64> {
    if !matches!(clause.predicate, PredicateKind::Equality | PredicateKind::Membership) {
        return None;
    }
    if clause.value_lo != clause.value_hi {
        return None;
    }
    u64::try_from(clause.value_lo).ok()
}

/// Decodes only the header, coverage, and page directory — everything before any page's bytes — so a reader can plan
/// which pages to fetch before paying for them. Structure only: the seal spans the whole object and is verified by
/// [`IndexArtifact::decode`] (or by the manifest's recorded hash at fetch time).
pub fn decode_artifact_directory(
    bytes: &[u8],
) -> Result<(ArtifactHeader, ArtifactCoverage, Vec<ArtifactPageEntry>), FormatError> {
    let (header, coverage, directory, _) = decode_artifact_prefix(bytes)?;
    Ok((header, coverage, directory))
}

fn decode_artifact_prefix(
    bytes: &[u8],
) -> Result<(ArtifactHeader, ArtifactCoverage, Vec<ArtifactPageEntry>, &[u8]), FormatError> {
    let mut reader = Reader::new(bytes);
    let magic = reader.take(4, "artifact magic")?;
    if magic != ARTIFACT_MAGIC {
        return Err(FormatError::Structural {
            rule: "index artifact magic mismatch",
        });
    }
    let kind = ArtifactKind::from_u8(reader.u8("artifact kind")?)?;
    let column_id = reader.u32("artifact column")?;
    let projection_id = reader.u32("artifact projection")?;
    let exact = reader.u8("artifact exactness")? != 0;
    let false_positive_ppm = reader.u32("artifact fpr")?;
    let file_id = reader.u128("artifact file id")?;
    let deletion_vector_generation = reader.u64("artifact dv generation")?;
    let mut schema_fingerprint = [0u8; 32];
    schema_fingerprint.copy_from_slice(reader.take(32, "artifact schema fingerprint")?);
    let range_count = reader.u32("artifact coverage range count")? as usize;
    let range_count = range_count.min(reader.remaining() / 8 + 1);
    let mut granule_ranges = Vec::with_capacity(range_count);
    for _ in 0..range_count {
        let first = reader.u32("artifact coverage first granule")?;
        let last = reader.u32("artifact coverage last granule")?;
        if last < first {
            return Err(FormatError::Structural {
                rule: "artifact coverage range is inverted",
            });
        }
        granule_ranges.push((first, last));
    }
    let page_count = reader.u32("artifact page count")? as usize;
    let page_count = page_count.min(reader.remaining() / 24 + 1);
    let mut directory = Vec::with_capacity(page_count);
    for _ in 0..page_count {
        let first_granule = reader.u32("artifact page first granule")?;
        let last_granule = reader.u32("artifact page last granule")?;
        let offset = reader.u64("artifact page offset")?;
        let len = reader.u64("artifact page length")?;
        if last_granule < first_granule {
            return Err(FormatError::Structural {
                rule: "artifact page range is inverted",
            });
        }
        directory.push(ArtifactPageEntry {
            first_granule,
            last_granule,
            len,
            offset,
        });
    }
    let pages_area = reader.take(reader.remaining(), "artifact pages area")?;
    Ok((
        ArtifactHeader {
            column_id,
            exact,
            false_positive_ppm,
            kind,
            projection_id,
        },
        ArtifactCoverage {
            deletion_vector_generation,
            file_id,
            granule_ranges,
            schema_fingerprint,
        },
        directory,
        pages_area,
    ))
}

/// Plans which artifacts a builder should produce for an opened file: one membership filter per promoted (hot)
/// column — the columns whose presence entries the footer records. Today this plans the membership kind only;
/// other kinds join the plan when their builders land.
pub fn plan_artifact_builds(file: &crate::layout::reader::HefFile) -> Vec<(ArtifactKind, u32)> {
    file.footer()
        .presence
        .iter()
        .map(|entry| (ArtifactKind::BinaryFuse, entry.column_id))
        .collect()
}

/// Builds one membership artifact — a filter page per granule — for a column of an opened, sealed file, touching
/// none of the file's bytes: values are read through the ordinary column reads, strings hash through the index
/// family's shared hash, and the result is sealed and ready to persist. Binary fuse is preferred; a key set the
/// peeling budget cannot handle falls the whole artifact back to split-block bloom (one kind per artifact), so no
/// key set is ever left without a sound filter. The caller stores the encoded artifact and references it from the
/// next manifest generation.
pub fn build_membership_artifact(
    file: &crate::layout::reader::HefFile,
    column_id: u32,
    deletion_vector_generation: u64,
) -> Result<IndexArtifact, FormatError> {
    let mut granule_keys: Vec<(u32, Vec<u64>)> = Vec::new();
    let mut granule_ranges: Vec<(u32, u32)> = Vec::new();
    for granule in &file.footer().granules {
        let read = file.read_column(column_id, granule.granule_id)?;
        let Some(keys) = membership_keys(&read.data) else {
            continue;
        };
        extend_granule_ranges(&mut granule_ranges, granule.granule_id);
        granule_keys.push((granule.granule_id, keys));
    }
    let page = |granule_id: u32, bytes: Vec<u8>| ArtifactPage {
        bytes,
        first_granule: granule_id,
        last_granule: granule_id,
    };
    let fuse: Result<Vec<ArtifactPage>, FormatError> = granule_keys
        .iter()
        .map(|(granule_id, keys)| BinaryFuseFilter::build(keys).map(|filter| page(*granule_id, filter.encode())))
        .collect();
    let (kind, false_positive_ppm, pages) = match fuse {
        Ok(pages) => (ArtifactKind::BinaryFuse, 4_000, pages),
        Err(_) => (
            ArtifactKind::SplitBlockBloom,
            BLOOM_FALLBACK_FALSE_POSITIVE_PPM,
            granule_keys
                .iter()
                .map(|(granule_id, keys)| {
                    page(
                        *granule_id,
                        SplitBlockBloomFilter::build(keys, BLOOM_FALLBACK_BITS_PER_KEY).encode(),
                    )
                })
                .collect(),
        ),
    };
    Ok(IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation,
            file_id: file.header().file_id,
            granule_ranges,
            schema_fingerprint: file.footer().schema_fingerprint,
        },
        ArtifactHeader {
            column_id,
            exact: false,
            false_positive_ppm,
            kind,
            projection_id: 0,
        },
        pages,
    ))
}

/// The filter keys one decoded column block contributes to a membership, range, or bitmap artifact, or `None` for a
/// column type the u64 key space cannot carry: hashed bytes for strings, the value itself for u64, the non-negative
/// values for i64.
fn membership_keys(data: &crate::encoding::ColumnData) -> Option<Vec<u64>> {
    Some(match data {
        crate::encoding::ColumnData::Strings(values) => values
            .iter()
            .flatten()
            .map(|value| super::stable_hash(value.as_bytes()))
            .collect(),
        crate::encoding::ColumnData::U64(values) => values.clone(),
        crate::encoding::ColumnData::I64(values) => {
            values.iter().filter_map(|value| u64::try_from(*value).ok()).collect()
        }
        _ => return None,
    })
}

/// Extends the coverage range list by one granule, merging it into the last range when contiguous.
fn extend_granule_ranges(ranges: &mut Vec<(u32, u32)>, granule_id: u32) {
    match ranges.last_mut() {
        Some((_, last)) if *last + 1 == granule_id => *last = granule_id,
        _ => ranges.push((granule_id, granule_id)),
    }
}

/// The recorded false-positive rate of a path-presence page: ten filter bits per path is roughly a 1% rate (see
/// `path_presence::BITS_PER_PATH`).
const PATH_PRESENCE_FALSE_POSITIVE_PPM: u32 = 10_000;

/// Conservative recorded false-positive rate for a range-filter page: a granule's keys occupy well under 1% of the
/// default 4096-bucket space, so a random query range that misses every real key rarely lands in an occupied bucket.
const RANGE_FILTER_FALSE_POSITIVE_PPM: u32 = 10_000;

/// Collects the [`hash_path`] hash of every payload path present in `value` in the canonical dotted form — top-level
/// fields and nested object fields alike (`attributes`, `attributes.revenue`, `attributes.revenue.amount`).
///
/// `prefix` is a scratch buffer the walk extends and truncates in place; `out` is keyed by the path's hash, which is
/// all the filter this builds ever stores, so a repeated path across a granule's rows costs one hash-set probe and
/// never a string clone.
fn collect_payload_paths(prefix: &mut String, value: &VariantValue, out: &mut HashSet<u64>) {
    let VariantValue::Object(fields) = value else {
        return;
    };
    for (key, field_value) in fields {
        let parent_len = prefix.len();
        if parent_len > 0 {
            prefix.push('.');
        }
        prefix.push_str(key);
        collect_payload_paths(prefix, field_value, out);
        out.insert(hash_path(prefix));
        prefix.truncate(parent_len);
    }
}

/// Builds the path-presence artifact for a sealed file — one filter page per granule over the payload paths its rows
/// actually carry — touching none of the file's bytes. First in the build-priority order: it prunes whole granules for
/// the dominant "does this path appear at all" predicates. The header's column is the payload column
/// (`column_ids::PAYLOAD_REF`), the anchor a path-presence clause names; the probe key is the hashed dotted path.
pub fn build_path_presence_artifact(
    file: &crate::layout::reader::HefFile,
    deletion_vector_generation: u64,
) -> Result<IndexArtifact, FormatError> {
    let mut pages = Vec::new();
    let mut granule_ranges: Vec<(u32, u32)> = Vec::new();
    for granule in &file.footer().granules {
        let mut path_hashes: HashSet<u64> = HashSet::new();
        let mut path = String::new();
        for row in granule.first_row_ordinal..granule.first_row_ordinal + u64::from(granule.row_count) {
            if let PayloadRead::Value(value) = file.payload(row)? {
                collect_payload_paths(&mut path, &value, &mut path_hashes);
            }
        }
        let hashes: Vec<u64> = path_hashes.into_iter().collect();
        extend_granule_ranges(&mut granule_ranges, granule.granule_id);
        pages.push(ArtifactPage {
            bytes: PathPresenceIndex::build(&hashes).encode(),
            first_granule: granule.granule_id,
            last_granule: granule.granule_id,
        });
    }
    Ok(IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation,
            file_id: file.header().file_id,
            granule_ranges,
            schema_fingerprint: file.footer().schema_fingerprint,
        },
        ArtifactHeader {
            column_id: crate::columns::column_ids::PAYLOAD_REF,
            exact: false,
            false_positive_ppm: PATH_PRESENCE_FALSE_POSITIVE_PPM,
            kind: ArtifactKind::PathPresence,
            projection_id: 0,
        },
        pages,
    ))
}

/// Builds the range-emptiness artifact for one column of a sealed file — one Grafite-style filter page per granule —
/// touching none of the file's bytes. Keys hash and map exactly as the membership builders' do, so a range probe and a
/// point probe over the same column agree on the key space.
pub fn build_range_filter_artifact(
    file: &crate::layout::reader::HefFile,
    column_id: u32,
    deletion_vector_generation: u64,
) -> Result<IndexArtifact, FormatError> {
    let mut pages = Vec::new();
    let mut granule_ranges: Vec<(u32, u32)> = Vec::new();
    for granule in &file.footer().granules {
        let read = file.read_column(column_id, granule.granule_id)?;
        let Some(keys) = membership_keys(&read.data) else {
            continue;
        };
        extend_granule_ranges(&mut granule_ranges, granule.granule_id);
        pages.push(ArtifactPage {
            bytes: RangeFilter::build(&keys).encode(),
            first_granule: granule.granule_id,
            last_granule: granule.granule_id,
        });
    }
    Ok(IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation,
            file_id: file.header().file_id,
            granule_ranges,
            schema_fingerprint: file.footer().schema_fingerprint,
        },
        ArtifactHeader {
            column_id,
            exact: false,
            false_positive_ppm: RANGE_FILTER_FALSE_POSITIVE_PPM,
            kind: ArtifactKind::RangeFilter,
            projection_id: 0,
        },
        pages,
    ))
}

/// Builds the learned-position artifact for one sorted i64 column of a sealed file: a single page holding the
/// piecewise linear model over the whole file's rows, touching none of the file's bytes. Refuses when the column's
/// values are not non-decreasing in row order — the model is only valid under a covering sortedness proof, and an
/// unsorted column would silently produce windows that miss the true boundary.
pub fn build_learned_position_artifact(
    file: &crate::layout::reader::HefFile,
    column_id: u32,
    deletion_vector_generation: u64,
    error_bound: u64,
) -> Result<IndexArtifact, FormatError> {
    let mut keys: Vec<i64> = Vec::new();
    let mut granule_ranges: Vec<(u32, u32)> = Vec::new();
    for granule in &file.footer().granules {
        let read = file.read_column(column_id, granule.granule_id)?;
        let crate::encoding::ColumnData::I64(values) = &read.data else {
            return Err(FormatError::Structural {
                rule: "a learned position artifact models an i64 column",
            });
        };
        extend_granule_ranges(&mut granule_ranges, granule.granule_id);
        keys.extend_from_slice(values);
    }
    if keys.windows(2).any(|pair| pair[1] < pair[0]) {
        return Err(FormatError::Structural {
            rule: "a learned position artifact requires the column sorted ascending",
        });
    }
    let (first_granule, last_granule) = (
        granule_ranges.first().map_or(0, |range| range.0),
        granule_ranges.last().map_or(0, |range| range.1),
    );
    Ok(IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation,
            file_id: file.header().file_id,
            granule_ranges,
            schema_fingerprint: file.footer().schema_fingerprint,
        },
        ArtifactHeader {
            column_id,
            exact: false,
            false_positive_ppm: 0,
            kind: ArtifactKind::LearnedPosition,
            projection_id: 0,
        },
        vec![ArtifactPage {
            bytes: LearnedPositionIndex::build(column_id, 0, &keys, error_bound).encode(),
            first_granule,
            last_granule,
        }],
    ))
}

/// Builds the exact bitmap artifact for one low-cardinality column of a sealed file — one value-to-rows bitmap index
/// page per granule (row ids granule-relative), touching none of the file's bytes. Exact, so a superseded
/// deletion-vector generation retires it until rebuilt (see [`IndexArtifact::usable_for`]).
pub fn build_bitmap_artifact(
    file: &crate::layout::reader::HefFile,
    column_id: u32,
    deletion_vector_generation: u64,
) -> Result<IndexArtifact, FormatError> {
    let mut pages = Vec::new();
    let mut granule_ranges: Vec<(u32, u32)> = Vec::new();
    for granule in &file.footer().granules {
        let read = file.read_column(column_id, granule.granule_id)?;
        let Some(keys) = membership_keys(&read.data) else {
            continue;
        };
        // Map each stored value back to its granule-relative row: stored position i is row i for a dense column, or
        // the i-th set presence bit for a presence-gated one.
        let rows: Vec<u64> = if read.presence.is_empty() {
            (0..keys.len() as u64).collect()
        } else {
            (0..u64::from(granule.row_count))
                .filter(|row| {
                    read.presence
                        .get((row / 8) as usize)
                        .is_some_and(|byte| byte & (1 << (row % 8)) != 0)
                })
                .collect()
        };
        // Group the granule's rows by value first, then record each value once: inserting row by row would rebuild
        // that value's bitmap on every row, which is quadratic in exactly the dense low-cardinality columns this
        // artifact targets.
        let mut rows_by_value: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (key, row) in keys.iter().zip(rows) {
            rows_by_value.entry(*key).or_default().push(row);
        }
        let mut index = BitmapIndex::new();
        for (value, value_rows) in rows_by_value {
            index.insert(value, value_rows);
        }
        extend_granule_ranges(&mut granule_ranges, granule.granule_id);
        pages.push(ArtifactPage {
            bytes: index.encode(),
            first_granule: granule.granule_id,
            last_granule: granule.granule_id,
        });
    }
    Ok(IndexArtifact::new(
        ArtifactCoverage {
            deletion_vector_generation,
            file_id: file.header().file_id,
            granule_ranges,
            schema_fingerprint: file.footer().schema_fingerprint,
        },
        ArtifactHeader {
            column_id,
            exact: true,
            false_positive_ppm: 0,
            kind: ArtifactKind::Bitmap,
            projection_id: 0,
        },
        pages,
    ))
}

#[cfg(test)]
#[path = "test/artifact.rs"]
mod tests;
