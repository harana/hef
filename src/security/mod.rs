//! Locks up the personal data inside an event file and lets a single person be forgotten by throwing away one key.
//!
//! Every sensitive block is sealed with a real authenticated cipher (AES-256-GCM by default, XChaCha20-Poly1305 as the
//! named alternative), so a tampered block is rejected on decrypt instead of quietly handing back altered bytes. Keys
//! come in a chain — a deployment master key wraps each tenant's data-encryption key (DEK), and the tenant DEK wraps a
//! per-subject content key — and "erasing" a person means destroying their content key so their ciphertext can never be
//! read again, anywhere it was copied, without rewriting a single immutable file.
//!
//! The nonce each seal uses is built from the key epoch and the block identity, so no two blocks under one key ever
//! share a nonce; when a key has sealed as many blocks as its per-key limit allows, the epoch is rolled to a fresh key
//! before the nonce space runs out. Each block also seals under its own subkey, derived from the content key and its
//! `(block_id, epoch)`, so even two seals whose deterministic nonces collide can never reuse one key-and-nonce
//! combination.

use crate::events::TenantId;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key as XChaChaKey, XChaCha20Poly1305, XNonce};
use hardware_rust_crypto::aes_gcm::HardwareAes256Gcm;
use hashbrown::{HashMap, HashSet};
use hkdf::Hkdf;
use sha2::Sha256;
use std::fmt;
use std::mem::size_of;
use std::sync::{Arc, Mutex, PoisonError};

/// HKDF-SHA256 label binding a wrapping key derived from a master key to its one job: wrapping per-tenant DEKs.
const MASTER_KEY_TENANT_DEK_INFO: &[u8] = b"harana/hef/master-key/tenant-dek/aes-256-gcm/v1";

/// HKDF-SHA256 label binding a wrapping key derived from a tenant DEK to its one job: wrapping per-subject content keys.
const TENANT_DEK_CONTENT_KEY_INFO: &[u8] = b"harana/hef/tenant-dek/content-key/aes-256-gcm/v1";

/// HKDF-SHA256 label binding a per-block sealing subkey derived from a content key to the single block and epoch it
/// seals, so a repeated `(block_id, key_epoch)` can never seal two different plaintexts under one key and nonce.
const CONTENT_KEY_BLOCK_SEAL_INFO: &[u8] = b"harana/hef/content-key/block-seal/v1";

/// Byte length of every AES-256 key, HKDF output, and content-key material this module handles (256 bits).
const KEY_LEN: usize = 32;

/// Derives a [`KEY_LEN`]-byte AES key from [`KEY_LEN`] bytes of input keying material with HKDF-SHA256, bound to
/// `info` so the same material fed to a different purpose can never derive the same key. Returns `None` only if HKDF
/// rejects the output length, which never happens for [`KEY_LEN`] bytes — the fallible signature just keeps this
/// panic-free. The derived key is scrubbed from memory when dropped.
fn derive_wrapping_key(input_key_material: &[u8; KEY_LEN], info: &[u8]) -> Option<zeroize::Zeroizing<[u8; KEY_LEN]>> {
    let hkdf = Hkdf::<Sha256>::new(None, input_key_material);
    let mut key = zeroize::Zeroizing::new([0u8; KEY_LEN]);
    hkdf.expand(info, &mut *key).ok()?;
    Some(key)
}

/// Derives the per-block sealing subkey for block `block_id` under epoch `key_epoch` from a content key, so every
/// distinct `(block_id, key_epoch)` seals under its own key material. Two seals whose 12-byte AES-256-GCM nonces
/// collide because they share the low 32 bits of the epoch still derive different subkeys here — the derivation binds
/// the full 64-bit epoch — so no `(key, nonce)` pair is ever reused across different plaintexts. Returns `None` only if
/// HKDF rejects the [`KEY_LEN`]-byte output length, which never happens.
fn derive_block_subkey(
    content_key: &[u8; KEY_LEN],
    block_id: u64,
    key_epoch: u64,
) -> Option<zeroize::Zeroizing<[u8; KEY_LEN]>> {
    let mut info = Vec::with_capacity(CONTENT_KEY_BLOCK_SEAL_INFO.len() + size_of::<u64>() * 2);
    info.extend_from_slice(CONTENT_KEY_BLOCK_SEAL_INFO);
    info.extend_from_slice(&block_id.to_le_bytes());
    info.extend_from_slice(&key_epoch.to_le_bytes());
    derive_wrapping_key(content_key, &info)
}

/// Splits a sealed blob into its leading nonce and the ciphertext that follows, or `None` if the blob is too short to
/// carry a full nonce for `scheme`.
fn split_nonce(sealed: &[u8], scheme: AeadScheme) -> Option<(&[u8], &[u8])> {
    if sealed.len() < scheme.nonce_len() {
        return None;
    }
    Some(sealed.split_at(scheme.nonce_len()))
}

