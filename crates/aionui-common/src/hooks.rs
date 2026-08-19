//! Cross-crate lifecycle hook traits.
//!
//! Hooks defined here let lower-layer crates (e.g. `aionui-ai-agent`,
//! `aionui-cron`) react to events owned by higher-layer crates (e.g.
//! `aionui-conversation`) without forming a dependency cycle.

use async_trait::async_trait;

/// Notified before a conversation row is deleted via
/// `ConversationService::delete`.
///
/// Implementors are responsible for cleaning up their per-conversation state
/// (kill agent processes, drop cron job state, etc.). Hooks run sequentially
/// in registration order; failures must be logged inside the hook and not
/// propagated.
#[async_trait]
pub trait OnConversationDelete: Send + Sync {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str);
}

/// Notified when a conversation's turn was actually cancelled via
/// `ConversationService::cancel`.
///
/// Exists so an upper-layer crate (`aionui-session-message`) can drop the
/// pending deliveries aimed at that conversation without
/// `aionui-conversation` depending upwards. Without it, "stop" is a lie: the
/// user cancels A's turn, and a second later the drainer delivers B's queued
/// message to A, which starts a new turn — whack-a-mole the user cannot win.
///
/// Only fired on the branches where a cancel really took effect. A cancel whose
/// `turn_id` did not match the active turn cancelled nothing, and must NOT
/// clear the queue — doing so would silently drop messages, which is the worst
/// failure mode this feature has.
///
/// Hooks run sequentially in registration order; failures must be logged
/// inside the hook and not propagated.
#[async_trait]
pub trait OnConversationTurnCancelled: Send + Sync {
    async fn on_turn_cancelled(&self, user_id: &str, conversation_id: &str, turn_id: &str);
}
