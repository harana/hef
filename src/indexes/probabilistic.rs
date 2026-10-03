//! Point-membership filters that answer "could this block hold key K?" cheaply and, crucially, never say "no" about a
//! key that is actually present.
//!
//! These are the small acceleration structures a writer attaches to a high-cardinality lookup column (an entity id, an
//! account id, an event id) so a reader can skip a whole block when none of the keys it asks for could be inside. Every
//! filter here is *one-sided*: it may occasionally claim a key is present when it is not (a false positive, which only
//! costs a wasted read), but it will never claim a key is absent when it is present (a false negative, which would lose
//! data). Because they can be wrong in the "present" direction, a query that prunes with one must still re-check the
//! real predicate on the surviving rows — every filter here reports its exactness as
//! [`Exactness::InexactNoFalseNegative`].
//!
//! Two concrete filters live here. [`SplitBlockBloomFilter`] is the required fallback: a Parquet-style split-block
//! Bloom filter that works for any key set and is friendly to range reads. [`BinaryFuseFilter`] is the preferred choice
//! for an *immutable* block whose full key set is known when the filter is built: it is a build-once 3-wise xor filter
//! (a member of the binary-fuse family) that reaches a smaller size at the same false-positive rate. The functions
//! [`should_build_probabilistic_filter`] and [`choose_membership_filter`] capture the workload-adaptive policy: skip
//! these filters for low-cardinality columns (a bitmap or value set serves those better), and otherwise prefer binary
//! fuse, falling back to the split-block Bloom filter for any key set.
//!
//! HEF's skip-index taxonomy names a `ribbon` filter representation, but no genuine ribbon is built here. Rather than a
//! stub that emits split-block Bloom bytes under the ribbon name — which would make a declared `ribbon` mean something
//! else on disk — this module never selects or declares `ribbon`, so a declared representation always matches its bytes.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::indexes::Exactness;
use multiversion::multiversion;

/// Format tag at the front of an encoded [`SplitBlockBloomFilter`], so a decoder rejects bytes that are not one (fail
/// closed).
const SPLIT_BLOCK_BLOOM_MAGIC: u32 = 0x5342_4631; // "SBF1"

/// Format tag at the front of an encoded [`BinaryFuseFilter`]. Bumped to `BFU2` when the slot mapping moved from a
/// remainder to a multiply-shift reduction: the fingerprints a `BFU1` filter holds sit at slots this code no longer
/// derives, so those bytes must be refused (the reader then simply does not prune) rather than probed for answers that
/// could come back "absent" for a key that is present.
const BINARY_FUSE_MAGIC: u32 = 0x4246_5532; // "BFU2"

/// A 256-bit split-block is eight 32-bit lanes.
const LANES_PER_BLOCK: usize = 8;

/// The width of one split-block in bits: `LANES_PER_BLOCK` lanes of 32 bits each.
const BLOCK_BITS: u64 = LANES_PER_BLOCK as u64 * 32;

/// The encoded size of one lane: a `u32`.
const LANE_BYTES: usize = 4;

/// The encoded size of a [`SplitBlockBloomFilter`]'s fixed header: the magic tag and the block count, each a `u32`.
const SPLIT_BLOCK_BLOOM_HEADER_BYTES: usize = 8;

/// Eight odd 32-bit multipliers, one per lane, used to spread a key's hash into one set bit per lane within its chosen
/// block. These are the fixed constants from the Parquet split-block Bloom filter so any builder produces identical
/// bits; do not reorder or change them.
const LANE_SALT: [u32; LANES_PER_BLOCK] = [
    0x47b6_137b,
    0x4429_0993,
    0xd6e4_98a5,
    0x8b1d_0e9d,
    0x9f49_6df1,
    0x14db_dcdd,
    0xb96c_ff5b,
    0xc2b2_ae35,
];

/// The smallest filter we will ever build: even an empty key set gets one block so [`SplitBlockBloomFilter::contains`]
/// always has a lane to probe.
const MIN_BLOCKS: usize = 1;

