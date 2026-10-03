//! Tracks each stored file through its life — being written, sealed, visible to queries, replaced, and finally deleted
//! — and decides which files a new query may read.
//!
//! A file becomes visible only because a published catalogue entry says so, never because it happens to exist on
//! storage: the catalogue entry carries the file's current state, and only files marked active join a new query
//! snapshot.

use super::events::{SequenceRange, TenantId};
use hashbrown::HashMap;

/// The role a stored file plays in the published catalogue.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FileType {
    /// Model outputs or promotion-backfill projections, row-aligned ordinal-for-ordinal to a base HEF file.
    DerivedColumns,
    /// One fragment of a multi-part SuperHEF compaction generation.
    GenerationPart,
    /// A committed base event file.
    #[default]
    HefFile,
}

/// Where a stored file is in its life, from being written to deleted. A file only ever advances: `OpenTmp → Sealed →
/// Active → Outdated → DeleteOnDestroy → Deleted`. `Ord` follows that progression; see `rank()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PartState {
    /// Manifest references the file and coverage. Queryable.
    Active,
    /// No longer used by any readable snapshot; awaiting sweeper deletion.
    DeleteOnDestroy,
    /// Removed after retention and safety window.
    Deleted,
    /// File is being written. Not queryable.
    OpenTmp,
    /// Replaced by newer generation files; retained for in-flight snapshots.
    Outdated,
    /// Footer and checksum are valid. Not yet visible.
    Sealed,
}

impl PartState {
    fn rank(self) -> u8 {
        match self {
            PartState::OpenTmp => 0,
            PartState::Sealed => 1,
            PartState::Active => 2,
            PartState::Outdated => 3,
            PartState::DeleteOnDestroy => 4,
            PartState::Deleted => 5,
        }
    }

    /// The legal forward edges of the part-state machine. `OpenTmp → Sealed` is the only entry edge (the dual roll
    /// trigger produces it); every other edge advances visibility or retirement.
    pub fn can_transition_to(self, next: PartState) -> bool {
        matches!(
            (self, next),
            (PartState::OpenTmp, PartState::Sealed)
                | (PartState::Sealed, PartState::Active)
                | (PartState::Active, PartState::Outdated)
                | (PartState::Outdated, PartState::DeleteOnDestroy)
                | (PartState::DeleteOnDestroy, PartState::Deleted)
        )
    }

    /// Only `Active` files may be selected for new query snapshots. `Outdated` files remain readable only for in-flight
    /// snapshots that already selected them.
    pub fn selectable_for_new_snapshots(self) -> bool {
        matches!(self, PartState::Active)
    }
}

impl Ord for PartState {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank().cmp(&other.rank())
    }
}

impl PartialOrd for PartState {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// One stored file as recorded in the published catalogue. Its mere existence on storage means nothing; this entry is
/// what makes it visible to queries and records the journal range it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HefFileEntry {
    /// The contiguous journal range this file covers.
    pub coverage: SequenceRange,
    /// Day-scoped service rollup payload. Valid only on `GenerationPart` entries with `part_index == 0`; must be
    /// `None` on all other entries.
    pub feature_metadata: Option<Vec<u8>>,
    /// Authoritative domain-separated segment seal. Stripe payloads enter through their authenticated BLAKE3 roots,
    /// so publication does not hash the same large bytes again.
    pub file_seal: [u8; 32],
    /// File identity (content-addressed by the publisher; stable across idempotent re-publication of the same journal
    /// range).
    pub file_id: u128,
    pub file_type: FileType,
    /// Byte length of the file's trailing footer region — footer blob, footer-length word, and `"HEF1"` magic — so a
    /// remote reader that knows `size_bytes` fetches the footer in one exact-range GET. Additive: `None` on entries
    /// written before tail geometry was recorded, which drop the reader to a speculative tail fetch rather than fail.
    pub footer_len: Option<u64>,
    pub optional_feature_flags: u64,
    /// Zero-based index within a multi-part SuperHEF; always 0 for `HefFile` and `DerivedColumns` entries.
    pub part_index: u32,
    pub part_state: PartState,
    pub required_feature_flags: u64,
    pub size_bytes: u64,
    pub tenant_id: TenantId,
    /// Byte length of the complete BLAKE3 verified-streaming appendix appended after the footer: concatenated
    /// per-stripe proof nodes plus its length word and `"HEFT"` magic. Added to `footer_len` to size the exact tail GET.
    /// `None` when every stripe is small enough to verify with a whole-stripe read.
    pub tree_len: Option<u64>,
}

