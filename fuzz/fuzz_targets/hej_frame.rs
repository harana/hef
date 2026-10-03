#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = hef::artifacts::frame::precheck_header(data);
    let _ = hef::artifacts::frame::decode_frame(data);
});