/// Mixes a 64-bit key into a well-spread 64-bit hash. This is the finalizer of SplitMix64 — a cheap, deterministic,
/// dependency-free scrambler with no weak input bias, so two callers on different machines derive identical filters.
fn mix64(key: u64) -> u64 {
    let mut z = key.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Re-mixes a hash with a seed so a filter that fails to build with one seed can retry deterministically with the next.
fn mix64_seeded(key: u64, seed: u64) -> u64 {
    mix64(key ^ seed.wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

/// A space-efficient, range-read-friendly membership filter that works for any key set — HEF's required fallback when
/// no smaller filter is available.
///
/// It is an array of 256-bit *blocks*. Every key picks exactly one block (from the top bits of its hash) and, inside
/// that block, sets one bit in each of the eight 32-bit lanes (from eight slices of the same hash). A lookup recomputes
/// those eight bit positions and answers "present" only if all eight are set, so a key that was inserted always answers
/// present (no false negatives) while an absent key clears with high probability. Confining every key to one block
/// keeps a lookup to a single cache-line-sized read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitBlockBloomFilter {
    /// Row-major `block_count * 8` lanes; lanes `[b*8 .. b*8+8]` form block `b`.
    lanes: Vec<u32>,
}

impl SplitBlockBloomFilter {
    /// Builds a filter over `keys`, sized so a key set of this size meets about `bits_per_key` bits of budget (roughly
    /// eight bits per key gives a ~2% false-positive rate). Duplicate keys are harmless. The result is a deterministic
    /// function of the key multiset, so any node building over the same keys produces byte-identical bits.
    pub fn build(keys: &[u64], bits_per_key: u32) -> Self {
        Self::build_from_iter(keys.iter().copied(), keys.len(), bits_per_key)
    }

    /// Builds a filter from an iterator whose exact item count is already known. This is the allocation-free form used
    /// by index builders that deduplicate into a set: insertion order cannot affect the result because every insert is
    /// an OR into the same deterministic lane bits.
    pub(crate) fn build_from_iter(keys: impl IntoIterator<Item = u64>, key_count: usize, bits_per_key: u32) -> Self {
        let block_count = Self::block_count_for(key_count, bits_per_key);
        let mut filter = SplitBlockBloomFilter {
            lanes: vec![0u32; block_count.saturating_mul(LANES_PER_BLOCK)],
        };
        for key in keys {
            filter.insert(key);
        }
        filter
    }

    /// Chooses how many 256-bit blocks back `key_count` keys at the requested bit budget. Each block is 256 bits, so
    /// the block count is the total bit budget divided by 256, clamped to at least one block.
    fn block_count_for(key_count: usize, bits_per_key: u32) -> usize {
        let bits_per_key = bits_per_key.max(1) as u64;
        let total_bits = (key_count as u64).saturating_mul(bits_per_key);
        let blocks = total_bits.div_ceil(BLOCK_BITS);
        (blocks as usize).max(MIN_BLOCKS)
    }

    /// Picks the block index for a hash. The high 32 bits select the block via a multiply-shift map (`hi * block_count
    /// >> 32`), which spreads keys evenly across blocks without a division.
    fn block_index(hash: u64, block_count: usize) -> usize {
        let hi = hash >> 32;
        (hi.wrapping_mul(block_count as u64) >> 32) as usize
    }

    fn block_count(&self) -> usize {
        self.lanes.len() / LANES_PER_BLOCK
    }

    /// Sets this key's eight bits, one per lane of its chosen block.
    fn insert(&mut self, key: u64) {
        let hash = mix64(key);
        let base = Self::block_index(hash, self.block_count()).wrapping_mul(LANES_PER_BLOCK);
        let mask = Self::lane_mask(hash as u32);
        let Some(block) = self.lanes.get_mut(base..base.wrapping_add(LANES_PER_BLOCK)) else {
            return;
        };
        for (lane, bit) in block.iter_mut().zip(mask) {
            *lane |= bit;
        }
    }

    /// Computes the one bit each lane of the block gets, as eight one-hot `u32` masks. Multiplying the low hash word by
    /// a lane's odd salt and taking the top five bits gives that lane a uniform position in `0..32`; the eight lanes
    /// are independent of one another, so the whole set is one eight-lane step.
    fn lane_mask(lo: u32) -> [u32; LANES_PER_BLOCK] {
        let mut mask = [0u32; LANES_PER_BLOCK];
        for (bit, salt) in mask.iter_mut().zip(LANE_SALT) {
            *bit = 1u32 << (lo.wrapping_mul(salt) >> 27);
        }
        mask
    }

    /// Reports whether `key` might be present. A `true` answer may be a false positive (re-check the real predicate); a
    /// `false` answer is certain — the key was never inserted.
    pub fn contains(&self, key: u64) -> bool {
        probe_block(&self.lanes, self.block_count(), key)
    }

    /// Reports whether `key` might be present in a filter that is still in its [`SplitBlockBloomFilter::encode`]
    /// form, without rebuilding it. Answers exactly as [`SplitBlockBloomFilter::contains`] would on the decoded
    /// filter, and refuses the same malformed bytes [`SplitBlockBloomFilter::decode`] refuses.
    ///
    /// This is for a caller holding many encoded filters and probing each one a handful of times — the per-granule
    /// identity-hash filter set, say — where decoding every filter to ask one question would copy far more bytes than
    /// the probe reads. A key lands in exactly one 256-bit block, so this reads eight lanes and nothing else.
    pub fn contains_encoded(bytes: &[u8], key: u64) -> Result<bool, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("split-block bloom magic")? != SPLIT_BLOCK_BLOOM_MAGIC {
            return Err(FormatError::Structural {
                rule: "split-block bloom filter bad magic",
            });
        }
        let block_count = reader.u32("split-block bloom block count")? as usize;
        if block_count < MIN_BLOCKS {
            return Err(FormatError::Structural {
                rule: "split-block bloom filter has no blocks",
            });
        }
        let lane_bytes = reader.take(
            block_count.saturating_mul(LANES_PER_BLOCK).saturating_mul(4),
            "split-block bloom lanes",
        )?;
        let hash = mix64(key);
        let base = Self::block_index(hash, block_count)
            .wrapping_mul(LANES_PER_BLOCK)
            .wrapping_mul(4);
        let Some(block) = lane_bytes.get(base..base.wrapping_add(LANES_PER_BLOCK * 4)) else {
            return Err(FormatError::Structural {
                rule: "split-block bloom block index outside the filter",
            });
        };
        let mut missing = 0u32;
        for (lane, bit) in block.chunks_exact(4).zip(Self::lane_mask(hash as u32)) {
            let lane = u32::from_le_bytes(lane.try_into().unwrap_or_default());
            missing |= !lane & bit;
        }
        Ok(missing == 0)
    }

    /// How trustworthy this filter is for pruning: always [`Exactness::InexactNoFalseNegative`], since it can report a
    /// false positive but never a false negative.
    pub fn exactness(&self) -> Exactness {
        Exactness::InexactNoFalseNegative
    }

    /// Serializes the filter to bytes (magic, block count, then the lanes little-endian). Pairs with
    /// [`SplitBlockBloomFilter::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(SPLIT_BLOCK_BLOOM_HEADER_BYTES + self.lanes.len() * LANE_BYTES);
        out.put_u32(SPLIT_BLOCK_BLOOM_MAGIC);
        out.put_u32(self.block_count() as u32);
        out.put_u32_slice(&self.lanes);
        out.into_bytes()
    }

    /// Rebuilds a filter from [`SplitBlockBloomFilter::encode`] output. Refuses on a wrong tag or a truncated body
    /// rather than guessing.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("split-block bloom magic")? != SPLIT_BLOCK_BLOOM_MAGIC {
            return Err(FormatError::Structural {
                rule: "split-block bloom filter bad magic",
            });
        }
        let block_count = reader.u32("split-block bloom block count")? as usize;
        if block_count < MIN_BLOCKS {
            return Err(FormatError::Structural {
                rule: "split-block bloom filter has no blocks",
            });
        }
        let lane_count = block_count.saturating_mul(LANES_PER_BLOCK);
        let lanes = reader.u32_vec(lane_count, "split-block bloom lanes")?;
        Ok(SplitBlockBloomFilter { lanes })
    }
}