impl HefFileEntry {
    /// The trailing byte range a remote reader should GET to open this file. Exact — the last `footer_len + tree_len`
    /// bytes — when `footer_len` is recorded, so the cold open is a single request; otherwise a speculative last-256 KiB
    /// fetch that may need one exact retry (see [`crate::layout::reader::HefFooter::open_speculative`]).
    pub fn tail_range(&self) -> crate::layout::reader::TailRange {
        crate::layout::reader::tail_range(self.size_bytes, self.footer_len, self.tree_len)
    }

    /// True when the `feature_metadata` placement satisfies the spec rule: day-scoped feature metadata is valid only on
    /// `GenerationPart` entries with `part_index == 0`, and must be `None` on every other entry type.
    pub fn feature_metadata_placement_valid(&self) -> bool {
        match self.feature_metadata {
            None => true,
            Some(_) => matches!(self.file_type, FileType::GenerationPart) && self.part_index == 0,
        }
    }
}

/// Decides when accumulated small files earn a merge, so low-volume tenants converge toward roll-target-sized files
/// without the newest file being rewritten over and over.
///
/// The publish policy's time-based roll guarantees a steady stream of small files from trickle tenants; without a
/// scheduling policy they accumulate forever, and with a naive one ("merge whenever there are two small files") the
/// tail file is rewritten at every cycle — quadratic write amplification, costlier here than in a mutable store because
/// every merge is a new manifest generation and object-store round trips. The doubling bound below is the rule that
/// prevents that thrashing. Every parameter is a pinned code parameter, never an operator key.
///
/// See: hef-file-lifecycle/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionPolicy {
    /// Merge jobs one scheduling cycle may emit; further eligible runs wait for later cycles so maintenance work per
    /// cycle stays bounded.
    pub max_jobs_per_cycle: usize,
    /// Most source files one merge job may fold together.
    pub max_merge_width: usize,
    /// A run of small files merges only when its combined bytes reach this multiple of its largest input — the
    /// anti-thrashing doubling bound — or reach `roll_target_bytes` outright.
    pub min_output_over_largest: u64,
    /// The publish policy's roll byte target: a file at or above it is finished and is neither a merge candidate nor
    /// re-merged, and a run whose combined bytes reach it always qualifies.
    pub roll_target_bytes: u64,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            max_jobs_per_cycle: 4,
            max_merge_width: 8,
            min_output_over_largest: 2,
            roll_target_bytes: 1024 * 1024 * 1024,
        }
    }
}

/// One merge a scheduling cycle decided to run: which adjacent source files to fold into one replacement file, and the
/// combined input bytes the output is expected to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionJob {
    pub expected_output_bytes: u64,
    /// The source files to merge, in ascending sequence order.
    pub file_ids: Vec<u128>,
}

