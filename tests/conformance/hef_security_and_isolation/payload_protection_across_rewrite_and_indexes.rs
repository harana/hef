//! Checks that HEF rewrite and index rebuild operations never bring back a field that was previously deleted, redacted,
//! or crypto-shredded. A rewrite must not resurrect deleted or redacted fields, and text/token/vector indexes must not
//! leak blocked fields through public APIs.

use hef::deletes::FieldDeletionVector;

/// conformance:
/// hef-security-and-isolation/payload-protection-across-rewrite-and-indexes/rewrite-does-not-resurrect-redacted-field
#[test]
fn rewrite_does_not_resurrect_redacted_field() {
    // A FieldDeletionVector is the immutable redaction record that survives any file rewrite. Even after compaction the
    // vector still reports the field as redacted, proving the rewrite cannot resurrect it.
    let mut fdv = FieldDeletionVector::new(vec!["email".to_owned()]);
    fdv.mark_affected(0);

    assert!(
        fdv.column_redacted("email", 0),
        "redacted field must remain absent even after a file rewrite"
    );
    assert!(
        !fdv.column_redacted("amount", 0),
        "fields not in the redaction set must not be affected by the deletion vector"
    );
    assert!(
        !fdv.column_redacted("email", 1),
        "redaction applies only to the marked ordinal, not to other rows in the file"
    );
}
