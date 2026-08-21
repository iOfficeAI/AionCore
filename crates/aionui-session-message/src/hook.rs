//! `OnConversationTurnCancelled` implementation: cancelling a turn drops the
//! deliveries queued for that conversation.
//!
//! "Cancel ⇒ clear pending inbox" is existing semantics in this repo, not a new
//! idea — see `Mailbox::mark_all_unread_for_agents_read`
//! (`crates/aionui-team/src/mailbox.rs`), whose comment reads "Runtime-only
//! cancel helper: mark currently unread mailbox rows as read for the provided
//! agents so a cancelled run does not consume them later."

use std::sync::Arc;

use aionui_common::{OnConversationTurnCancelled, TurnCancelCause};
use async_trait::async_trait;
use tracing::{debug, info};

use crate::queue::DeliveryQueue;

pub struct QueueClearingCancelHook {
    queue: Arc<DeliveryQueue>,
}

impl QueueClearingCancelHook {
    pub fn new(queue: Arc<DeliveryQueue>) -> Self {
        Self { queue }
    }
}

#[async_trait]
impl OnConversationTurnCancelled for QueueClearingCancelHook {
    async fn on_turn_cancelled(&self, _user_id: &str, conversation_id: &str, turn_id: &str, cause: TurnCancelCause) {
        // A runtime restart is NOT a request to abandon work aimed here. It
        // cancels the active turn only so the agent process can be killed and
        // rebuilt, and it leaves the conversation idle — the exact state a
        // pending delivery has been waiting for. Clearing at that moment would
        // discard a message one tick before it became deliverable, and silent
        // message loss is the worst failure mode this feature has.
        if cause != TurnCancelCause::UserRequested {
            debug!(
                conversation_id,
                turn_id,
                cause = ?cause,
                "turn cancelled without a user stop; pending cross-session deliveries kept"
            );
            return;
        }
        let cleared = self.queue.clear_for(conversation_id);
        if cleared > 0 {
            info!(
                conversation_id,
                turn_id, cleared, "cleared pending cross-session deliveries because the turn was cancelled"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::queue::{PendingDelivery, TestClock};

    fn delivery(to: &str) -> PendingDelivery {
        PendingDelivery {
            to: to.to_owned(),
            user_id: "user_1".to_owned(),
            from_conversation_id: "conv_from".to_owned(),
            message: "m".to_owned(),
            expires_at_ms: 10_000_000,
        }
    }

    #[tokio::test]
    async fn cancelling_a_conversation_clears_only_that_conversations_backlog() {
        let queue = Arc::new(DeliveryQueue::new(Arc::new(TestClock::new(0))));
        queue.push(delivery("b")).unwrap();
        queue.push(delivery("b")).unwrap();
        queue.push(delivery("c")).unwrap();

        let hook = QueueClearingCancelHook::new(queue.clone());
        hook.on_turn_cancelled("user_1", "b", "turn_1", TurnCancelCause::UserRequested)
            .await;

        assert_eq!(queue.len_for("b"), 0, "the cancelled target's backlog must be gone");
        assert_eq!(queue.len_for("c"), 1, "an unrelated target must be untouched");
    }

    #[tokio::test]
    async fn clearing_an_empty_backlog_is_a_no_op() {
        let queue = Arc::new(DeliveryQueue::new(Arc::new(TestClock::new(0))));
        let hook = QueueClearingCancelHook::new(queue.clone());
        hook.on_turn_cancelled("user_1", "b", "turn_1", TurnCancelCause::UserRequested)
            .await;
        assert!(queue.is_empty());
    }

    /// The hook clears by TARGET, so a cancel on the SENDER must leave the
    /// messages that sender already queued for someone else alone.
    #[tokio::test]
    async fn cancelling_the_sender_does_not_clear_what_it_queued_elsewhere() {
        let queue = Arc::new(DeliveryQueue::new(Arc::new(TestClock::new(0))));
        queue.push(delivery("b")).unwrap();

        let hook = QueueClearingCancelHook::new(queue.clone());
        hook.on_turn_cancelled("user_1", "conv_from", "turn_1", TurnCancelCause::UserRequested)
            .await;

        assert_eq!(queue.len_for("b"), 1);
    }

    /// A runtime restart cancels the active turn only to recycle the agent
    /// process, and leaves the conversation IDLE — the state a queued delivery
    /// has been waiting for. Clearing there would drop a message one tick before
    /// it became deliverable, silently.
    #[tokio::test]
    async fn a_runtime_restart_keeps_the_backlog() {
        let queue = Arc::new(DeliveryQueue::new(Arc::new(TestClock::new(0))));
        queue.push(delivery("b")).unwrap();
        queue.push(delivery("b")).unwrap();

        let hook = QueueClearingCancelHook::new(queue.clone());
        hook.on_turn_cancelled("user_1", "b", "turn_1", TurnCancelCause::RuntimeRestart)
            .await;

        assert_eq!(
            queue.len_for("b"),
            2,
            "a process recycle must not discard work aimed at the conversation"
        );
    }
}