/// The two approved authenticated-encryption primitives for HEF.
///
/// AES-256-GCM is the pinned default and covers the bulk of deployments. XChaCha20-Poly1305 is the named alternative
/// for high-volume paths that prefer a random extended (192-bit) nonce or a guaranteed constant-time software path. No
/// other cipher, and no hand-rolled AEAD construction, is permitted; a plain XOR or stream cipher with no
/// authentication tag is never a valid control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AeadScheme {
    /// AES-256-GCM via the hardware-only `hardware-rust-crypto` crate (AES-NI/PCLMULQDQ on x86_64, ARMv8 AES/PMULL on
    /// aarch64; no software fallback). 96-bit (12-byte) nonce. Pinned default for all HEF AEAD operations.
    #[default]
    AesGcm256,
    /// XChaCha20-Poly1305 via the RustCrypto `chacha20poly1305` crate. 192-bit (24-byte) nonce. Named alternative for
    /// extended-nonce or constant-time-software deployments.
    XChaCha20Poly1305,
}

impl AeadScheme {
    /// Nonce length in bytes: 12 for AES-256-GCM, 24 for XChaCha20-Poly1305.
    pub const fn nonce_len(self) -> usize {
        match self {
            AeadScheme::AesGcm256 => 12,
            AeadScheme::XChaCha20Poly1305 => 24,
        }
    }

    /// The crate name that implements this scheme. Recorded in `encryption_metadata` so any reader can verify the
    /// primitive without a full crate scan.
    pub const fn crate_name(self) -> &'static str {
        match self {
            AeadScheme::AesGcm256 => "hardware-rust-crypto",
            AeadScheme::XChaCha20Poly1305 => "chacha20poly1305",
        }
    }

    /// Seals `plaintext` under `key` and `nonce`, binding `aad` so a ciphertext cannot be relocated to another block or
    /// have its associated data altered undetected. Returns the ciphertext with its authentication tag appended, or
    /// `None` if the nonce length does not match the scheme. This is the one pinned primitive; callers reach it through
    /// the nonce-deriving helpers so the nonce construction stays pinned.
    pub(crate) fn seal(self, key: &[u8; KEY_LEN], nonce: &[u8], aad: &[u8], plaintext: &[u8]) -> Option<Vec<u8>> {
        match self {
            AeadScheme::AesGcm256 => HardwareAes256Gcm::new(key)
                .ok()?
                .encrypt_with_nonce(nonce, aad, plaintext)
                .ok(),
            AeadScheme::XChaCha20Poly1305 => {
                let nonce: [u8; 24] = nonce.try_into().ok()?;
                let key: XChaChaKey = (*key).into();
                let payload = Payload { msg: plaintext, aad };
                XChaCha20Poly1305::new(&key).encrypt(&XNonce::from(nonce), payload).ok()
            }
        }
    }

    /// Opens a sealed `ciphertext` under `key`, `nonce`, and `aad`. Returns the recovered plaintext, or `None` when the
    /// authentication tag fails — a tampered ciphertext, altered associated data, or the wrong key recovers nothing
    /// rather than altered plaintext.
    pub(crate) fn open(self, key: &[u8; KEY_LEN], nonce: &[u8], aad: &[u8], ciphertext: &[u8]) -> Option<Vec<u8>> {
        match self {
            AeadScheme::AesGcm256 => HardwareAes256Gcm::new(key)
                .ok()?
                .decrypt_with_nonce(nonce, aad, ciphertext)
                .ok(),
            AeadScheme::XChaCha20Poly1305 => {
                let nonce: [u8; 24] = nonce.try_into().ok()?;
                let key: XChaChaKey = (*key).into();
                let payload = Payload { msg: ciphertext, aad };
                XChaCha20Poly1305::new(&key).decrypt(&XNonce::from(nonce), payload).ok()
            }
        }
    }
}

/// The unique nonce for one AEAD seal, built from the key epoch and the block identity so that no two blocks under one
/// key ever share a nonce.
///
/// The first 8 bytes carry `block_id` (little-endian) and the next 8 carry `key_epoch` (little-endian); the whole is
/// then trimmed or zero-padded to the scheme's nonce length — 12 bytes for AES-256-GCM (block id plus the low 32 bits
/// of the epoch), 24 for XChaCha20-Poly1305 (block id, full epoch, then zero padding). Because the block identity is
/// also bound as associated data, the `(key_epoch, block_id)` pair is a collision-free counter and needs no random
/// draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockNonce {
    pub bytes: Vec<u8>,
    pub scheme: AeadScheme,
}

