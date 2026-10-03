//! Guards the primary projection's sequence-range coverage metadata against `occurred_at` bucket clustering that would
//! interleave sequence numbers across bucket boundaries.

/// Returns `true` when sorting rows by their `occurred_at` bucket would keep each bucket's sequences contiguous — no
/// other bucket's sequences fall within any bucket's `[min_seq, max_seq]` interval — so granule-level sequence-range
/// coverage metadata stays exact after the clustering is applied.
///
/// Coverage breaks the moment two buckets interleave in sequence order: at that point a bucket-grouped granule would
/// carry a `[min, max]` range that includes sequences belonging to a different granule, turning the per-granule
/// coverage into a loose approximation and admitting false positives on sequence-range pruning. When `false` is
/// returned the caller should keep the primary, sequence-ordered projection unchanged.
///
/// `rows` must be supplied in primary (sequence) order: each entry is `(sequence, occurred_at_bucket)`.
pub fn clustering_preserves_sequence_coverage(rows: &[(u64, u32)]) -> bool {
    // Walk rows in sequence order. A bucket's presence is recorded the first time we see it. If we later return to a
    // bucket we had already left, the two buckets interleave — coverage breaks.
    let mut iter = rows.iter();
    let Some(&(_, first_bucket)) = iter.next() else {
        return true;
    };
    let mut seen = hashbrown::HashSet::new();
    let mut last_bucket = first_bucket;
    seen.insert(last_bucket);
    for &(_seq, bucket) in iter {
        if bucket != last_bucket {
            if !seen.insert(bucket) {
                // Bucket already seen and now re-entered: interleaved.
                return false;
            }
            last_bucket = bucket;
        }
    }
    true
}

#[cfg(test)]
#[path = "test/clustering.rs"]
mod tests;
