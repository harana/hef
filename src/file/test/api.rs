use super::*;
use crate::file::error::FileError;
use crate::file::model::ByteRange;

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

struct SliceSource(Vec<u8>);

impl RangeSource for SliceSource {
    fn read_range(&self, _object: u128, offset: u64, len: u64) -> Result<Vec<u8>, FileError> {
        let end = offset.checked_add(len).ok_or(FileError::OutOfBounds)?;
        self.0
            .get(offset as usize..end as usize)
            .map(<[u8]>::to_vec)
            .ok_or(FileError::OutOfBounds)
    }
}

#[test]
fn read_ranges_returns_each_range_in_the_order_asked() {
    let source = SliceSource(b"abcdefgh".to_vec());
    let ranges = [ByteRange { len: 2, offset: 6 }, ByteRange { len: 3, offset: 0 }];
    assert_eq!(
        source.read_ranges(1, &ranges).unwrap(),
        vec![b"gh".to_vec(), b"abc".to_vec()]
    );
    assert_eq!(
        source.read_ranges(1, &[ByteRange { len: 4, offset: 6 }]),
        Err(FileError::OutOfBounds)
    );
}
