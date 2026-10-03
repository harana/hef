use super::*;
use crate::file::error::FileError;

#[test]
fn slice_range_clamps_length_and_rejects_a_past_end_offset() {
    let bytes = b"abcdef";
    assert_eq!(slice_range(bytes, 2, Some(2)).unwrap(), b"cd");
    assert_eq!(slice_range(bytes, 2, Some(100)).unwrap(), b"cdef");
    assert_eq!(slice_range(bytes, 2, None).unwrap(), b"cdef");
    assert_eq!(slice_range(bytes, 6, None).unwrap(), b"");
    assert!(matches!(
        slice_range(bytes, 7, None),
        Err(FileError::InvalidRange { .. })
    ));
}

#[test]
fn verify_checksum_accepts_matching_bytes_and_rejects_mismatches() {
    let bytes = b"payload";
    let hash = *blake3::hash(bytes).as_bytes();
    assert!(verify_checksum(bytes, &hash, bytes.len() as u64).is_ok());
    assert_eq!(verify_checksum(bytes, &hash, 999), Err(FileError::ChecksumMismatch));
    let mut wrong = hash;
    wrong[0] ^= 1;
    assert_eq!(
        verify_checksum(bytes, &wrong, bytes.len() as u64),
        Err(FileError::ChecksumMismatch)
    );
}
