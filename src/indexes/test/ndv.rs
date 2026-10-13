use super::*;

#[test]
fn small_counts_are_exact_across_types() {
    let mut sketch = NdvSketch::new();
    sketch.observe_column(&ColumnData::U64(vec![1, 2, 2, 3, 3, 3]));
    assert_eq!(sketch.distinct(), (3, true));
    sketch.observe_column(&ColumnData::U64(vec![3, 4]));
    assert_eq!(sketch.distinct(), (4, true));

    let mut strings = NdvSketch::new();
    strings.observe_column(&ColumnData::Strings(
        vec![Some("a".to_owned()), Some("b".to_owned()), None, Some("a".to_owned())].into(),
    ));
    assert_eq!(strings.distinct(), (2, true), "nulls are not values");
}

#[test]
fn large_counts_estimate_within_the_register_error() {
    let mut sketch = NdvSketch::new();
    let values: Vec<u64> = (0..50_000u64).collect();
    sketch.observe_column(&ColumnData::U64(values));
    let (estimate, exact) = sketch.distinct();
    assert!(!exact);
    let error = (estimate as f64 - 50_000.0).abs() / 50_000.0;
    assert!(error < 0.15, "estimate {estimate} strays {error:.3} from 50000");
}

#[test]
fn merging_stripes_matches_observing_them_together() {
    let mut left = NdvSketch::new();
    left.observe_column(&ColumnData::I64((0..40_000).collect()));
    let mut right = NdvSketch::new();
    right.observe_column(&ColumnData::I64((20_000..60_000).collect()));
    let mut together = NdvSketch::new();
    together.observe_column(&ColumnData::I64((0..60_000).collect()));

    left.merge(&right);
    assert_eq!(left.distinct(), together.distinct(), "merge equals one pass");

    // Exact-mode merging stays exact within the cap.
    let mut a = NdvSketch::new();
    a.observe_column(&ColumnData::U64(vec![1, 2, 3]));
    let mut b = NdvSketch::new();
    b.observe_column(&ColumnData::U64(vec![3, 4]));
    a.merge(&b);
    assert_eq!(a.distinct(), (4, true));
}

#[test]
fn sketches_are_deterministic() {
    let mut a = NdvSketch::new();
    let mut b = NdvSketch::new();
    for sketch in [&mut a, &mut b] {
        sketch.observe_column(&ColumnData::Strings(
            (0..5_000).map(|i| Some(format!("value-{i}"))).collect(),
        ));
    }
    assert_eq!(a.distinct(), b.distinct());
    assert_eq!(a, b);
}

fn sketch_of(values: std::ops::Range<u64>) -> NdvSketch {
    let mut sketch = NdvSketch::new();
    sketch.observe_column(&ColumnData::U64(values.collect()));
    sketch
}

#[test]
fn merge_stays_exact_up_to_the_cap_and_degrades_past_it() {
    for (union, exact) in [(1023u64, true), (1024, true), (1025, false), (2048, false)] {
        let mut left = sketch_of(0..union / 2);
        left.merge(&sketch_of(union / 2..union));
        assert_eq!(left.distinct().1, exact, "union of {union}");
        assert_eq!(left, sketch_of(0..union), "union of {union}");
    }
}

#[test]
fn overlapping_merge_that_fits_stays_exact() {
    let mut left = sketch_of(0..800);
    left.merge(&sketch_of(200..1000));
    assert_eq!(left.distinct(), (1000, true));
    assert_eq!(left, sketch_of(0..1000));
}

#[test]
fn degraded_merge_keeps_register_parity() {
    let mut left = sketch_of(0..1000);
    left.merge(&sketch_of(500..1600));
    assert!(!left.distinct().1);
    assert_eq!(left.registers, sketch_of(0..1600).registers);
}