/// Plans one tenant's compaction cycle: which runs of sequence-adjacent small files merge now, and which wait.
///
/// Candidates are the tenant's `Active` base event files below the roll byte target, in ascending sequence order; a
/// finished (roll-target-sized) file breaks a run. From the front of each run the planner takes up to
/// `max_merge_width` files and schedules them only when the doubling bound holds — the combined bytes reach
/// `min_output_over_largest` times the largest input, or the roll target — so a trickle tenant's newest small file is
/// never rewritten at every cycle just because it has one small neighbour. At most `max_jobs_per_cycle` jobs are
/// returned; everything else waits. Scheduling is deterministic: the same generation always plans the same jobs.
pub fn plan_compaction_cycle(
    policy: &CompactionPolicy,
    generation: &ManifestGeneration,
    tenant_id: TenantId,
) -> Vec<CompactionJob> {
    // A merge needs at least two inputs; a narrower width would degenerate into empty or single-file jobs.
    if policy.max_merge_width < 2 {
        return Vec::new();
    }
    let mut candidates: Vec<&HefFileEntry> = generation
        .files
        .iter()
        .filter(|entry| {
            entry.tenant_id == tenant_id
                && entry.part_state == PartState::Active
                && entry.file_type == FileType::HefFile
        })
        .collect();
    candidates.sort_by_key(|entry| (entry.coverage.epoch, entry.coverage.first_sequence));

    let mut jobs = Vec::new();
    let mut run: Vec<&HefFileEntry> = Vec::new();
    // One trailing sentinel flushes the final run through the same path as an interior break.
    for entry in candidates.iter().copied().map(Some).chain([None]) {
        let small = entry.is_some_and(|entry| entry.size_bytes < policy.roll_target_bytes);
        if small {
            if let Some(entry) = entry {
                run.push(entry);
            }
            continue;
        }
        // A finished file (or the end of the candidates) closes the current run; schedule what qualifies. The window
        // slides so one larger-but-unfinished file at the head of a run does not block its small tail from merging.
        let mut start = 0usize;
        while jobs.len() < policy.max_jobs_per_cycle && start + 2 <= run.len() {
            let window = &run[start..run.len().min(start + policy.max_merge_width)];
            let total: u64 = window.iter().map(|entry| entry.size_bytes).sum();
            let largest = window.iter().map(|entry| entry.size_bytes).max().unwrap_or(0);
            let qualifies =
                total >= largest.saturating_mul(policy.min_output_over_largest) || total >= policy.roll_target_bytes;
            if qualifies {
                jobs.push(CompactionJob {
                    expected_output_bytes: total,
                    file_ids: window.iter().map(|entry| entry.file_id).collect(),
                });
                start += window.len();
            } else {
                // The doubling bound defers this window — the run's newest files are not rewritten just for existing.
                start += 1;
            }
        }
        run.clear();
    }
    jobs
}

/// One published version of the file catalogue: the set of stored files and their states that queries see at this
/// generation. Publishing a new version produces the next generation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ManifestGeneration {
    pub files: Vec<HefFileEntry>,
    /// This generation's footer-mirror object, when one has been built (see
    /// `crate::layout::mirror::FooterMirror`). Additive and droppable: `None` means no mirror was built (or
    /// this generation predates the feature), and every file opens through its own authoritative tail read.
    pub footer_mirror: Option<FooterMirrorObject>,
    pub generation: u64,
    /// Index artifacts riding this generation: identity, seal, and coverage summary, published atomically with the
    /// file set. Dropping an artifact is omitting it from the next generation; the sweeper retires its object only
    /// after the safety window, once no live generation references it (see [`retired_artifact_keys`]).
    pub index_artifacts: Vec<IndexArtifactRef>,
}

/// Where one generation's footer-mirror object is stored: its object key, the BLAKE3 checksum of its bytes, and its
/// size, so a reader can fetch and verify the mirror without trusting the store.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct FooterMirrorObject {
    /// Authoritative BLAKE3 of the object bytes.
    pub blake3: [u8; 32],
    /// The tenant-qualified object-store key the mirror was committed under.
    pub path: String,
    pub size_bytes: u64,
}

/// One manifest-referenced index artifact: enough to fetch, verify, and judge the object without opening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexArtifactRef {
    pub artifact_blake3: [u8; 32],
    pub column_id: u32,
    pub covered_file_id: u128,
    pub kind: crate::indexes::artifact::ArtifactKind,
    pub object_key: String,
}

/// The artifact objects `next` dropped relative to `previous` — the sweeper's retirement candidates, deletable only
/// after the safety window since a reader pinned to `previous` may still fetch them.
pub fn retired_artifact_keys(previous: &ManifestGeneration, next: &ManifestGeneration) -> Vec<String> {
    let kept: hashbrown::HashSet<&str> = next
        .index_artifacts
        .iter()
        .map(|kept| kept.object_key.as_str())
        .collect();
    previous
        .index_artifacts
        .iter()
        .filter(|artifact| !kept.contains(artifact.object_key.as_str()))
        .map(|artifact| artifact.object_key.clone())
        .collect()
}

