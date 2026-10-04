//! Decides which local disks a cached block's copies land on: one disk, a full copy on every disk (mirror), or the
//! bytes split across disks (stripe).
//!
//! Everything placed here is rebuildable: a lost copy is re-fetched from object storage, and volume ids and local paths
//! never leave the node.
//!
//! See: hef-hardware-deployment/spec.md

use super::constant::DISK_TIER_DIR;
use super::error::PlacementError;
use super::volume::CacheVolume;
use hashbrown::HashSet;
use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

/// How a block's bytes are spread across the configured local disks.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalLayout {
    /// A full copy on every volume, so losing a disk loses nothing.
    Mirror,
    /// One copy on one volume, chosen by weight.
    Single,
    /// Contiguous chunks, one per volume, so reads and writes use every disk's bandwidth.
    Stripe,
}

/// One configured local disk the cache writes to.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVolume {
    pub id: String,
    pub path: String,
    /// Share of `single`-layout blocks this volume receives, relative to the other volumes' weights.
    pub weight: u16,
}

/// One planned copy or chunk of a block.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// Path relative to the volume root.
    pub relative_path: String,
    /// Which stripe chunk this write carries (`None` for whole copies).
    pub stripe_index: Option<u32>,
    pub volume_id: String,
}

/// Where a block's bytes go, and how many writes must succeed for the block to count as cached.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementPlan {
    /// Every configured volume for `mirror`, every chunk for `stripe`, one for `single`.
    pub required_writes: usize,
    pub writes: Vec<PlannedWrite>,
}

/// The validated volume and layout configuration, and the plans it produces.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Clone)]
pub struct PlacementPolicy {
    layout: LocalLayout,
    volumes: Vec<LocalVolume>,
}

impl PlacementPolicy {
    /// Validates the configuration: at least one volume, no duplicate ids or roots (two spellings of one directory count
    /// as a duplicate), and at least two volumes for `mirror` or `stripe`.
    pub fn new(layout: LocalLayout, volumes: Vec<LocalVolume>) -> Result<Self, PlacementError> {
        if volumes.is_empty() {
            return Err(PlacementError::NoVolumes);
        }
        let mut seen_ids = HashSet::new();
        let mut seen_roots: HashSet<PathBuf> = HashSet::new();
        for volume in &volumes {
            if !seen_ids.insert(volume.id.clone()) || !seen_roots.insert(distinct_location(&volume.path)) {
                return Err(PlacementError::DuplicateVolume { id: volume.id.clone() });
            }
        }
        let found = volumes.len();
        match layout {
            LocalLayout::Mirror if found < 2 => {
                return Err(PlacementError::InsufficientVolumes {
                    found,
                    layout: "mirror",
                });
            }
            LocalLayout::Stripe if found < 2 => {
                return Err(PlacementError::InsufficientVolumes {
                    found,
                    layout: "stripe",
                });
            }
            LocalLayout::Mirror | LocalLayout::Single | LocalLayout::Stripe => {}
        }
        Ok(Self { layout, volumes })
    }

    /// The configured layout.
    pub fn layout(&self) -> LocalLayout {
        self.layout
    }

    /// The configured volumes, in configuration order.
    pub fn volumes(&self) -> &[LocalVolume] {
        &self.volumes
    }

    /// Plans the writes for one block stored under `name`, a path-safe identifier chosen by the caller. The same name
    /// always plans onto the same volumes, on this node and after a restart.
    pub fn plan(&self, name: &str) -> PlacementPlan {
        let relative = format!("{DISK_TIER_DIR}/{name}");
        let writes: Vec<PlannedWrite> = match self.layout {
            LocalLayout::Single => self
                .single_volume(name)
                .map(|volume| PlannedWrite {
                    relative_path: relative.clone(),
                    stripe_index: None,
                    volume_id: volume.id.clone(),
                })
                .into_iter()
                .collect(),
            LocalLayout::Mirror => self
                .volumes
                .iter()
                .map(|volume| PlannedWrite {
                    relative_path: relative.clone(),
                    stripe_index: None,
                    volume_id: volume.id.clone(),
                })
                .collect(),
            LocalLayout::Stripe => self
                .volumes
                .iter()
                .enumerate()
                .map(|(index, volume)| PlannedWrite {
                    relative_path: format!("{relative}.stripe-{index:03}"),
                    stripe_index: u32::try_from(index).ok(),
                    volume_id: volume.id.clone(),
                })
                .collect(),
        };
        PlacementPlan {
            required_writes: writes.len(),
            writes,
        }
    }

