//! Stores a run of floating-point readings by keeping only the bits that changed since the reading before, which is
//! very little for a metric that moves slowly or holds still.
//!
//! This is the Chimp encoding (Liakos, Papakonstantinou and Kotidis, VLDB 2022), a refinement of Facebook's Gorilla.
//! The first value is stored whole. Every later value is XORed with the one before it and the XOR is written as a
//! two-bit control code plus a payload:
//!
//! - `00`: the XOR is zero, so the value repeats the previous one; nothing follows.
//! - `01`: the XOR has more than [`CHIMP_TRAILING_ZERO_THRESHOLD`] trailing zeros: a 3-bit leading-zero code
//!   ([`CHIMP_LEADING_ZEROS`]), a 6-bit count of the centre bits, then the centre bits.
//! - `10`: the XOR's rounded leading-zero count equals the one the last `11` value set: every bit after the leading
//!   zeros follows.
//! - `11`: a new 3-bit leading-zero code, then every bit after the leading zeros.
//!
//! Block layout: `u32` value count, `u32` stream byte length, then the stream, bits packed most significant first
//! and the last byte zero-padded. The codec works on the values' bit patterns and never on their arithmetic, so it
//! reproduces every value bit for bit: NaN payloads, both zeros and both infinities included. Decoding runs from the
//! first value to the last; a block has no per-value offsets.
//!
//! See: hef-encodings-and-compression/spec.md

use super::MAX_BLOCK_VALUES;
use super::constant::{CHIMP_LEADING_ZEROS, CHIMP_TRAILING_ZERO_THRESHOLD};
use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};

/// Marks that no `11` value has set a leading-zero count to reuse since the stream began or since a `00` or `01`
/// value cleared it.
const NO_STORED_LEADING: u32 = u32::MAX;

/// Writes `values` as one Chimp block.
pub(super) fn encode_chimp(values: &[f64], out: &mut Writer) {
    let mut bits = BitWriter::default();
    let mut previous = 0u64;
    let mut stored_leading = NO_STORED_LEADING;
    for (index, value) in values.iter().enumerate() {
        let current = value.to_bits();
        if index == 0 {
            bits.put(current, 64);
            previous = current;
            continue;
        }
        let xor = previous ^ current;
        previous = current;
        if xor == 0 {
            bits.put(0b00, 2);
            stored_leading = NO_STORED_LEADING;
            continue;
        }
        let (code, leading) = round_leading_zeros(xor.leading_zeros());
        let trailing = xor.trailing_zeros();
        if trailing > CHIMP_TRAILING_ZERO_THRESHOLD {
            let centre = 64 - leading - trailing;
            bits.put(0b01, 2);
            bits.put(code, 3);
            bits.put(u64::from(centre), 6);
            bits.put(xor >> trailing, centre);
            stored_leading = NO_STORED_LEADING;
        } else if leading == stored_leading {
            bits.put(0b10, 2);
            bits.put(xor, 64 - leading);
        } else {
            bits.put(0b11, 2);
            bits.put(code, 3);
            bits.put(xor, 64 - leading);
            stored_leading = leading;
        }
    }
    let stream = bits.finish();
    out.put_u32(values.len() as u32);
    out.put_u32(stream.len() as u32);
    out.put_slice(&stream);
}

/// Reads one Chimp block back into its values. Refuses a count the stream is too short to hold before allocating
/// for it, a malformed control sequence, a stream that ends mid-value, and any bytes or set padding bits past the
/// last value.
pub(super) fn decode_chimp(reader: &mut Reader<'_>) -> Result<Vec<f64>, FormatError> {
    let count = reader.u32("chimp value count")? as usize;
    let len = reader.u32("chimp stream length")? as usize;
    let stream = reader.take(len, "chimp stream")?;
    if count == 0 {
        if len != 0 {
            return Err(FormatError::Structural {
                rule: "an empty chimp block stores no stream",
            });
        }
        return Ok(Vec::new());
    }
    // The first value takes 64 bits and every later one at least two, so a forged count is refused here, before it
    // can size an allocation the stream could never fill.
    let needed_bits = (count as u64 - 1).saturating_mul(2).saturating_add(64);
    if count > MAX_BLOCK_VALUES || needed_bits > (len as u64).saturating_mul(8) {
        return Err(FormatError::Structural {
            rule: "chimp value count beyond what the stream can hold",
        });
    }
    let mut bits = BitReader { bit: 0, bytes: stream };
    let mut values = Vec::with_capacity(count);
    let mut previous = bits.take(64)?;
    values.push(f64::from_bits(previous));
    let mut stored_leading = NO_STORED_LEADING;
    for _ in 1..count {
        let xor = match bits.take(2)? {
            0b00 => {
                stored_leading = NO_STORED_LEADING;
                0
            }
            0b01 => {
                let leading = leading_zeros_for(bits.take(3)?);
                let centre = bits.take(6)? as u32;
                if centre == 0 || leading + centre > 64 {
                    return Err(FormatError::Structural {
                        rule: "chimp centre bits do not fit the value",
                    });
                }
                stored_leading = NO_STORED_LEADING;
                bits.take(centre)? << (64 - leading - centre)
            }
            0b10 => {
                if stored_leading == NO_STORED_LEADING {
                    return Err(FormatError::Structural {
                        rule: "chimp reuses a leading-zero count no value has set",
                    });
                }
                bits.take(64 - stored_leading)?
            }
            _ => {
                let leading = leading_zeros_for(bits.take(3)?);
                stored_leading = leading;
                bits.take(64 - leading)?
            }
        };
        previous ^= xor;
        values.push(f64::from_bits(previous));
    }
    if bits.bit.div_ceil(8) != stream.len() || bits.padding()? != 0 {
        return Err(FormatError::Structural {
            rule: "chimp stream continues past its last value",
        });
    }
    Ok(values)
}

