//! Seals and opens the blocks of a vector segment file under its own data key, and wraps that key under the caller's.

use super::constant::*;
use super::error::VectorError;
use crate::security::AeadScheme;
use zeroize::Zeroizing;

/// A 32-byte key that wraps each file's data key: a device key for files that stay on one device, or the user memory
/// key for files shared between a user's devices. HEF never stores it.
///
/// See: hef-vector-columns/spec.md
#[derive(Clone)]
pub struct WrappingKey(Zeroizing<[u8; KEY_LEN]>);

impl WrappingKey {
    /// Wraps the caller's 32 key bytes.
    pub fn new(bytes: [u8; KEY_LEN]) -> Self {
        WrappingKey(Zeroizing::new(bytes))
    }

    pub(crate) fn bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for WrappingKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WrappingKey(..)")
    }
}

/// One file's random data key and nonce prefix.
pub(crate) struct FileKeys {
    pub(crate) data_key: Zeroizing<[u8; KEY_LEN]>,
    pub(crate) nonce_prefix: [u8; NONCE_PREFIX_LEN],
}

impl FileKeys {
    pub(crate) fn random() -> Result<Self, VectorError> {
        let mut data_key = Zeroizing::new([0u8; KEY_LEN]);
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        getrandom::fill(data_key.as_mut()).map_err(|_| VectorError::Random)?;
        getrandom::fill(&mut nonce_prefix).map_err(|_| VectorError::Random)?;
        Ok(FileKeys { data_key, nonce_prefix })
    }

    /// Seals block `index` with hardware AES-256-GCM, the pinned default, so a scan decrypts at memory speed; its index is both in the nonce and the associated data, so a block moved to another slot
    /// or another file fails to open.
    pub(crate) fn seal(&self, index: u64, plaintext: &[u8]) -> Result<Vec<u8>, VectorError> {
        AeadScheme::AesGcm256
            .seal(&self.data_key, &self.nonce(index), &self.aad(index), plaintext)
            .ok_or(VectorError::Authentication { what: "seal block" })
    }

    pub(crate) fn open(&self, index: u64, sealed: &[u8]) -> Result<Zeroizing<Vec<u8>>, VectorError> {
        AeadScheme::AesGcm256
            .open(&self.data_key, &self.nonce(index), &self.aad(index), sealed)
            .map(Zeroizing::new)
            .ok_or(VectorError::Authentication { what: "block" })
    }

    /// The data key is fresh per file, so `prefix ‖ block index` never repeats under it.
    fn nonce(&self, index: u64) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce
            .iter_mut()
            .zip(self.nonce_prefix.iter().chain(index.to_le_bytes().iter()))
            .for_each(|(slot, byte)| {
                *slot = *byte;
            });
        nonce
    }

    fn aad(&self, index: u64) -> Vec<u8> {
        let mut aad = Vec::with_capacity(4 + 2 + NONCE_PREFIX_LEN + 8);
        aad.extend_from_slice(SEGMENT_MAGIC);
        aad.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
        aad.extend_from_slice(&self.nonce_prefix);
        aad.extend_from_slice(&index.to_le_bytes());
        aad
    }

    /// The clear header: magic, version, nonce prefix, and the data key wrapped under `wrapping`.
    pub(crate) fn header(&self, wrapping: &WrappingKey) -> Result<Vec<u8>, VectorError> {
        let mut wrap_nonce = [0u8; WRAP_NONCE_LEN];
        getrandom::fill(&mut wrap_nonce).map_err(|_| VectorError::Random)?;
        let wrapped = AeadScheme::XChaCha20Poly1305
            .seal(wrapping.bytes(), &wrap_nonce, &self.wrap_aad(), self.data_key.as_ref())
            .ok_or(VectorError::Authentication { what: "wrap data key" })?;
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(SEGMENT_MAGIC);
        header.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
        header.extend_from_slice(&0u16.to_le_bytes());
        header.extend_from_slice(&self.nonce_prefix);
        header.extend_from_slice(&wrap_nonce);
        header.extend_from_slice(&wrapped);
        header.resize(HEADER_LEN, 0);
        Ok(header)
    }

    /// Reads the clear header and unwraps the data key with `wrapping`.
    pub(crate) fn from_header(header: &[u8], wrapping: &WrappingKey) -> Result<Self, VectorError> {
        if header.len() < HEADER_USED_LEN || header.get(..4) != Some(SEGMENT_MAGIC.as_slice()) {
            return Err(VectorError::Format(crate::error::FormatError::BadMagic {
                expected: "HEFV",
            }));
        }
        let version = u16::from_le_bytes([header.get(4).copied().unwrap_or(0), header.get(5).copied().unwrap_or(0)]);
        if version != SEGMENT_VERSION {
            return Err(VectorError::Format(crate::error::FormatError::UnsupportedVersion {
                field: "vector segment version",
                found: u32::from(version),
            }));
        }
        let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
        nonce_prefix.copy_from_slice(header.get(8..8 + NONCE_PREFIX_LEN).unwrap_or(&[0u8; NONCE_PREFIX_LEN]));
        let wrap_start = 8 + NONCE_PREFIX_LEN;
        let wrap_nonce = header.get(wrap_start..wrap_start + WRAP_NONCE_LEN).unwrap_or_default();
        let wrapped = header
            .get(wrap_start + WRAP_NONCE_LEN..HEADER_USED_LEN)
            .unwrap_or_default();
        let mut keys = FileKeys {
            data_key: Zeroizing::new([0u8; KEY_LEN]),
            nonce_prefix,
        };
        let opened = AeadScheme::XChaCha20Poly1305
            .open(wrapping.bytes(), wrap_nonce, &keys.wrap_aad(), wrapped)
            .map(Zeroizing::new)
            .ok_or(VectorError::Authentication { what: "data key" })?;
        keys.data_key
            .copy_from_slice(opened.get(..KEY_LEN).unwrap_or(&[0u8; KEY_LEN]));
        Ok(keys)
    }

    fn wrap_aad(&self) -> Vec<u8> {
        let mut aad = Vec::with_capacity(4 + 2 + NONCE_PREFIX_LEN + 8);
        aad.extend_from_slice(SEGMENT_MAGIC);
        aad.extend_from_slice(&SEGMENT_VERSION.to_le_bytes());
        aad.extend_from_slice(&self.nonce_prefix);
        aad.extend_from_slice(b"datakey\0");
        aad
    }
}
