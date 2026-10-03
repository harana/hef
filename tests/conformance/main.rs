//! The HEF conformance harness: one test binary (`conformance`, rooted at this `tests/conformance/main.rs`) over a `mod`
//! tree mirroring `tests/conformance/<capability>/<requirement_slug>.rs` (Cargo only auto-discovers top-level files and
//! `*/main.rs` under `tests/`, so the subdirectory files are reached via modules from this root). One `#[test]` per
//! spec scenario, named by the scenario slug and annotated `/// conformance: <scenario-id>`; `conformance/ledger.toml`
//! tracks status.
//!
//! Stub convention: a scenario whose THEN depends on a subsystem outside this crate is a named stub; stubs may assert
//! the in-scope fragment but are recorded as `stub` in the ledger.
// Panic-shortcut lints are exempt in the conformance suite: a panicking test is the mechanism working.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

mod support;

mod hef_apis;
mod hef_benchmarks_and_acceptance_gates;
mod hef_column_design;
mod hef_core_invariants;
mod hef_deletes_and_corrections;
mod hef_encodings_and_compression;
mod hef_file_layout;
mod hef_file_lifecycle;
mod hef_hardware_deployment;
mod hef_layout_and_clustering;
mod hef_logical_event_model;
mod hef_manifest_integration;
mod hef_physical_artifacts;
mod hef_query_metadata_and_indexes;
mod hef_reader_compatibility;
mod hef_security_and_isolation;
mod hef_write_path;
