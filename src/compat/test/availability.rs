use super::*;

#[test]
fn live_families_validate_across_versions() {
    for window in PINNED_AVAILABILITY_WINDOWS {
        if window.retired.is_none() {
            assert!(validate_pipeline_window_pinned(window.transform_id, (1, 0)).is_ok());
            assert!(validate_pipeline_window_pinned(window.transform_id, (1, 7)).is_ok());
        }
    }
}

#[test]
fn the_retired_vortex_transform_is_rejected_at_every_version() {
    assert!(validate_pipeline_window_pinned(11, (1, 0)).is_err());
    assert!(validate_pipeline_window_pinned(11, (2, 3)).is_err());
}

#[test]
fn a_pipeline_from_the_future_is_rejected_for_an_older_file() {
    // A synthetic table with a family introduced at 1.2: a 1.0 or 1.1 file cannot legally carry it, a 1.2 file can.
    let windows = [AvailabilityWindow {
        introduced: (1, 2),
        retired: None,
        transform_id: 42,
    }];
    assert!(validate_pipeline_window(42, (1, 0), &windows).is_err());
    assert!(validate_pipeline_window(42, (1, 1), &windows).is_err());
    assert!(validate_pipeline_window(42, (1, 2), &windows).is_ok());
    assert!(validate_pipeline_window(42, (2, 0), &windows).is_ok());
}

#[test]
fn retirement_closes_the_writer_never_the_reader() {
    // Retired at 1.3: files declaring 1.0–1.2 keep decoding it, files declaring 1.3+ reject it.
    let windows = [AvailabilityWindow {
        introduced: (1, 0),
        retired: Some((1, 3)),
        transform_id: 42,
    }];
    assert!(validate_pipeline_window(42, (1, 0), &windows).is_ok());
    assert!(validate_pipeline_window(42, (1, 2), &windows).is_ok());
    assert!(validate_pipeline_window(42, (1, 3), &windows).is_err());
    assert!(validate_pipeline_window(42, (2, 0), &windows).is_err());
}

#[test]
fn an_unknown_family_has_no_window_and_refuses() {
    assert!(validate_pipeline_window_pinned(200, (1, 0)).is_err());
}
