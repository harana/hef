use super::*;

use crate::columns::REQUIRED_COLUMNS;

#[test]
fn payload_flags_is_blocked_for_public_callers() {
    assert!(!column_allowed("payload_flags", Caller::Public));
    assert!(column_allowed("payload_flags", Caller::Internal));

    let (allowed, dropped) = authorize_columns(&["occurred_at", "payload_flags"], Caller::Public);
    assert_eq!(allowed, vec!["occurred_at"]);
    assert_eq!(dropped, vec!["payload_flags"]);
}

#[test]
fn public_safe_column_is_allowed_and_unknown_column_is_blocked_by_default() {
    // A genuinely public required column passes; a column absent from the schema is blocked by default rather than
    // leaking, so a newly added internal column stays withheld until it is deliberately declared public.
    assert!(column_allowed("occurred_at", Caller::Public));
    assert!(!column_allowed("embedding_vector", Caller::Public));
    assert!(!column_allowed("some_future_internal_column", Caller::Public));
    assert!(column_allowed("some_future_internal_column", Caller::Internal));
}

#[test]
fn every_internal_only_required_column_is_blocked_for_public_callers() {
    for spec in REQUIRED_COLUMNS {
        if spec.internal_only {
            assert!(
                !column_allowed(spec.name, Caller::Public),
                "internal-only column `{}` must be blocked for public callers",
                spec.name
            );
        }
    }
}
