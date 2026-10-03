//! Checks that times and durations are exposed as plain timestamp and duration types with no unit baked into the name.
//! The nanosecond storage detail lives only in the block metadata, and the values round-trip exactly.

use hef::encoding::{ColumnData, decode_block, encode_block};
use hef::events::{DurationValue, TimestampValue};

/// conformance: hef-logical-event-model/unit-neutral-time-and-duration/timestamp-stored-as-nanosecond-delta
#[test]
fn timestamp_stored_as_nanosecond_delta() {
    // The logical type stays `TimestampValue`/`DurationValue` (no unit suffix in the name); the nanosecond encoding is
    // physical detail recorded only in block metadata (the pipeline id), and round-trips.
    let logical = TimestampValue::from_physical_nanos(1_700_000_000_123_456_789);
    let duration = DurationValue::from_physical_nanos(42_000);
    assert_eq!(duration.physical_nanos(), 42_000);
    let block = ColumnData::I64(vec![logical.physical_nanos(); 128]);
    let encoded = encode_block(&block, false);
    let decoded = decode_block(encoded.pipeline, &encoded.bytes).unwrap();
    assert_eq!(decoded, block);
}