impl ManifestGeneration {
    /// The files a new query snapshot may select: manifest-referenced and `Active`, nothing else — unpublished files
    /// are invisible whatever exists on storage.
    pub fn snapshot_files(&self) -> impl Iterator<Item = &HefFileEntry> {
        self.files
            .iter()
            .filter(|entry| entry.part_state.selectable_for_new_snapshots())
    }

    /// True when `range` is fully covered, for `tenant_id`, by published coverage in a still-retained state — `Active`,
    /// `Outdated` (once Active, kept for in-flight snapshots), or `DeleteOnDestroy` (retired but not yet swept) — the
    /// gate for LiveOverlay eviction and HEJ retention. A file published by a different tenant never counts, even over
    /// an identical range, so one tenant's coverage can never authorize dropping another's data.
    pub fn covers(&self, range: &SequenceRange, tenant_id: TenantId) -> bool {
        // Coverage entries are per contiguous published range; a single entry must contain the candidate range
        // (publication packs one contiguous durable range per file).
        self.files.iter().any(|entry| {
            matches!(
                entry.part_state,
                PartState::Active | PartState::Outdated | PartState::DeleteOnDestroy
            ) && entry.tenant_id == tenant_id
                && entry.coverage.contains(range)
        })
    }

    /// Builds a [`CoverageIndex`] once over this generation's still-retained coverage, so a caller checking many
    /// candidate ranges — an eviction pass walking every overlay segment — pays one indexing pass instead of an
    /// `covers` scan of every file, across every tenant, per candidate.
    pub fn coverage_index(&self) -> CoverageIndex {
        let mut grouped: HashMap<(TenantId, u64), Vec<(u64, u64)>> = HashMap::default();
        for entry in &self.files {
            if matches!(
                entry.part_state,
                PartState::Active | PartState::Outdated | PartState::DeleteOnDestroy
            ) {
                grouped
                    .entry((entry.tenant_id, entry.coverage.epoch))
                    .or_default()
                    .push((entry.coverage.first_sequence, entry.coverage.last_sequence));
            }
        }
        let by_tenant_epoch = grouped
            .into_iter()
            .map(|(key, mut ranges)| {
                ranges.sort_unstable_by_key(|&(first, _)| first);
                let mut firsts = Vec::with_capacity(ranges.len());
                let mut running_max_last = Vec::with_capacity(ranges.len());
                let mut max_last = 0u64;
                for (first, last) in ranges {
                    firsts.push(first);
                    max_last = max_last.max(last);
                    running_max_last.push(max_last);
                }
                (key, (firsts, running_max_last))
            })
            .collect();
        CoverageIndex { by_tenant_epoch }
    }
}

/// A per-tenant, per-epoch sorted coverage index built once by [`ManifestGeneration::coverage_index`]. Within a
/// tenant/epoch bucket, entries are sorted by `first_sequence` alongside a running maximum of `last_sequence`, so
/// checking whether one range is covered is a binary search plus one lookup rather than a scan.
#[derive(Debug, Default)]
pub struct CoverageIndex {
    by_tenant_epoch: HashMap<(TenantId, u64), (Vec<u64>, Vec<u64>)>,
}

impl CoverageIndex {
    /// True when some indexed entry for `tenant_id` fully contains `range` — the same containment `covers` checks,
    /// against the snapshot the index was built from.
    pub fn covers(&self, range: &SequenceRange, tenant_id: TenantId) -> bool {
        let Some((firsts, running_max_last)) = self.by_tenant_epoch.get(&(tenant_id, range.epoch)) else {
            return false;
        };
        let idx = firsts.partition_point(|&first| first <= range.first_sequence);
        idx > 0
            && running_max_last
                .get(idx - 1)
                .is_some_and(|&max_last| max_last >= range.last_sequence)
    }
}

#[cfg(test)]
#[path = "test/lifecycle.rs"]
mod tests;
