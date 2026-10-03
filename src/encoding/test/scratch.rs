use super::*;

/// The point of the buffer: the next decode starts from the capacity the last one grew to, and starts empty.
#[test]
fn the_buffer_comes_back_grown_and_empty() {
    with_unpack_buffer(|buffer| buffer.resize(4096, 7));
    with_unpack_buffer(|buffer| {
        assert!(buffer.is_empty(), "handed to a decode empty");
        assert!(buffer.capacity() >= 4096, "kept the capacity the last decode needed");
    });
}

/// A decode that starts another one must not have its own values written over.
#[test]
fn a_nested_decode_gets_a_buffer_of_its_own() {
    with_unpack_buffer(|outer| {
        outer.push(1);
        with_unpack_buffer(|inner| {
            assert!(inner.is_empty());
            inner.push(2);
        });
        assert_eq!(outer.as_slice(), &[1], "the outer decode still holds its own values");
    });
}

/// One unusually wide decode must not leave the thread holding its buffer for good.
#[test]
fn a_buffer_past_the_retained_bound_is_dropped() {
    with_unpack_buffer(|buffer| buffer.resize(MAX_RETAINED_SCRATCH_VALUES + 1, 0));
    with_unpack_buffer(|buffer| {
        assert!(buffer.capacity() <= MAX_RETAINED_SCRATCH_VALUES);
    });
}

/// The arena buffer keeps the same bargain for the block searches that fill it.
#[test]
fn the_arena_buffer_comes_back_grown_and_empty() {
    with_arena_buffer(|buffer| buffer.resize(4096, b'x'));
    with_arena_buffer(|buffer| {
        assert!(buffer.is_empty(), "handed to a search empty");
        assert!(buffer.capacity() >= 4096, "kept the capacity the last search needed");
    });
    with_arena_buffer(|buffer| buffer.resize(MAX_RETAINED_ARENA_BYTES + 1, 0));
    with_arena_buffer(|buffer| {
        assert!(
            buffer.capacity() <= MAX_RETAINED_ARENA_BYTES,
            "one wide block is not pinned"
        );
    });
}