/// Probes one 256-bit block for a key: reads the eight lanes the key would have set and answers whether every one of
/// those bits is set.
///
/// This is the whole hot path of [`SplitBlockBloomFilter::contains`], kept as a free function so it can be compiled
/// once per instruction set the host might have, with the best one picked on the first call — the shipped binary runs
/// the eight-lane vector form where the CPU has 256-bit vectors and the scalar form everywhere else.
#[multiversion(targets = "simd")]
fn probe_block(lanes: &[u32], block_count: usize, key: u64) -> bool {
    let hash = mix64(key);
    let base = SplitBlockBloomFilter::block_index(hash, block_count).wrapping_mul(LANES_PER_BLOCK);
    let mask = SplitBlockBloomFilter::lane_mask(hash as u32);
    let Some(block) = lanes.get(base..base.wrapping_add(LANES_PER_BLOCK)) else {
        return false;
    };
    // Branchless across the block: a lane whose bit is clear leaves that bit standing in `missing`, so the key is
    // present exactly when nothing is left over. One bounds check and one branch for the whole probe.
    let mut missing = 0u32;
    for (lane, bit) in block.iter().zip(mask) {
        missing |= !lane & bit;
    }
    missing == 0
}

/// A static membership filter for an immutable block whose entire key set is known when it is built — smaller than a
/// Bloom filter at the same accuracy.
///
/// It belongs to the *binary-fuse* family and is built as a 3-wise xor filter: each key hashes to three slots in a
/// backing array, and the array is filled so that XOR-ing the three slots a key maps to reproduces that key's one-byte
/// fingerprint. A lookup recomputes the three slots and the fingerprint and answers "present" only on a match, so an
/// inserted key always answers present (no false negatives) while an absent key collides with probability about
/// `1/256`. Construction "peels" the keys one at a time; if a particular key set cannot be peeled within a bounded
/// number of reseed attempts, [`BinaryFuseFilter::build`] reports an error so the caller can fall back to a split-block
/// Bloom filter rather than risk a false negative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryFuseFilter {
    fingerprints: Vec<u8>,
    seed: u64,
    /// Number of slots in one of the three hash segments; the backing array is `segment_len * 3` long, and slot `i` of
    /// segment `s` lives at `s * segment_len + i`.
    segment_len: u32,
}

