//! Keeps recently read bytes of HEF files close at hand — in RAM, and optionally on a local NVMe disk — so a repeat read
//! never pays another round trip to object storage.
//!
//! Every tier plugs in through one interface, [`CacheTier`], so an embedding application can stack the tiers here or
//! supply its own. [`MemoryTier`] holds the hottest blocks in RAM and demotes the least-recently-used ones to a
//! compressed cold segment before dropping them; [`DiskTier`] keeps blocks on a local disk and re-verifies each one
//! against its BLAKE3 before serving it, placed on one disk, mirrored, or striped across several ([`placement`]) and
//! written on the accelerated `fallocate` path where the host supports it ([`volume`]); [`TieredCache`] stacks one tier
//! in front of another and counts the reads that had to fall through to durable storage. [`PageAdmissionCache`] keeps
//! only the repeatedly read pages of a large file, and [`peer`] asks other nodes for a file before object storage.
//!
//! Everything here is rebuildable acceleration state. Object storage plus the manifest stay authoritative: a lost,
//! evicted, or corrupt cached copy only costs a re-fetch, and a cached copy never decides what a reader may see. Blocks
//! are cached exactly as they sit on media, so a subject-encrypted block stays encrypted here and crypto-shredding still
//! works.
//!
//! See: hef-hardware-deployment/spec.md

pub mod api;
pub mod constant;
pub mod disk;
pub mod error;
pub mod memory;
pub mod model;
pub mod observability;
pub mod pages;
pub mod peer;
pub mod placement;
pub mod tiered;
pub mod volume;

pub use api::CacheTier;
pub use disk::DiskTier;
pub use error::*;
pub use memory::MemoryTier;
pub use model::{BlockKey, BlockKind};
pub use observability::{ColdSegmentMetrics, TieredCacheMetrics};
pub use pages::PageAdmissionCache;
pub use placement::{LocalLayout, LocalVolume, PlacementPolicy};
pub use tiered::TieredCache;
pub use volume::{AcceleratedCacheVolume, CacheVolume, FsCacheVolume};
