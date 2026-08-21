//! Cascade that keeps `user_order` in sync when a conversation leaves the
//! sidebar's visible domain (design §4.3, path 1).
//!
//! When a conversation is deleted — directly, or as a team member during
//! `remove_team` (which routes member deletion through
//! `ConversationService::delete`) — its ordering rows must go too, or an orphan
//! pinned row would resurface a ghost entry on the next read. This runs as an
//! `OnConversationDelete` hook, i.e. after the owning delete has already been
//! authorized, so it is best-effort: a failure here degrades to an orphan row
//! that the read side already drops during hydration (page_pinned skips ids that
//! no longer hydrate), never a failed user-facing delete.

use std::sync::Arc;

use aionui_common::OnConversationDelete;
use aionui_db::{IUserOrderStore, OrderItemRef, OrderItemType};
use async_trait::async_trait;

/// `OnConversationDelete` hook that drops a deleted conversation's `user_order`
/// rows across all scenes. Registered on `ConversationService` via
/// `with_delete_hook` in `aionui-app`.
pub struct UserOrderDeleteHook {
    user_order: Arc<dyn IUserOrderStore>,
}

impl UserOrderDeleteHook {
    pub fn new(user_order: Arc<dyn IUserOrderStore>) -> Self {
        Self { user_order }
    }
}

#[async_trait]
impl OnConversationDelete for UserOrderDeleteHook {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        let item = OrderItemRef::new(OrderItemType::Conversation, conversation_id);
        if let Err(err) = self.user_order.remove_item(user_id, &item).await {
            // Best-effort: the read side drops orphan rows during hydration, so a
            // failure here self-heals rather than breaking the delete.
            tracing::warn!(
                user_id = %user_id,
                conversation_id = %conversation_id,
                error = %err,
                "sidebar: failed to cascade-delete user_order rows for deleted conversation"
            );
        }
    }
}

#[cfg(test)]
#[path = "cascade_test.rs"]
mod cascade_test;
