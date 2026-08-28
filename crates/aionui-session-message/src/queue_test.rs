use std::sync::Arc;

use super::*;

fn queue() -> (DeliveryQueue, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000));
    (DeliveryQueue::new(clock.clone()), clock)
}

fn delivery(to: &str, message: &str, expires_at_ms: i64) -> PendingDelivery {
    PendingDelivery {
        to: to.to_owned(),
        user_id: "user_1".to_owned(),
        from_conversation_id: "conv_from".to_owned(),
        message: message.to_owned(),
        expires_at_ms,
    }
}

#[test]
fn a_fresh_queue_is_empty() {
    let (q, _clock) = queue();
    assert!(q.is_empty());
    assert_eq!(q.total_len(), 0);
}

#[test]
fn snapshot_heads_returns_exactly_one_head_per_target() {
    let (q, _clock) = queue();
    q.push(delivery("b", "1", 99_999)).unwrap();
    q.push(delivery("b", "2", 99_999)).unwrap();
    q.push(delivery("c", "3", 99_999)).unwrap();

    let heads = q.snapshot_heads();
    assert_eq!(heads.len(), 2, "one head per target, not one per message");
    let mut messages: Vec<&str> = heads.iter().map(|h| h.message.as_str()).collect();
    messages.sort_unstable();
    assert_eq!(messages, vec!["1", "3"]);
}

#[test]
fn pop_head_advances_fifo_order() {
    let (q, _clock) = queue();
    q.push(delivery("b", "1", 99_999)).unwrap();
    q.push(delivery("b", "2", 99_999)).unwrap();

    assert_eq!(q.snapshot_heads()[0].message, "1");
    q.pop_head("b");
    assert_eq!(q.snapshot_heads()[0].message, "2");
    q.pop_head("b");
    assert!(q.is_empty());
}

#[test]
fn per_target_limit_is_twenty_and_the_twenty_first_is_rejected() {
    let (q, _clock) = queue();
    for index in 0..PER_TARGET_LIMIT {
        q.push(delivery("b", &index.to_string(), 99_999)).expect("within limit");
    }
    let error = q.push(delivery("b", "overflow", 99_999)).expect_err("must reject");
    assert!(matches!(error, SessionMessageError::QueueFull), "{error}");
    assert_eq!(q.len_for("b"), PER_TARGET_LIMIT);
}

#[test]
fn global_limit_is_two_hundred_across_targets() {
    let (q, _clock) = queue();
    let mut pushed = 0;
    for target in 0..20 {
        for _ in 0..PER_TARGET_LIMIT {
            if q.push(delivery(&format!("t{target}"), "m", 99_999)).is_ok() {
                pushed += 1;
            }
        }
    }
    assert_eq!(pushed, GLOBAL_LIMIT);
    let error = q.push(delivery("t_new", "m", 99_999)).expect_err("global cap");
    assert!(matches!(error, SessionMessageError::QueueFull), "{error}");
}

#[test]
fn drop_expired_removes_only_items_past_their_deadline() {
    let (q, clock) = queue();
    q.push(delivery("b", "soon", 2_000)).unwrap();
    q.push(delivery("b", "later", 50_000)).unwrap();

    assert_eq!(q.drop_expired(), 0, "nothing is expired at t=1000");
    clock.advance(1_500); // t = 2_500
    assert_eq!(q.drop_expired(), 1);
    assert_eq!(q.len_for("b"), 1);
    assert_eq!(q.snapshot_heads()[0].message, "later");
}

#[test]
fn dropping_every_item_of_a_target_also_frees_its_bucket_and_the_global_budget() {
    // A leaked bucket or a stale `total` would silently shrink the global cap
    // for the rest of the process's life.
    let (q, clock) = queue();
    q.push(delivery("b", "soon", 2_000)).unwrap();
    clock.advance(1_500);
    assert_eq!(q.drop_expired(), 1);
    assert_eq!(q.total_len(), 0, "the global counter must come back down");
    assert!(q.is_empty());
    assert!(q.push(delivery("b", "fresh", 99_999)).is_ok());
}

#[test]
fn clear_for_removes_one_targets_backlog_and_reports_the_count() {
    let (q, _clock) = queue();
    q.push(delivery("b", "1", 99_999)).unwrap();
    q.push(delivery("b", "2", 99_999)).unwrap();
    q.push(delivery("c", "3", 99_999)).unwrap();

    assert_eq!(q.clear_for("b"), 2);
    assert_eq!(q.len_for("b"), 0);
    assert_eq!(q.len_for("c"), 1);
    assert_eq!(q.total_len(), 1, "the global counter must track the removal");
}

#[test]
fn clear_for_an_unknown_target_is_a_no_op() {
    let (q, _clock) = queue();
    q.push(delivery("b", "1", 99_999)).unwrap();
    assert_eq!(q.clear_for("nobody"), 0);
    assert_eq!(q.total_len(), 1);
}

#[test]
fn a_target_that_drains_completely_frees_its_slot_for_the_global_budget() {
    let (q, _clock) = queue();
    q.push(delivery("b", "1", 99_999)).unwrap();
    q.pop_head("b");
    assert_eq!(q.total_len(), 0);
    assert_eq!(q.len_for("b"), 0);
}

#[test]
fn pop_head_on_an_unknown_target_does_not_corrupt_the_counter() {
    let (q, _clock) = queue();
    q.push(delivery("b", "1", 99_999)).unwrap();
    q.pop_head("nobody");
    assert_eq!(q.total_len(), 1, "a miss must not decrement the total");
}
