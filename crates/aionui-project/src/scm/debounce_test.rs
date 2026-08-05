//! Debounce tests: coalescing and the quiet-window rule.
//!
//! Time is passed in explicitly rather than slept, so these are deterministic and
//! do not depend on a clock.

use super::*;

#[test]
fn nothing_is_ready_before_the_window_elapses() {
    let mut debouncer = DirtyDebouncer::default();
    debouncer.mark("scm:pe1".to_owned(), 0);

    assert!(
        debouncer.take_ready(DEBOUNCE_MS - 1).is_empty(),
        "still within the window"
    );
    assert!(!debouncer.is_idle(), "and it is still pending");
}

#[test]
fn a_repository_is_released_once_quiet() {
    let mut debouncer = DirtyDebouncer::default();
    debouncer.mark("scm:pe1".to_owned(), 0);

    assert_eq!(debouncer.take_ready(DEBOUNCE_MS), vec!["scm:pe1".to_owned()]);
    assert!(debouncer.is_idle(), "taking it clears the pending set");
}

#[test]
fn repeated_signals_coalesce_into_one_recompute() {
    let mut debouncer = DirtyDebouncer::default();
    for tick in 0..5 {
        debouncer.mark("scm:pe1".to_owned(), tick);
    }

    // One burst of five signals must not become five recomputes.
    assert_eq!(debouncer.take_ready(4 + DEBOUNCE_MS).len(), 1);
}

#[test]
fn a_continuing_burst_re_arms_the_window() {
    let mut debouncer = DirtyDebouncer::default();
    debouncer.mark("scm:pe1".to_owned(), 0);
    // A later signal while still churning pushes the deadline out, so the
    // recompute happens after things settle rather than mid-burst.
    debouncer.mark("scm:pe1".to_owned(), DEBOUNCE_MS - 10);

    assert!(
        debouncer.take_ready(DEBOUNCE_MS).is_empty(),
        "the window restarted from the newer signal"
    );
    assert_eq!(debouncer.take_ready(DEBOUNCE_MS * 2), vec!["scm:pe1".to_owned()]);
}

#[test]
fn repositories_are_tracked_independently() {
    let mut debouncer = DirtyDebouncer::default();
    debouncer.mark("scm:pe1".to_owned(), 0);
    debouncer.mark("scm:pe2".to_owned(), DEBOUNCE_MS);

    // Only the settled one is released; the other keeps its own deadline.
    assert_eq!(debouncer.take_ready(DEBOUNCE_MS), vec!["scm:pe1".to_owned()]);
    assert_eq!(debouncer.take_ready(DEBOUNCE_MS * 2), vec!["scm:pe2".to_owned()]);
}

#[test]
fn ready_repositories_come_back_in_a_stable_order() {
    let mut debouncer = DirtyDebouncer::default();
    debouncer.mark("scm:pe9".to_owned(), 0);
    debouncer.mark("scm:pe1".to_owned(), 0);
    debouncer.mark("scm:pe5".to_owned(), 0);

    // Order must not depend on signal arrival order.
    assert_eq!(
        debouncer.take_ready(DEBOUNCE_MS),
        vec!["scm:pe1".to_owned(), "scm:pe5".to_owned(), "scm:pe9".to_owned()]
    );
}

#[test]
fn an_idle_debouncer_has_nothing_to_release() {
    let mut debouncer = DirtyDebouncer::default();
    assert!(debouncer.is_idle());
    assert!(debouncer.take_ready(u64::MAX).is_empty());
}
