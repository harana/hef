//! Requirement: Bounded string min/max in skip indexes.

use hef::indexes::Exactness;
use hef::indexes::minmax::StringMinMax;

/// conformance:
/// hef-query-metadata-and-indexes/bounded-string-min-max-in-skip-indexes/long-string-zone-map-stays-bounded
#[test]
fn long_string_zone_map_stays_bounded() {
    // Values far longer than the configured truncation length: the stored bounds are shortened to at most that length
    // and `is_truncated` is set, so the entry size stays bounded no matter how long the strings get.
    let max_stored_len = 8;
    for input_len in [16usize, 64, 4096, 65_536] {
        let low = vec![b'a'; input_len];
        let high = vec![b'q'; input_len];
        let values = [Some(low.as_slice()), Some(high.as_slice())];
        let zone = StringMinMax::build(&values, max_stored_len);

        assert!(zone.is_truncated, "len {input_len} must truncate");
        assert!(
            zone.min.len() <= max_stored_len,
            "stored min length {} exceeds budget at input length {input_len}",
            zone.min.len()
        );
        assert!(
            zone.max.as_ref().map(Vec::len).unwrap_or(0) <= max_stored_len,
            "stored max length exceeds budget at input length {input_len}"
        );

        // The serialized entry is bounded too — its size does not grow with the underlying string length.
        assert!(
            zone.encode().len() <= 2 * max_stored_len + 16,
            "encoded entry size must stay bounded"
        );
    }
}

/// conformance:
/// hef-query-metadata-and-indexes/bounded-string-min-max-in-skip-indexes/truncated-bounds-keep-pruning-sound
#[test]
fn truncated_bounds_keep_pruning_sound() {
    // Build a truncated zone map over long strings, then check two things: the stored lower bound is byte-wise <= the
    // true minimum and the stored upper bound is byte-wise >= the true maximum; and any predicate window the zone map
    // prunes truly contains none of the real values.
    let raw: Vec<Vec<u8>> = vec![
        b"alpha-1234567890abcdef".to_vec(),
        b"bravo-the-second-value".to_vec(),
        b"charlie-zzzzzzzzzzzzzz".to_vec(),
        b"delta-0000000000000000".to_vec(),
    ];
    let values: Vec<Option<&[u8]>> = raw.iter().map(|v| Some(v.as_slice())).collect();
    let true_min = raw.iter().min().unwrap();
    let true_max = raw.iter().max().unwrap();

    let zone = StringMinMax::build(&values, 5);
    assert!(zone.is_truncated);

    // Lower bound rounded down, upper bound rounded up.
    assert!(
        zone.min.as_slice() <= true_min.as_slice(),
        "stored lower {:?} must be <= true min {:?}",
        zone.min,
        true_min
    );
    match &zone.max {
        Some(max) => assert!(
            max.as_slice() >= true_max.as_slice(),
            "stored upper {:?} must be >= true max {:?}",
            max,
            true_max
        ),
        None => { /* unbounded above is >= every value */ }
    }

    // Pruning soundness: sweep many short predicate windows; whenever the zone map claims a window can be pruned, no
    // real value may lie inside it.
    let probes: &[&[u8]] = &[
        b"", b"a", b"al", b"b", b"br", b"c", b"ch", b"d", b"de", b"e", b"y", b"z", b"zz",
    ];
    for lo in probes {
        for hi in probes {
            if lo > hi {
                continue;
            }
            if zone.can_prune(lo, hi) {
                for value in &raw {
                    let inside = value.as_slice() >= *lo && value.as_slice() <= *hi;
                    assert!(
                        !inside,
                        "pruned window [{lo:?}, {hi:?}] but value {value:?} falls inside it",
                    );
                }
            }
        }
    }
}

/// conformance:
/// hef-query-metadata-and-indexes/bounded-string-min-max-in-skip-indexes/truncated-entry-forces-a-residual-filter
#[test]
fn truncated_entry_forces_a_residual_filter() {
    // A truncated entry whose predicate range overlaps [min, max] is inexact, so the exactness contract requires
    // keeping a real filter above the scan.
    let long_min = vec![b'm'; 100];
    let long_max = vec![b'p'; 100];
    let values = [Some(long_min.as_slice()), Some(long_max.as_slice())];
    let zone = StringMinMax::build(&values, 4);

    assert!(zone.is_truncated);
    // A predicate that overlaps the stored span survives pruning.
    assert!(!zone.can_prune(b"mmmm", b"pppp"), "overlapping range is kept");

    assert_eq!(zone.exactness(), Exactness::InexactNoFalseNegative);
    assert!(
        zone.exactness().requires_residual_filter(),
        "a truncated entry forces an exact residual filter above the scan"
    );
}

/// conformance: hef-query-metadata-and-indexes/bounded-string-min-max-in-skip-indexes/untruncated-entry-stays-exact
#[test]
fn untruncated_entry_stays_exact() {
    // Every value fits within the budget, so neither bound is shortened: the entry holds the exact min and max and
    // behaves like a plain zone map.
    let values = [Some(b"apple".as_slice()), Some(b"cherry"), Some(b"banana")];
    let zone = StringMinMax::build(&values, 16);

    assert!(!zone.is_truncated);
    assert_eq!(zone.min, b"apple");
    assert_eq!(zone.max, Some(b"cherry".to_vec()));
    assert_eq!(zone.exactness(), Exactness::Exact);
    assert!(!zone.exactness().requires_residual_filter());
}

/// conformance: hef-query-metadata-and-indexes/bounded-string-min-max-in-skip-indexes/same-bounds-on-every-node
#[test]
fn same_bounds_on_every_node() {
    // Two independent builds over the same rows with the same truncation length produce byte-identical `min`, `max`,
    // and `is_truncated` — so nodes building the index separately stay in agreement.
    let raw: Vec<Vec<u8>> = vec![
        b"node-independent-value-one".to_vec(),
        b"node-independent-value-two".to_vec(),
        b"node-independent-value-three".to_vec(),
    ];
    let values: Vec<Option<&[u8]>> = raw.iter().map(|v| Some(v.as_slice())).collect();

    let first = StringMinMax::build(&values, 6);
    let second = StringMinMax::build(&values, 6);

    assert_eq!(first.min, second.min);
    assert_eq!(first.max, second.max);
    assert_eq!(first.is_truncated, second.is_truncated);
    assert_eq!(first, second);
    // The serialized forms are byte-identical too.
    assert_eq!(first.encode(), second.encode());
}