impl BlockNonce {
    /// Derives the nonce that seals block `block_id` under key epoch `key_epoch` for `scheme`.
    pub fn derive(scheme: AeadScheme, block_id: u64, key_epoch: u64) -> Self {
        // block_id (8 bytes, LE) then key_epoch (8 bytes, LE), resized to the scheme's nonce length: AES-256-GCM keeps
        // the block id and the low 32 bits of the epoch (12 bytes); XChaCha20-Poly1305 keeps both in full and
        // zero-pads to 24. `resize`/`extend_from_slice` avoid the index/slice expressions the workspace
        // `indexing_slicing = "deny"` lint rejects.
        let mut bytes = Vec::with_capacity(scheme.nonce_len().max(size_of::<u64>() * 2));
        bytes.extend_from_slice(&block_id.to_le_bytes());
        bytes.extend_from_slice(&key_epoch.to_le_bytes());
        bytes.resize(scheme.nonce_len(), 0);
        BlockNonce { bytes, scheme }
    }
}

/// Counts how many blocks a key has sealed under its current epoch and rolls to a fresh epoch before the per-key limit
/// is reached, so a key never exhausts its nonce space and never reuses a nonce.
///
/// The limit is the per-key message bound the spec requires each path to state: below `2^32` for random 96-bit GCM
/// nonces, the extended-nonce bound for XChaCha20-Poly1305. Rolling the epoch is the one mechanism that retires an
/// exhausted nonce space.
#[derive(Debug, Clone)]
pub struct KeyEpochOdometer {
    epoch: u64,
    messages_per_epoch: u64,
    sealed_this_epoch: u64,
}

impl KeyEpochOdometer {
    /// Starts at epoch 0 with a limit of `messages_per_epoch` seals before the epoch must roll. A limit of 0 is treated
    /// as 1 so the odometer always makes progress.
    pub fn new(messages_per_epoch: u64) -> Self {
        Self {
            epoch: 0,
            messages_per_epoch: messages_per_epoch.max(1),
            sealed_this_epoch: 0,
        }
    }

    /// The epoch the next seal will use.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// How many blocks have been sealed under the current epoch.
    pub fn sealed_this_epoch(&self) -> u64 {
        self.sealed_this_epoch
    }

    /// Reserves the next seal, rolling to a fresh epoch first if the current one has reached its limit, and returns the
    /// epoch to seal under. Call this once per block so the epoch rolls before the per-key nonce space is exhausted.
    pub fn reserve(&mut self) -> u64 {
        if self.sealed_this_epoch >= self.messages_per_epoch {
            self.epoch += 1;
            self.sealed_this_epoch = 0;
        }
        self.sealed_this_epoch += 1;
        self.epoch
    }
}

/// The deployment root key that wraps each tenant's data-encryption key. It is the top of the envelope chain and never
/// seals payload bytes directly. Its material is scrubbed from memory when the key is dropped.
#[derive(Clone)]
pub struct MasterKey(zeroize::Zeroizing<[u8; KEY_LEN]>);

impl MasterKey {
    /// Wraps a master key around [`KEY_LEN`] bytes of key material.
    pub fn new(key_material: [u8; KEY_LEN]) -> Self {
        Self(zeroize::Zeroizing::new(key_material))
    }

    /// Wraps `dek` for storage, binding it to `tenant` so a wrapped DEK cannot be moved to another tenant. The result
    /// is the random nonce followed by the sealed key material.
    pub fn wrap_tenant_dek(&self, dek: &TenantDek, tenant: TenantId, scheme: AeadScheme) -> Option<Vec<u8>> {
        let wrapping_key = derive_wrapping_key(&self.0, MASTER_KEY_TENANT_DEK_INFO)?;
        wrap_key_material(&wrapping_key, &dek.0, &tenant.uuid().to_bytes_le(), scheme)
    }

    /// Unwraps a tenant DEK from `wrapped`, checking it was wrapped for `tenant`. Returns `None` if the material was
    /// tampered with or was wrapped for a different tenant.
    pub fn unwrap_tenant_dek(&self, wrapped: &[u8], tenant: TenantId, scheme: AeadScheme) -> Option<TenantDek> {
        let wrapping_key = derive_wrapping_key(&self.0, MASTER_KEY_TENANT_DEK_INFO)?;
        let material = unwrap_key_material(&wrapping_key, wrapped, &tenant.uuid().to_bytes_le(), scheme)?;
        Some(TenantDek(material))
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MasterKey(<redacted>)")
    }
}

/// A per-tenant data-encryption key. It sits in the middle of the envelope chain: unwrapped from the master key, it in
/// turn wraps that tenant's per-subject content keys. Its material is scrubbed from memory when the key is dropped.
#[derive(Clone)]
pub struct TenantDek(zeroize::Zeroizing<[u8; KEY_LEN]>);

