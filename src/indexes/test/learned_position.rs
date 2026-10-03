use crate::indexes::learned_position::{LearnedPositionIndex, plan_learned_seek};

fn uniform_keys(start: i64, step: i64, count: usize) -> Vec<i64> {
    (0..count as i64).map(|i| start + i * step).collect()
}

#[test]
fn estimate_lands_within_error_bound() {
    // 100 rows with keys 0, 10, 20, ..., 990.
    let keys = uniform_keys(0, 10, 100);
    let index = LearnedPositionIndex::build(1, 0, &keys, 4);
    // Key 300 is at position 30.
    let window = index.estimate_position(300);
    assert!(
        window.search_from <= 30 && 30 <= window.search_to,
        "window [{}, {}] must contain true position 30",
        window.search_from,
        window.search_to,
    );
    // Window must be bounded.
    assert!(
        window.search_to - window.search_from <= 2 * index.error_bound,
        "window width must not exceed 2 * error_bound"
    );
}

#[test]
fn estimate_clamped_to_row_count() {
    let keys = uniform_keys(0, 1, 50);
    let index = LearnedPositionIndex::build(1, 0, &keys, 3);
    // Key beyond the last key is clamped.
    let window = index.estimate_position(9999);
    assert!(window.search_to < 50, "search_to must not exceed row_count - 1");
    // Key below the first is also clamped.
    let window2 = index.estimate_position(-100);
    assert_eq!(window2.search_from, 0);
}

#[test]
fn single_key_column() {
    let keys = vec![42i64];
    let index = LearnedPositionIndex::build(0, 0, &keys, 0);
    let window = index.estimate_position(42);
    assert_eq!(window.approximate_row, 0);
    assert_eq!(window.search_from, 0);
    assert_eq!(window.search_to, 0);
}

#[test]
fn duplicated_key_seek_finds_earliest_row() {
    // 10 rows all carrying the same key, spanning more segments than one error budget can cover.
    let keys = vec![5i64; 10];
    let index = LearnedPositionIndex::build(1, 0, &keys, 2);
    let window = index.estimate_position(5);
    assert!(
        window.search_from == 0,
        "window must start at row 0 to cover the earliest occurrence of the duplicated key, got [{}, {}]",
        window.search_from,
        window.search_to,
    );
}

#[test]
fn duplicate_run_straddling_a_segment_break_brackets_the_true_row() {
    // Keys [0, 0, 0, 1000] with error_bound 1 split the run of 0s across two segments (rows 0..=1 in the first, row 2
    // onward in the second), so both segments carry key_lo 0. Seeking key 0 must resolve to the earliest segment and
    // the window must still contain the true boundary row 0 — the first row with key >= 0.
    let keys = vec![0i64, 0, 0, 1000];
    let index = LearnedPositionIndex::build(1, 0, &keys, 1);
    let window = index.estimate_position(0);
    assert!(
        window.search_from <= 0 && 0 <= window.search_to,
        "window [{}, {}] must contain the true boundary row 0 for the duplicated key 0",
        window.search_from,
        window.search_to,
    );
}

#[test]
fn absent_key_between_fitted_keys_stays_inside_the_window() {
    // The ±error fit guarantee holds at the fitted keys; the insertion point of a key absent from them can sit one
    // row further out. Keys [0, 1, 1_000_000] fit one near-flat segment, so seeking 2 estimates row 0 while the true
    // boundary (first key >= 2) is row 2 — the window must still contain it (issue #4036).
    let keys = vec![0i64, 1, 1_000_000];
    let index = LearnedPositionIndex::build(1, 0, &keys, 1);
    let window = index.estimate_position(2);
    assert!(
        window.search_from <= 2 && 2 <= window.search_to,
        "window [{}, {}] must contain the true boundary row 2 for absent key 2",
        window.search_from,
        window.search_to,
    );
}

#[test]
fn absent_key_in_an_inter_segment_gap_stays_inside_the_window() {
    // A steep first segment (keys 0..=10, slope 1) extrapolated across the gap to the second segment (keys from
    // 1_000_000) lands thousands of rows past the true boundary; the estimate must be capped at the next segment's
    // first row so the window still contains it (issue #4036).
    let keys: Vec<i64> = (0..=10).chain((0..=100).map(|i| 1_000_000 + i)).collect();
    let index = LearnedPositionIndex::build(1, 0, &keys, 1);
    // True boundary for 500_000 is row 11, the second segment's first row.
    let window = index.estimate_position(500_000);
    assert!(
        window.search_from <= 11 && 11 <= window.search_to,
        "window [{}, {}] must contain the true boundary row 11 for a key in the inter-segment gap",
        window.search_from,
        window.search_to,
    );
}

#[test]
fn plan_returns_none_without_sortedness_proof() {
    let keys = uniform_keys(0, 5, 20);
    let index = LearnedPositionIndex::build(7, 0, &keys, 2);
    // No proof: must not use the learned index.
    assert!(
        plan_learned_seek(&index, false, 7).is_none(),
        "learned seek must not be used without a sortedness proof"
    );
}

#[test]
fn plan_returns_none_for_wrong_column() {
    let keys = uniform_keys(0, 5, 20);
    let index = LearnedPositionIndex::build(7, 0, &keys, 2);
    // Proof present but column mismatch.
    assert!(
        plan_learned_seek(&index, true, 99).is_none(),
        "learned seek must not be used for a different column"
    );
}

#[test]
fn plan_returns_seek_when_proof_and_column_match() {
    let keys = uniform_keys(0, 5, 20);
    let index = LearnedPositionIndex::build(7, 0, &keys, 2);
    let seek = plan_learned_seek(&index, true, 7);
    assert!(
        seek.is_some(),
        "learned seek must be available with proof + matching column"
    );
    // The seek correctly estimates the start of a range.
    let window = seek.unwrap().seek_range_start(50); // key 50 is at position 10
    assert!(
        window.search_from <= 10 && 10 <= window.search_to,
        "window must contain the true position of key 50"
    );
}

#[test]
fn keys_spanning_the_whole_signed_domain_fit_and_look_up_without_overflow() {
    // `key - key0` overflowed for sorted inputs at both extremes of i64, and this crate aborts on overflow, so
    // legitimate data could terminate the process while building or querying the index (issue #8925).
    let keys = [i64::MIN, 0, i64::MAX];
    let index = LearnedPositionIndex::build(1, 0, &keys, 2);

    for (position, key) in keys.iter().enumerate() {
        let window = index.estimate_position(*key);
        assert!(
            window.search_from <= position as u64 && position as u64 <= window.search_to,
            "window must contain the true position of {key}"
        );
    }
    // A query at the opposite extreme from the matching segment's key_lo is the other overflow path.
    let _ = index.estimate_position(i64::MAX);
    let _ = index.estimate_position(i64::MIN);
}
