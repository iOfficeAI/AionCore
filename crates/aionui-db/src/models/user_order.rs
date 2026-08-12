use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Ordering scene, stored as the TEXT `user_order.scene` column.
///
/// A closed enum; v1 has only `pinned` (a row's existence means the item is
/// pinned). Lives in `aionui-db` so the store trait does not depend upward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderScene {
    /// Pinned items, ordered by `order_key` ascending.
    Pinned,
}

impl OrderScene {
    /// The canonical TEXT column value.
    pub fn as_str(self) -> &'static str {
        match self {
            OrderScene::Pinned => "pinned",
        }
    }

    /// Parse a TEXT column value; `None` for unknown (out-of-enum) values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "pinned" => Some(OrderScene::Pinned),
            _ => None,
        }
    }
}

/// Ordered item kind, stored as the TEXT `user_order.item_type` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderItemType {
    Conversation,
    Team,
}

impl OrderItemType {
    /// The canonical TEXT column value.
    pub fn as_str(self) -> &'static str {
        match self {
            OrderItemType::Conversation => "conversation",
            OrderItemType::Team => "team",
        }
    }

    /// Parse a TEXT column value; `None` for unknown values.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "conversation" => Some(OrderItemType::Conversation),
            "team" => Some(OrderItemType::Team),
            _ => None,
        }
    }
}

/// Row mapping for the `user_order` table.
///
/// `scene` / `item_type` are TEXT enum-like columns ([`OrderScene`] /
/// [`OrderItemType`]). `order_key` is not unique within a `(user_id, scene)`;
/// callers tie-break on the full `(order_key, item_type, item_id)` triple.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct UserOrderRow {
    pub user_id: String,
    /// One of: "pinned" (see [`OrderScene`]).
    pub scene: String,
    /// One of: "conversation", "team" (see [`OrderItemType`]).
    pub item_type: String,
    pub item_id: String,
    pub order_key: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[cfg(test)]
#[path = "user_order_test.rs"]
mod user_order_test;
