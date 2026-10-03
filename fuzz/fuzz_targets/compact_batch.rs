#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Some((count, payload)) = data.split_first() {
        let _ = hef::artifacts::batch::decode_batch(payload, u32::from(*count));
    }
});
