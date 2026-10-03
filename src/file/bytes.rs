//! Safe little tools for reading and writing the raw bytes of the on-disk formats, used everywhere a format is parsed
//! or built.
//!
//! Every number in these formats is stored little-endian, and every read is bounds-checked: a decoder handed malformed
//! or hostile bytes returns an error instead of panicking or reading past the end of the buffer (the workspace denies
//! `indexing_slicing` precisely to force all byte access through helpers like these).

use super::error::CodecError;

/// A checked forward-only cursor over untrusted bytes.
#[derive(Debug, Clone, Copy)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    pub fn position(&self) -> usize {
        self.position
    }

    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.position)
    }

    pub fn take(&mut self, len: usize, what: &'static str) -> Result<&'a [u8], CodecError> {
        let end = self.position.checked_add(len).ok_or(CodecError::Truncated { what })?;
        let slice = self
            .bytes
            .get(self.position..end)
            .ok_or(CodecError::Truncated { what })?;
        self.position = end;
        Ok(slice)
    }

    pub fn u8(&mut self, what: &'static str) -> Result<u8, CodecError> {
        Ok(self.take(1, what)?.first().copied().unwrap_or(0))
    }

    pub fn u16(&mut self, what: &'static str) -> Result<u16, CodecError> {
        let bytes = self.take(2, what)?;
        Ok(u16::from_le_bytes(bytes.try_into().unwrap_or([0; 2])))
    }

    pub fn u32(&mut self, what: &'static str) -> Result<u32, CodecError> {
        let bytes = self.take(4, what)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap_or([0; 4])))
    }

    pub fn u64(&mut self, what: &'static str) -> Result<u64, CodecError> {
        let bytes = self.take(8, what)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8])))
    }

    pub fn i64(&mut self, what: &'static str) -> Result<i64, CodecError> {
        Ok(self.u64(what)? as i64)
    }

    pub fn u128(&mut self, what: &'static str) -> Result<u128, CodecError> {
        let bytes = self.take(16, what)?;
        Ok(u128::from_le_bytes(bytes.try_into().unwrap_or([0; 16])))
    }

    /// Reads `count` little-endian `u64` values in one go.
    ///
    /// The output is a `u64` allocation, so on a little-endian target the stream's bytes are already the values'
    /// in-memory form and the whole run copies with a single `memcpy` instead of a `from_le_bytes` per value.
    pub fn u64_vec(&mut self, count: usize, what: &'static str) -> Result<Vec<u64>, CodecError> {
        let bytes = self.take(count.saturating_mul(8), what)?;
        let mut values = vec![0u64; bytes.len() / 8];
        bytemuck::cast_slice_mut::<u64, u8>(&mut values).copy_from_slice(bytes);
        if cfg!(target_endian = "big") {
            for value in &mut values {
                *value = value.swap_bytes();
            }
        }
        Ok(values)
    }

    /// Reads `count` little-endian `u32` values in one go, the u32 analog of [`Reader::u64_vec`].
    ///
    /// The output is a `u32` allocation, so on a little-endian target the stream's bytes are already the values'
    /// in-memory form and the whole run copies with a single `memcpy` instead of a bounds-checked `u32()` call per
    /// value.
    pub fn u32_vec(&mut self, count: usize, what: &'static str) -> Result<Vec<u32>, CodecError> {
        let bytes = self.take(count.saturating_mul(4), what)?;
        let mut values = vec![0u32; bytes.len() / 4];
        bytemuck::cast_slice_mut::<u32, u8>(&mut values).copy_from_slice(bytes);
        if cfg!(target_endian = "big") {
            for value in &mut values {
                *value = value.swap_bytes();
            }
        }
        Ok(values)
    }

    /// Bounds an untrusted element count for use as a preallocation hint: no more elements than the remaining input
    /// could possibly encode at `min_element_bytes` apiece. Decode loops still read exactly `count` elements and fail
    /// closed on truncation; this only keeps a forged count field from driving an unbounded `Vec::with_capacity`.
    pub fn capacity_hint(&self, count: usize, min_element_bytes: usize) -> usize {
        count.min(self.remaining() / min_element_bytes.max(1))
    }
}

