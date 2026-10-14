//! Seals a tenant's state chunks so that two identical chunks seal to identical bytes and are stored once.
//!
//! A state chunk is addressed by its content, so it must seal the same way every time. Two keys come from the tenant
//! DEK: an address key, whose keyed BLAKE3 of the plaintext is the chunk's address, and a seal key for
//! XChaCha20-Poly1305. The nonce is the address's first 24 bytes, a keyed hash of the whole plaintext (a
//! synthetic-nonce construction, as in SIV), so a nonce only ever repeats for the same plaintext. Within a tenant this
//! reveals which chunks are equal; across tenants it reveals nothing, since the same chunk gets unrelated addresses.
//!
//! See: hef-security-and-isolation/spec.md

use super::constant::{STATE_CHUNK_ADDRESS_CONTEXT, STATE_CHUNK_SEAL_CONTEXT};
use super::{AeadScheme, KEY_LEN, TenantDek};
use std::fmt;
use zeroize::Zeroizing;

/// One tenant's key for its state chunks: it gives each chunk its address and seals it so that the same chunk always
/// seals to the same bytes. Derived from the tenant DEK, so it is never stored on its own; its material is scrubbed
/// from memory when dropped.
///
/// See: hef-security-and-isolation/spec.md
#[derive(Clone)]
pub struct StateChunkKey {
    address: Zeroizing<[u8; KEY_LEN]>,
    seal: Zeroizing<[u8; KEY_LEN]>,
}

impl StateChunkKey {
    /// The state chunk key of the tenant whose DEK is `tenant_dek`. The same DEK always gives the same key; after a
    /// DEK rotation no earlier chunk keeps its address.
    pub fn derive(tenant_dek: &TenantDek) -> Self {
        Self {
            address: Zeroizing::new(blake3::derive_key(STATE_CHUNK_ADDRESS_CONTEXT, &*tenant_dek.0)),
            seal: Zeroizing::new(blake3::derive_key(STATE_CHUNK_SEAL_CONTEXT, &*tenant_dek.0)),
        }
    }

    /// The address a chunk with these plaintext bytes is stored under: 32 bytes that differ between tenants for the
    /// same chunk.
    pub fn address(&self, plaintext: &[u8]) -> [u8; 32] {
        *blake3::keyed_hash(&self.address, plaintext).as_bytes()
    }

    /// Seals `plaintext` and returns its address and the bytes to store under it. Sealing the same plaintext again
    /// returns the same address and bytes. `None` only for a plaintext far larger than any chunk, which the cipher
    /// refuses.
    pub fn seal(&self, plaintext: &[u8]) -> Option<([u8; 32], Vec<u8>)> {
        let address = self.address(plaintext);
        let nonce = address.first_chunk::<24>()?;
        let sealed = AeadScheme::XChaCha20Poly1305.seal(&self.seal, nonce, &address, plaintext)?;
        Some((address, sealed))
    }

    /// Opens the bytes stored under `address`. Returns the plaintext, or `None` when the bytes were altered, belong to
    /// another address, or were sealed with another tenant's key.
    pub fn open(&self, address: &[u8; 32], sealed: &[u8]) -> Option<Vec<u8>> {
        let nonce = address.first_chunk::<24>()?;
        let plaintext = AeadScheme::XChaCha20Poly1305.open(&self.seal, nonce, address, sealed)?;
        (self.address(&plaintext) == *address).then_some(plaintext)
    }
}

impl fmt::Debug for StateChunkKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StateChunkKey(<redacted>)")
    }
}

#[cfg(test)]
#[path = "test/state_chunk.rs"]
mod tests;
