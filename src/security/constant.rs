//! Fixed values the security module stamps into stored bytes.
//!
//! See: hef-security-and-isolation/spec.md

/// The leading bytes of a payload sealed under a data subject's content key. A row's whole payload is stored as a
/// binary value that starts with these 16 bytes, then the subject id (16 bytes, little-endian), then the sealed blob;
/// a reader that sees them asks the key store for that subject's key before handing the payload out.
pub(crate) const SEALED_PAYLOAD_MAGIC: &[u8; 16] = b"harana/hef/seal1";

/// BLAKE3 `derive_key` context for the key that turns a state chunk's plaintext into its address. Changing it changes
/// every address, so the format fixes it.
pub(crate) const STATE_CHUNK_ADDRESS_CONTEXT: &str = "harana/hef 2026-10-11 state chunk address key v1";

/// BLAKE3 `derive_key` context for the key that seals a state chunk's plaintext.
pub(crate) const STATE_CHUNK_SEAL_CONTEXT: &str = "harana/hef 2026-10-11 state chunk seal key v1";
