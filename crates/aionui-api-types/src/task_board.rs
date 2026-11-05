//! Work-item board (M8 E6-3) request/response types.
//!
//! Board columns (`board_status`) are the user-facing workflow state and are
//! separate from the run state in `TaskSessionResponse::status`.

use serde::{Deserialize, Deserializer, Serialize};

/// Board columns in display order.
pub const TASK_BOARD_COLUMNS: [TaskBoardStatus; 6] = [
    TaskBoardStatus::Backlog,
    TaskBoardStatus::Todo,
    TaskBoardStatus::InProgress,
    TaskBoardStatus::InReview,
    TaskBoardStatus::Done,
    TaskBoardStatus::Cancelled,
];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoardStatus {
    Backlog,
    Todo,
    InProgress,
    InReview,
    Done,
    Cancelled,
}

impl TaskBoardStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Todo => "todo",
            Self::InProgress => "in_progress",
            Self::InReview => "in_review",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        TASK_BOARD_COLUMNS.into_iter().find(|status| status.as_str() == value)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssigneeKind {
    User,
    Agent,
    Squad,
}

impl TaskAssigneeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Squad => "squad",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        [Self::User, Self::Agent, Self::Squad]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskAssignee {
    pub kind: TaskAssigneeKind,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardItemResponse {
    pub id: String,
    pub title: String,
    pub project_id: Option<String>,
    pub conversation_id: Option<String>,
    /// `agent` | `plan` | `goal`.
    pub mode: String,
    /// Run state (`TaskSessionStatus` wire value), shown as a badge.
    pub status: String,
    pub board_status: TaskBoardStatus,
    pub assignee: Option<TaskAssignee>,
    /// 0 none, 1 low, 2 medium, 3 high, 4 urgent.
    pub priority: i64,
    pub labels: Vec<String>,
    /// Lexicographic order inside a column; `null` sorts last.
    pub board_rank: Option<String>,
    pub external_ref: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// M8 E7-6: run state of the newest Squad instance started for this item
    /// (`POST /api/squads/{id}/instantiate` with `work_item_id`). Omitted when
    /// the item is not assigned to a Squad or no instance has anything to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squad_run: Option<crate::SquadRunPayload>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardColumnResponse {
    pub board_status: TaskBoardStatus,
    pub items: Vec<TaskBoardItemResponse>,
}

/// `GET /api/task-sessions/board`: always all six columns, in display order.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardResponse {
    pub columns: Vec<TaskBoardColumnResponse>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardQuery {
    pub project_id: Option<String>,
    pub assignee_kind: Option<TaskAssigneeKind>,
    pub assignee_id: Option<String>,
    pub label: Option<String>,
}

/// Distinguishes an absent field (`None`) from an explicit `null` (`Some(None)`).
fn double_option<'de, T, D>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

/// `PATCH /api/task-sessions/{id}/board`. Absent fields are unchanged;
/// `board_rank: null` / `assignee: null` clear the value. `done` cannot be set
/// here; use the review endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateTaskBoardRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub board_status: Option<TaskBoardStatus>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub board_rank: Option<Option<String>>,
    #[serde(default, deserialize_with = "double_option", skip_serializing_if = "Option::is_none")]
    pub assignee: Option<Option<TaskAssignee>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// Optimistic concurrency: the `updated_at` the client last saw. When it
    /// no longer matches, the update is refused with 409 `board_item_stale`.
    /// Omit it to keep last-write-wins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_updated_at: Option<i64>,
}

/// WebSocket event names for the board (M8 E6). Every payload carries
/// `user_id` (routing) and `task_session_id`.
pub const EVENT_TASK_BOARD_UPDATED: &str = "task.board.updated";
pub const EVENT_TASK_COMMENT_CREATED: &str = "task.comment.created";
pub const EVENT_TASK_COMMENT_UPDATED: &str = "task.comment.updated";