/// How many distinct seeds construction will try before giving up. Peeling a 3-wise xor filter at ~1.23x sizing
/// succeeds with overwhelming probability on the first seed; the extra attempts only guard pathological key sets.
const FUSE_MAX_SEEDS: u32 = 64;

/// Constant slots added on top of the ~1.23x load factor when sizing the backing array. The additive term dominates for
/// tiny key sets, where a bare ratio leaves too few slots to peel reliably; for large sets it is negligible and the
/// ~1.23x factor governs the size.
const FUSE_CAPACITY_SLACK: usize = 48;

/// The number of hash segments a binary fuse filter is built from: each key hashes to one slot in each segment. Fixed
/// by the 3-wise xor construction this filter implements.
const FUSE_SEGMENTS: usize = 3;

/// The backing array's load factor, as a percentage of the distinct key count, that the classic 3-wise xor
/// construction needs to peel reliably (~1.23x).
const FUSE_LOAD_FACTOR_PERCENT: usize = 123;

/// The encoded size of a [`BinaryFuseFilter`]'s fixed header: the magic tag and segment length (each a `u32`) and the
/// seed (a `u64`).
const FUSE_HEADER_BYTES: usize = 16;

impl BinaryFuseFilter {
    /// Builds a filter over the distinct values of `keys` (duplicates are folded). On success every one of those keys
    /// is guaranteed to answer present from [`BinaryFuseFilter::contains`]. Returns a [`FormatError::Structural`] if
    /// the key set cannot be peeled within the reseed budget — the caller should then fall back to a
    /// [`SplitBlockBloomFilter`], which always succeeds, so no key set is ever left without a sound filter.
    pub fn build(keys: &[u64]) -> Result<Self, FormatError> {
        let mut distinct: Vec<u64> = keys.to_vec();
        distinct.sort_unstable();
        distinct.dedup();

        // Three equal segments. Size to ~1.23x distinct keys plus a constant slack — the classic 3-wise xor load
        // factor, where the additive term dominates for tiny key sets and guarantees there is enough room to peel even
        // one or two keys.
        let n = distinct.len();
        let capacity = n.saturating_mul(FUSE_LOAD_FACTOR_PERCENT) / 100 + FUSE_CAPACITY_SLACK;
        let segment_len = (capacity.div_ceil(FUSE_SEGMENTS)).max(1) as u32;

        for attempt in 0..FUSE_MAX_SEEDS {
            let seed = 0x51_2b_9f_01_u64.wrapping_mul(u64::from(attempt) + 1);
            if let Some(filter) = Self::try_build(&distinct, segment_len, seed) {
                return Ok(filter);
            }
        }
        Err(FormatError::Structural {
            rule: "binary fuse filter could not peel key set within reseed budget",
        })
    }

