//! Conformance tests for the `hef-manifest-integration` capability — the manifest as the boundary controlling which
//! data a query can see, including atomic publication, snapshot fields, metadata placement, and the stable cross-file
//! dictionary for coded columns.

mod atomic_publication_and_consistent_snapshot_fields;
mod metadata_placement_discipline;
mod object_store_conditional_write_publication;
