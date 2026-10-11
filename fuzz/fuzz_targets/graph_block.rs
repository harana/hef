#![no_main]
use libfuzzer_sys::fuzz_target;

use hef::layout::graph::decode_granule_edges;

// The leading 6 bytes pick the granule's first sequence number, its row count, and how far past it the file ends, so
// corrupt offsets and out-of-range targets meet every boundary.
fuzz_target!(|data: &[u8]| {
    if let [a, b, c, d, e, f, block @ ..] = data {
        let first_sn = u64::from(u16::from_le_bytes([*a, *b]));
        let rows = u32::from(u16::from_le_bytes([*c, *d]));
        let file_end_sn = first_sn + u64::from(rows) + u64::from(u16::from_le_bytes([*e, *f]));
        let _ = decode_granule_edges(block, first_sn, rows, file_end_sn);
    }
});
