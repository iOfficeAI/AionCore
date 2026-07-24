use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Row mapping for the `teams` table.
///
/// The `agents` column stores a JSON array of `TeamAgent` objects.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TeamRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub workspace: String,
    pub workspace_mode: String,
    /// JSON array: serialized `TeamAgent[]`.
    pub agents: String,
    pub lead_agent_id: Option<String>,
    pub session_mode: Option<String>,
    pub agents_version: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    /// Project binding (project-bind side branch); NULL until bound/backfilled.
    pub project_id: Option<String>,
    /// Workspace folder binding; NULL until bound/backfilled.
    pub folder_id: Option<String>,
}

/// Row mapping for the `mailbox` table.
///
/// Represents an inter-agent message within a team.
/// The `read` column tracks whether the message has been consumed.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MailboxMessageRow {
    pub id: String,
    pub team_id: String,
    pub to_agent_id: String,
    pub from_agent_id: String,
    /// Message type: 'message', 'idle_notification', or 'shutdown_request'.
    #[sqlx(rename = "type")]
    pub msg_type: String,
    pub content: String,
    pub summary: Option<String>,
    /// JSON-serialized file paths attached to the message.
    pub files: Option<String>,
    pub read: bool,
    pub created_at: TimestampMs,
}

/// Row mapping for the `team_tasks` table.
///
/// Task board entry with dependency tracking via `blocked_by` / `blocks`
/// JSON arrays forming a bidirectional link graph.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct TeamTaskRow {
    pub id: String,
    pub team_id: String,
    pub subject: String,
    pub description: Option<String>,
    /// Task status: 'pending', 'in_progress', 'completed', or 'deleted'.
    pub status: String,
    pub owner: Option<String>,
    /// JSON array of task IDs that block this task.
    pub blocked_by: String,
    /// JSON array of task IDs that this task blocks.
    pub blocks: String,
    /// JSON object: arbitrary extension metadata.
    pub metadata: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn team_row_default_agents_is_empty_json_array() {
        let row = TeamRow {
            id: "t1".into(),
            user_id: "system_default_user".into(),
            name: "Team".into(),
            workspace: "/tmp/ws".into(),
            workspace_mode: "shared".into(),
            agents: "[]".into(),
            lead_agent_id: None,
            session_mode: None,
            agents_version: "1.0.1".into(),
            created_at: 0,
            updated_at: 0,
            project_id: None,
            folder_id: None,
        };
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&row.agents).expect("agents should be valid JSON");
        assert!(parsed.is_empty());
    }

    #[test]
    fn mailbox_row_msg_type_field_maps_correctly() {
        let row = MailboxMessageRow {
            id: "m1".into(),
            team_id: "t1".into(),
            to_agent_id: "a1".into(),
            from_agent_id: "a2".into(),
            msg_type: "message".into(),
            content: "hello".into(),
            summary: None,
            files: None,
            read: false,
            created_at: 0,
        };
        assert_eq!(row.msg_type, "message");
    }

    #[test]
    fn team_task_row_default_blocked_by_is_empty_json_array() {
        let row = TeamTaskRow {
            id: "tk1".into(),
            team_id: "t1".into(),
            subject: "Task".into(),
            description: None,
            status: "pending".into(),
            owner: None,
            blocked_by: "[]".into(),
            blocks: "[]".into(),
            metadata: None,
            created_at: 0,
            updated_at: 0,
        };
        let blocked: Vec<String> = serde_json::from_str(&row.blocked_by).expect("blocked_by should be valid JSON");
        assert!(blocked.is_empty());
        let blocks: Vec<String> = serde_json::from_str(&row.blocks).expect("blocks should be valid JSON");
        assert!(blocks.is_empty());
    }

    #[test]
    fn team_task_row_serialization_roundtrip() {
        let row = TeamTaskRow {
            id: "tk1".into(),
            team_id: "t1".into(),
            subject: "Implement feature".into(),
            description: Some("Details".into()),
            status: "in_progress".into(),
            owner: Some("agent-1".into()),
            blocked_by: r#"["tk0"]"#.into(),
            blocks: r#"["tk2","tk3"]"#.into(),
            metadata: Some(r#"{"priority":"high"}"#.into()),
            created_at: 1000,
            updated_at: 2000,
        };
        let json = serde_json::to_string(&row).expect("serialize");
        let restored: TeamTaskRow = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.id, row.id);
        assert_eq!(restored.status, row.status);
        assert_eq!(restored.blocked_by, row.blocked_by);
        assert_eq!(restored.blocks, row.blocks);
    }
}