impl TenantDek {
    /// Wraps a tenant DEK around [`KEY_LEN`] bytes of key material.
    pub fn new(key_material: [u8; KEY_LEN]) -> Self {
        Self(zeroize::Zeroizing::new(key_material))
    }

    /// Wraps `content_key` for storage under `id`, binding it to that id so a wrapped content key cannot be moved to
    /// another subject. The result is the random nonce followed by the sealed key material.
    pub fn wrap_content_key(&self, content_key: &ContentKey, id: ContentKeyId, scheme: AeadScheme) -> Option<Vec<u8>> {
        let wrapping_key = derive_wrapping_key(&self.0, TENANT_DEK_CONTENT_KEY_INFO)?;
        wrap_key_material(&wrapping_key, &content_key.0, &id.uuid().to_bytes_le(), scheme)
    }

    /// Unwraps a content key from `wrapped`, checking it was wrapped under `id`. Returns `None` if the material was
    /// tampered with, was wrapped under a different id, or was wrapped under a different tenant DEK.
    pub fn unwrap_content_key(&self, wrapped: &[u8], id: ContentKeyId, scheme: AeadScheme) -> Option<ContentKey> {
        let wrapping_key = derive_wrapping_key(&self.0, TENANT_DEK_CONTENT_KEY_INFO)?;
        let material = unwrap_key_material(&wrapping_key, wrapped, &id.uuid().to_bytes_le(), scheme)?;
        Some(ContentKey(material))
    }
}

impl fmt::Debug for TenantDek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TenantDek(<redacted>)")
    }
}

/// A per-subject content key: the leaf of the envelope chain that actually seals one data subject's payload bytes.
/// Destroying it (crypto-shredding) is what makes that subject's ciphertext unrecoverable everywhere it persists. Its
/// material is scrubbed from memory when the key is dropped, so a destroyed key does not linger in freed memory.
#[derive(Clone)]
pub struct ContentKey(zeroize::Zeroizing<[u8; KEY_LEN]>);

impl ContentKey {
    /// Wraps a content key around [`KEY_LEN`] bytes of key material.
    pub fn new(key_material: [u8; KEY_LEN]) -> Self {
        Self(zeroize::Zeroizing::new(key_material))
    }

    /// Seals `plaintext` as block `block_id` of file `file_id` under key epoch `key_epoch`, binding both the block and
    /// the file identity as associated data. Each block seals under its own subkey, derived by HKDF from the content
    /// key and the full `(block_id, key_epoch)`, so even two seals whose deterministic nonces collide never reuse one
    /// `(key, nonce)` pair. The returned blob carries its own nonce and the epoch, so [`open`](Self::open) needs
    /// nothing but the blob, the key, and the block and file ids it expects.
    ///
    /// **Prefer [`SealedContentKey::encrypt`]**, which tracks used nonces and rolls the epoch so one block id never
    /// seals twice under the same epoch. Calling this directly and re-sealing the same `(block_id, key_epoch)` with
    /// different bytes still reuses that pair's `(subkey, nonce)`.
    pub(crate) fn seal(
        &self,
        block_id: u64,
        file_id: u128,
        key_epoch: u64,
        plaintext: &[u8],
        scheme: AeadScheme,
    ) -> Option<Vec<u8>> {
        let subkey = derive_block_subkey(&self.0, block_id, key_epoch)?;
        let nonce = BlockNonce::derive(scheme, block_id, key_epoch);
        let ciphertext = scheme.seal(&subkey, &nonce.bytes, &block_seal_aad(block_id, file_id), plaintext)?;
        let mut sealed = nonce.bytes;
        sealed.extend_from_slice(&ciphertext);
        sealed.extend_from_slice(&key_epoch.to_le_bytes());
        Some(sealed)
    }

    /// Opens a blob produced by [`seal`](Self::seal), checking it really is block `block_id`. Returns the plaintext, or
    /// `None` when the authentication tag fails — a tampered blob, or one replayed into a different block slot, is
    /// rejected rather than returning altered bytes.
    pub fn open(&self, block_id: u64, file_id: u128, sealed: &[u8], scheme: AeadScheme) -> Option<Vec<u8>> {
        // The trailing 8 bytes carry the full key epoch the block was sealed under, so the per-block subkey can be
        // re-derived even though AES-256-GCM's 12-byte nonce keeps only the low 32 bits of that epoch. The block and
        // file identity are authenticated as associated data using the ids the caller expects for this slot, not the
        // ids recovered from the blob itself — so replaying a whole sealed blob into a different block slot, or into a
        // different file, fails the tag rather than validating.
        let body_len = sealed.len().checked_sub(8)?;
        let (body, epoch_bytes) = sealed.split_at(body_len);
        let key_epoch = u64::from_le_bytes(epoch_bytes.try_into().ok()?);
        let subkey = derive_block_subkey(&self.0, block_id, key_epoch)?;
        let (nonce, ciphertext) = split_nonce(body, scheme)?;
        scheme.open(&subkey, nonce, &block_seal_aad(block_id, file_id), ciphertext)
    }
}

