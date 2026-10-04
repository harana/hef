//! Fixed values the security module stamps into stored bytes.
//!
//! See: hef-security-and-isolation/spec.md

/// The leading bytes of a payload sealed under a data subject's content key. A row's whole payload is stored as a
/// binary value that starts with these 16 bytes, then the subject id (16 bytes, little-endian), then the sealed blob;
/// a reader that sees them asks the key store for that subject's key before handing the payload out.
pub(crate) const SEALED_PAYLOAD_MAGIC: &[u8; 16] = b"harana/hef/seal1";
