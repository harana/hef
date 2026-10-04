//! Fixed numbers for how HEF lays out its objects: how often the catalogue writes a full checkpoint, and the smallest
//! multipart part an object store accepts.

/// Every this-many generations the catalogue stores a full checkpoint; the generations in between store only their
/// changes since it. Bounds both a delta object's size (by the churn of at most this many publishes) and a reader's
/// work (one checkpoint plus one delta).
pub const CHECKPOINT_INTERVAL: u64 = 64;

/// The smallest part an S3-style multipart upload accepts for every part but the last: 5 MiB. A HEF smaller than this
/// is uploaded in one create-only PUT.
pub const MIN_MULTIPART_PART_BYTES: u64 = 5 * 1024 * 1024;