impl fmt::Debug for ContentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ContentKey(<redacted>)")
    }
}

/// Byte length of the associated data every block seal binds: a `u64` block id followed by a `u128` file id, both
/// little-endian.
const BLOCK_SEAL_AAD_LEN: usize = size_of::<u64>() + size_of::<u128>();

/// The associated data every per-subject block seal binds: the block id (8 bytes, little-endian) followed by the file
/// id (16 bytes, little-endian). Binding the file identity as well as the block identity means a sealed block cannot be
/// opened as part of a different file — a ciphertext relocated across files under a mishandled key fails the
/// authentication tag rather than decrypting. The file id rides the associated data, not the nonce, so the pinned nonce
/// layout `(block_id, key_epoch)` is unchanged.
fn block_seal_aad(block_id: u64, file_id: u128) -> [u8; BLOCK_SEAL_AAD_LEN] {
    let mut aad = [0u8; BLOCK_SEAL_AAD_LEN];
    let (block_bytes, file_bytes) = aad.split_at_mut(size_of::<u64>());
    block_bytes.copy_from_slice(&block_id.to_le_bytes());
    file_bytes.copy_from_slice(&file_id.to_le_bytes());
    aad
}

/// Wraps [`KEY_LEN`] bytes of key material under `wrapping_key` and a fresh random nonce, binding `aad`. Returns the
/// nonce followed by the sealed material. A random nonce is safe here because only a handful of keys are wrapped per
/// subject, far below any birthday bound; the payload-sealing path uses the deterministic block nonce instead.
fn wrap_key_material(
    wrapping_key: &[u8; KEY_LEN],
    material: &[u8; KEY_LEN],
    aad: &[u8],
    scheme: AeadScheme,
) -> Option<Vec<u8>> {
    let mut nonce = vec![0u8; scheme.nonce_len()];
    getrandom::fill(&mut nonce).ok()?;
    let ciphertext = scheme.seal(wrapping_key, &nonce, aad, material)?;
    let mut wrapped = nonce;
    wrapped.extend_from_slice(&ciphertext);
    Some(wrapped)
}

/// Reverses [`wrap_key_material`]: splits the nonce from `wrapped`, opens it under `wrapping_key` and `aad`, and
/// returns the [`KEY_LEN`] bytes of key material — or `None` if the tag fails or the material is not [`KEY_LEN`] bytes.
/// Both the transient plaintext buffer and the returned material are scrubbed from memory when dropped.
fn unwrap_key_material(
    wrapping_key: &[u8; KEY_LEN],
    wrapped: &[u8],
    aad: &[u8],
    scheme: AeadScheme,
) -> Option<zeroize::Zeroizing<[u8; KEY_LEN]>> {
    let (nonce, ciphertext) = split_nonce(wrapped, scheme)?;
    let material = zeroize::Zeroizing::new(scheme.open(wrapping_key, nonce, aad, ciphertext)?);
    let key: [u8; KEY_LEN] = material.as_slice().try_into().ok()?;
    Some(zeroize::Zeroizing::new(key))
}

/// Whether a file's footer (column directory and schema) is encrypted.
///
/// When `Encrypted`, the footer bytes are sealed under the file DEK so the column schema is opaque to a reader without
/// the key. The default is `Plaintext`; a tenant whose policy marks the column schema sensitive opts in by setting this
/// to `Encrypted` in the build configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FooterEncryption {
    /// Footer sealed under the file DEK. Required when tenant policy marks the column schema sensitive.
    Encrypted,
    /// Footer written as plaintext. Default when tenant policy does not require schema protection.
    #[default]
    Plaintext,
}

crate::typed_id::define_typed_id!(
    /// Stable identity of a per-subject content key in the keystore. Every single-subject PII block in HEF records
    /// this ID in its `encryption_metadata` so erasure can destroy exactly the right key.
    ContentKeyId, ContentKeyIdTag, "content_key"
);

