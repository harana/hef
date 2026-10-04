//! Builds and reads the footer-mirror object: one compressed object per manifest generation that concatenates every
//! published file's footer tail, keyed by `file_id`, so a planner opens every file in the generation from a single
//! object read instead of one tail read per file.
//!
//! The mirror is manifest-native auxiliary metadata (`hef-core-invariants` — Requirement: "Barriers, deletion
//! vectors, projections, and layout classes") and droppable acceleration state: each section carries its own BLAKE3
//! and the `file_id`/generation it mirrors, so [`open_footer`] validates a section before trusting it and falls back
//! to that file's own authoritative tail read ([`super::reader::tail_range`]) on any mismatch, a missing section, or
//! a missing mirror.

use super::cache::{BlockCache, BlockKey, BlockKind};
use super::reader::{HefFooter, TailRange};
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};
use crate::lifecycle::HefFileEntry;
use hashbrown::HashMap;
use std::sync::Arc;

/// Fixed per-section overhead a capacity estimate accounts for ahead of a section's tail bytes: the `u128` file_id
/// (16), `u64` generation (8), and BLAKE3 checksum (32). The trailing `u64` tail-length word adds another 8 bytes
/// this leaves out — fine since it only sizes a capacity hint, not an exact bound.
const MIRROR_SECTION_FIXED_OVERHEAD_BYTES: usize = 56;

/// One file's footer tail as carried inside a footer-mirror object.
#[derive(Debug)]
struct MirrorSection {
    generation: u64,
    tail_bytes: Vec<u8>,
}

/// An opened footer-mirror object: every section that passed its own BLAKE3 check, keyed by `file_id`. A section
/// whose BLAKE3 does not verify is dropped at open time rather than failing the whole object, so one corrupt section
/// costs that file's fallback, never the rest of the generation.
#[derive(Debug, Default)]
pub struct FooterMirror {
    sections: HashMap<u128, MirrorSection>,
}

impl FooterMirror {
    /// Builds the compressed footer-mirror object for one manifest generation from every published file's `file_id`
    /// and exact tail bytes — the same bytes [`super::reader::tail_range`] would have fetched remotely for that file.
    /// Each section is stamped with the generation and its own BLAKE3 before the whole object is compressed once.
    pub fn build(generation: u64, files: &[(u128, Vec<u8>)]) -> Vec<u8> {
        let mut plain = Writer::with_capacity(
            files
                .iter()
                .map(|(_, tail)| tail.len() + MIRROR_SECTION_FIXED_OVERHEAD_BYTES)
                .sum(),
        );
        plain.put_u64(files.len() as u64);
        for (file_id, tail_bytes) in files {
            plain.put_u128(*file_id);
            plain.put_u64(generation);
            plain.put_slice(&section_checksum(*file_id, generation, tail_bytes));
            plain.put_u64(tail_bytes.len() as u64);
            plain.put_slice(tail_bytes);
        }
        crate::encoding::deflate::compress(plain.bytes())
    }

    /// Opens a footer-mirror object, verifying each section's own BLAKE3 and dropping any that fail rather than
    /// rejecting the whole object. Truncated or unparsable framing refuses for the whole object; the caller
    /// treats that exactly like a missing mirror and falls back to per-file tail reads for every entry.
    pub fn open(bytes: &[u8]) -> Result<Self, FormatError> {
        let plain = crate::encoding::deflate::decompress(bytes)?;
        let mut reader = Reader::new(&plain);
        let count = reader.u64("footer mirror section count")? as usize;
        let mut sections = HashMap::with_capacity(reader.capacity_hint(count, MIRROR_SECTION_FIXED_OVERHEAD_BYTES));
        for _ in 0..count {
            let file_id = reader.u128("footer mirror section file_id")?;
            let generation = reader.u64("footer mirror section generation")?;
            let checksum = reader.take(32, "footer mirror section blake3")?;
            let tail_len = reader.u64("footer mirror section tail length")? as usize;
            let tail_bytes = reader.take(tail_len, "footer mirror section tail bytes")?;
            if section_checksum(file_id, generation, tail_bytes) == checksum {
                sections.insert(
                    file_id,
                    MirrorSection {
                        generation,
                        tail_bytes: tail_bytes.to_vec(),
                    },
                );
            }
        }
        Ok(Self { sections })
    }

