use crate::indexes::constant_flags::{
    ColumnFlags, MetadataAnswer, answer_equality_from_flags, answer_presence_from_flags,
};

#[test]
fn all_null_rejects_equality() {
    let flags = ColumnFlags::all_null();
    assert_eq!(answer_equality_from_flags(&flags, 42), MetadataAnswer::NoRowsMatch);
}

#[test]
fn all_null_rejects_presence() {
    let flags = ColumnFlags::all_null();
    assert_eq!(answer_presence_from_flags(&flags), MetadataAnswer::NoRowsMatch);
}

#[test]
fn constant_matching_value_returns_all_rows_match() {
    let flags = ColumnFlags::constant(7);
    assert_eq!(answer_equality_from_flags(&flags, 7), MetadataAnswer::AllRowsMatch);
}

#[test]
fn constant_differing_value_returns_no_rows_match() {
    let flags = ColumnFlags::constant(7);
    assert_eq!(answer_equality_from_flags(&flags, 99), MetadataAnswer::NoRowsMatch);
}

#[test]
fn constant_block_must_decode_for_presence() {
    // A constant block could satisfy any non-null predicate; flags can't rule it out.
    let flags = ColumnFlags::constant(0);
    assert_eq!(answer_presence_from_flags(&flags), MetadataAnswer::MustDecode);
}

#[test]
fn mixed_block_must_decode_for_equality() {
    let flags = ColumnFlags::mixed();
    assert_eq!(answer_equality_from_flags(&flags, 1), MetadataAnswer::MustDecode);
}

#[test]
fn mixed_block_must_decode_for_presence() {
    let flags = ColumnFlags::mixed();
    assert_eq!(answer_presence_from_flags(&flags), MetadataAnswer::MustDecode);
}

/// The flags derive from the exact per-block statistics alone: all-stored-null means all-null, equal integer bounds
/// with no null mean constant, anything else stays mixed and decodes.
#[test]
fn flags_derive_from_exact_stats() {
    assert_eq!(ColumnFlags::from_stats(8, 8, None, None), ColumnFlags::all_null());
    assert_eq!(
        ColumnFlags::from_stats(8, 0, Some(42), Some(42)),
        ColumnFlags::constant(42)
    );
    assert_eq!(ColumnFlags::from_stats(8, 0, Some(1), Some(9)), ColumnFlags::mixed());
    assert_eq!(ColumnFlags::from_stats(8, 3, Some(42), Some(42)), ColumnFlags::mixed());
    assert_eq!(ColumnFlags::from_stats(0, 0, None, None), ColumnFlags::mixed());
}

/// The widened value domain carries decimal mantissas and timestamps beyond i64.
#[test]
fn constants_beyond_the_i64_range_answer_equality() {
    let wide = i128::from(i64::MAX) * 1_000;
    let flags = ColumnFlags::constant(wide);
    assert_eq!(answer_equality_from_flags(&flags, wide), MetadataAnswer::AllRowsMatch);
    assert_eq!(
        answer_equality_from_flags(&flags, wide - 1),
        MetadataAnswer::NoRowsMatch
    );
}
