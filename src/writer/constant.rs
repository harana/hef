//! Constants the write path shares across its builders.

use crate::error::FormatError;

/// Bloom bits spent per distinct reference in a granule's reference filter: about a 1% false-positive rate, the same
/// budget the identity-hash point filters use.
pub const REFERENCE_FILTER_BITS_PER_KEY: u32 = 10;

/// The error every path that would replace an audit file returns.
pub(crate) const AUDIT_REWRITE_REFUSED: FormatError = FormatError::Structural {
    rule: "an audit file is never rewritten, spliced or compacted",
};

/// Why a compaction publish refuses an audit input or output.
/// Rows an audit build digests at once before folding them into the hash chain, which bounds the digest scratch
/// whatever the file's size.
pub(crate) const AUDIT_DIGEST_BATCH_ROWS: usize = 4096;

pub(crate) const AUDIT_COMPACTION_REFUSED: &str = "an audit file is never compacted";

/// The width of a time-series file's rollup buckets when the build configuration names none: one minute of
/// `occurred_at`, in nanoseconds. Matches the finest resolution usage and stats views chart at, so their queries are
/// answered from whole buckets.
pub const DEFAULT_TIME_SERIES_BUCKET_NANOS: i64 = 60_000_000_000;

/// How many events a subject's open graph file takes before it is sealed: 4096, half a default 8192-row granule, so a
/// busy subject seals often enough that its newest events reach storage quickly and compaction merges the small files.
pub const DEFAULT_GRAPH_SEAL_EVENTS: u64 = 4096;

/// How long a subject's open graph file stays open before it is sealed whatever its size: five minutes, in nanoseconds,
/// matching the publish roll window so a quiet subject's events reach storage on the same schedule as everything else.
pub const DEFAULT_GRAPH_SEAL_AGE_NANOS: i64 = 300 * 1_000_000_000;

/// How long a state chunk no published root references stays stored after an upload last stored or reused it: 24
/// hours, in nanoseconds. It must be at least the longest reader lease and the longest an upload may take to publish
/// the file whose roots name its chunks.
pub const STATE_CHUNK_GRACE_NANOS: i64 = 24 * 60 * 60 * 1_000_000_000;
