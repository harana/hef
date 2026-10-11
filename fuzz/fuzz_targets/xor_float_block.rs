#![no_main]
use libfuzzer_sys::fuzz_target;

use hef::encoding::{Compression, PipelineId, Transform, ValueKind, decode_block};
use hef::layout::time_series::TimeSeriesRollups;

// Every input is read as a Chimp float block body, so the fuzzer spends its time inside the XOR decoder rather than on
// pipeline ids that fail to parse, and as a time-series rollups section.
fuzz_target!(|data: &[u8]| {
    let _ = decode_block(PipelineId::new(Transform::Chimp, Compression::None, ValueKind::F64), data);
    let _ = TimeSeriesRollups::decode(data);
});
