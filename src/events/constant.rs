//! Byte-length constants for the signature schemes signed protocol events are verified against, shared between the
//! live provenance code and its deterministic test signer, plus the bounds on external identifiers and the Matrix
//! room versions the engine re-verifies, and the stack bound on validating an rkyv ingest body.

/// The byte length of a public key, a protocol event id, or any other 32-byte value this scheme hashes, hashes over,
/// or keys with: BIP-340's x-only public keys, the SHA-256 digests they sign, and secp256k1 private-key scalars are
/// all 32 bytes.
pub const PROTOCOL_ID_BYTES: usize = 32;

/// The byte length of a BIP-340 Schnorr signature.
pub const SIGNATURE_BYTES: usize = 64;

/// Nanoseconds in one second: the protocol claims a whole-second timestamp, the envelope stores nanoseconds.
pub const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// The longest external identifier (a Matrix event id, say) an event or a relationship target may carry, in bytes.
pub const EXTERNAL_ID_MAX_BYTES: usize = 255;

/// Byte length of a target in the `subject_sequence` space: a `u64` sequence number, big-endian.
pub const SUBJECT_SEQUENCE_TARGET_LEN: usize = 8;

/// The largest integer magnitude Matrix canonical JSON admits: 2^53 - 1, the range every JSON parser holds exactly.
pub const MATRIX_MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// The Matrix room versions whose redaction and event-id rules this engine re-verifies, oldest first.
pub const MATRIX_ROOM_VERSIONS: [&str; 11] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"];

/// How many out-of-line hops (into a nested array, object B-tree node, or long string) rkyv validation follows down
/// one path of an ingest body before refusing it. Validation recurses once per hop, so this bounds its stack use; it
/// leaves room for a value nested to the variant depth limit with several hops per level.
pub const RKYV_MAX_SUBTREE_DEPTH: usize = 1024;
