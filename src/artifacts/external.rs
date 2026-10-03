//! The pinned shape of a reference to payload bytes stored outside the arena — an oversized body kept in its own
//! object, inside an existing container, or in a shared sidecar.
//!
//! Only the reference tuple is pinned here; no placement machinery ships with it, and the writer emits no external
//! references yet. Pinning the descriptor before the first byte depends on it is the point: when oversized payloads
//! become real, every fleet already agrees on the bytes. A reader verifies whatever it fetches against the
//! descriptor's own BLAKE3 — an external payload never weakens the integrity chain — and refuses a placement kind it
//! does not recognize rather than dereferencing by guess.
//!
//! See: hef-physical-artifacts/spec.md

use crate::error::FormatError;
use crate::file::bytes::{Reader, Writer};

/// Longest `uri` an external-payload descriptor may carry, bounding hostile-descriptor decode.
const MAX_EXTERNAL_URI_LEN: usize = 4096;

/// Where an externally stored payload's bytes live. The domain is closed and pinned; a descriptor carrying any other
/// tag refuses to decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalPayloadKind {
    /// The payload is its own object, addressed by the surrounding context; `position`/`size` locate the bytes in it.
    DedicatedObject,
    /// A range inside an existing container named by `uri` — an oversized payload referenced in place, no copy.
    ExternalUri,
    /// A range inside a shared sidecar object packing many payloads.
    PackedSidecar,
}

impl ExternalPayloadKind {
    fn tag(self) -> u8 {
        match self {
            ExternalPayloadKind::DedicatedObject => 1,
            ExternalPayloadKind::ExternalUri => 2,
            ExternalPayloadKind::PackedSidecar => 3,
        }
    }
}

/// One external payload reference: which kind of target holds the bytes, where in it they sit, and the BLAKE3 of
/// exactly those bytes, so the reader admits only what the writer referenced. `uri` is present exactly for the
/// [`ExternalPayloadKind::ExternalUri`] kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPayloadRef {
    pub blake3: [u8; 32],
    pub kind: ExternalPayloadKind,
    /// Byte offset of the payload within the target.
    pub position: u64,
    /// Byte length of the payload within the target.
    pub size: u64,
    pub uri: Option<String>,
}

impl ExternalPayloadRef {
    /// Serializes the descriptor in its pinned byte shape: kind tag, position, size, BLAKE3, then — for the
    /// external-target kind only — the length-prefixed UTF-8 `uri`.
    pub fn encode(&self) -> Vec<u8> {
        let uri = self.uri.as_deref().unwrap_or_default();
        let mut out = Writer::with_capacity(1 + 8 + 8 + 32 + 4 + uri.len());
        out.put_u8(self.kind.tag());
        out.put_u64(self.position);
        out.put_u64(self.size);
        out.put_slice(&self.blake3);
        if self.kind == ExternalPayloadKind::ExternalUri {
            out.put_u32(uri.len() as u32);
            out.put_slice(uri.as_bytes());
        }
        out.into_bytes()
    }

    /// Reads a descriptor back, refusing an unknown placement kind, a malformed `uri`, or a `uri` on a kind that
    /// carries none — a reader never dereferences a reference it does not fully understand.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let mut reader = Reader::new(bytes);
        let kind = match reader.u8("external payload kind")? {
            1 => ExternalPayloadKind::DedicatedObject,
            2 => ExternalPayloadKind::ExternalUri,
            3 => ExternalPayloadKind::PackedSidecar,
            _ => {
                return Err(FormatError::Structural {
                    rule: "unknown external payload placement kind",
                });
            }
        };
        let position = reader.u64("external payload position")?;
        let size = reader.u64("external payload size")?;
        let mut blake3 = [0u8; 32];
        blake3.copy_from_slice(reader.take(32, "external payload blake3")?);
        let uri = if kind == ExternalPayloadKind::ExternalUri {
            let len = reader.u32("external payload uri length")? as usize;
            if len > MAX_EXTERNAL_URI_LEN {
                return Err(FormatError::Structural {
                    rule: "external payload uri exceeds its length bound",
                });
            }
            let raw = reader.take(len, "external payload uri")?;
            Some(
                std::str::from_utf8(raw)
                    .map_err(|_| FormatError::Structural {
                        rule: "external payload uri is not UTF-8",
                    })?
                    .to_owned(),
            )
        } else {
            None
        };
        Ok(Self {
            blake3,
            kind,
            position,
            size,
            uri,
        })
    }

    /// Admits fetched bytes only when they are exactly the referenced range: the declared size and the declared
    /// BLAKE3 both match. Anything else is rejected — an external payload never weakens the integrity chain.
    pub fn verify(&self, fetched: &[u8]) -> Result<(), FormatError> {
        if fetched.len() as u64 != self.size {
            return Err(FormatError::Structural {
                rule: "external payload bytes differ from the declared size",
            });
        }
        if crate::file::integrity::hash_tree(fetched).as_bytes() != &self.blake3 {
            return Err(FormatError::Structural {
                rule: "external payload bytes do not match the declared blake3",
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/external.rs"]
mod tests;
