//! Turns float vectors into the stored row bytes of each element type, and back.

use super::constant::*;
use super::model::VectorElement;
use half::f16;

/// Appends one stored row of `vector` in `element` form to `out`: the elements, then the per-row trailer for the
/// quantized types.
pub fn encode_row(element: VectorElement, vector: &[f32], out: &mut Vec<u8>) {
    match element {
        VectorElement::F32 => vector
            .iter()
            .for_each(|value| out.extend_from_slice(&value.to_le_bytes())),
        VectorElement::F16 => vector
            .iter()
            .for_each(|value| out.extend_from_slice(&f16::from_f32(*value).to_le_bytes())),
        VectorElement::I8 => {
            let (scale, offset) = i8_factors(vector);
            out.extend(vector.iter().map(|value| quantize_i8(*value, scale, offset) as u8));
            out.extend_from_slice(&scale.to_le_bytes());
            out.extend_from_slice(&offset.to_le_bytes());
        }
        VectorElement::U1 => {
            out.extend(vector.chunks(8).map(|chunk| {
                chunk.iter().enumerate().fold(
                    0u8,
                    |byte, (bit, value)| if *value > 0.0 { byte | (1 << bit) } else { byte },
                )
            }));
            let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
            let alignment = if norm > 0.0 {
                vector.iter().map(|value| value.abs()).sum::<f32>() / (norm * (vector.len() as f32).sqrt())
            } else {
                0.0
            };
            out.extend_from_slice(&norm.to_le_bytes());
            out.extend_from_slice(&alignment.to_le_bytes());
        }
    }
}

/// Decodes one stored row back to floats. A 1-bit row decodes to `±norm / √d` per element, its best unbiased guess.
pub fn decode_row(element: VectorElement, dimension: u32, row: &[u8]) -> Vec<f32> {
    let dimension = dimension as usize;
    match element {
        VectorElement::F32 => row
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(array(bytes)))
            .collect(),
        VectorElement::F16 => row
            .chunks_exact(2)
            .map(|bytes| f16::from_le_bytes(array(bytes)).to_f32())
            .collect(),
        VectorElement::I8 => {
            let (values, trailer) = row.split_at(dimension.min(row.len()));
            let (scale, offset) = trailer_pair(trailer);
            values.iter().map(|q| f32::from(*q as i8) * scale + offset).collect()
        }
        VectorElement::U1 => {
            let (bits, trailer) = row.split_at(dimension.div_ceil(8).min(row.len()));
            let (norm, _) = trailer_pair(trailer);
            let magnitude = norm / (dimension as f32).sqrt();
            (0..dimension)
                .map(|index| {
                    let set = bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0);
                    if set { magnitude } else { -magnitude }
                })
                .collect()
        }
    }
}

/// The RaBitQ estimate of `⟨query, x⟩` from `x`'s stored 1-bit row: `norm × ⟨query, s⟩ / alignment`, where `s` is the
/// sign vector scaled to unit length. Corrects the bias a plain sign-bit score carries.
pub fn u1_estimated_dot(query: &[f32], row: &[u8]) -> f32 {
    let dimension = query.len();
    let (bits, trailer) = row.split_at(dimension.div_ceil(8).min(row.len()));
    let (norm, alignment) = trailer_pair(trailer);
    if alignment <= 0.0 {
        return 0.0;
    }
    let signed: f32 = query
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let set = bits.get(index / 8).is_some_and(|byte| byte & (1 << (index % 8)) != 0);
            if set { *value } else { -*value }
        })
        .sum();
    norm * signed / ((dimension as f32).sqrt() * alignment)
}

/// The `scale` and `offset` that map `[min, max]` onto `[-128, 127]`.
pub(crate) fn i8_factors(vector: &[f32]) -> (f32, f32) {
    let min = vector.iter().copied().fold(f32::INFINITY, f32::min);
    let max = vector.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !min.is_finite() || !max.is_finite() || max <= min {
        return (1.0, if min.is_finite() { min } else { 0.0 });
    }
    let scale = (max - min) / 255.0;
    (scale, min + 128.0 * scale)
}

fn quantize_i8(value: f32, scale: f32, offset: f32) -> i8 {
    ((value - offset) / scale).round().clamp(-128.0, 127.0) as i8
}

pub(crate) fn trailer_pair(trailer: &[u8]) -> (f32, f32) {
    let first = trailer.get(0..4).map_or(0.0, |bytes| f32::from_le_bytes(array(bytes)));
    let second = trailer.get(4..8).map_or(0.0, |bytes| f32::from_le_bytes(array(bytes)));
    (first, second)
}

fn array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.iter_mut().zip(bytes).for_each(|(slot, byte)| *slot = *byte);
    out
}

pub(crate) fn binarize(vector: &[f32]) -> Vec<u8> {
    let mut row = Vec::with_capacity(vector.len().div_ceil(8) + U1_TRAILER_BYTES);
    encode_row(VectorElement::U1, vector, &mut row);
    row.truncate(vector.len().div_ceil(8));
    row
}
