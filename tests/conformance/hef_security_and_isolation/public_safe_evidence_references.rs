//! Checks that investigation outputs and evidence cards can only reference events through public-safe identifiers —
//! never through raw storage paths, row offsets, or internal sequence numbers that would reveal the engine's physical
//! layout to outside callers.

use hef::events::families::{Caller, column_allowed};

/// conformance: hef-security-and-isolation/public-safe-evidence-references/evidence-ref-hides-storage-identity
#[test]
fn evidence_ref_hides_storage_identity() {
    // Every column that would reveal a file path, row offset, payload pointer, or raw sequence is blocked at the scan
    // boundary before any output leaves the engine. Investigation outputs that reference an event as evidence must
    // therefore use a public-safe ID or opaque cursor, because the underlying storage columns are never reachable by
    // public callers.
    let storage_identity_columns = [
        "object_store_path",
        "local_cache_path",
        "payload_ref",
        "payload_bytes",
        "row_offset",
        "sequence",
        "sequence_key",
        "epoch",
    ];

    for col in storage_identity_columns {
        assert!(
            !column_allowed(col, Caller::Public),
            "{col} must be blocked for public callers — evidence refs may not expose storage identity"
        );
        assert!(
            column_allowed(col, Caller::Internal),
            "{col} must remain readable by internal callers that build the opaque cursor"
        );
    }

    // Entity hashes and actor hashes are also storage-internal join keys that must never appear in public evidence
    // output.
    for hash_col in [
        "entity_id_hash",
        "entity_id_hash_low",
        "entity_id_hash_high",
        "actor_id_hash_low",
        "account_id_hash_low",
        "trace_id_hash_low",
        "dedupe_hash_low",
        "dedupe_hash_high",
    ] {
        assert!(
            !column_allowed(hash_col, Caller::Public),
            "{hash_col} must be blocked for public callers"
        );
    }
}
