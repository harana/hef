//! Requirement: Learned position index over sorted keys.

use hef::indexes::learned_position::{LearnedPositionIndex, plan_learned_seek};

/// conformance: hef-query-metadata-and-indexes/learned-position-index-over-sorted-keys/range-seek-via-the-learned-model
#[test]
fn range_seek_via_the_learned_model() {
    // A sequence-sorted column with 200 rows, keys 0, 5, 10, ..., 995. The reader holds a learned_position index and a covering sortedness proof.
    let keys: Vec<i64> = (0..200).map(|i| i * 5).collect();
    let column_id = 1u32;
    let index = LearnedPositionIndex::build(column_id, 0, &keys, 4);

    // The sortedness proof is present: the planner issues a learned seek.
    let seek =
        plan_learned_seek(&index, true, column_id).expect("learned seek must be available when proof is present");

    // Range query: sequence BETWEEN 300 AND 400. key 300 is at true position 60 (300 / 5 = 60).
    let key_lo = 300i64;
    let true_position = 60u64;

    let window = seek.seek_range_start(key_lo);

    // The model must land within the error bound of the true position.
    assert!(
        window.search_from <= true_position && true_position <= window.search_to,
        "window [{}, {}] must contain true row {} for key {}",
        window.search_from,
        window.search_to,
        true_position,
        key_lo,
    );

    // The window is bounded — much smaller than the full granule.
    assert!(
        window.search_to - window.search_from <= 2 * index.error_bound,
        "confirmation window must be bounded by 2 × error_bound rows, not a full scan"
    );

    // After confirming within the window, positions are exact: the reader scans only the matching run [300, 400] without touching the rest of the granule.
    let key_hi = 400i64;
    let window_hi = seek.seek_range_start(key_hi);
    let true_end = 80u64; // key 400 / 5 = 80
    assert!(
        window_hi.search_from <= true_end && true_end <= window_hi.search_to,
        "end-of-range window must also contain the true boundary"
    );
}

/// conformance: hef-query-metadata-and-indexes/learned-position-index-over-sorted-keys/no-proof-no-learned-seek
#[test]
fn no_proof_no_learned_seek() {
    // The same index, but NO sortedness proof is available for this granule.
    let keys: Vec<i64> = (0..100).map(|i| i * 10).collect();
    let column_id = 2u32;
    let index = LearnedPositionIndex::build(column_id, 0, &keys, 5);

    // Without a proof, plan_learned_seek returns None regardless of the column.
    let no_proof = plan_learned_seek(&index, false, column_id);
    assert!(
        no_proof.is_none(),
        "learned seek must not be used when no sortedness proof covers the column"
    );

    // The reader falls back to the bucketed range filter or full scan — this is represented by the planner receiving None and choosing its next option. A proof for a different column also does not unlock the seek.
    let wrong_column = plan_learned_seek(&index, true, column_id + 1);
    assert!(
        wrong_column.is_none(),
        "learned seek must not be used when the proof covers a different column"
    );
}