    /// Maps a key to its three slot indices (one per segment) and its fingerprint, all from a single seeded hash so the
    /// mapping is reproducible.
    fn slots_and_fingerprint(key: u64, segment_len: u32, seed: u64) -> ([usize; FUSE_SEGMENTS], u8) {
        let h = mix64_seeded(key, seed);
        let seg = segment_len as u64;
        // Three positions from 32-bit windows of the hash. The windows overlap, but a multiply-shift reduction is
        // decided by a word's high bits, and those are the hash's bits 16..32, 32..48 and 48..64 — three disjoint
        // slices, so the positions stay independent.
        let i0 = Self::slot_in_segment(h as u32, seg);
        let i1 = Self::slot_in_segment((h >> 16) as u32, seg);
        let i2 = Self::slot_in_segment((h >> 32) as u32, seg);
        let base1 = seg;
        let base2 = seg.wrapping_mul(2);
        let slots = [
            i0 as usize,
            base1.wrapping_add(i1) as usize,
            base2.wrapping_add(i2) as usize,
        ];
        // The fingerprint comes from the low byte, which no reduction above depends on: a fingerprint the slots can
        // predict would let far more absent keys match than the one-in-a-fingerprint the filter promises. It is forced
        // non-zero because zero is reserved for "empty slot", and folding a zero in would let an absent key that lands
        // on empty slots match.
        let fp = (h as u8) | 1;
        (slots, fp)
    }

    /// Reduces a 32-bit hash word to a slot in `0..segment_len` by multiply-shift: the word times the segment length,
    /// keeping the top 32 bits of the 64-bit product. That is as uniform as a remainder for a well-spread word, and it
    /// costs one multiply instead of a division — three of which sit on every probe.
    fn slot_in_segment(word: u32, segment_len: u64) -> u64 {
        (u64::from(word).wrapping_mul(segment_len)) >> 32
    }

