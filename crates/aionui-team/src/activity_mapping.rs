//! Row/domain → response mapping for the read-only team activity view.
//!
//! Keeps the projection out of `aionui-api-types` (which must not depend on
//! domain types) and out of the repository layer.

use aionui_api_types::{TeamActivityItemResponse, TeamActivityKind, TeamMailboxMessageResponse, TeamTaskResponse};
use aionui_db::PageDirection;
use aionui_db::models::{MailboxMessageRow, TeamTaskRow};
use tracing::warn;

use crate::types::TeamTask;

/// Maps a `mailbox` row to its read-only response DTO.
///
/// `files` is stored as an optional JSON array string. A malformed value is
/// treated as "no files" (logged at `warn`, no content included) so a single
/// bad row never breaks the whole activity feed.
pub fn mailbox_row_to_response(row: &MailboxMessageRow) -> TeamMailboxMessageResponse {
    let files = match row.files.as_deref() {
        None => Vec::new(),
        Some(raw) => serde_json::from_str::<Vec<String>>(raw).unwrap_or_else(|_| {
            warn!(
                message_id = %row.id,
                team_id = %row.team_id,
                "mailbox row has malformed files JSON; treating as empty"
            );
            Vec::new()
        }),
    };
    TeamMailboxMessageResponse {
        id: row.id.clone(),
        team_id: row.team_id.clone(),
        from_agent_id: row.from_agent_id.clone(),
        to_agent_id: row.to_agent_id.clone(),
        msg_type: row.msg_type.clone(),
        content: row.content.clone(),
        summary: row.summary.clone(),
        files,
        read: row.read,
        created_at: row.created_at,
    }
}

/// Maps a parsed domain `TeamTask` to its read-only response DTO.
///
/// `metadata` is intentionally dropped (not exposed by the v1 activity view).
pub fn task_to_response(task: &TeamTask) -> TeamTaskResponse {
    TeamTaskResponse {
        id: task.id.clone(),
        team_id: task.team_id.clone(),
        subject: task.subject.clone(),
        description: task.description.clone(),
        status: task.status.to_string(),
        owner: task.owner.clone(),
        blocked_by: task.blocked_by.clone(),
        blocks: task.blocks.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
    }
}

/// Wraps a mailbox row as a unified activity item (`kind = message`).
pub fn message_row_to_activity_item(row: &MailboxMessageRow) -> TeamActivityItemResponse {
    let message = mailbox_row_to_response(row);
    TeamActivityItemResponse {
        kind: TeamActivityKind::Message,
        created_at: message.created_at,
        id: message.id.clone(),
        message: Some(message),
        task: None,
    }
}

/// Wraps a task row as a unified activity item (`kind = task`). A row whose
/// stored fields fail to parse is skipped (returns `None`) so one bad row never
/// breaks the whole feed.
pub fn task_row_to_activity_item(row: &TeamTaskRow) -> Option<TeamActivityItemResponse> {
    let task = TeamTask::from_row(row).ok()?;
    let resp = task_to_response(&task);
    Some(TeamActivityItemResponse {
        kind: TeamActivityKind::Task,
        created_at: resp.created_at,
        id: resp.id.clone(),
        task: Some(resp),
        message: None,
    })
}

/// Sorts unified activity items by `(created_at, id)` in the given direction,
/// matching the repository `ORDER BY created_at, id` used by the paged queries.
pub fn sort_activity_items(items: &mut [TeamActivityItemResponse], direction: PageDirection) {
    match direction {
        PageDirection::Desc => items.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id))),
        PageDirection::Asc => items.sort_by(|a, b| a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TaskStatus;

    fn row(files: Option<&str>) -> MailboxMessageRow {
        MailboxMessageRow {
            id: "m1".into(),
            team_id: "t1".into(),
            to_agent_id: "a1".into(),
            from_agent_id: "a2".into(),
            msg_type: "message".into(),
            content: "hello".into(),
            summary: Some("hi".into()),
            files: files.map(str::to_owned),
            read: true,
            created_at: 42,
        }
    }

    #[test]
    fn mailbox_row_maps_all_fields() {
        let resp = mailbox_row_to_response(&row(Some(r#"["/tmp/a.txt","/tmp/b.txt"]"#)));
        assert_eq!(resp.id, "m1");
        assert_eq!(resp.team_id, "t1");
        assert_eq!(resp.from_agent_id, "a2");
        assert_eq!(resp.to_agent_id, "a1");
        assert_eq!(resp.msg_type, "message");
        assert_eq!(resp.content, "hello");
        assert_eq!(resp.summary.as_deref(), Some("hi"));
        assert_eq!(resp.files, vec!["/tmp/a.txt", "/tmp/b.txt"]);
        assert!(resp.read);
        assert_eq!(resp.created_at, 42);
    }

    #[test]
    fn mailbox_row_missing_files_is_empty() {
        let resp = mailbox_row_to_response(&row(None));
        assert!(resp.files.is_empty());
    }

    #[test]
    fn mailbox_row_malformed_files_degrades_to_empty() {
        let resp = mailbox_row_to_response(&row(Some("{not valid json")));
        assert!(resp.files.is_empty());
    }

    #[test]
    fn task_maps_all_fields_without_metadata() {
        let task = TeamTask {
            id: "tk1".into(),
            team_id: "t1".into(),
            subject: "Build".into(),
            description: Some("desc".into()),
            status: TaskStatus::InProgress,
            owner: Some("a1".into()),
            blocked_by: vec!["tk0".into()],
            blocks: vec!["tk2".into()],
            metadata: Some(serde_json::json!({"priority": "high"})),
            created_at: 1,
            updated_at: 2,
        };
        let resp = task_to_response(&task);
        assert_eq!(resp.id, "tk1");
        assert_eq!(resp.status, "in_progress");
        assert_eq!(resp.owner.as_deref(), Some("a1"));
        assert_eq!(resp.blocked_by, vec!["tk0"]);
        assert_eq!(resp.blocks, vec!["tk2"]);
        assert_eq!(resp.created_at, 1);
        assert_eq!(resp.updated_at, 2);
    }
}