/// Reads a whole little-endian sub-slice without a cursor.
pub fn slice<'a>(bytes: &'a [u8], start: usize, len: usize, what: &'static str) -> Result<&'a [u8], CodecError> {
    let end = start.checked_add(len).ok_or(CodecError::Truncated { what })?;
    bytes.get(start..end).ok_or(CodecError::Truncated { what })
}

/// Little-endian writer over a growable buffer.
#[derive(Debug, Default)]
pub struct Writer {
    bytes: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
        }
    }

    /// Wraps an existing buffer, keeping its allocated capacity — for a caller reusing one scratch buffer across
    /// many encodes instead of allocating a fresh one each time. `bytes` becomes the writer's initial content, so a
    /// caller starting a new encode should clear it first.
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Empties the buffer without releasing its allocation, so a writer reused for the next value — or a caller that
    /// wrote a failed attempt and wants another try — allocates nothing.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn put_slice(&mut self, slice: &[u8]) {
        self.bytes.extend_from_slice(slice);
    }

    /// Reserves `len` zeroed bytes at the end of the buffer and returns them for the caller to fill in place —
    /// avoids building a separate `Vec<u8>` only to copy it in right after with [`Writer::put_slice`].
    pub fn reserve_bytes(&mut self, len: usize) -> &mut [u8] {
        let start = self.bytes.len();
        self.bytes.resize(start + len, 0);
        self.bytes.get_mut(start..).unwrap_or(&mut [])
    }

    pub fn put_u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    pub fn put_u16(&mut self, value: u16) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn put_u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn put_u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn put_i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    pub fn put_u128(&mut self, value: u128) {
        self.bytes.extend_from_slice(&value.to_le_bytes());
    }

    /// Writes every value little-endian, in order.
    ///
    /// On a little-endian target the slice's own bytes are already the stream, so the run appends with a single
    /// `memcpy` instead of an eight-byte push per value.
    pub fn put_u64_slice(&mut self, values: &[u64]) {
        if cfg!(target_endian = "little") {
            self.bytes.extend_from_slice(bytemuck::cast_slice(values));
        } else {
            for value in values {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }

    /// Writes every value little-endian, in order. The u32 analog of [`Writer::put_u64_slice`].
    pub fn put_u32_slice(&mut self, values: &[u32]) {
        if cfg!(target_endian = "little") {
            self.bytes.extend_from_slice(bytemuck::cast_slice(values));
        } else {
            for value in values {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }

    /// Writes every value little-endian, in order — the `u128` sibling of [`Writer::put_u64_slice`].
    pub fn put_u128_slice(&mut self, values: &[u128]) {
        if cfg!(target_endian = "little") {
            self.bytes.extend_from_slice(bytemuck::cast_slice(values));
        } else {
            for value in values {
                self.bytes.extend_from_slice(&value.to_le_bytes());
            }
        }
    }

    /// Writes a little-endian unsigned integer in exactly `width` bytes (1, 2, 3, or 4). The value must fit.
    pub fn put_uint_width(&mut self, value: u32, width: usize) {
        let le = value.to_le_bytes();
        self.bytes.extend_from_slice(le.get(..width).unwrap_or(&le));
    }

    /// Zero-pads to the next multiple of `alignment`.
    pub fn pad_to(&mut self, alignment: usize) {
        if alignment == 0 {
            return;
        }
        let rem = self.bytes.len() % alignment;
        if rem != 0 {
            self.bytes.resize(self.bytes.len() + (alignment - rem), 0);
        }
    }
}

#[cfg(test)]
#[path = "test/bytes.rs"]
mod tests;
