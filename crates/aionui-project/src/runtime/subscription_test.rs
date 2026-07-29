use super::{GRACE_TTL_MS, SubscribeOutcome, Subscriber, SubscriptionRegistry, UnsubscribeOutcome};

fn sub(session: &str, pe: &str, rel: &str) -> Subscriber {
    Subscriber {
        session: session.to_owned(),
        pe_id: pe.to_owned(),
        rel: rel.to_owned(),
    }
}

const C: &str = "file:///work/app";

#[test]
fn first_subscriber_mounts_second_is_already_live() {
    let mut reg = SubscriptionRegistry::new();
    assert_eq!(reg.subscribe(sub("s1", "pe1", ""), C, 0), SubscribeOutcome::Mount);
    // A different pe on the same session pointing at the same canonical shares.
    assert_eq!(reg.subscribe(sub("s1", "pe2", ""), C, 0), SubscribeOutcome::AlreadyLive);
    assert_eq!(reg.subscribers_of(C).unwrap().len(), 2);
}

#[test]
fn multi_session_shares_one_canonical() {
    let mut reg = SubscriptionRegistry::new();
    reg.subscribe(sub("s1", "pe1", ""), C, 0);
    assert_eq!(reg.subscribe(sub("s2", "pe1", ""), C, 0), SubscribeOutcome::AlreadyLive);
    assert_eq!(reg.subscribers_of(C).unwrap().len(), 2);

    assert_eq!(
        reg.unsubscribe(&sub("s1", "pe1", ""), C, 0),
        UnsubscribeOutcome::StillLive
    );
    assert_eq!(
        reg.unsubscribe(&sub("s2", "pe1", ""), C, 0),
        UnsubscribeOutcome::EnteredGrace
    );
}

#[test]
fn empty_enters_grace_and_resubscribe_rescues() {
    let mut reg = SubscriptionRegistry::new();
    reg.subscribe(sub("s1", "pe1", ""), C, 0);
    assert_eq!(
        reg.unsubscribe(&sub("s1", "pe1", ""), C, 100),
        UnsubscribeOutcome::EnteredGrace
    );
    assert_eq!(reg.watched_count(), 1); // still watched (warm)

    // Re-subscribe within TTL → rescued, no fresh mount.
    assert_eq!(
        reg.subscribe(sub("s1", "pe1", ""), C, 200),
        SubscribeOutcome::RescuedFromGrace
    );
    assert_eq!(reg.subscribers_of(C).unwrap().len(), 1);
}

#[test]
fn unsubscribe_unknown_canonical_is_not_subscribed() {
    let mut reg = SubscriptionRegistry::new();
    // Unsubscribing a canonical no one ever subscribed to is a well-defined no-op.
    assert_eq!(
        reg.unsubscribe(&sub("s1", "pe1", ""), "file:///never", 0),
        UnsubscribeOutcome::NotSubscribed
    );
    assert_eq!(reg.watched_count(), 0);
}

#[test]
fn reap_evicts_only_after_ttl() {
    let mut reg = SubscriptionRegistry::new();
    reg.subscribe(sub("s1", "pe1", ""), C, 0);
    reg.unsubscribe(&sub("s1", "pe1", ""), C, 1000);

    assert!(reg.reap(1000 + GRACE_TTL_MS - 1).is_empty());
    assert_eq!(reg.reap(1000 + GRACE_TTL_MS), vec![C.to_owned()]);
    assert_eq!(reg.watched_count(), 0);
}

#[test]
fn rescue_cancels_pending_eviction() {
    let mut reg = SubscriptionRegistry::new();
    reg.subscribe(sub("s1", "pe1", ""), C, 0);
    reg.unsubscribe(&sub("s1", "pe1", ""), C, 0);
    reg.subscribe(sub("s1", "pe1", ""), C, 100); // rescued

    // Well past the original TTL: nothing to evict, it's live again.
    assert!(reg.reap(GRACE_TTL_MS * 10).is_empty());
    assert_eq!(reg.subscribers_of(C).unwrap().len(), 1);
}

#[test]
fn drop_session_removes_all_its_subscriptions() {
    let mut reg = SubscriptionRegistry::new();
    let c2 = "file:///work/lib";
    reg.subscribe(sub("s1", "pe1", ""), C, 0);
    reg.subscribe(sub("s1", "pe2", ""), c2, 0);
    reg.subscribe(sub("s2", "pe1", ""), C, 0); // s2 also on C

    let graced = reg.drop_session("s1", 500);

    // C still live (s2 remains); c2 emptied → grace.
    assert_eq!(reg.subscribers_of(C).unwrap().len(), 1);
    assert_eq!(graced, vec![c2.to_owned()]);
}

#[test]
fn warm_lru_evicts_oldest_over_budget() {
    let mut reg = SubscriptionRegistry::new();
    // Two canonicals put into grace at different times.
    reg.subscribe(sub("s1", "pe1", ""), "file:///a", 0);
    reg.unsubscribe(&sub("s1", "pe1", ""), "file:///a", 10); // grace evict_at = 10+TTL
    reg.subscribe(sub("s1", "pe2", ""), "file:///b", 0);
    reg.unsubscribe(&sub("s1", "pe2", ""), "file:///b", 20); // grace evict_at = 20+TTL
    assert_eq!(reg.watched_count(), 2);

    // Budget 1 → evict the oldest-entered (a, smallest evict_at).
    let evicted = reg.enforce_watch_budget(1);
    assert_eq!(evicted, vec!["file:///a".to_owned()]);
    assert_eq!(reg.watched_count(), 1);
}