    /// One construction attempt at a fixed seed. Returns `None` if the hypergraph of (key → three slots) cannot be
    /// peeled, i.e. some slots stay shared by two or more keys to the end.
    fn try_build(distinct: &[u64], segment_len: u32, seed: u64) -> Option<Self> {
        let array_len = (segment_len as usize).checked_mul(FUSE_SEGMENTS)?;

        // Each key's three slots and fingerprint depend only on (key, segment_len, seed), so hash every key once here
        // rather than recomputing it on each of the three passes below (build, peel, fingerprint-assign).
        let precomputed: Vec<([usize; 3], u8)> = distinct
            .iter()
            .map(|&key| Self::slots_and_fingerprint(key, segment_len, seed))
            .collect();

        // Per-slot XOR of the *indices* into `precomputed` for every key still touching it, and a count of how many
        // keys touch it. A slot with count 1 is "peelable", and its xor_index is then exactly the surviving key's
        // index (XOR of one item is itself) — recovered without hashing anything again.
        let mut xor_index = vec![0u64; array_len];
        let mut counts = vec![0u32; array_len];
        for (index, &(slots, _)) in precomputed.iter().enumerate() {
            for &s in &slots {
                let xi = xor_index.get_mut(s)?;
                *xi ^= index as u64;
                let c = counts.get_mut(s)?;
                *c = c.wrapping_add(1);
            }
        }

        // Peel: repeatedly remove a key that is alone in some slot, recording the order. Each removal may free up
        // further slots.
        let mut stack: Vec<(usize, usize)> = Vec::with_capacity(distinct.len());
        let mut queue: Vec<usize> = (0..array_len).filter(|&s| counts.get(s) == Some(&1)).collect();

        while let Some(slot) = queue.pop() {
            if counts.get(slot).copied().unwrap_or(0) != 1 {
                continue;
            }
            let index = xor_index.get(slot).copied().unwrap_or(0) as usize;
            let (slots, _) = *precomputed.get(index)?;
            stack.push((index, slot));
            for &s in &slots {
                let c = counts.get_mut(s)?;
                *c = c.wrapping_sub(1);
                let xi = xor_index.get_mut(s)?;
                *xi ^= index as u64;
                if counts.get(s) == Some(&1) {
                    queue.push(s);
                }
            }
        }

        if stack.len() != distinct.len() {
            return None;
        }

        // Assign fingerprints in reverse peel order. Each key owns the slot it was peeled from, so writing `fp ^ (other
        // two slots)` there makes the three-way XOR equal its fingerprint, without disturbing keys peeled later (which
        // are filled earlier here).
        let mut fingerprints = vec![0u8; array_len];
        for &(index, slot) in stack.iter().rev() {
            let (slots, fp) = *precomputed.get(index)?;
            let mut value = fp;
            for &s in &slots {
                if s != slot {
                    value ^= fingerprints.get(s).copied().unwrap_or(0);
                }
            }
            if let Some(cell) = fingerprints.get_mut(slot) {
                *cell = value;
            }
        }

        Some(BinaryFuseFilter {
            fingerprints,
            seed,
            segment_len,
        })
    }

    /// Reports whether `key` might be present. A `true` answer may be a false positive (probability about `1/256`); a
    /// `false` answer is certain.
    pub fn contains(&self, key: u64) -> bool {
        let (slots, fp) = Self::slots_and_fingerprint(key, self.segment_len, self.seed);
        let mut acc = 0u8;
        for &s in &slots {
            acc ^= self.fingerprints.get(s).copied().unwrap_or(0);
        }
        acc == fp
    }

    /// How trustworthy this filter is for pruning: always [`Exactness::InexactNoFalseNegative`].
    pub fn exactness(&self) -> Exactness {
        Exactness::InexactNoFalseNegative
    }

    /// Serializes the filter to bytes (magic, seed, segment length, fingerprint bytes). Pairs with
    /// [`BinaryFuseFilter::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Writer::with_capacity(FUSE_HEADER_BYTES + self.fingerprints.len());
        out.put_u32(BINARY_FUSE_MAGIC);
        out.put_u32(self.segment_len);
        out.put_u64(self.seed);
        out.put_slice(&self.fingerprints);
        out.into_bytes()
    }

    /// Rebuilds a filter from [`BinaryFuseFilter::encode`] output, refusing on a wrong tag, an impossible length,
    /// or a truncated body.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("binary fuse magic")? != BINARY_FUSE_MAGIC {
            return Err(FormatError::Structural {
                rule: "binary fuse filter bad magic",
            });
        }
        let segment_len = reader.u32("binary fuse segment length")?;
        if segment_len == 0 {
            return Err(FormatError::Structural {
                rule: "binary fuse filter has empty segment",
            });
        }
        let seed = reader.u64("binary fuse seed")?;
        let array_len = (segment_len as usize)
            .checked_mul(FUSE_SEGMENTS)
            .ok_or(FormatError::Structural {
                rule: "binary fuse filter length overflow",
            })?;
        let fingerprints = reader.take(array_len, "binary fuse fingerprints")?.to_vec();
        Ok(BinaryFuseFilter {
            fingerprints,
            seed,
            segment_len,
        })
    }
}

