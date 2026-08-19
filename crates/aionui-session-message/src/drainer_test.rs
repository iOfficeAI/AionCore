use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;

use super::*;
use crate::queue::{DeliveryQueue, PendingDelivery, TestClock};

#[derive(Default)]
struct RecordingSink {
    delivered: std::sync::Mutex<Vec<String>>,
    /// Report `Busy` for the first N calls overall.
    busy_until_call: usize,
    /// Report `Busy` for this target, always.
    busy_for: Option<String>,
    calls: AtomicUsize,
    hard_error_for: Option<String>,
}

#[async_trait]
impl DeliverySink for RecordingSink {
    async fn deliver(&self, item: &PendingDelivery) -> DeliveryOutcome {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.hard_error_for.as_deref() == Some(item.to.as_str()) {
            return DeliveryOutcome::HardError("boom".to_owned());
        }
        if self.busy_for.as_deref() == Some(item.to.as_str()) {
            return DeliveryOutcome::Busy;
        }
        if call <= self.busy_until_call {
            return DeliveryOutcome::Busy;
        }
        self.delivered.lock().unwrap().push(item.message.clone());
        DeliveryOutcome::Delivered
    }
}

struct AlwaysEnabled;

#[async_trait]
impl DrainGate for AlwaysEnabled {
    async fn is_enabled_for(&self, _user_id: &str) -> bool {
        true
    }
}

struct AlwaysDisabled;

#[async_trait]
impl DrainGate for AlwaysDisabled {
    async fn is_enabled_for(&self, _user_id: &str) -> bool {
        false
    }
}

fn delivery(to: &str, message: &str) -> PendingDelivery {
    PendingDelivery {
        to: to.to_owned(),
        user_id: "user_1".to_owned(),
        from_conversation_id: "conv_from".to_owned(),
        from_name: "A".to_owned(),
        from_workspace: None,
        message: message.to_owned(),
        expires_at_ms: 10_000_000,
    }
}

fn harness(sink: Arc<RecordingSink>, gate: Arc<dyn DrainGate>) -> (Drainer, Arc<DeliveryQueue>, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(1_000));
    let queue = Arc::new(DeliveryQueue::new(clock.clone()));
    let drainer = Drainer::new(queue.clone(), sink, gate);
    (drainer, queue, clock)
}

#[tokio::test]
async fn a_queued_message_is_delivered_on_the_next_tick() {
    let sink = Arc::new(RecordingSink::default());
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    queue.push(delivery("b", "hello")).unwrap();

    drainer.tick_once().await;

    assert_eq!(*sink.delivered.lock().unwrap(), vec!["hello"]);
    assert!(queue.is_empty(), "a delivered message must leave the queue");
}

#[tokio::test]
async fn a_busy_target_keeps_its_message_queued_and_retries_next_tick() {
    let sink = Arc::new(RecordingSink {
        busy_until_call: 1,
        ..Default::default()
    });
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    queue.push(delivery("b", "hello")).unwrap();

    drainer.tick_once().await;
    assert!(sink.delivered.lock().unwrap().is_empty(), "still busy");
    assert_eq!(queue.len_for("b"), 1, "must stay queued while busy");

    drainer.tick_once().await;
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["hello"]);
    assert!(queue.is_empty());
}

#[tokio::test]
async fn three_messages_to_one_target_are_delivered_fifo_one_per_tick() {
    // Locks spec §6.4: per-message, FIFO, one turn each — NOT merged.
    let sink = Arc::new(RecordingSink::default());
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    queue.push(delivery("b", "1")).unwrap();
    queue.push(delivery("b", "2")).unwrap();
    queue.push(delivery("b", "3")).unwrap();

    drainer.tick_once().await;
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["1"], "one per tick, not merged");
    drainer.tick_once().await;
    drainer.tick_once().await;
    assert_eq!(*sink.delivered.lock().unwrap(), vec!["1", "2", "3"]);
}