/// A content key that tracks every nonce it has used, rolling the epoch before any nonce would ever repeat so a caller
/// that uses only this API can never trigger catastrophic AES-GCM nonce reuse.
///
/// `seal` on the underlying [`ContentKey`] is `pub(crate)` precisely so callers must route sealing through this type.
/// Sealing callers obtain one from a durable keystore through [`with_epoch_range`](Self::with_epoch_range):
/// each checkout reserves a fresh range of epochs from the key's durable high-water mark, so blocks sealed after a
/// restart — or on a second live node — resume past every epoch a previous checkout used instead of restarting at
/// epoch 0 and repeating its nonces. Callers that only need to open (the HEF reader path) can use the raw `ContentKey`
/// directly, as a read-only operation cannot cause nonce reuse.
#[derive(Clone)]
pub struct SealedContentKey {
    content_key: ContentKey,
    /// Exclusive upper bound of the epochs this instance may seal under. Epoch rolls stop here so this instance can
    /// never wander into an epoch range another checkout owns; `u64::MAX` for callers that manage epochs themselves.
    epoch_limit: u64,
    scheme: AeadScheme,
    /// The epoch counter and used-nonce set, shared across every clone so a clone guards nonce uniqueness against the
    /// same history its origin does. A private per-clone copy would let two clones at the same state each seal a
    /// different plaintext under one nonce — exactly the AES-GCM reuse this type exists to prevent.
    state: Arc<Mutex<SealState>>,
}

/// The mutable sealing progress for one content key: the epoch the next seal derives its nonce from, and every nonce
/// already sealed under this key. Held behind a lock and shared by clones so nonce uniqueness holds no matter how many
/// handles to the same key exist.
struct SealState {
    key_epoch: u64,
    /// Every AEAD nonce this key has already sealed under. `encrypt` rolls the epoch to a fresh nonce before it would
    /// ever repeat one of these, so no two blocks under this key can share a nonce.
    sealed_nonces: HashSet<Vec<u8>>,
}

impl SealedContentKey {
    /// Wraps an unwrapped `content_key` that seals under `key_epoch` and `scheme`. Prefer a durable keystore checkout
    /// through [`with_epoch_range`](Self::with_epoch_range), which resumes the epoch from the durable high-water mark;
    /// use this directly only to open sealed blocks or when the caller persists and resumes the epoch itself. A fresh
    /// instance at a previously used epoch can repeat a nonce.
    pub fn new(content_key: ContentKey, key_epoch: u64, scheme: AeadScheme) -> Self {
        Self::with_epoch_range(content_key, key_epoch, u64::MAX, scheme)
    }

    /// Wraps `content_key` sealing under epochs `key_epoch..epoch_limit` only. A durable keystore hands these out,
    /// one reserved range per checkout, so no two checkouts can ever seal under the same epoch. The caller must have
    /// durably reserved `key_epoch..epoch_limit` for this key before calling.
    pub fn with_epoch_range(content_key: ContentKey, key_epoch: u64, epoch_limit: u64, scheme: AeadScheme) -> Self {
        Self {
            content_key,
            epoch_limit,
            scheme,
            state: Arc::new(Mutex::new(SealState {
                key_epoch,
                sealed_nonces: HashSet::new(),
            })),
        }
    }

    /// The key epoch the next seal will use.
    pub fn key_epoch(&self) -> u64 {
        self.lock_state().key_epoch
    }

    /// The AEAD scheme this key seals with.
    pub fn scheme(&self) -> AeadScheme {
        self.scheme
    }

    /// Seals `plaintext` as block `block_id` of file `file_id`. Nonce uniqueness is enforced here: the nonce for `(block_id, key_epoch)` is
    /// derived, and if it has already sealed a block under this key (a re-seal of the same `block_id` within the same
    /// epoch), the epoch is rolled forward until the nonce is fresh before sealing. This makes AES-GCM nonce reuse —
    /// catastrophic for the cipher — impossible through this API regardless of what `block_id` the caller supplies.
    /// Returns `None` once the instance's reserved epoch range is spent: it refuses to seal rather than roll into an
    /// epoch another checkout may own — check out a fresh sealing key to continue.
    pub fn encrypt(&mut self, block_id: u64, file_id: u128, plaintext: &[u8]) -> Option<Vec<u8>> {
        let mut state = self.lock_state();
        let mut nonce = BlockNonce::derive(self.scheme, block_id, state.key_epoch);
        while state.sealed_nonces.contains(&nonce.bytes) {
            let next_epoch = state.key_epoch.checked_add(1).filter(|next| *next < self.epoch_limit)?;
            state.key_epoch = next_epoch;
            nonce = BlockNonce::derive(self.scheme, block_id, state.key_epoch);
        }
        let sealed = self
            .content_key
            .seal(block_id, file_id, state.key_epoch, plaintext, self.scheme)?;
        state.sealed_nonces.insert(nonce.bytes);
        Some(sealed)
    }

