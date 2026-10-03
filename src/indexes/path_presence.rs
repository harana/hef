//! A path-presence filter: answers "could this block contain payload path P?" without ever wrongly saying "no".
//!
//! Shredded payload fields are sparse — most events carry only a small subset of the possible payload paths. A
//! [`PathPresenceIndex`] per granule or page records which paths appear in that block as a compact hash set backed by a
//! split-block Bloom filter. Before reading the bytes for a path-presence predicate, the scan probes the index: a
//! `false` answer lets the whole block be skipped; a `true` answer is a candidate that the exact predicate still
//! confirms over the materialized values.
//!
//! A path is identified by a `u64` hash of its canonical dotted string form (for example
//! `"attributes.revenue.amount"`). Hashing at index-build time means the on-disk filter needs no variable-length
//! strings and lookup is a single hash probe. Exactness is always [`Exactness::InexactNoFalseNegative`]: an absent path
//! never produces a miss, but a false positive (the filter reports present when the path is absent) is allowed and only
//! costs a wasted read.

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::indexes::probabilistic::SplitBlockBloomFilter;
use crate::indexes::{Exactness, stable_hash};

/// Format tag at the front of an encoded [`PathPresenceIndex`].
const PATH_PRESENCE_MAGIC: u32 = 0x5050_5831; // "PPX1"

/// Bits per path hash inserted into the backing Bloom filter. Ten bits per path gives roughly a 1% false-positive rate,
/// which limits wasted reads while keeping the per-granule metadata small even for event schemas with many optional
/// paths.
const BITS_PER_PATH: u32 = 10;

/// The encoded size of an index's fixed header: the magic tag, a single `u32`.
const PATH_PRESENCE_HEADER_BYTES: usize = 4;

/// A compact membership filter over the payload paths that appear in a block.
///
/// Built by hashing the canonical dotted-string form of every shredded path that is present in at least one row of the
/// covered granule or page, then inserting those hashes into a split-block Bloom filter. A query path presence
/// predicate (`HAS_PATH('attributes.revenue.amount')`) hashes the path string the same way and probes the filter.
///
/// The filter is exact in the "no false negative" sense: a path that genuinely appears in the block is always reported
/// as present. It may give a false positive — reporting a path as present when it is not — but that only forces an
/// exact check on the materialized payload, never silently drops a matching row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathPresenceIndex {
    filter: SplitBlockBloomFilter,
}

impl PathPresenceIndex {
    /// Builds a path-presence index over `path_hashes` (one `u64` per distinct present path). The result is a
    /// deterministic function of the hash multiset, so any node building over the same rows produces byte-identical
    /// bits. Duplicate hashes are harmless.
    pub fn build(path_hashes: &[u64]) -> Self {
        PathPresenceIndex {
            filter: SplitBlockBloomFilter::build(path_hashes, BITS_PER_PATH),
        }
    }

    /// Builds a path-presence index directly from path strings, hashing each with [`hash_path`]. Convenience wrapper
    /// over [`PathPresenceIndex::build`].
    pub fn build_from_paths(paths: &[&str]) -> Self {
        let hashes: Vec<u64> = paths.iter().map(|&p| hash_path(p)).collect();
        Self::build(&hashes)
    }

    /// Reports whether this block might contain rows where payload path `path` is present. A `false` answer is certain
    /// — no row in this block carries the path. A `true` answer may be a false positive; the caller must confirm with
    /// an exact path-presence check over the materialized payload.
    pub fn might_contain_path(&self, path: &str) -> bool {
        self.filter.contains(hash_path(path))
    }

    /// Reports whether this block might contain `path_hash` (the caller has already hashed the path with
    /// [`hash_path`]). Avoids re-hashing when the same hash is probed against several indexes.
    pub fn might_contain_hash(&self, path_hash: u64) -> bool {
        self.filter.contains(path_hash)
    }

    /// How trustworthy this index is for pruning: always [`Exactness::InexactNoFalseNegative`] — it never drops a block
    /// that contains the queried path, but may keep blocks that don't.
    pub fn exactness(&self) -> Exactness {
        Exactness::InexactNoFalseNegative
    }

    /// Serializes the index to bytes (magic then the Bloom filter bytes). Pairs with [`PathPresenceIndex::decode`].
    pub fn encode(&self) -> Vec<u8> {
        let filter_bytes = self.filter.encode();
        let mut out = Writer::with_capacity(PATH_PRESENCE_HEADER_BYTES + filter_bytes.len());
        out.put_u32(PATH_PRESENCE_MAGIC);
        out.put_slice(&filter_bytes);
        out.into_bytes()
    }

    /// Rebuilds an index from [`PathPresenceIndex::encode`] output. Refuses on a wrong tag or malformed Bloom
    /// bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        if reader.u32("path presence magic")? != PATH_PRESENCE_MAGIC {
            return Err(FormatError::Structural {
                rule: "path presence index bad magic",
            });
        }
        let remaining = reader.remaining();
        let rest = reader.take(remaining, "path presence filter bytes")?;
        let filter = SplitBlockBloomFilter::decode(rest)?;
        Ok(PathPresenceIndex { filter })
    }
}

/// Hashes a canonical payload path string to a `u64` for insertion into and lookup in the backing filter. Uses an
/// FNV-1a-seeded polynomial hash followed by the SplitMix64 finalizer, so the result is stable across platforms and any
/// two distinct path strings produce different hashes with overwhelming probability.
pub fn hash_path(path: &str) -> u64 {
    stable_hash(path.as_bytes())
}

#[cfg(test)]
#[path = "test/path_presence.rs"]
mod tests;
