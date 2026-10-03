#![deny(unsafe_code)]
#![cfg_attr(
    test,
    allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)
)]

//! Stores a stream of events on disk so they can be written once and queried fast, and never lost or corrupted.
//!
//! This is the storage engine for Harana. Events first land in an append-only journal (HEJ, the durable record), then
//! get packed into immutable, self-describing column files (HEF) for analytical queries. Everything is checksummed end
//! to end so corruption is always caught, never silently served.
//!
//! Module layout follows `conformance/capability-map.toml`: one module per `hef-*` capability. The storage core
//! implements `events`, `invariants`, `columns`, `encoding`, `layout`, `artifacts`, `writer`, `lifecycle`, and
//! `compat`; `indexes` adds the query metadata and skip-index structures; `introspection` exposes public-safe system
//! tables; `file` is the shared byte codec, integrity checks, and block interface everything above sits on.
//!
//! Reading these files back as a query (planning, columnar Arrow execution, and the SQL frontend) is **not** here: that
//! is the query engine of the application that embeds HEF. Durable homes for keys, objects, and jobs (a relational
//! keystore, an object store, a work queue) also belong to that application and plug in through the interfaces here.
//!
//! See: hef-apis/spec.md

pub mod artifacts;
pub mod benchmarks;
pub mod clock;
pub mod columns;
pub mod compat;
pub mod deletes;
// `hef::encoding` is the one sanctioned place for `unsafe` in this crate:
// FSST string compression reuses one scratch buffer per block through fsst-rs's capacity-contract `compress_into`,
// which the pinned 0.5.11 release only exposes as an `unsafe fn`. The allow opens here only; every unsafe block inside
// carries a safety comment naming the capacity guarantee. See implementation-toolchain — "Unsafe code is confined to
// dedicated crates and documented" and hef-encodings-and-compression — "FSST write path reuses compression buffers".
#[allow(unsafe_code)]
pub mod encoding;
pub mod error;
pub mod events;
pub mod file;
#[cfg(test)]
mod fuzz_smoke;
pub mod indexes;
pub mod integrity;
pub mod introspection;
pub mod invariants;
pub mod layout;
pub mod lifecycle;
pub mod security;
pub mod typed_id;

#[cfg(feature = "write")]
pub mod writer;
