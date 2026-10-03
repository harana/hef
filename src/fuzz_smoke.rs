//! Throws junk and mangled bytes at every decoder to prove none of them ever crash on bad input.
//!
//! It is a deterministic smoke corpus: seeded random buffers plus byte-level mutations of valid artifacts, run on the
//! pinned stable toolchain in CI. The decoders must return `Ok`/`Err` without panicking, reading out of bounds, or
//! allocating unboundedly. The coverage-guided soak lives in `fuzz/` (nightly cargo-fuzz) and runs with the
//! verification change.

#![cfg(test)]

use super::artifacts::batch::{decode_batch, decode_variant_dictionary};
use super::artifacts::frame;
use super::encoding::{PipelineId, decode_block};
use super::events::variant::VariantRef;
use super::layout::footer::decode_footer;
use super::layout::{decode_header, reader::HefFile};

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next() as u8).collect()
    }
}

fn exercise_all(bytes: &[u8]) {
    let _ = frame::precheck_header(bytes);
    let _ = frame::decode_frame(bytes);
    if let Some((count, payload)) = bytes.split_first() {
        let _ = decode_batch(payload, u32::from(*count));
    }
    let (dict_bytes, value_bytes) = bytes.split_at(bytes.len() / 2);
    if let Ok(dictionary) = decode_variant_dictionary(dict_bytes) {
        let _ = VariantRef::new(value_bytes).validate(&dictionary);
    }
    let _ = decode_footer(bytes);
    let _ = decode_header(bytes);
    let _ = HefFile::open(bytes.to_vec(), None);
    if bytes.len() >= 4 {
        let id = u32::from_le_bytes(bytes.get(..4).unwrap_or(&[0; 4]).try_into().unwrap());
        let _ = decode_block(PipelineId(id), bytes.get(4..).unwrap_or_default());
    }
}

#[test]
fn random_buffers_never_panic_any_decoder() {
    let mut rng = XorShift(0xDEAD_BEEF_0BAD_F00D);
    for len in [0usize, 1, 7, 64, 191, 192, 193, 4096, 9000] {
        for _ in 0..16 {
            exercise_all(&rng.bytes(len));
        }
    }
}

#[test]
fn mutated_valid_frame_never_panics() {
    let frame_bytes = super::artifacts::batch::tests::sample_frame(3, 1, 1);
    let mut rng = XorShift(7);
    // Flip one byte at a spread of positions across the frame.
    let mut position = 0usize;
    while position < frame_bytes.len() {
        let mut mutated = frame_bytes.clone();
        mutated[position] ^= (rng.next() as u8) | 1;
        let _ = frame::precheck_header(&mutated);
        let _ = frame::decode_frame(&mutated);
        position += 37;
    }
    // Truncations at every boundary class.
    for cut in [0usize, 1, 191, 192, 256, frame_bytes.len() - 1] {
        let _ = frame::decode_frame(&frame_bytes[..cut]);
    }
}
