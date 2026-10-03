//! An in-memory fleet and portable decoder for tests and examples, plus the toy optional encoding they read.
//!
//! Real deployments resolve a decoder name to software the fleet ships out of band and certifies against the
//! conformance suite. Here the "fleet" is just a map from name to decoder, and the "new encoding" is a stand-in: rows
//! stored big-endian, which an older reader's little-endian native code cannot read but a portable decoder transcodes
//! back to the canonical little-endian bytes. A decoder records whether the fleet certified it, so a test can model
//! both a trusted decoder and an uncertified one.

use super::model::{PortableDecoder, PortableDecoderFleet};
use crate::error::FormatError;
use hashbrown::HashMap;
use std::mem::size_of;

/// A fleet that resolves decoder names from an in-memory table.
#[derive(Debug, Default)]
pub struct SimulatedDecoderFleet {
    decoders: HashMap<String, SimulatedDecoder>,
}

impl SimulatedDecoderFleet {
    /// An empty fleet that resolves nothing — every escape hatch falls back to a scan.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a decoder under `decoder_ref`, returning the fleet so calls chain.
    pub fn with_decoder(mut self, decoder_ref: impl Into<String>, decoder: SimulatedDecoder) -> Self {
        self.decoders.insert(decoder_ref.into(), decoder);
        self
    }
}

impl PortableDecoderFleet for SimulatedDecoderFleet {
    fn resolve(&self, decoder_ref: &str) -> Option<&dyn PortableDecoder> {
        self.decoders
            .get(decoder_ref)
            .map(|decoder| decoder as &dyn PortableDecoder)
    }
}

/// A portable decoder for the toy big-endian encoding, recorded with whether the fleet certified it and the lowest
/// reader version it satisfies.
#[derive(Debug, Clone, Copy)]
pub struct SimulatedDecoder {
    conformant: bool,
    provides_version: u32,
}

impl SimulatedDecoder {
    /// A decoder the fleet has certified against the conformance suite and software-parity check, satisfying readers at
    /// or above `provides_version`.
    pub fn conformant(provides_version: u32) -> Self {
        Self {
            conformant: true,
            provides_version,
        }
    }

    /// A decoder the fleet ships but has *not* certified — a reader must decline it and fall back to a scan.
    pub fn uncertified(provides_version: u32) -> Self {
        Self {
            conformant: false,
            provides_version,
        }
    }
}

impl PortableDecoder for SimulatedDecoder {
    fn is_conformant_at(&self, min_reader_version: u32) -> bool {
        self.conformant && self.provides_version >= min_reader_version
    }

    fn decode(&self, block: &[u8]) -> Result<Vec<u8>, FormatError> {
        transcode_be_to_le(block)
    }
}

/// Encodes `values` into the toy big-endian optional block the examples and tests read back through a portable decoder.
pub fn encode_demo_block(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * size_of::<u64>());
    for value in values {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out
}

/// The canonical little-endian row bytes a *native* decoder would produce for the toy block. A portable decoder must
/// produce exactly these bytes, which is what "byte-for-byte identical to native" means in the conformance tests.
pub fn native_reference_decode(block: &[u8]) -> Result<Vec<u8>, FormatError> {
    transcode_be_to_le(block)
}

fn transcode_be_to_le(block: &[u8]) -> Result<Vec<u8>, FormatError> {
    if !block.len().is_multiple_of(size_of::<u64>()) {
        return Err(FormatError::Structural {
            rule: "optional block length must be a multiple of 8",
        });
    }
    let mut out = Vec::with_capacity(block.len());
    for chunk in block.chunks_exact(size_of::<u64>()) {
        let value = u64::from_be_bytes(chunk.try_into().unwrap_or([0; size_of::<u64>()]));
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(out)
}

#[cfg(test)]
#[path = "test/sim.rs"]
mod tests;
