#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = hef::layout::decode_header(data);
    let _ = hef::layout::reader::HefFile::open(data.to_vec(), None);
});
