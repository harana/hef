#![no_main]
use libfuzzer_sys::fuzz_target;

use hef::artifacts::state_chunk::{StateChunkStore, StateKey, StateRoot, state_diff, state_get};
use hef::error::StorageError;
use hef::file::integrity::crc64_nvme;
use hef::security::{StateChunkKey, TenantDek};
use std::collections::HashMap;

struct Chunks(HashMap<[u8; 32], Vec<u8>>);

impl StateChunkStore for Chunks {
    fn get(&self, address: &[u8; 32]) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.0.get(address).cloned())
    }
}

// Each input is a state chunk's plaintext, stored twice under one tenant's key: as given, so a bad CRC is met, and with
// a CRC that matches it appended, so the fuzzer spends its time on the header, the entry count and the entries. A
// lookup and a diff then start from each as a root.
fuzz_target!(|data: &[u8]| {
    let key = StateChunkKey::derive(&TenantDek::new([7; 32]));
    let mut stamped = data.to_vec();
    stamped.extend_from_slice(&crc64_nvme(data).to_le_bytes());
    let mut chunks = Chunks(HashMap::new());
    let mut roots = Vec::new();
    for plaintext in [data, &stamped] {
        if let Some((address, sealed)) = key.seal(plaintext) {
            chunks.0.insert(address, sealed);
            roots.push(StateRoot {
                address,
                entry_count: 0,
            });
        }
    }
    let probe = StateKey {
        state_key_id: 1,
        type_id: 2,
    };
    for root in &roots {
        let _ = state_get(&chunks, &key, root, probe);
    }
    if let [first, second] = roots.as_slice() {
        let _ = state_diff(&chunks, &key, first, second);
    }
});
