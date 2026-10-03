//! Lends out the working buffer a block decode fills and throws away, so a scan reuses one allocation instead of
//! taking a fresh one for every page it reads.
//!
//! Decoding a page of bit-packed integers needs somewhere to put the unpacked values before they become the column's
//! real output. That buffer's contents never outlive the decode, so each thread keeps one and hands it back grown to
//! whatever the widest page it has read needed. A scan over many pages and many columns then allocates once, not once
//! per page — the same reuse the Zstandard decoding context already gets in
//! [`decompressor`](super::decompressor).
//!
//! See: hef-encodings-and-compression/spec.md

use super::constant::{MAX_RETAINED_ARENA_BYTES, MAX_RETAINED_SCRATCH_VALUES};
use std::cell::RefCell;

thread_local! {
    /// This thread's plaintext arena buffer, absent while a search holds it.
    static ARENA_BUFFER: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    /// This thread's decode buffer, absent while a decode holds it.
    static UNPACK_BUFFER: RefCell<Option<Vec<u64>>> = const { RefCell::new(None) };
}

/// Runs `search` with this thread's reusable plaintext buffer, emptied first, and keeps the buffer for the next block.
///
/// A scan that searches every block of a column decompresses each one into this buffer rather than taking a fresh
/// allocation per block. As with the unpack buffer, one unusually wide block's buffer is dropped rather than held for
/// the life of the thread.
pub(crate) fn with_arena_buffer<R>(search: impl FnOnce(&mut Vec<u8>) -> R) -> R {
    let mut buffer = ARENA_BUFFER.with(|slot| slot.borrow_mut().take()).unwrap_or_default();
    buffer.clear();
    let found = search(&mut buffer);
    if buffer.capacity() <= MAX_RETAINED_ARENA_BYTES {
        ARENA_BUFFER.with(|slot| *slot.borrow_mut() = Some(buffer));
    }
    found
}

/// Runs `decode` with this thread's reusable value buffer, emptied first, and keeps the buffer for the next decode.
///
/// The buffer is taken out of the thread's slot for the duration of the call, so a decode that starts another one
/// gets a buffer of its own rather than colliding with the outer decode's. A buffer that grew past the retained
/// bound — one wide page in a scan of narrow ones — is dropped instead of held for the life of the thread.
pub(crate) fn with_unpack_buffer<R>(decode: impl FnOnce(&mut Vec<u64>) -> R) -> R {
    let mut buffer = UNPACK_BUFFER.with(|slot| slot.borrow_mut().take()).unwrap_or_default();
    buffer.clear();
    let decoded = decode(&mut buffer);
    if buffer.capacity() <= MAX_RETAINED_SCRATCH_VALUES {
        UNPACK_BUFFER.with(|slot| *slot.borrow_mut() = Some(buffer));
    }
    decoded
}

#[cfg(test)]
#[path = "test/scratch.rs"]
mod tests;
