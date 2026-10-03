//! Checks that steadily-increasing columns like timestamps are stored with an encoding that exploits their pattern
//! (storing the gaps between values), not just dropped into a general-purpose byte compressor. General compression may
//! only be a final touch-up after that pattern-aware step.
use hef::encoding::{ColumnData, Transform, encode_block};

/// conformance:
/// hef-encodings-and-compression/monotonic-columns-use-fastlanes-style-candidates/reject-lz4-only-timestamp-storage
#[test]
fn reject_lz4_only_timestamp_storage() {
    // occurred_at-shaped data selects an adaptive transform (FOR/DELTA family); LZ4 appears only as a trailing stage
    // after the transform, never as the storage itself.
    let nanos: Vec<i64> = (0..8192).map(|i| 1_700_000_000_000_000_000 + i * 1_000).collect();
    let encoded = encode_block(&ColumnData::I64(nanos), false);
    let transform = encoded.pipeline.transform().unwrap();
    assert!(
        matches!(
            transform,
            Transform::ForBitpack | Transform::DeltaBitpack | Transform::Rle
        ),
        "timestamps must use an adaptive transform, got {transform:?}"
    );
}
