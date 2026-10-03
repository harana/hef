//! Checks when the writer closes the current file and starts a new one. A busy tenant hits the size limit and seals the
//! file on bytes; a quiet tenant never reaches that size, so a time limit seals its small file instead. Either way the
//! file gets closed so downstream cleanup can move forward.
use hef::invariants::MonotonicClock;
use hef::invariants::sim::SimClock;
use hef::writer::publish::{OpenFileState, RollPolicy, RollTrigger, should_roll};

/// conformance: hef-write-path/file-roll-boundary-owned-by-publish-policy/high-volume-tenant-rolls-on-bytes
#[test]
fn high_volume_tenant_rolls_on_bytes() {
    // The compressed byte target seals the file before the time window.
    let clock = SimClock::new(7);
    let policy = RollPolicy::default();
    let state = OpenFileState {
        opened_at_monotonic_nanos: clock.monotonic_nanos(),
        compressed_bytes: policy.byte_target + 1,
    };
    assert_eq!(should_roll(&state, &policy, &clock), Some(RollTrigger::ByteTarget));
}

/// conformance: hef-write-path/file-roll-boundary-owned-by-publish-policy/low-volume-tenant-rolls-on-time
#[test]
fn low_volume_tenant_rolls_on_time() {
    // A trickle tenant never reaches the byte target; the open-time window seals the small compact file so HEJ
    // retention and LiveOverlay eviction can advance.
    let clock = SimClock::new(7);
    let policy = RollPolicy::default();
    let state = OpenFileState {
        opened_at_monotonic_nanos: clock.monotonic_nanos(),
        compressed_bytes: 512,
    };
    assert_eq!(should_roll(&state, &policy, &clock), None);
    clock.advance(policy.max_open_nanos + 1);
    assert_eq!(should_roll(&state, &policy, &clock), Some(RollTrigger::TimeWindow));
}