/// The 3-bit code for a leading-zero count, and the count that code stands for: the largest entry of
/// [`CHIMP_LEADING_ZEROS`] not above `leading_zeros`.
fn round_leading_zeros(leading_zeros: u32) -> (u64, u32) {
    let code = CHIMP_LEADING_ZEROS
        .iter()
        .rposition(|&rounded| rounded <= leading_zeros)
        .unwrap_or(0);
    (code as u64, leading_zeros_for(code as u64))
}

/// The leading-zero count a 3-bit code stands for.
fn leading_zeros_for(code: u64) -> u32 {
    CHIMP_LEADING_ZEROS.get(code as usize).copied().unwrap_or(0)
}

/// Packs bit fields most significant bit first.
#[derive(Default)]
struct BitWriter {
    /// Holds the `pending` bits not yet written out, in its low bits; never more than seven between calls.
    accumulator: u128,
    bytes: Vec<u8>,
    pending: u32,
}

impl BitWriter {
    /// Appends the low `width` bits of `value`, `width` at most 64.
    fn put(&mut self, value: u64, width: u32) {
        if width == 0 {
            return;
        }
        let field = if width == 64 {
            value
        } else {
            value & ((1u64 << width) - 1)
        };
        self.accumulator = (self.accumulator << width) | u128::from(field);
        self.pending += width;
        while self.pending >= 8 {
            self.pending -= 8;
            self.bytes.push((self.accumulator >> self.pending) as u8);
        }
        self.accumulator &= (1u128 << self.pending) - 1;
    }

    /// The packed bytes, the last one padded with zero bits.
    fn finish(mut self) -> Vec<u8> {
        if self.pending > 0 {
            self.bytes.push((self.accumulator << (8 - self.pending)) as u8);
        }
        self.bytes
    }
}

/// Reads bit fields most significant bit first, refusing to read past the end.
struct BitReader<'a> {
    bit: usize,
    bytes: &'a [u8],
}

impl BitReader<'_> {
    /// The next `width` bits as an integer, `width` at most 64.
    fn take(&mut self, width: u32) -> Result<u64, FormatError> {
        if width == 0 {
            return Ok(0);
        }
        let end = self.bit + width as usize;
        let window = self
            .bytes
            .get(self.bit / 8..end.div_ceil(8))
            .ok_or(FormatError::Truncated { what: "chimp stream" })?;
        // At most nine bytes: a 64-bit field that starts mid-byte.
        let gathered = window
            .iter()
            .fold(0u128, |gathered, byte| (gathered << 8) | u128::from(*byte));
        let unused_tail = window.len() * 8 - (self.bit % 8) - width as usize;
        self.bit = end;
        Ok(((gathered >> unused_tail) & ((1u128 << width) - 1)) as u64)
    }

    /// The bits after the read position in its current byte, which a well-formed stream leaves zero.
    fn padding(&self) -> Result<u8, FormatError> {
        if self.bit.is_multiple_of(8) {
            return Ok(0);
        }
        let last = self
            .bytes
            .get(self.bit / 8)
            .ok_or(FormatError::Truncated { what: "chimp stream" })?;
        Ok(last & (0xFF >> (self.bit % 8)))
    }
}

#[cfg(test)]
#[path = "test/chimp.rs"]
mod tests;