    /// The volume a `single`-layout block is placed on: spread across the volumes in proportion to their weights, keyed
    /// by the block's name. A configuration whose weights are all zero falls back to the first volume.
    fn single_volume(&self, name: &str) -> Option<&LocalVolume> {
        let total: u64 = self.volumes.iter().map(|volume| u64::from(volume.weight)).sum();
        if total == 0 {
            return self.volumes.first();
        }
        let mut point = stable_hash(name) % total;
        for volume in &self.volumes {
            let weight = u64::from(volume.weight);
            if point < weight {
                return Some(volume);
            }
            point -= weight;
        }
        self.volumes.last()
    }

    /// The bytes write `index` of `plan` carries: the whole block for `single`/`mirror`, one contiguous chunk for
    /// `stripe`.
    fn chunk<'a>(&self, plan: &PlacementPlan, bytes: &'a [u8], index: usize) -> &'a [u8] {
        match self.layout {
            LocalLayout::Mirror | LocalLayout::Single => bytes,
            LocalLayout::Stripe => {
                let stripes = plan.writes.len().max(1);
                let chunk = bytes.len().div_ceil(stripes);
                let start = (index * chunk).min(bytes.len());
                let end = ((index + 1) * chunk).min(bytes.len());
                bytes.get(start..end).unwrap_or_default()
            }
        }
    }
}

/// Writes every planned copy or chunk of `bytes` to its volume, enforcing the plan's required write count. A `stripe`
/// plan fails on its first lost chunk and a `mirror` plan fails when any copy is lost; either way the writes that did
/// land are deleted, so a failed placement leaves the volumes clean.
pub fn stage_local_copies(
    policy: &PlacementPolicy,
    plan: &PlacementPlan,
    bytes: &[u8],
    volumes: &BTreeMap<String, Box<dyn CacheVolume>>,
) -> Result<(), PlacementError> {
    let mut achieved: Vec<&PlannedWrite> = Vec::new();
    let mut first_stripe_failure: Option<u32> = None;
    for (index, write) in plan.writes.iter().enumerate() {
        let chunk = policy.chunk(plan, bytes, index);
        let written = match volumes.get(&write.volume_id) {
            Some(volume) => volume.write_blob(&write.relative_path, chunk),
            None => Err(PlacementError::VolumeIo {
                detail: "no such configured volume".to_owned(),
                volume_id: write.volume_id.clone(),
            }),
        };
        match written {
            Ok(()) => achieved.push(write),
            Err(_) => {
                if first_stripe_failure.is_none() {
                    first_stripe_failure = write.stripe_index;
                }
            }
        }
    }
    let failure = match first_stripe_failure {
        Some(stripe_index) => Some(PlacementError::StripeWriteFailed { stripe_index }),
        None if achieved.len() < plan.required_writes => Some(PlacementError::MirrorCopiesNotMet {
            achieved: achieved.len(),
            required: plan.required_writes,
        }),
        None => None,
    };
    match failure {
        Some(error) => {
            delete_writes(volumes, achieved);
            Err(error)
        }
        None => Ok(()),
    }
}

/// Deletes every planned copy or chunk in `writes` from its volume. A copy that is already gone is not an error.
pub(super) fn delete_writes<'a>(
    volumes: &BTreeMap<String, Box<dyn CacheVolume>>,
    writes: impl IntoIterator<Item = &'a PlannedWrite>,
) {
    for write in writes {
        if let Some(volume) = volumes.get(&write.volume_id) {
            volume.delete_blob(&write.relative_path);
        }
    }
}

/// The physical location a configured volume path names, so two spellings of one place (`/mnt/cache` and
/// `/mnt/cache/.`, or two symlinks to the same directory) cannot be counted as two distinct volumes.
///
/// Each prefix that exists is resolved through the filesystem, which collapses trailing separators and symlinks, so a
/// `..` is taken off an already-resolved prefix. A path that does not exist yet keeps its missing tail appended to the
/// longest ancestor that does.
fn distinct_location(path: &str) -> PathBuf {
    let mut resolved = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            component => {
                resolved.push(component);
                if let Ok(real) = std::fs::canonicalize(&resolved) {
                    resolved = real;
                }
            }
        }
    }
    resolved
}

/// FNV-1a over a block name. Fixed by this code rather than by a library version, so the same block lands on the same
/// volume across processes and releases.
fn stable_hash(name: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
#[path = "test/placement.rs"]
mod tests;