/// Which point-membership filter the planner should build for a high-cardinality column, in order of preference.
///
/// `BinaryFuse` is the first choice for an immutable block whose key set is fully known at build time (it is the
/// smallest at a given accuracy); `SplitBlockBloom` is the required fallback that always works. This enum is only
/// chosen for columns that *should* get a probabilistic filter at all — see [`FilterRecommendation`] for the
/// low-cardinality case. HEF's taxonomy also names a `ribbon` representation, but the policy never selects it while no
/// genuine ribbon filter is built, so it is not a choice here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipFilterChoice {
    BinaryFuse,
    SplitBlockBloom,
}

/// The planner's top-level recommendation for accelerating equality lookups on a column: either build a probabilistic
/// membership filter of a particular kind, or skip it because the column is low-cardinality and a bitmap or value set
/// serves it better.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterRecommendation {
    /// Low-cardinality: prefer an exact bitmap or value set, not a filter.
    UseBitmapOrValueSet,
    /// High-cardinality: build this probabilistic membership filter.
    UseMembershipFilter(MembershipFilterChoice),
}

/// The smallest number of distinct values below which a column is treated as low-cardinality regardless of row count,
/// so a tiny enum-like column (a status, a country) is never given a probabilistic filter. Below this floor a bitmap or
/// value set is both exact and smaller.
const LOW_CARDINALITY_DISTINCT_FLOOR: u64 = 256;

/// The distinct-to-rows ratio (as a percentage) a column must reach to be considered high-cardinality enough for a
/// probabilistic filter once it clears the absolute floor. At or above this share of rows being distinct, equality
/// lookups are selective and a membership filter earns its bytes.
const HIGH_CARDINALITY_RATIO_PERCENT: u64 = 10;

/// Decides whether a column is worth a probabilistic membership filter at all.
///
/// Returns `false` for low-cardinality columns — few distinct values over many rows — because those are better served
/// by an exact bitmap or value set; returns `true` for high-cardinality equality/lookup columns where a filter prunes
/// well for a bounded cost. A column qualifies when it has at least [`LOW_CARDINALITY_DISTINCT_FLOOR`] distinct values
/// *and* those distinct values are at least [`HIGH_CARDINALITY_RATIO_PERCENT`] of the rows (a column with only a
/// handful of distinct values, however many rows, is always low cardinality). With no rows there is nothing to filter,
/// so the answer is `false`.
pub fn should_build_probabilistic_filter(distinct_count: u64, row_count: u64) -> bool {
    if row_count == 0 {
        return false;
    }
    if distinct_count < LOW_CARDINALITY_DISTINCT_FLOOR {
        return false;
    }
    // distinct_count * 100 >= row_count * ratio, done without floats and without overflow on realistic counts.
    distinct_count.saturating_mul(100) >= row_count.saturating_mul(HIGH_CARDINALITY_RATIO_PERCENT)
}

/// Picks which membership filter to build for a column that has already been judged worth one (see
/// [`should_build_probabilistic_filter`]).
///
/// Binary fuse for an immutable block whose key set is fully known at build time (the smallest at a given accuracy);
/// otherwise the split-block Bloom fallback, which works for any key set. HEF's taxonomy names a `ribbon` filter too,
/// but this policy never selects it, so a build never declares `ribbon` while emitting another representation's bytes.
pub fn choose_membership_filter(immutable_known_key_set: bool) -> MembershipFilterChoice {
    if immutable_known_key_set {
        MembershipFilterChoice::BinaryFuse
    } else {
        MembershipFilterChoice::SplitBlockBloom
    }
}

/// Combines the two policy questions into one recommendation: should this column get a probabilistic filter, and if so
/// which kind?
///
/// Returns [`FilterRecommendation::UseBitmapOrValueSet`] for low-cardinality columns (an exact bitmap or value set is
/// the right tool), and otherwise the chosen membership filter from [`choose_membership_filter`].
pub fn recommend_filter(distinct_count: u64, row_count: u64, immutable_known_key_set: bool) -> FilterRecommendation {
    if should_build_probabilistic_filter(distinct_count, row_count) {
        FilterRecommendation::UseMembershipFilter(choose_membership_filter(immutable_known_key_set))
    } else {
        FilterRecommendation::UseBitmapOrValueSet
    }
}

#[cfg(test)]
#[path = "test/probabilistic.rs"]
mod tests;
