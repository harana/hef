#![no_main]
use libfuzzer_sys::fuzz_target;

use hef::encoding::{PipelineId, decode_block, decode_string_block_views};

fuzz_target!(|data: &[u8]| {
    if data.len() >= 4 {
        let (id_bytes, body) = data.split_at(4);
        let id = u32::from_le_bytes(id_bytes.try_into().unwrap_or([0; 4]));
        let _ = decode_block(PipelineId(id), body);
        let _ = decode_string_block_views(PipelineId(id), body);
    }
});
