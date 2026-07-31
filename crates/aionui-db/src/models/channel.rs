use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `assistant_plugins` table.
///
/// Stores channel plugin configurations. The `config` column holds an
/// encrypted JSON blob containing credentials and options.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelPluginRow {
    pub id: String,
    pub owner_user_id: String,
    /// Platform type (telegram, lark, dingtalk, weixin, slack, discord).
    #[sqlx(rename = "type")]
    pub r#type: String,
    pub name: String,
    pub enabled: bool,
    /// JSON blob: `{ credentials, config }`. Stored encrypted at rest.
    pub config: String,
    pub status: Option<String>,
    pub last_connected: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `assistant_users` table.
///
/// Represents an IM user authorized to chat with the assistant.
/// UNIQUE constraint on (owner_user_id, platform_user_id, platform_type).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantUserRow {
    pub id: String,
    pub owner_user_id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    pub display_name: Option<String>,
    pub authorized_at: TimestampMs,
    pub last_active: Option<TimestampMs>,
    pub session_id: Option<String>,
}

/// Row mapping for the `assistant_sessions` table.
///
/// Per-chat session linking an authorized user to a conversation.
/// FK: user_id → assistant_users(id) ON DELETE CASCADE.
/// FK: conversation_id → conversations(id) ON DELETE SET NULL.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssistantSessionRow {
    pub id: String,
    pub user_id: String,
    pub agent_type: String,
    pub conversation_id: Option<String>,
    pub workspace: Option<String>,
    pub chat_id: Option<String>,
    pub created_at: TimestampMs,
    pub last_activity: TimestampMs,
}

/// Row mapping for the `assistant_pairing_codes` table.
///
/// 6-digit pairing code with 10-minute expiry. Status transitions:
/// pending → approved | rejected | expired.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct PairingCodeRow {
    pub code: String,
    pub owner_user_id: String,
    pub platform_user_id: String,
    pub platform_type: String,
    pub display_name: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub status: String,
}
