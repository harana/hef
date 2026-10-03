use super::*;

#[test]
fn fixed_clock_now_matches_now_nanos() {
    let nanos = 1_700_000_000_123_456_789_i64;
    let clock = FixedClock::at(nanos);

    assert_eq!(clock.now(), DateTime::from_timestamp_nanos(nanos));
}
