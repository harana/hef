//! Requirement: Constant and all-null flags enable metadata-only answers.

use hef::indexes::constant_flags::{
    ColumnFlags, MetadataAnswer, answer_equality_from_flags, answer_presence_from_flags,
};

/// conformance:
/// hef-query-metadata-and-indexes/constant-and-all-null-flags-enable-metadata-only-answers/
/// equality-on-a-constant-column-answered-from-metadata
#[test]
fn equality_on_a_constant_column_answered_from_metadata() {
    // A page whose `currency` column is flagged constant = 1 (the code for "USD"). The flag is exact: every row in the
    // page holds exactly this value.
    let flags = ColumnFlags::constant(1);

    // Filter `currency = 'USD'` (target = 1): the flag proves all rows match, and the column block is never read.
    let answer = answer_equality_from_flags(&flags, 1);
    assert_eq!(
        answer,
        MetadataAnswer::AllRowsMatch,
        "a constant-flag page whose value matches the filter must answer AllRowsMatch without decode"
    );

    // Filter `currency = 'EUR'` (target = 2): the constant differs from the target, so no row can match — still
    // answered from the flag, still no decode.
    let answer_no = answer_equality_from_flags(&flags, 2);
    assert_eq!(
        answer_no,
        MetadataAnswer::NoRowsMatch,
        "a constant-flag page whose value differs from the filter must answer NoRowsMatch without decode"
    );

    // A projection of `currency` over a matching page is filled from the constant (value = 1) without decoding the
    // block, so MustDecode is never returned for a page flagged constant.
    assert_ne!(
        answer_equality_from_flags(&flags, 1),
        MetadataAnswer::MustDecode,
        "a constant-flag page must never fall through to MustDecode"
    );
}

/// conformance:
/// hef-query-metadata-and-indexes/constant-and-all-null-flags-enable-metadata-only-answers/
/// predicate-on-an-all-null-column-rejects-without-decode
#[test]
fn predicate_on_an_all_null_column_rejects_without_decode() {
    // A page whose `amount` column is flagged all_null. The flag is exact: there is not a single non-NULL byte in the
    // block.
    let flags = ColumnFlags::all_null();

    // A filter requiring a non-NULL value (e.g. `amount > 0`) is answered by the presence-answering function, which
    // returns NoRowsMatch without fetching the `amount` block.
    let answer = answer_presence_from_flags(&flags);
    assert_eq!(
        answer,
        MetadataAnswer::NoRowsMatch,
        "an all-null page must answer NoRowsMatch for any value predicate without decode"
    );

    // Equality is likewise ruled out: no row holds `amount = 42` when every value is NULL.
    let eq_answer = answer_equality_from_flags(&flags, 42);
    assert_eq!(
        eq_answer,
        MetadataAnswer::NoRowsMatch,
        "an all-null page must also answer equality predicates with NoRowsMatch, no decode"
    );

    // The flag is exact, so MustDecode is never returned for an all-null page.
    assert_ne!(
        answer_presence_from_flags(&flags),
        MetadataAnswer::MustDecode,
        "an all-null page must never fall through to MustDecode"
    );
}
