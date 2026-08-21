use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `channel_connections` table.
///
/// One connection instance of a channel plugin. `id` is a generated,
/// meaning-free connection id; the platform lives in `plugin_key`. The
/// `config` column holds an encrypted JSON blob containing credentials and
/// options. Phase 1 keeps one connection per (owner, plugin_key).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelConnectionRow {
    pub id: String,
    pub owner_user_id: String,
    /// Platform key (telegram, lark, dingtalk, weixin, slack, discord, or an
    /// extension-contributed plugin id).
    pub plugin_key: String,
    pub name: String,
    pub enabled: bool,
    /// JSON blob: `{ credentials, config }`. Stored encrypted at rest.
    pub config: String,
    pub status: Option<String>,
    pub last_connected: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `channel_users` table (+ derived platform).
///
/// Represents an IM user authorized to chat with the assistant, attached to
/// the connection that authorized them. Revocation is a soft delete
/// (`status` = active|revoked) so authorization history survives for audit.
///
/// Field-name bridge (channel refactor A2): the DB column is
/// `external_user_id`, surfaced here as `platform_user_id` to keep the wide
/// caller surface stable; `platform_type` is NOT a column — reads derive it
/// from the connection (`channel_connections.plugin_key`), and writes ignore
/// it in favor of `connection_id`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelUserRow {
    pub id: String,
    pub owner_user_id: String,
    pub connection_id: String,
    #[sqlx(rename = "external_user_id")]
    pub platform_user_id: String,
    /// Derived from the joined connection; not stored on this table.
    pub platform_type: String,
    pub display_name: Option<String>,
    pub status: String,
    pub revoked_at: Option<TimestampMs>,
    pub authorized_at: TimestampMs,
    pub last_active: Option<TimestampMs>,
}

/// Row mapping for the `channel_conversation_bindings` table.
///
/// Per-chat binding linking an authorized channel user to a conversation.
/// FK: (owner_user_id, connection_id, channel_user_id) → channel_users
/// ON DELETE CASCADE; conversation_id → conversations(id) ON DELETE SET NULL,
/// with triggers making cross-account conversation bindings unrepresentable.
///
/// Field-name bridge (channel refactor A3): `user_id` maps the
/// `channel_user_id` column, `chat_id` maps `external_chat_id`, and
/// `last_activity` maps `last_active_at` — names kept to limit caller churn.
/// The legacy `agent_type`/`workspace` columns are gone: agent configuration
/// is owned by channel settings + the conversation snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelConversationBindingRow {
    pub id: String,
    pub owner_user_id: String,
    pub connection_id: String,
    #[sqlx(rename = "channel_user_id")]
    pub user_id: String,
    #[sqlx(rename = "external_chat_id")]
    pub chat_id: Option<String>,
    pub conversation_id: Option<String>,
    pub created_at: TimestampMs,
    #[sqlx(rename = "last_active_at")]
    pub last_activity: TimestampMs,
}

/// Row mapping for the `channel_pairing_requests` table (+ derived platform).
///
/// 6-digit pairing code with 10-minute expiry. Status transitions:
/// pending → approved | rejected | expired. Only a server-side HMAC of the
/// code is stored (`code_hash`); the plaintext code exists solely in the
/// transient pairing flow (IM message + WebSocket event).
///
/// Same field-name bridge as [`ChannelUserRow`]: `platform_user_id` maps the
/// `external_user_id` column and `platform_type` is derived from the joined
/// connection.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ChannelPairingRequestRow {
    pub id: String,
    pub owner_user_id: String,
    pub connection_id: String,
    #[sqlx(rename = "external_user_id")]
    pub platform_user_id: String,
    /// Derived from the joined connection; not stored on this table.
    pub platform_type: String,
    pub display_name: Option<String>,
    pub code_hash: String,
    pub status: String,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub approved_channel_user_id: Option<String>,
}