/// What caused a `task.board.updated` event.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoardChange {
    /// `PATCH /api/task-sessions/{id}/board`.
    Board,
    /// `POST /api/task-sessions/{id}/review`.
    Review,
    /// M8 E7-6: the linked Squad instance's `squad_run` changed state; the
    /// card did not move (`from_board_status == item.board_status`).
    SquadRun,
    /// M8 E6-7: created or refreshed by a read-only GitHub Issue import.
    Import,
}

/// `POST /api/task-sessions/import/github` (M8 E6-7, D6-6): one manual,
/// read-only import of GitHub Issues into the backlog.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportGithubIssuesRequest {
    /// `owner/name`.
    pub repo: String,
    /// GitHub issue search qualifiers (`label:bug`, `milestone:"v2"`, …).
    /// Only open issues are imported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Maximum issues to fetch (1–100, default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// What an import did to one issue.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportedIssueOutcome {
    /// New backlog work item.
    Created,
    /// Existing item refreshed (title / body / labels only).
    Updated,
    /// Existing item already matched the issue.
    Unchanged,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportedIssueItem {
    /// `github:owner/name#123`.
    pub external_ref: String,
    pub task_session_id: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub outcome: ImportedIssueOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImportGithubIssuesResponse {
    /// Normalized `owner/name`.
    pub repo: String,
    pub created: u32,
    pub updated: u32,
    pub unchanged: u32,
    pub items: Vec<ImportedIssueItem>,
}

/// `task.board.updated` payload: the item after the change.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardUpdatedEvent {
    pub user_id: String,
    pub task_session_id: String,
    pub change: TaskBoardChange,
    /// Column before the change (equal to `item.board_status` when it did not move).
    pub from_board_status: TaskBoardStatus,
    pub item: TaskBoardItemResponse,
}

/// `task.comment.created` / `task.comment.updated` payload. Timeline events
/// (`status_change`, `assignment`, `review_decision`) are comments too.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCommentEvent {
    pub user_id: String,
    pub task_session_id: String,
    pub comment: TaskCommentResponse,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskCommentKind {
    Comment,
    StatusChange,
    Assignment,
    ReviewDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCommentResponse {
    pub id: String,
    pub task_session_id: String,
    /// `user` | `agent` | `system`.
    pub author_kind: String,
    pub author_id: String,
    pub kind: TaskCommentKind,
    /// Plain text for `comment`; a JSON object for the event kinds:
    /// `status_change` `{"from","to"}`, `assignment` `{"assignee"}`,
    /// `review_decision` `{"decision","comment"}`.
    pub body: String,
    pub created_at: i64,
    pub edited_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CreateTaskCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EditTaskCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskBoardReviewDecision {
    Approve,
    Reject,
}

/// `POST /api/task-sessions/{id}/review`; user credential only. `reject`
/// requires a non-empty `comment` and sends the item back to `todo` (D6-5).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardReviewRequest {
    pub decision: TaskBoardReviewDecision,
    #[serde(default)]
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBoardReviewResponse {
    pub item: TaskBoardItemResponse,
    pub decision: TaskCommentResponse,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_request_distinguishes_absent_and_null() {
        let absent: UpdateTaskBoardRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(absent, UpdateTaskBoardRequest::default());
        let cleared: UpdateTaskBoardRequest = serde_json::from_str(r#"{"assignee":null,"board_rank":null}"#).unwrap();
        assert_eq!(cleared.assignee, Some(None));
        assert_eq!(cleared.board_rank, Some(None));
        let set: UpdateTaskBoardRequest = serde_json::from_str(
            r#"{"board_status":"in_progress","assignee":{"kind":"agent","id":"codex"},"labels":["a"]}"#,
        )
        .unwrap();
        assert_eq!(set.board_status, Some(TaskBoardStatus::InProgress));
        assert_eq!(
            set.assignee,
            Some(Some(TaskAssignee {
                kind: TaskAssigneeKind::Agent,
                id: "codex".into()
            }))
        );
    }

    #[test]
    fn status_round_trips_wire_values() {
        for status in TASK_BOARD_COLUMNS {
            assert_eq!(TaskBoardStatus::parse(status.as_str()), Some(status));
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().into())
            );
        }
    }
}