    /// Opens a sealed blob produced by [`encrypt`](Self::encrypt), checking it really is block `block_id` of file
    /// `file_id`. Returns the plaintext, or `None` when the authentication tag fails.
    pub fn decrypt(&self, block_id: u64, file_id: u128, sealed: &[u8]) -> Option<Vec<u8>> {
        self.content_key.open(block_id, file_id, sealed, self.scheme)
    }

    /// Borrows the shared sealing state, recovering the guard even if a previous holder panicked — a poisoned lock still
    /// carries a valid nonce set and epoch counter, so it is safe to keep using rather than a reason to refuse sealing.
    fn lock_state(&self) -> std::sync::MutexGuard<'_, SealState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl fmt::Debug for SealedContentKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SealedContentKey")
            .field("key_epoch", &self.key_epoch())
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

/// Outcome when accessing a single-subject PII block.
#[derive(Debug, PartialEq, Eq)]
pub enum DecryptOutcome {
    /// The content key is active and the block authenticated: the plaintext is available.
    Plaintext(Vec<u8>),
    /// The content key is active but the block failed authentication — its ciphertext or bound associated data was
    /// tampered with. The decrypt is rejected rather than returning altered plaintext.
    Rejected,
    /// The content key was destroyed by crypto-shredding (or was never present); the ciphertext is permanently
    /// unrecoverable. The block renders as a tombstone.
    Tombstone,
}

/// One entry in the keystore: either a live, wrapped content key, or the tombstone left after crypto-shredding.
enum ContentKeyEntry {
    Active { wrapped_key: Vec<u8> },
    Destroyed,
}

/// Holds every tenant's per-subject content keys in wrapped form and turns "erase this person" into "throw away one
/// key", never a file rewrite.
///
/// The store keeps only the wrapped form of each content key (wrapped under the tenant DEK it was handed at
/// construction). Reading a subject's block unwraps their content key with that DEK and then opens the block — both
/// steps reject tampering. Destroying a subject's key renders their ciphertext unrecoverable everywhere it was copied —
/// across HEF files, HEJ frames, and all backups — without touching any immutable file; the caller must also purge the
/// key from keystore backups within the erasure deadline.
pub struct ContentKeyStore {
    keys: HashMap<ContentKeyId, ContentKeyEntry>,
    scheme: AeadScheme,
    tenant_dek: TenantDek,
}

impl ContentKeyStore {
    /// Opens an empty keystore that unwraps content keys with `tenant_dek` and seals with the default AEAD.
    pub fn new(tenant_dek: TenantDek) -> Self {
        Self::with_scheme(tenant_dek, AeadScheme::default())
    }

    /// Opens an empty keystore that unwraps content keys with `tenant_dek` and uses `scheme` for every block.
    pub fn with_scheme(tenant_dek: TenantDek, scheme: AeadScheme) -> Self {
        Self {
            keys: HashMap::new(),
            scheme,
            tenant_dek,
        }
    }

    /// Registers `id` with its content key already wrapped under this store's tenant DEK. A key that has already been
    /// destroyed is a terminal tombstone: registering it again is ignored, so a late or replayed registration can never
    /// resurrect erased material.
    pub fn register(&mut self, id: ContentKeyId, wrapped_key: Vec<u8>) {
        if matches!(self.keys.get(&id), Some(ContentKeyEntry::Destroyed)) {
            return;
        }
        self.keys.insert(id, ContentKeyEntry::Active { wrapped_key });
    }

    /// Destroys the content key for `id`, making its ciphertext permanently unrecoverable. This is constant-cost — one
    /// hash-map update, independent of how many keys the tenant holds — so a tenant with millions of keys erases a
    /// subject just as fast as one with a handful. The caller must also purge the key from keystore backups within the
    /// erasure deadline. HEF/HEJ files and their backups are never touched.
    ///
    /// Erasure is terminal even for an id that was never registered: it writes a destroyed tombstone unconditionally, so
    /// a request to erase a subject whose key registration is still in flight (late or replayed) can never be undone by
    /// that registration landing afterwards — [`register`](Self::register) sees the tombstone and ignores it.
    pub fn destroy(&mut self, id: ContentKeyId) {
        self.keys.insert(id, ContentKeyEntry::Destroyed);
    }

    /// Returns true if the key for `id` has been destroyed.
    pub fn is_destroyed(&self, id: ContentKeyId) -> bool {
        matches!(self.keys.get(&id), Some(ContentKeyEntry::Destroyed))
    }

    /// Re-wraps the content key for `id` under new wrapped material (e.g. because entity resolution has assigned a
    /// subject to a previously tenant-associated payload). Only the wrapped key material in the keystore changes; the
    /// ciphertext bytes in HEF/HEJ blocks are never touched.
    ///
    /// Returns `true` if the key was active and re-wrapped, `false` if the key is missing or already destroyed.
    pub fn rekey(&mut self, id: ContentKeyId, new_wrapped_key: Vec<u8>) -> bool {
        match self.keys.get_mut(&id) {
            Some(ContentKeyEntry::Active { wrapped_key }) => {
                *wrapped_key = new_wrapped_key;
                true
            }
            _ => false,
        }
    }

