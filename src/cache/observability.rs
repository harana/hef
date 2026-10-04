//! Counters that say whether the cache earns its keep: how many reads each tier caught, how well the memory tier's
//! cold segment compresses, and how many reads still had to go to durable storage.
//!
//! See: hef-hardware-deployment/spec.md

use std::sync::atomic::{AtomicU64, Ordering};

/// The running counters a memory tier's compressed cold segment is judged and sized from: hits by segment, how much
/// smaller demoted blocks got, and the time spent compressing and decompressing. Every figure only ever grows; read
/// them at any time.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Default)]
pub struct ColdSegmentMetrics {
    cold_hits: AtomicU64,
    compress_nanos: AtomicU64,
    decompress_nanos: AtomicU64,
    demoted_compressed_bytes: AtomicU64,
    demoted_original_bytes: AtomicU64,
    hot_hits: AtomicU64,
    incompressible_drops: AtomicU64,
    misses: AtomicU64,
}

impl ColdSegmentMetrics {
    /// How many reads found their block in the cold segment — each one a decompression instead of a slower re-fetch.
    pub fn cold_hits(&self) -> u64 {
        self.cold_hits.load(Ordering::Relaxed)
    }

    /// Total nanoseconds spent compressing evicted blocks, kept and dropped alike.
    pub fn compress_nanos(&self) -> u64 {
        self.compress_nanos.load(Ordering::Relaxed)
    }

    /// Total nanoseconds spent decompressing cold blocks on a hit.
    pub fn decompress_nanos(&self) -> u64 {
        self.decompress_nanos.load(Ordering::Relaxed)
    }

    /// Total compressed size of every block kept in the cold segment so far.
    pub fn demoted_compressed_bytes(&self) -> u64 {
        self.demoted_compressed_bytes.load(Ordering::Relaxed)
    }

    /// Total original size of every block kept in the cold segment so far.
    pub fn demoted_original_bytes(&self) -> u64 {
        self.demoted_original_bytes.load(Ordering::Relaxed)
    }

    /// How many reads found their block uncompressed in the hot segment.
    pub fn hot_hits(&self) -> u64 {
        self.hot_hits.load(Ordering::Relaxed)
    }

    /// How many evicted blocks missed the compression bar and were dropped instead of kept.
    pub fn incompressible_drops(&self) -> u64 {
        self.incompressible_drops.load(Ordering::Relaxed)
    }

    /// How many reads found nothing in either segment.
    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    /// Achieved compression ratio across every block kept in the cold segment (original / compressed; e.g. 4.0 means
    /// kept blocks shrank to a quarter of their size), or 0.0 while nothing has been kept.
    pub fn compression_ratio(&self) -> f64 {
        let compressed = self.demoted_compressed_bytes();
        if compressed == 0 {
            return 0.0;
        }
        self.demoted_original_bytes() as f64 / compressed as f64
    }

    pub(crate) fn record_cold_hit(&self, decompress_nanos: u64) {
        self.cold_hits.fetch_add(1, Ordering::Relaxed);
        self.decompress_nanos.fetch_add(decompress_nanos, Ordering::Relaxed);
    }

    pub(crate) fn record_demotion(&self, original_bytes: u64, compressed_bytes: u64, compress_nanos: u64) {
        self.demoted_original_bytes.fetch_add(original_bytes, Ordering::Relaxed);
        self.demoted_compressed_bytes
            .fetch_add(compressed_bytes, Ordering::Relaxed);
        self.compress_nanos.fetch_add(compress_nanos, Ordering::Relaxed);
    }

    pub(crate) fn record_hot_hit(&self) {
        self.hot_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_incompressible_drop(&self, compress_nanos: u64) {
        self.incompressible_drops.fetch_add(1, Ordering::Relaxed);
        self.compress_nanos.fetch_add(compress_nanos, Ordering::Relaxed);
    }

    pub(crate) fn record_miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }
}

/// The running counters of a [`super::TieredCache`]: how many reads the lower tier caught after the upper tier missed,
/// and how many had to fall through to durable storage (object-store GETs). Every figure only ever grows.
///
/// See: hef-hardware-deployment/spec.md
#[derive(Debug, Default)]
pub struct TieredCacheMetrics {
    durable_reads: AtomicU64,
    lower_hits: AtomicU64,
}

impl TieredCacheMetrics {
    /// How many reads missed every tier and fetched from durable storage. On object storage each one is a paid GET;
    /// the target for recently active data is zero.
    pub fn durable_reads(&self) -> u64 {
        self.durable_reads.load(Ordering::Relaxed)
    }

    /// How many reads missed the upper tier but were served by the lower one.
    pub fn lower_hits(&self) -> u64 {
        self.lower_hits.load(Ordering::Relaxed)
    }

    pub(crate) fn record_durable_read(&self) {
        self.durable_reads.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_lower_hit(&self) {
        self.lower_hits.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
#[path = "test/observability.rs"]
mod tests;
