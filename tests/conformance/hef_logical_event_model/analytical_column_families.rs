//! Checks that columns are grouped into families, and that families marked internal-only are never returned to outside
//! callers. When a public caller asks for an internal family (such as embeddings), it is silently dropped from the
//! result rather than served.
use hef::events::families::{Caller, authorize_columns};

/// conformance: hef-logical-event-model/analytical-column-families/internal-embedding-column-requested-publicly
#[test]
fn internal_embedding_column_requested_publicly() {
    // A public caller requesting an internal-only family gets it dropped, never returned.
    let (allowed, dropped) = authorize_columns(&["event_type_id", "embedding_v1", "internal_score"], Caller::Public);
    assert_eq!(allowed, vec!["event_type_id"]);
    assert_eq!(dropped, vec!["embedding_v1", "internal_score"]);
}