    /// Recovers the plaintext of `sealed_block`, which the caller expects to be block `block_id` of file `file_id` for
    /// subject `id`. Returns `Tombstone` when the key has been destroyed (crypto-shredded) or is unknown, `Rejected`
    /// when the key is live but the block or its wrapped key fails authentication (tamper, or a block replayed into a
    /// different slot or a different file), and `Plaintext` when the key is live and the block authenticates as
    /// `block_id` of `file_id`. An erased subject never surfaces
    /// a decryption error — they surface a tombstone, exactly as an erasure-aware rebuild or HEJ replay would.
    pub fn decrypt_or_tombstone(
        &self,
        id: ContentKeyId,
        block_id: u64,
        file_id: u128,
        sealed_block: &[u8],
    ) -> DecryptOutcome {
        match self.keys.get(&id) {
            Some(ContentKeyEntry::Active { wrapped_key }) => {
                match self.tenant_dek.unwrap_content_key(wrapped_key, id, self.scheme) {
                    Some(content_key) => {
                        let sealed_key = SealedContentKey::new(content_key, 0, self.scheme);
                        match sealed_key.decrypt(block_id, file_id, sealed_block) {
                            Some(plaintext) => DecryptOutcome::Plaintext(plaintext),
                            None => DecryptOutcome::Rejected,
                        }
                    }
                    None => DecryptOutcome::Rejected,
                }
            }
            Some(ContentKeyEntry::Destroyed) | None => DecryptOutcome::Tombstone,
        }
    }
}

/// The key scope a background job declares it needs. The scheduler uses this to limit job placement to nodes that hold
/// a valid lease for the required scope, so no encrypted output is ever produced without a valid key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyScope {
    /// The job reads no encrypted blocks; any node is eligible.
    None,
    /// The job reads per-subject content-key-encrypted blocks; the node must hold both the tenant DEK lease and
    /// subject-key leases, and must satisfy residency/region constraints.
    SubjectKeys,
    /// The job reads file-DEK-encrypted analytical columns; the node must hold the tenant DEK lease.
    TenantDek,
}

/// The data-residency region a key lease is confined to — the geographic/regulatory region where the key material may
/// be used. A job that reads encrypted blocks may only run on a node whose lease is for the region the job requires, so
/// key material never leaves its required residency. Region strings are opaque tenant/deployment labels (the query
/// layer's `ScanEligibility` uses the same string form).
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct KeyResidencyRegion(pub String);

/// A valid key lease held by a processing node for one tenant, confined to one residency region.
///
/// A node acquires a lease when the keystore grants it access to a key scope in a region. A revoked lease (including one
/// revoked by erasure) makes the node ineligible for any job requiring that scope, and a lease for one region never
/// satisfies a job that must run in another.
#[derive(Debug, Clone)]
pub struct KeyLease {
    pub residency: KeyResidencyRegion,
    pub scope: KeyScope,
    pub tenant_id: TenantId,
}

/// Returns `true` if a node holding `node_leases` is eligible to run a job that requires `required_scope` for
/// `tenant_id` in `required_region`.
///
/// A node without the required lease must be skipped so no job output derived from encrypted blocks is produced without
/// a valid key. A required lease must also be for `required_region`: a lease granted for another region does not let a
/// node process a tenant's keys outside their residency. A `SubjectKeys` job requires both the subject-key lease and the
/// tenant-DEK lease in that region: content keys are unwrapped via the tenant DEK, so a node whose DEK lease was revoked
/// (e.g. by erasure) or is for the wrong region must not be handed a decrypting job on the strength of a stale
/// subject-key lease alone. A `None`-scope job reads no encrypted blocks, so it carries no residency constraint and is
/// eligible on any node.
pub fn is_eligible_for_job(
    tenant_id: TenantId,
    required_scope: KeyScope,
    required_region: &KeyResidencyRegion,
    node_leases: &[KeyLease],
) -> bool {
    let holds_in_region = |scope: KeyScope| {
        node_leases
            .iter()
            .any(|lease| lease.tenant_id == tenant_id && lease.scope == scope && lease.residency == *required_region)
    };
    match required_scope {
        KeyScope::None => true,
        KeyScope::SubjectKeys => holds_in_region(KeyScope::SubjectKeys) && holds_in_region(KeyScope::TenantDek),
        KeyScope::TenantDek => holds_in_region(KeyScope::TenantDek),
    }
}

#[cfg(test)]
#[path = "test/mod.rs"]
mod tests;
