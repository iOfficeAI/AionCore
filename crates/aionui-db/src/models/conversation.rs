use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `conversations` table.
///
/// Enum-like fields (`type`, `status`, `source`) are stored as TEXT strings.
/// The service layer converts them to/from `aionui_common` enums
/// (`AgentType`, `ConversationStatus`, `ConversationSource`).
///
/// JSON fields (`extra`, `model`) are stored as TEXT in SQLite and
/// deserialized by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConversationRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// Agent type string (e.g. "gemini", "acp", "remote").
    #[sqlx(rename = "type")]
    pub r#type: String,
    /// JSON object: type-specific extra data.
    pub extra: String,
    /// JSON object: `ProviderWithModel` serialized.
    pub model: Option<String>,
    /// One of: "pending", "running", "finished". NULL in legacy rows.
    pub status: Option<String>,
    /// One of: "aionui", "telegram", "lark", "dingtalk", "weixin".
    pub source: Option<String>,
    /// Channel isolation ID (e.g. "user:xxx", "group:xxx").
    pub channel_chat_id: Option<String>,
    /// Whether this conversation is pinned (SQLite INTEGER 0/1).
    pub pinned: bool,
    pub pinned_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for the `conversation_assistant_snapshots` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct ConversationAssistantSnapshotRow {
    pub conversation_id: String,
    pub assistant_definition_id: String,
    pub assistant_id: String,
    pub assistant_source: String,
    pub agent_id: String,
    pub rules_content: String,
    pub default_model_mode: String,
    pub resolved_model_id: Option<String>,
    pub default_permission_mode: String,
    pub resolved_permission_value: Option<String>,
    pub default_thought_level_mode: String,
    pub resolved_thought_level_value: Option<String>,
    pub default_skills_mode: String,
    pub resolved_skill_ids: String,
    pub resolved_disabled_builtin_skill_ids: String,
    pub default_mcps_mode: String,
    pub resolved_mcp_ids: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Insert-or-update parameters for `conversation_assistant_snapshots`.
#[derive(Debug, Clone)]
pub struct UpsertConversationAssistantSnapshotParams<'a> {
    pub conversation_id: &'a str,
    pub assistant_definition_id: &'a str,
    pub assistant_id: &'a str,
    pub assistant_source: &'a str,
    pub agent_id: &'a str,
    pub rules_content: &'a str,
    pub default_model_mode: &'a str,
    pub resolved_model_id: Option<&'a str>,
    pub default_permission_mode: &'a str,
    pub resolved_permission_value: Option<&'a str>,
    pub default_thought_level_mode: &'a str,
    pub resolved_thought_level_value: Option<&'a str>,
    pub default_skills_mode: &'a str,
    pub resolved_skill_ids: &'a str,
    pub resolved_disabled_builtin_skill_ids: &'a str,
    pub default_mcps_mode: &'a str,
    pub resolved_mcp_ids: &'a str,
}
