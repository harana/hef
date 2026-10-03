//! Requirement: Deletes via immutable deletion vectors.

use hef::deletes::{DeletionAggregateDelta, DeletionVector, FieldDeletionVector};

/// conformance: hef-deletes-and-corrections/deletes-via-immutable-deletion-vectors/deleted-rows-excluded
#[test]
fn deleted_rows_excluded() {
    // A query applies all visible deletion vectors before returning rows: deleted ordinals are excluded and non-deleted
    // ordinals pass through.
    let mut dv = DeletionVector::new();
    dv.mark_deleted(1);
    dv.mark_deleted(3);

    assert!(dv.is_deleted(1));
    assert!(dv.is_deleted(3));
    assert!(!dv.is_deleted(0));
    assert!(!dv.is_deleted(2));
    assert!(!dv.is_deleted(4));

    let result = dv.apply(&[0, 1, 2, 3, 4]);
    assert_eq!(result, vec![0, 2, 4]);
    assert_eq!(dv.deleted_count(), 2);
}

/// conformance:
/// hef-deletes-and-corrections/deletes-via-immutable-deletion-vectors/field-level-redaction-keeps-co-mentioned-subjects
#[test]
fn field_level_redaction_keeps_co_mentioned_subjects() {
    // A FieldDeletionVector redacts only the listed PII columns at affected ordinals; co-mentioned subjects' non-PII
    // fields and all other ordinals are completely untouched — the event row is not deleted.
    let mut fdv = FieldDeletionVector::new(vec!["email".to_owned(), "phone".to_owned()]);
    fdv.mark_affected(0);
    fdv.mark_affected(2);

    // Listed columns are redacted only at affected ordinals.
    assert!(fdv.column_redacted("email", 0));
    assert!(fdv.column_redacted("phone", 0));
    assert!(fdv.column_redacted("email", 2));

    // Ordinal 1 is not affected: PII columns are not redacted there.
    assert!(!fdv.column_redacted("email", 1));
    assert!(!fdv.column_redacted("phone", 1));

    // Non-PII columns are never redacted, even at affected ordinals.
    assert!(!fdv.column_redacted("amount", 0));
    assert!(!fdv.column_redacted("event_type", 2));
}

/// conformance: hef-deletes-and-corrections/deletes-via-immutable-deletion-vectors/aggregate-over-deleted-rows
#[test]
fn aggregate_over_deleted_rows() {
    // An exact aggregate shortcut subtracts the co-published DeletionAggregateDelta for invertible aggregates (SUM,
    // COUNT) rather than rescanning the file.
    let delta = DeletionAggregateDelta {
        deleted_count: 3,
        deleted_sum: Some(450),
    };
    assert_eq!(delta.subtract_count(1_000), 997);
    assert_eq!(delta.subtract_sum(10_000), Some(9_550));

    // When no sum was precomputed the caller must fall back to a scan.
    let no_sum = DeletionAggregateDelta {
        deleted_count: 2,
        deleted_sum: None,
    };
    assert_eq!(no_sum.subtract_count(10), 8);
    assert_eq!(no_sum.subtract_sum(5_000), None);
}
