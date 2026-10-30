//! Stores embeddings as typed, fixed-width vector columns in encrypted HEF files, and searches them in place.
//!
//! A vector segment file holds a batch of embeddings (one room and time window, or one user's memory facts) next to
//! their event ids and the room, sender and time columns a search filters by. Each embedding can be stored several
//! ways at once (1-bit, int8, f16, f32), all linked to one logical embedding column, so a search scans the small
//! 1-bit copy and re-scores the best candidates with a finer one. Every block and the footer are sealed under the
//! file's own random data key, wrapped under a key the caller supplies; vectors from different models or metrics are
//! never compared.
//!
//! See: hef-vector-columns/spec.md

mod constant;
mod error;
mod kernel;
mod model;
mod quantize;
mod seal;
mod search;
mod segment;

pub use constant::{
    DEFAULT_OVERSAMPLE, EMBEDDING_COLUMN_ID, EVENT_ID_COLUMN, GRANULE_ROWS, ROOM_ID_COLUMN, SENDER_COLUMN,
    TIMESTAMP_COLUMN,
};
pub use error::*;
pub use kernel::{PreparedQuery, TopK, top_k};
pub use model::*;
pub use quantize::{decode_row, encode_row, u1_estimated_dot};
pub use seal::WrappingKey;
pub use search::*;
pub use segment::*;

#[cfg(test)]
#[path = "test/mod.rs"]
mod tests;
