//! Byte-length constants for the BIP-340/secp256k1 signature scheme signed protocol events are verified against,
//! shared between the live provenance code and its deterministic test signer.

/// The byte length of a public key, a protocol event id, or any other 32-byte value this scheme hashes, hashes over,
/// or keys with: BIP-340's x-only public keys, the SHA-256 digests they sign, and secp256k1 private-key scalars are
/// all 32 bytes.
pub const PROTOCOL_ID_BYTES: usize = 32;

/// The byte length of a BIP-340 Schnorr signature.
pub const SIGNATURE_BYTES: usize = 64;

/// Nanoseconds in one second: the protocol claims a whole-second timestamp, the envelope stores nanoseconds.
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;
