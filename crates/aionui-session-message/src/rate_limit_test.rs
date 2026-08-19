use std::sync::Arc;

use super::*;
use crate::queue::TestClock;

fn limiter() -> (RateLimiter, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(0));
    (RateLimiter::new(clock.clone()), clock)
}

#[test]
fn the_pair_gate_trips_at_the_eleventh_message_between_the_same_two() {
    let (limiter, _clock) = limiter();
    for index in 0..PAIR_LIMIT {
        assert!(
            matches!(limiter.check_and_record("a", "b"), RateVerdict::Allowed),
            "message {index} must be allowed"
        );
    }
    match limiter.check_and_record("a", "b") {
        RateVerdict::Tripped { gate, window_count } => {
            assert_eq!(gate, SessionRateGate::Pair);
            assert_eq!(window_count, PAIR_LIMIT);
        }
        RateVerdict::Allowed => panic!("the 11th message between one pair must trip the pair gate"),
    }
}

#[test]
fn tripping_the_pair_gate_still_allows_a_different_target() {
    // Proves it is a pair gate, not a global one (spec §11).
    let (limiter, _clock) = limiter();
    for _ in 0..=PAIR_LIMIT {
        let _ = limiter.check_and_record("a", "b");
    }
    assert!(
        matches!(limiter.check_and_record("a", "c"), RateVerdict::Allowed),
        "a different target must still be reachable"
    );
}

#[test]
fn the_outbound_gate_trips_at_the_twenty_first_message_from_one_sender() {
    let (limiter, _clock) = limiter();
    // Spread across targets so the pair gate (10) never trips first.
    for index in 0..OUTBOUND_LIMIT {
        let target = format!("t{index}");
        assert!(
            matches!(limiter.check_and_record("a", &target), RateVerdict::Allowed),
            "message {index} must be allowed"
        );
    }
    match limiter.check_and_record("a", "t_new") {
        RateVerdict::Tripped { gate, window_count } => {
            assert_eq!(gate, SessionRateGate::Outbound);
            assert_eq!(window_count, OUTBOUND_LIMIT);
        }
        RateVerdict::Allowed => panic!("the 21st outbound message must trip the outbound gate"),
    }
}

#[test]
fn the_window_slides_so_old_entries_stop_counting() {
    let (limiter, clock) = limiter();
    for _ in 0..PAIR_LIMIT {
        let _ = limiter.check_and_record("a", "b");
    }
    assert!(matches!(
        limiter.check_and_record("a", "b"),
        RateVerdict::Tripped { .. }
    ));

    clock.advance(WINDOW_MS + 1);
    assert!(
        matches!(limiter.check_and_record("a", "b"), RateVerdict::Allowed),
        "entries older than the window must stop counting"
    );
}

#[test]
fn a_tripped_check_does_not_consume_a_slot() {
    // Otherwise a caller that keeps retrying would extend its own lockout
    // forever, and the window would never actually drain.
    let (limiter, clock) = limiter();
    for _ in 0..PAIR_LIMIT {
        let _ = limiter.check_and_record("a", "b");
    }
    for _ in 0..5 {
        assert!(matches!(
            limiter.check_and_record("a", "b"),
            RateVerdict::Tripped { .. }
        ));
    }
    clock.advance(WINDOW_MS + 1);
    assert!(matches!(limiter.check_and_record("a", "b"), RateVerdict::Allowed));
}

#[test]
fn a_tripped_pair_check_does_not_consume_the_senders_outbound_slot_either() {
    // The pair gate returns BEFORE recording, so a hammered pair must not
    // silently burn the sender's outbound budget for other targets.
    let (limiter, _clock) = limiter();
    for _ in 0..PAIR_LIMIT {
        let _ = limiter.check_and_record("a", "b");
    }
    for _ in 0..50 {
        assert!(matches!(
            limiter.check_and_record("a", "b"),
            RateVerdict::Tripped { .. }
        ));
    }
    // 10 outbound slots used so far, so 10 remain before the outbound cap.
    for index in 0..(OUTBOUND_LIMIT - PAIR_LIMIT) {
        assert!(
            matches!(
                limiter.check_and_record("a", &format!("t{index}")),
                RateVerdict::Allowed
            ),
            "outbound slot {index} must still be available"
        );
    }
}

#[test]
fn direction_matters_for_the_pair_gate_key() {
    // spec §6.7 calls this the "(from, to) round-trip" gate. A→B and B→A are
    // counted separately; the ping-pong is throttled because each direction
    // has its own budget and both are small.
    let (limiter, _clock) = limiter();
    for _ in 0..PAIR_LIMIT {
        let _ = limiter.check_and_record("a", "b");
    }
    assert!(
        matches!(limiter.check_and_record("b", "a"), RateVerdict::Allowed),
        "the reverse direction has its own budget"
    );
}

#[test]
fn the_pair_gate_is_reported_when_both_gates_would_trip() {
    // Deterministic precedence: the pair gate is the tighter one and the one
    // that actually describes a two-agent loop, so it must win.
    let (limiter, _clock) = limiter();
    // Fill the outbound budget across other targets first…
    for index in 0..(OUTBOUND_LIMIT - PAIR_LIMIT) {
        let _ = limiter.check_and_record("a", &format!("t{index}"));
    }
    // …then fill the pair budget for `b`, which also tops out outbound.
    for _ in 0..PAIR_LIMIT {
        let _ = limiter.check_and_record("a", "b");
    }
    match limiter.check_and_record("a", "b") {
        RateVerdict::Tripped { gate, window_count } => {
            assert_eq!(gate, SessionRateGate::Pair, "the pair gate must be reported first");
            assert_eq!(window_count, PAIR_LIMIT);
        }
        RateVerdict::Allowed => panic!("both gates are full; this must trip"),
    }
}

#[test]
fn separate_senders_have_separate_outbound_budgets() {
    let (limiter, _clock) = limiter();
    for index in 0..OUTBOUND_LIMIT {
        let _ = limiter.check_and_record("a", &format!("t{index}"));
    }
    assert!(
        matches!(limiter.check_and_record("z", "t0"), RateVerdict::Allowed),
        "a different sender must not inherit another's exhausted budget"
    );
}