    /// Loads the mirrored tail of every entry in `entries` into `cache`, as the first thing a node does for a new
    /// generation, so [`super::reader::HefFile::open_remote`] then opens those files from the cache with no tail
    /// request of their own. Only an entry with exact tail geometry (`footer_len` recorded) whose mirrored tail has
    /// exactly that length is loaded; every other entry keeps its per-file tail read. A loaded tail is still bound
    /// to the manifest seal when the file opens, and one that fails is dropped and fetched from the file itself.
    pub fn seed_cache(&self, cache: &BlockCache, generation: u64, entries: &[HefFileEntry]) {
        for entry in entries {
            let range = entry.tail_range();
            if let Some(tail_bytes) = self.tail_bytes(entry.file_id, generation)
                && range.exact
                && tail_bytes.len() as u64 == range.len
            {
                let key = BlockKey {
                    file_id: entry.file_id,
                    kind: BlockKind::Footer,
                    len: range.len,
                    offset: range.start,
                    tenant_id: entry.tenant_id,
                };
                cache.insert(key, Arc::from(tail_bytes));
            }
        }
    }

    /// The tail bytes mirrored for `file_id` at `generation`, or `None` when no section matches both — the single
    /// signal [`open_footer`] needs to decide whether to use the mirror or fall back to a per-file tail read (a
    /// section whose BLAKE3 failed to verify was already dropped by [`FooterMirror::open`]).
    fn tail_bytes(&self, file_id: u128, generation: u64) -> Option<&[u8]> {
        self.sections
            .get(&file_id)
            .filter(|section| section.generation == generation)
            .map(|section| section.tail_bytes.as_slice())
    }
}

/// The checksum a mirror section carries: BLAKE3 over the file id, the generation, and the tail bytes together.
///
/// The identifiers are inside the hash, not beside it, so a section cannot be relabelled or moved between generations
/// and still verify. Hashing the tail alone would let an intact section be served under another file's `file_id`, and a
/// planner would then read that file's schema, pruning statistics, and byte ranges — silently wrong query results
/// instead of a failed check and a fallback to the authoritative file tail.
fn section_checksum(file_id: u128, generation: u64, tail_bytes: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&file_id.to_le_bytes());
    hasher.update(&generation.to_le_bytes());
    hasher.update_rayon(tail_bytes);
    *hasher.finalize().as_bytes()
}

/// Where a planner should open `entry`'s footer from: the mirror, when it holds a valid section, or the per-file
/// tail geometry ([`super::reader::tail_range`]) otherwise.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the footer variant is the common outcome and is moved out at once; boxing it would cost an allocation per open"
)]
pub enum FooterSource {
    /// The mirror held a valid section; the footer is already open.
    Mirror(HefFooter),
    /// No usable mirror section — read `entry`'s own tail at this range instead.
    PerFileTail(TailRange),
}

/// The "fetch one, open all" planner step: given the generation's footer-mirror object (or `None`, when absent or
/// unreadable) and one manifest entry, decides whether that file opens from the mirror or needs its own tail read.
/// Called once per entry after a single mirror fetch, so a whole generation opens from one object request whenever
/// every entry hits the mirror.
pub fn open_footer(mirror: Option<&FooterMirror>, entry: &HefFileEntry, generation: u64) -> FooterSource {
    if let Some(tail_bytes) = mirror.and_then(|mirror| mirror.tail_bytes(entry.file_id, generation))
        && let Ok(footer) = HefFooter::open(tail_bytes)
    {
        return FooterSource::Mirror(footer);
    }
    FooterSource::PerFileTail(entry.tail_range())
}

#[cfg(test)]
#[path = "test/mirror.rs"]
mod tests;
