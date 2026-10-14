#![no_main]
use libfuzzer_sys::fuzz_target;

use hef::file::integrity::hash_tree;
use hef::layout::footer::decode_footer;

// Each input is decoded as given, then again with every section checksum recomputed over the bytes its directory entry
// names, so mutations inside a section (the graph event-id index, chain cover, unresolved parents and state roots
// among them) reach that section's decoder instead of stopping at its checksum. The preamble is the format version,
// both feature words, the schema fingerprint and the section count (56 bytes); each directory entry is the id, offset,
// length and checksum (52 bytes).
fuzz_target!(|data: &[u8]| {
    let _ = decode_footer(data);
    let Some(count) = data
        .get(52..56)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4])) as usize)
    else {
        return;
    };
    let Some(area) = data.get(56 + count * 52..) else {
        return;
    };
    let mut resealed = data.to_vec();
    for (index, entry) in data[56..56 + count * 52].chunks_exact(52).enumerate() {
        let number = |at: usize| u64::from_le_bytes(entry[at..at + 8].try_into().unwrap_or([0; 8])) as usize;
        let (offset, len) = (number(4), number(12));
        if let Some(section) = offset.checked_add(len).and_then(|end| area.get(offset..end)) {
            let at = 56 + index * 52 + 20;
            resealed[at..at + 32].copy_from_slice(hash_tree(section).as_bytes());
        }
    }
    let _ = decode_footer(&resealed);
});