#[tokio::test]
async fn a_hard_error_drops_the_head_instead_of_retrying_forever() {
    let sink = Arc::new(RecordingSink {
        hard_error_for: Some("b".to_owned()),
        ..Default::default()
    });
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    queue.push(delivery("b", "doomed")).unwrap();

    drainer.tick_once().await;

    assert!(queue.is_empty(), "a hard error must drop the head");
    assert!(sink.delivered.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_hard_error_on_the_head_still_lets_the_next_message_through() {
    // Dropping the head must advance the queue, not wedge it.
    let sink = Arc::new(RecordingSink {
        hard_error_for: Some("b".to_owned()),
        ..Default::default()
    });
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    queue.push(delivery("b", "doomed-1")).unwrap();
    queue.push(delivery("b", "doomed-2")).unwrap();

    drainer.tick_once().await;
    assert_eq!(queue.len_for("b"), 1, "only the head is dropped per tick");
    drainer.tick_once().await;
    assert!(queue.is_empty());
}

#[tokio::test]
async fn one_slow_target_does_not_starve_another() {
    // Locks "must use send_message, not run_agent_turn": a target stuck busy
    // must not stop a different target's message from going out this same tick.
    let sink = Arc::new(RecordingSink {
        busy_for: Some("busy_one".to_owned()),
        ..Default::default()
    });
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    queue.push(delivery("busy_one", "stuck")).unwrap();
    queue.push(delivery("free_one", "goes out")).unwrap();

    drainer.tick_once().await;

    assert_eq!(
        *sink.delivered.lock().unwrap(),
        vec!["goes out"],
        "the free target must be served in the same tick, the busy one not at all"
    );
    assert_eq!(queue.len_for("busy_one"), 1, "the busy target stays queued");
    assert_eq!(queue.len_for("free_one"), 0);
}

#[tokio::test]
async fn expired_messages_are_dropped_and_never_delivered() {
    let sink = Arc::new(RecordingSink::default());
    let (drainer, queue, clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    let mut item = delivery("b", "too late");
    item.expires_at_ms = 2_000;
    queue.push(item).unwrap();

    clock.advance(5_000); // past the deadline
    drainer.tick_once().await;

    assert!(queue.is_empty(), "expired items must be dropped");
    assert!(
        sink.delivered.lock().unwrap().is_empty(),
        "an expired message must never be delivered"
    );
}

#[tokio::test]
async fn an_expired_head_does_not_block_a_still_valid_message_behind_it() {
    let sink = Arc::new(RecordingSink::default());
    let (drainer, queue, clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    let mut stale = delivery("b", "too late");
    stale.expires_at_ms = 2_000;
    queue.push(stale).unwrap();
    queue.push(delivery("b", "still good")).unwrap();

    clock.advance(5_000);
    drainer.tick_once().await;

    assert_eq!(
        *sink.delivered.lock().unwrap(),
        vec!["still good"],
        "the expired head is dropped and the next one goes out in the SAME tick"
    );
}

#[tokio::test]
async fn a_disabled_user_is_not_drained_and_the_queue_is_cleared() {
    let sink = Arc::new(RecordingSink::default());
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysDisabled));
    queue.push(delivery("b", "hello")).unwrap();

    drainer.tick_once().await;

    assert!(
        sink.delivered.lock().unwrap().is_empty(),
        "must not deliver when disabled"
    );
    assert!(
        queue.is_empty(),
        "a disabled user's backlog must be cleared, so re-enabling does not flush stale messages"
    );
}

#[tokio::test]
async fn an_empty_tick_is_harmless() {
    let sink = Arc::new(RecordingSink::default());
    let (drainer, queue, _clock) = harness(sink.clone(), Arc::new(AlwaysEnabled));
    drainer.tick_once().await;
    assert!(queue.is_empty());
    assert!(sink.delivered.lock().unwrap().is_empty());
}
