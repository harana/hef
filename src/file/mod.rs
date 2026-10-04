//! Reads and writes durable bytes the same way everywhere, so the byte-level work is done once instead of three times.
//!
//! The HEF event files and the HEJ journal both sit on this one foundation. It owns the machinery they share: the
//! 4096-byte-aligned append-only block interface with a durability barrier ([`api::BlockStore`]), BLAKE3 as the
//! authoritative integrity check with a CRC-64/NVME precheck and an outboard verified-streaming tree ([`integrity`]), a
//! bounds-checked little-endian byte codec ([`bytes`]), and an in-memory simulation of the block interface that owns no
//! kernel runtime ([`sim`]). A production block store (io_uring, NVMe) lives with the application that deploys HEF and
//! plugs in through [`api::BlockStore`].
//!
//! It owns only the bytes on media. It does not decide what a query may see, and it defines no new on-disk file shape.
//!
//! See: hef-core-invariants/spec.md

pub mod api;
pub mod bytes;
pub mod constant;
pub mod error;
pub mod integrity;
pub mod model;
pub mod sim;

pub use api::{BlockStore, RangeSource};
pub use error::{CodecError, FileError};
pub use model::{Atomicity, BlockTarget, ByteRange, DurabilityMode, IoCapabilities};
