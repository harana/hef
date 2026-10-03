//! Checks that internal-only identifiers (tenant id, the internal sequence, storage paths, and the like) never reach
//! outside callers. The rule is enforced deep in the engine, below any API: such columns are allowed for internal use
//! but blocked for public callers by default.
use hef::events::families::{Caller, column_allowed};

/// conformance:
/// hef-logical-event-model/public-output-authorization-boundary/internal-identity-requested-by-public-caller
#[test]
fn internal_identity_requested_by_public_caller() {
    // Raw internal identity columns are blocked from public output at the scan-phase authorization layer, before any
    // row leaves the engine. Internal callers retain full access; the restriction applies only at the public boundary.
    for column in ["tenant_id", "epoch", "sequence", "payload_ref", "object_store_path"] {
        assert!(
            !column_allowed(column, Caller::Public),
            "{column} must be blocked for public callers"
        );
        assert!(
            column_allowed(column, Caller::Internal),
            "{column} must be allowed for internal callers"
        );
    }
    // Public callers also cannot request raw hash columns or storage paths.
    for column in ["entity_id_hash_low", "dedupe_hash_low", "local_cache_path"] {
        assert!(!column_allowed(column, Caller::Public), "{column} must be blocked");
    }
}
