use super::*;
use crate::clock::FixedClock;

const START_NANOS: i64 = 1_700_000_000_000_000_000;

#[test]
fn a_graph_file_seals_once_it_holds_the_event_count() {
    let clock = FixedClock::at(START_NANOS);
    let policy = GraphSealPolicy::default();
    assert_eq!(policy.max_events, 4096);
    let mut open = OpenGraphFile {
        event_count: 4095,
        first_event_at_nanos: START_NANOS,
    };
    assert_eq!(should_seal_graph(&open, &policy, &clock), None);
    open.event_count = 4096;
    assert_eq!(
        should_seal_graph(&open, &policy, &clock),
        Some(GraphSealTrigger::EventCount)
    );
}

#[test]
fn a_quiet_graph_file_seals_once_its_first_event_is_old_enough() {
    let clock = FixedClock::at(START_NANOS);
    let policy = GraphSealPolicy {
        max_age_nanos: 1_000,
        max_events: 4096,
    };
    let open = OpenGraphFile {
        event_count: 3,
        first_event_at_nanos: START_NANOS,
    };
    clock.advance(999);
    assert_eq!(should_seal_graph(&open, &policy, &clock), None);
    clock.advance(1);
    assert_eq!(should_seal_graph(&open, &policy, &clock), Some(GraphSealTrigger::Age));

    let empty = OpenGraphFile {
        event_count: 0,
        first_event_at_nanos: START_NANOS,
    };
    assert_eq!(
        should_seal_graph(&empty, &policy, &clock),
        None,
        "an empty file has nothing to seal"
    );
}
