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
/// (kill agent processes, drop cron jobs, etc.). Hooks run sequentially in
/// registration order; failures must be logged inside the hook and not
/// propagated.
#[async_trait]
pub trait OnConversationDelete: Send + Sync {
    async fn on_conversation_deleted(&self, conversation_id: &str);
}
