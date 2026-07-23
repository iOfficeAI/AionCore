use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ApprovalRequestRow {
    pub id: String,
    pub requester_user_id: String,
    pub project_id: Option<String>,
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub conversation_id: String,
    pub agent_id: Option<String>,
    pub call_id: String,
    pub action_type: String,
    pub command: Option<String>,
    pub working_directory: Option<String>,
    pub risk_level: String,
    /// JSON array of the immutable choices presented to the requester.
    pub options: String,
    pub status: String,
    pub approver_user_id: Option<String>,
    pub source_channel: Option<String>,
    pub source_user_id: Option<String>,
    pub source_chat_id: Option<String>,
    pub source_thread_id: Option<i64>,
    pub expires_at: TimestampMs,
    pub consumed_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
