//! Work-item board service (M8 E6-3, D6-1: lives in aionui-conversation).
//!
//! The board column (`board_status`) is the user-facing workflow state. This
//! module owns the column state machine and the review gate; the DB layer only
//! validates values.
//!
//! Rules (E6-tasks-board.md §2.2, E6-5 auto-start / auto-review):
//! - `done` is reachable only through the review endpoint (`in_review → done`)
//!   and only for a user credential. Agent runtime tokens are denied on
//!   `/api/task-sessions/*` by the agent scope; [`BoardActor::Agent`] is the
//!   service-side second check and may only move `in_progress → in_review`.
//! - `backlog` never starts work: `backlog → in_progress` is rejected; items go
//!   through `todo`. Assigning an Agent on backlog only records the assignee.
//! - Assigning an Agent on `todo` (plan/goal approved when required) auto-moves
//!   to `in_progress` and writes an agent `assignment` comment 「已开工：…」.
//! - When a run finishes `completed` with at least one artifact while the card
//!   is `in_progress`, the board auto-moves to `in_review` (Agent actor).
//! - Failed / blocked runs keep `in_progress` and rely on the Inbox sink
//!   (`source_kind=task_run`) — they do not change the column.
//! - `plan` / `goal` items need an approved plan/goal approval before
//!   `in_progress`.
//! - Review reject sends the item back to `todo` (D6-5) with a mandatory
//!   comment.
//!
//! Realtime: every successful board change pushes `task.board.updated`, and
//! every timeline entry (comments and the status/assignment/review events)
//! pushes `task.comment.created`; comment edits push `task.comment.updated`.
//! Events are published only after the write succeeded.
//!
//! Squad cards (M8 E7-6): items assigned to a Squad carry `squad_run` from
//! the newest instance started for them, via [`BoardSquadRunPort`].
//!
//! Concurrency: `UpdateTaskBoardRequest::expected_updated_at` is an optional
//! optimistic lock. When given, the row must still carry that `updated_at`
//! (checked before the state machine and again atomically in the UPDATE);
//! otherwise the call fails with 409 `board_item_stale`. Omitted, the update
//! stays last-write-wins.

pub mod github_import;

use aionui_api_types::{
    EVENT_TASK_BOARD_UPDATED, EVENT_TASK_COMMENT_CREATED, EVENT_TASK_COMMENT_UPDATED, SquadRunPayload, TaskBoardChange,
    TaskBoardUpdatedEvent, TaskCommentEvent, WebSocketMessage,
};
use aionui_api_types::{
    TASK_BOARD_COLUMNS, TaskAssignee, TaskAssigneeKind, TaskBoardColumnResponse, TaskBoardItemResponse, TaskBoardQuery,
    TaskBoardResponse, TaskBoardReviewDecision, TaskBoardReviewRequest, TaskBoardReviewResponse, TaskBoardStatus,
    TaskCommentKind, TaskCommentResponse, UpdateTaskBoardRequest,
};
use aionui_common::now_ms;
use aionui_db::models::{TaskBoardItemRow, TaskCommentRow};
use aionui_db::{
    AppendTaskCommentParams, BOARD_STALE_CONFLICT_PREFIX, BoardAssignee, DbError, TaskBoardFilter,
    UpdateTaskBoardParams,
};

use crate::error::ConversationError;
use crate::service::ConversationService;

pub const BOARD_MAX_LABELS: usize = 20;
pub const BOARD_MAX_LABEL_CHARS: usize = 50;
pub const BOARD_MAX_RANK_CHARS: usize = 64;
pub const BOARD_MAX_COMMENT_CHARS: usize = 10_000;

/// Who is asking for a column move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardActor {
    User,
    Agent,
}

/// Why a column move was refused. `code` is stable for the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRejection {
    pub code: &'static str,
    pub reason: String,
}

fn reject(code: &'static str, reason: impl Into<String>) -> BoardRejection {
    BoardRejection {
        code,
        reason: reason.into(),
    }
}

/// Facts the state machine needs about the item.
#[derive(Debug, Clone, Copy)]
pub struct TransitionContext<'a> {
    pub mode: &'a str,
    pub plan_approved: bool,
}

/// Column moves through `PATCH /board` (not the review endpoint).
pub fn check_transition(
    from: TaskBoardStatus,
    to: TaskBoardStatus,
    actor: BoardActor,
    ctx: TransitionContext<'_>,
) -> Result<(), BoardRejection> {
    use TaskBoardStatus::*;
    if from == to {
        return Ok(());
    }
    if actor == BoardActor::Agent {
        return if from == InProgress && to == InReview {
            Ok(())
        } else {
            Err(reject(
                "agent_review_gate",
                "An agent may only submit an in-progress item for review",
            ))
        };
    }
    if to == Done {
        return Err(reject(
            "review_required",
            "Items move to done only by approving the review",
        ));
    }
    if from == InReview && to == Todo {
        return Err(reject(
            "review_required",
            "Send an item back from review by rejecting the review with a comment",
        ));
    }
    let allowed = match from {
        Backlog => matches!(to, Todo | Cancelled),
        Todo => matches!(to, Backlog | InProgress | Cancelled),
        InProgress => matches!(to, Todo | InReview | Cancelled),
        InReview => matches!(to, Cancelled),
        Done => false,
        Cancelled => matches!(to, Backlog),
    };
    if !allowed {
        let code = if from == Backlog && to == InProgress {
            "backlog_not_started"
        } else {
            "not_allowed"
        };
        return Err(reject(
            code,
            format!("Cannot move from {} to {}", from.as_str(), to.as_str()),
        ));
    }
    if to == InProgress && matches!(ctx.mode, "plan" | "goal") && !ctx.plan_approved {
        return Err(reject(
            "plan_not_approved",
            format!("A {} item needs an approved {} before it can start", ctx.mode, ctx.mode),
        ));
    }
    Ok(())
}

/// Review decision: `in_review → done` (approve) or `→ todo` (reject).
pub fn check_review(from: TaskBoardStatus, actor: BoardActor) -> Result<(), BoardRejection> {
    if actor != BoardActor::User {
        return Err(reject("agent_review_gate", "Only a user can decide a review"));
    }
    if from != TaskBoardStatus::InReview {
        return Err(reject("not_in_review", "Only an item in review can be decided"));
    }
    Ok(())
}

fn rejection_error(from: TaskBoardStatus, to: TaskBoardStatus, rejection: BoardRejection) -> ConversationError {
    ConversationError::BoardTransitionRejected {
        from: from.as_str().into(),
        to: to.as_str().into(),
        code: rejection.code,
        reason: rejection.reason,
    }
}

fn item_response(row: TaskBoardItemRow) -> Result<TaskBoardItemResponse, ConversationError> {
    let board_status = TaskBoardStatus::parse(&row.board_status)
        .ok_or_else(|| ConversationError::internal(format!("Unknown board_status '{}'", row.board_status)))?;
    let assignee = match (row.assignee_kind.as_deref(), row.assignee_id) {
        (Some(kind), Some(id)) => TaskAssigneeKind::parse(kind).map(|kind| TaskAssignee { kind, id }),
        _ => None,
    };
    let labels = serde_json::from_str::<Vec<String>>(&row.labels).unwrap_or_default();
    Ok(TaskBoardItemResponse {
        id: row.id,
        title: row.title,
        project_id: row.project_id,
        conversation_id: row.conversation_id,
        mode: row.mode,
        status: row.status,
        board_status,
        assignee,
        priority: row.priority,
        labels,
        board_rank: row.board_rank,
        external_ref: row.external_ref,
        created_at: row.created_at,
        updated_at: row.updated_at,
        squad_run: None,
    })
}

fn comment_kind(kind: &str) -> TaskCommentKind {
    match kind {
        "status_change" => TaskCommentKind::StatusChange,
        "assignment" => TaskCommentKind::Assignment,
        "review_decision" => TaskCommentKind::ReviewDecision,
        _ => TaskCommentKind::Comment,
    }
}

fn comment_response(row: TaskCommentRow) -> TaskCommentResponse {
    TaskCommentResponse {
        kind: comment_kind(&row.kind),
        id: row.id,
        task_session_id: row.task_session_id,
        author_kind: row.author_kind,
        author_id: row.author_id,
        body: row.body,
        created_at: row.created_at,
        edited_at: row.edited_at,
    }
}

fn task_not_found(id: &str) -> ConversationError {
    ConversationError::not_found_reason(format!("Task session '{id}' not found"))
}

fn stale_error(id: &str, expected: i64, current: Option<i64>) -> ConversationError {
    ConversationError::BoardItemStale {
        id: id.to_owned(),
        expected,
        current,
    }
}

fn map_db(id: &str, error: DbError) -> ConversationError {
    match error {
        DbError::NotFound(_) => task_not_found(id),
        DbError::Conflict(reason) => ConversationError::bad_request(reason),
        other => other.into(),
    }
}

fn validate_comment_body(body: &str) -> Result<&str, ConversationError> {
    let body = body.trim();
    if body.is_empty() {
        return Err(ConversationError::bad_request("Comment body must not be empty"));
    }
    if body.chars().count() > BOARD_MAX_COMMENT_CHARS {
        return Err(ConversationError::bad_request(format!(
            "Comment body must be at most {BOARD_MAX_COMMENT_CHARS} characters"
        )));
    }
    Ok(body)
}

fn validate_labels(labels: &[String]) -> Result<Vec<String>, ConversationError> {
    if labels.len() > BOARD_MAX_LABELS {
        return Err(ConversationError::bad_request(format!(
            "At most {BOARD_MAX_LABELS} labels"
        )));
    }
    let mut out: Vec<String> = Vec::with_capacity(labels.len());
    for label in labels {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > BOARD_MAX_LABEL_CHARS {
            return Err(ConversationError::bad_request(format!(
                "Labels must be 1 to {BOARD_MAX_LABEL_CHARS} characters"
            )));
        }
        if !out.iter().any(|existing| existing == label) {
            out.push(label.to_string());
        }
    }
    Ok(out)
}

/// M8 E7-6: lets board cards show the run state of the Squad instance
/// started for a work item. Implemented by the composition layer over the
/// team service (this crate does not depend on `aionui-team`).
#[async_trait::async_trait]
pub trait BoardSquadRunPort: Send + Sync {
    /// `None` when no instance is linked or it has nothing to show. Lookup
    /// failures are logged by the implementation and answer `None`.
    async fn squad_run_for_work_item(&self, user_id: &str, work_item_id: &str) -> Option<SquadRunPayload>;

    /// M8 E7-4: work item dragged back to backlog → pause the linked Squad.
    /// Default no-op so decoration-only test doubles stay unchanged.
    async fn pause_for_work_item(&self, _user_id: &str, _work_item_id: &str) {}
}

fn assigned_to_squad(item: &TaskBoardItemResponse) -> bool {
    item.assignee
        .as_ref()
        .is_some_and(|assignee| assignee.kind == TaskAssigneeKind::Squad)
}

fn truncate_summary(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut out: String = trimmed.chars().take(max_chars).collect();
    if trimmed.chars().count() > max_chars {
        out.push('…');
    }
    out
}

impl ConversationService {
    /// Fills `squad_run` on cards assigned to a Squad.
    async fn decorate_squad_run(&self, user_id: &str, item: &mut TaskBoardItemResponse) {
        if !assigned_to_squad(item) {
            return;
        }
        if let Some(port) = self.board_squad_run_port() {
            item.squad_run = port.squad_run_for_work_item(user_id, &item.id).await;
        }
    }

    /// M8 E7-4: best-effort pause of the Squad linked to this work item.
    async fn pause_squad_after_backlog(&self, user_id: &str, work_item_id: &str) {
        let Some(port) = self.board_squad_run_port() else {
            return;
        };
        port.pause_for_work_item(user_id, work_item_id).await;
    }

    /// M8 E7-6: pushes `task.board.updated` (`change: "squad_run"`) after the
    /// linked Squad instance changed state. `squad_run` is the fresh value
    /// computed by the caller. Skipped (returns `false`) when the item is gone
    /// or no longer assigned to a Squad.
    pub async fn publish_squad_run_changed(
        &self,
        user_id: &str,
        work_item_id: &str,
        squad_run: Option<SquadRunPayload>,
    ) -> Result<bool, ConversationError> {
        let Some(row) = self.task_board_repo()?.get_board_item(user_id, work_item_id).await? else {
            return Ok(false);
        };
        let mut item = item_response(row)?;
        if !assigned_to_squad(&item) {
            return Ok(false);
        }
        item.squad_run = squad_run;
        self.publish_board_updated(user_id, TaskBoardChange::SquadRun, item.board_status, &item);
        Ok(true)
    }

    async fn board_item(&self, user_id: &str, id: &str) -> Result<TaskBoardItemRow, ConversationError> {
        self.task_board_repo()?
            .get_board_item(user_id, id)
            .await?
            .ok_or_else(|| task_not_found(id))
    }

    async fn plan_approved(&self, user_id: &str, id: &str, mode: &str) -> Result<bool, ConversationError> {
        let approvals = self.task_session_repo()?.list_approvals(user_id, id).await?;
        Ok(approvals
            .iter()
            .any(|approval| approval.status == "approved" && approval.approval_type == mode))
    }

    async fn append_event(
        &self,
        user_id: &str,
        id: &str,
        kind: &str,
        body: serde_json::Value,
        now: i64,
    ) -> Result<TaskCommentRow, ConversationError> {
        let body = body.to_string();
        self.task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: id,
                author_kind: "user",
                author_id: user_id,
                kind,
                body: &body,
                created_at: now,
            })
            .await
            .map_err(|error| map_db(id, error))
            .inspect(|row| self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment_response(row.clone())))
    }

    pub(crate) fn publish_board_updated(
        &self,
        user_id: &str,
        change: TaskBoardChange,
        from: TaskBoardStatus,
        item: &TaskBoardItemResponse,
    ) {
        let event = TaskBoardUpdatedEvent {
            user_id: user_id.to_owned(),
            task_session_id: item.id.clone(),
            change,
            from_board_status: from,
            item: item.clone(),
        };
        match serde_json::to_value(event) {
            Ok(data) => self
                .broadcaster()
                .broadcast(WebSocketMessage::new(EVENT_TASK_BOARD_UPDATED, data)),
            Err(error) => tracing::warn!(%error, "failed to serialize task.board.updated"),
        }
    }

    pub(crate) fn publish_comment(&self, name: &str, user_id: &str, comment: TaskCommentResponse) {
        let event = TaskCommentEvent {
            user_id: user_id.to_owned(),
            task_session_id: comment.task_session_id.clone(),
            comment,
        };
        match serde_json::to_value(event) {
            Ok(data) => self.broadcaster().broadcast(WebSocketMessage::new(name, data)),
            Err(error) => tracing::warn!(%error, name, "failed to serialize task comment event"),
        }
    }

    /// `GET /api/task-sessions/board`.
    pub async fn task_board(
        &self,
        user_id: &str,
        query: TaskBoardQuery,
    ) -> Result<TaskBoardResponse, ConversationError> {
        let filter = TaskBoardFilter {
            project_id: query.project_id.as_deref(),
            board_status: None,
            assignee_kind: query.assignee_kind.map(TaskAssigneeKind::as_str),
            assignee_id: query.assignee_id.as_deref(),
            label: query.label.as_deref(),
        };
        let rows = self.task_board_repo()?.list_board(user_id, &filter).await?;
        let mut columns: Vec<TaskBoardColumnResponse> = TASK_BOARD_COLUMNS
            .into_iter()
            .map(|board_status| TaskBoardColumnResponse {
                board_status,
                items: Vec::new(),
            })
            .collect();
        for row in rows {
            let mut item = item_response(row)?;
            self.decorate_squad_run(user_id, &mut item).await;
            if let Some(column) = columns
                .iter_mut()
                .find(|column| column.board_status == item.board_status)
            {
                column.items.push(item);
            }
        }
        Ok(TaskBoardResponse { columns })
    }

    /// `PATCH /api/task-sessions/{id}/board`: move, rank, assign, prioritize,
    /// label. Column and assignee changes are recorded on the timeline.
    pub async fn update_task_board(
        &self,
        user_id: &str,
        id: &str,
        actor: BoardActor,
        request: UpdateTaskBoardRequest,
    ) -> Result<TaskBoardItemResponse, ConversationError> {
        let current = self.board_item(user_id, id).await?;
        if let Some(expected) = request.expected_updated_at
            && expected != current.updated_at
        {
            return Err(stale_error(id, expected, Some(current.updated_at)));
        }
        let from = TaskBoardStatus::parse(&current.board_status)
            .ok_or_else(|| ConversationError::internal("Unknown board_status"))?;
        if actor == BoardActor::Agent
            && (request.board_rank.is_some()
                || request.assignee.is_some()
                || request.priority.is_some()
                || request.labels.is_some())
        {
            return Err(rejection_error(
                from,
                request.board_status.unwrap_or(from),
                reject("agent_review_gate", "An agent may only submit an item for review"),
            ));
        }
        if let Some(to) = request.board_status {
            let plan_approved = if to == TaskBoardStatus::InProgress && from != to {
                self.plan_approved(user_id, id, &current.mode).await?
            } else {
                false
            };
            check_transition(
                from,
                to,
                actor,
                TransitionContext {
                    mode: &current.mode,
                    plan_approved,
                },
            )
            .map_err(|rejection| rejection_error(from, to, rejection))?;
        }
        if let Some(priority) = request.priority
            && !(0..=aionui_db::models::BOARD_MAX_PRIORITY).contains(&priority)
        {
            return Err(ConversationError::bad_request(format!(
                "priority must be between 0 and {}",
                aionui_db::models::BOARD_MAX_PRIORITY
            )));
        }
        if let Some(Some(rank)) = &request.board_rank
            && (rank.is_empty() || rank.chars().count() > BOARD_MAX_RANK_CHARS)
        {
            return Err(ConversationError::bad_request(format!(
                "board_rank must be 1 to {BOARD_MAX_RANK_CHARS} characters"
            )));
        }
        if let Some(Some(assignee)) = &request.assignee
            && assignee.id.trim().is_empty()
        {
            return Err(ConversationError::bad_request("assignee.id must not be empty"));
        }
        let labels = request.labels.as_deref().map(validate_labels).transpose()?;
        // With a lock the new value must differ from the guarded one, or two
        // writes in the same millisecond would both pass the next guard.
        let now = match request.expected_updated_at {
            Some(expected) => now_ms().max(expected + 1),
            None => now_ms(),
        };
        let params = UpdateTaskBoardParams {
            board_status: request.board_status.map(TaskBoardStatus::as_str),
            board_rank: request.board_rank.as_ref().map(|rank| rank.as_deref()),
            assignee: request.assignee.as_ref().map(|assignee| {
                assignee.as_ref().map(|assignee| BoardAssignee {
                    kind: assignee.kind.as_str(),
                    id: assignee.id.trim(),
                })
            }),
            priority: request.priority,
            labels: labels.as_deref(),
            updated_at: now,
            expected_updated_at: request.expected_updated_at,
        };
        let row = self
            .task_board_repo()?
            .update_board(user_id, id, &params)
            .await
            .map_err(|error| match (error, request.expected_updated_at) {
                (DbError::Conflict(message), Some(expected)) if message.starts_with(BOARD_STALE_CONFLICT_PREFIX) => {
                    stale_error(id, expected, None)
                }
                (error, _) => map_db(id, error),
            })?;
        if let Some(to) = request.board_status
            && to != from
        {
            self.append_event(
                user_id,
                id,
                "status_change",
                serde_json::json!({ "from": from.as_str(), "to": to.as_str() }),
                now,
            )
            .await?;
        }
        let mut assignee_changed_to_agent: Option<String> = None;
        if let Some(assignee) = &request.assignee {
            let previous = current.assignee_kind.as_deref().zip(current.assignee_id.as_deref());
            let next = assignee.as_ref().map(|value| (value.kind.as_str(), value.id.trim()));
            if previous != next {
                self.append_event(
                    user_id,
                    id,
                    "assignment",
                    serde_json::json!({ "assignee": assignee }),
                    now,
                )
                .await?;
                if let Some(value) = assignee.as_ref()
                    && value.kind == TaskAssigneeKind::Agent
                    && actor == BoardActor::User
                {
                    assignee_changed_to_agent = Some(value.id.trim().to_owned());
                }
            }
        }
        let mut item = item_response(row)?;
        // E6-5: assigning an Agent on `todo` auto-starts (backlog stays put).
        // Also covers backlog→todo+assign in one PATCH; skip when the card is
        // already past todo (e.g. same PATCH set in_progress).
        if let Some(agent_id) = assignee_changed_to_agent
            && item.board_status == TaskBoardStatus::Todo
        {
            item = self
                .auto_start_after_agent_assign(user_id, id, &agent_id, &current.mode, item)
                .await?;
        }
        // M8 E7-4: dragging an in-flight work item back to backlog pauses the
        // linked Squad instance (instantiate already rejects backlog).
        if request.board_status == Some(TaskBoardStatus::Backlog) && from != TaskBoardStatus::Backlog {
            self.pause_squad_after_backlog(user_id, id).await;
        }
        if let Some(to) = request.board_status
            && to != from
        {
            self.note_work_item_closed(user_id, id, to).await;
        }
        self.decorate_squad_run(user_id, &mut item).await;
        self.publish_board_updated(user_id, TaskBoardChange::Board, from, &item);
        Ok(item)
    }

    /// M8 Q12: a work item entering `done` / `cancelled` keeps its shared
    /// Squad worktree. The worktree is only marked (trace +
    /// `worktree.changed`) and a system comment tells the user where it is;
    /// nothing is removed. Best effort: never fails the board write.
    async fn note_work_item_closed(&self, user_id: &str, id: &str, to: TaskBoardStatus) {
        if !matches!(to, TaskBoardStatus::Done | TaskBoardStatus::Cancelled) {
            return;
        }
        let Some(binding) = self.worktree_binding_optional() else {
            return;
        };
        let kept = binding.on_work_item_closed(user_id, id, to.as_str()).await;
        if kept.is_empty() {
            return;
        }
        let label = if to == TaskBoardStatus::Done {
            "已完成"
        } else {
            "已取消"
        };
        let list = kept
            .iter()
            .map(|wt| format!("{}（分支 {}）", wt.path, wt.branch))
            .collect::<Vec<_>>()
            .join("；");
        let body = format!("工作项{label}；共享 worktree 已保留，未删除：{list}");
        let Ok(repo) = self.task_board_repo() else {
            return;
        };
        match repo
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: id,
                author_kind: "system",
                author_id: "worktree",
                kind: "comment",
                body: &body,
                created_at: now_ms(),
            })
            .await
        {
            Ok(row) => self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment_response(row)),
            Err(error) => tracing::warn!(task_session_id = %id, %error, "failed to record kept work item worktree"),
        }
    }

    /// `POST /api/task-sessions/{id}/review` (user credential only).
    pub async fn review_task_board(
        &self,
        user_id: &str,
        id: &str,
        actor: BoardActor,
        request: TaskBoardReviewRequest,
    ) -> Result<TaskBoardReviewResponse, ConversationError> {
        let current = self.board_item(user_id, id).await?;
        let from = TaskBoardStatus::parse(&current.board_status)
            .ok_or_else(|| ConversationError::internal("Unknown board_status"))?;
        let to = match request.decision {
            TaskBoardReviewDecision::Approve => TaskBoardStatus::Done,
            TaskBoardReviewDecision::Reject => TaskBoardStatus::Todo,
        };
        check_review(from, actor).map_err(|rejection| rejection_error(from, to, rejection))?;
        let comment = request
            .comment
            .as_deref()
            .map(str::trim)
            .filter(|comment| !comment.is_empty());
        if request.decision == TaskBoardReviewDecision::Reject && comment.is_none() {
            return Err(ConversationError::bad_request("Rejecting a review needs a comment"));
        }
        if let Some(comment) = comment {
            validate_comment_body(comment)?;
        }
        let now = now_ms();
        let row = self
            .task_board_repo()?
            .update_board(
                user_id,
                id,
                &UpdateTaskBoardParams {
                    board_status: Some(to.as_str()),
                    updated_at: now,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| map_db(id, error))?;
        let decision = self
            .append_event(
                user_id,
                id,
                "review_decision",
                serde_json::json!({ "decision": request.decision, "comment": comment }),
                now,
            )
            .await?;
        if to != from {
            self.note_work_item_closed(user_id, id, to).await;
        }
        if request.decision == TaskBoardReviewDecision::Reject {
            self.record_work_item_returned(user_id, id, &current.title, comment.unwrap_or_default())
                .await;
        }
        let mut item = item_response(row)?;
        self.decorate_squad_run(user_id, &mut item).await;
        self.publish_board_updated(user_id, TaskBoardChange::Review, from, &item);
        Ok(TaskBoardReviewResponse {
            item,
            decision: comment_response(decision),
        })
    }

    /// M8 E10-6: a work item sent back from review becomes skill-evolution
    /// evidence, linked to the work item's conversation. Best effort: a failed
    /// write is logged and never fails the review.
    async fn record_work_item_returned(&self, user_id: &str, id: &str, title: &str, comment: &str) {
        let Some(repo) = self.evolution_evidence_repo() else {
            return;
        };
        let session = match self.task_session_repo() {
            Ok(sessions) => sessions.get(user_id, id).await.ok().flatten(),
            Err(_) => None,
        };
        let mut body = format!("## 工作项被退回\n\n- 工作项: `{id}`（{title}）\n- 退回意见: {comment}\n");
        if let Some(session) = session.as_ref() {
            if !session.objective.trim().is_empty() {
                body.push_str(&format!("\n### 目标\n\n{}\n", session.objective.trim()));
            }
            if !session.acceptance_criteria.trim().is_empty() {
                body.push_str(&format!("\n### 验收标准\n\n{}\n", session.acceptance_criteria.trim()));
            }
        }
        let title = format!("工作项被退回 · {}", truncate_summary(title, 60));
        let evidence = aionui_db::EvolutionEvidence {
            owner_user_id: user_id,
            signal: aionui_db::EvolutionSignal::WorkItemReturned,
            ref_id: id,
            conversation_id: session.as_ref().and_then(|s| s.conversation_id.as_deref()),
            title: &title,
            body_md: &body,
        };
        if let Err(error) = aionui_db::record_evolution_evidence(repo.as_ref(), &evidence).await {
            tracing::warn!(task_session_id = %id, %error, "failed to record work item evolution evidence");
        }
    }

    /// M8 E6-5: `todo` + Agent assignee → `in_progress` and an agent
    /// 「已开工」assignment comment. Plan/goal still need an approved plan;
    /// without one the card stays on `todo` (assignment already recorded).
    async fn auto_start_after_agent_assign(
        &self,
        user_id: &str,
        id: &str,
        agent_id: &str,
        mode: &str,
        current: TaskBoardItemResponse,
    ) -> Result<TaskBoardItemResponse, ConversationError> {
        let plan_approved = self.plan_approved(user_id, id, mode).await?;
        if let Err(rejection) = check_transition(
            TaskBoardStatus::Todo,
            TaskBoardStatus::InProgress,
            BoardActor::User,
            TransitionContext { mode, plan_approved },
        ) {
            tracing::info!(
                task_session_id = %id,
                code = rejection.code,
                "E6-5 auto-start skipped"
            );
            return Ok(current);
        }
        let summary = match self.task_session_repo()?.get(user_id, id).await? {
            Some(session) if !session.objective.trim().is_empty() => truncate_summary(&session.objective, 80),
            _ => truncate_summary(&current.title, 80),
        };
        let now = now_ms().max(current.updated_at + 1);
        let row = self
            .task_board_repo()?
            .update_board(
                user_id,
                id,
                &UpdateTaskBoardParams {
                    board_status: Some(TaskBoardStatus::InProgress.as_str()),
                    updated_at: now,
                    ..Default::default()
                },
            )
            .await
            .map_err(|error| map_db(id, error))?;
        self.append_event(
            user_id,
            id,
            "status_change",
            serde_json::json!({
                "from": TaskBoardStatus::Todo.as_str(),
                "to": TaskBoardStatus::InProgress.as_str(),
            }),
            now,
        )
        .await?;
        // Design: kind=assignment, author_kind=agent, plain-text body 「已开工：…」
        let body = format!("已开工：{summary}");
        self.task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: id,
                author_kind: "agent",
                author_id: agent_id,
                kind: "assignment",
                body: &body,
                created_at: now + 1,
            })
            .await
            .map_err(|error| map_db(id, error))
            .inspect(|row| self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment_response(row.clone())))?;
        item_response(row)
    }

    /// M8 E6-5: when a run finishes `completed` with ≥1 artifact and the card
    /// is still `in_progress`, move it to `in_review` as the Agent actor.
    /// Returns `Ok(None)` when the card is missing, not in progress, or has no
    /// artifacts (board column unchanged). Failures/blocks stay on
    /// `in_progress` and rely on the existing Inbox sink.
    pub async fn maybe_submit_board_for_review_on_completion(
        &self,
        user_id: &str,
        task_session_id: &str,
    ) -> Result<Option<TaskBoardItemResponse>, ConversationError> {
        let Some(row) = self.task_board_repo()?.get_board_item(user_id, task_session_id).await? else {
            return Ok(None);
        };
        let from = TaskBoardStatus::parse(&row.board_status)
            .ok_or_else(|| ConversationError::internal("Unknown board_status"))?;
        if from != TaskBoardStatus::InProgress {
            return Ok(None);
        }
        let artifacts = self
            .task_session_repo()?
            .list_artifacts(user_id, task_session_id)
            .await?;
        if artifacts.is_empty() {
            tracing::info!(task_session_id, "E6-5 auto in_review skipped: no artifacts yet");
            return Ok(None);
        }
        let item = self
            .update_task_board(
                user_id,
                task_session_id,
                BoardActor::Agent,
                UpdateTaskBoardRequest {
                    board_status: Some(TaskBoardStatus::InReview),
                    ..Default::default()
                },
            )
            .await?;
        Ok(Some(item))
    }

    pub async fn list_task_comments(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Vec<TaskCommentResponse>, ConversationError> {
        self.board_item(user_id, id).await?;
        let rows = self.task_board_repo()?.list_comments(user_id, id).await?;
        Ok(rows.into_iter().map(comment_response).collect())
    }

    pub async fn create_task_comment(
        &self,
        user_id: &str,
        id: &str,
        body: &str,
    ) -> Result<TaskCommentResponse, ConversationError> {
        let body = validate_comment_body(body)?;
        let row = self
            .task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: id,
                author_kind: "user",
                author_id: user_id,
                kind: "comment",
                body,
                created_at: now_ms(),
            })
            .await
            .map_err(|error| map_db(id, error))?;
        let comment = comment_response(row);
        self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment.clone());
        Ok(comment)
    }

    /// M8 Q9 (D6-7): create a work item on behalf of an agent, after the user
    /// approved the request (the auth layer holds the request until then, so
    /// nothing is persisted before approval). The item always lands in
    /// `backlog`, unassigned and unbound, so no E7-4 trigger can fire from it
    /// (backlog never starts, no assignee). The board timeline records which
    /// conversation asked for it and that the user approved.
    pub async fn create_agent_backlog_work_item(
        &self,
        user_id: &str,
        agent_conversation_id: &str,
        request: aionui_api_types::CreateTaskSessionRequest,
    ) -> Result<aionui_api_types::TaskSessionResponse, ConversationError> {
        if request.conversation_id.is_some() || request.agent_session_id.is_some() {
            return Err(ConversationError::bad_request(
                "An agent-created work item cannot bind a conversation or agent session",
            ));
        }
        if request.status != aionui_api_types::TaskSessionStatus::Draft {
            return Err(ConversationError::bad_request(
                "An agent-created work item must start as draft",
            ));
        }
        let session = self.create_task_session(user_id, request).await?;
        let row = self.board_item(user_id, &session.id).await?;
        // Defensive: creation never writes these, but the Q9 guarantee is
        // "backlog + unassigned" regardless of future column defaults.
        if row.board_status != TaskBoardStatus::Backlog.as_str() || row.assignee_kind.is_some() {
            return Err(ConversationError::internal(
                "agent-created work item did not land in an unassigned backlog",
            ));
        }
        let body =
            format!("由 Agent（会话 {agent_conversation_id}）提议创建，已经用户审批，进入 Backlog；需人工排期与指派。");
        match self
            .task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: &session.id,
                author_kind: "system",
                author_id: "agent-approval",
                kind: "comment",
                body: &body,
                created_at: now_ms(),
            })
            .await
        {
            Ok(comment) => self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment_response(comment)),
            Err(error) => {
                tracing::warn!(task_session_id = %session.id, %error, "failed to record agent work item origin")
            }
        }
        let item = item_response(row)?;
        self.publish_board_updated(user_id, TaskBoardChange::Board, TaskBoardStatus::Backlog, &item);
        Ok(session)
    }

    /// M8 E7-5: Leader (or other agent) round evaluation as a timeline comment.
    /// `kind=comment`, `author_kind=agent`. Verifies the work item belongs to the user.
    pub async fn append_agent_board_comment(
        &self,
        user_id: &str,
        work_item_id: &str,
        author_agent_id: &str,
        body: &str,
    ) -> Result<TaskCommentResponse, ConversationError> {
        let body = body.trim();
        if body.is_empty() {
            return Err(ConversationError::bad_request("comment body required"));
        }
        let Some(_) = self.task_board_repo()?.get_board_item(user_id, work_item_id).await? else {
            return Err(ConversationError::not_found_reason("task board item not found"));
        };
        let now = now_ms();
        let row = self
            .task_board_repo()?
            .append_comment(&AppendTaskCommentParams {
                user_id,
                task_session_id: work_item_id,
                author_kind: "agent",
                author_id: author_agent_id,
                kind: "comment",
                body,
                created_at: now,
            })
            .await
            .map_err(|error| map_db(work_item_id, error))?;
        let comment = comment_response(row);
        self.publish_comment(EVENT_TASK_COMMENT_CREATED, user_id, comment.clone());
        Ok(comment)
    }

    /// Author-only body correction within the edit window; otherwise 409.
    pub async fn edit_task_comment(
        &self,
        user_id: &str,
        id: &str,
        comment_id: &str,
        body: &str,
    ) -> Result<TaskCommentResponse, ConversationError> {
        let body = validate_comment_body(body)?;
        let row = self
            .task_board_repo()?
            .edit_comment(user_id, id, comment_id, user_id, body, now_ms())
            .await
            .map_err(|error| match error {
                DbError::NotFound(_) => {
                    ConversationError::not_found_reason(format!("Comment '{comment_id}' not found"))
                }
                DbError::Conflict(reason) => ConversationError::Busy { reason },
                other => other.into(),
            })?;
        let comment = comment_response(row);
        self.publish_comment(EVENT_TASK_COMMENT_UPDATED, user_id, comment.clone());
        Ok(comment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskBoardStatus::*;

    const AGENT_MODE: TransitionContext<'static> = TransitionContext {
        mode: "agent",
        plan_approved: false,
    };

    fn user(from: TaskBoardStatus, to: TaskBoardStatus) -> Result<(), &'static str> {
        check_transition(from, to, BoardActor::User, AGENT_MODE).map_err(|rejection| rejection.code)
    }

    #[test]
    fn user_moves_follow_the_column_graph() {
        assert_eq!(user(Backlog, Todo), Ok(()));
        assert_eq!(user(Todo, InProgress), Ok(()));
        assert_eq!(user(InProgress, InReview), Ok(()));
        assert_eq!(user(InProgress, Todo), Ok(()));
        assert_eq!(user(Cancelled, Backlog), Ok(()));
        for from in TASK_BOARD_COLUMNS {
            if from != Done && from != Cancelled {
                assert_eq!(user(from, Cancelled), Ok(()), "{from:?}");
            }
        }
        assert_eq!(user(Done, Todo), Err("not_allowed"));
        assert_eq!(user(Cancelled, InProgress), Err("not_allowed"));
    }

    #[test]
    fn done_needs_review_and_backlog_never_starts() {
        for from in TASK_BOARD_COLUMNS {
            if from != Done {
                assert_eq!(user(from, Done), Err("review_required"), "{from:?}");
            }
        }
        assert_eq!(user(InReview, Todo), Err("review_required"));
        assert_eq!(user(Backlog, InProgress), Err("backlog_not_started"));
    }

    #[test]
    fn plan_items_need_approval_before_in_progress() {
        let unapproved = TransitionContext {
            mode: "plan",
            plan_approved: false,
        };
        let approved = TransitionContext {
            mode: "plan",
            plan_approved: true,
        };
        assert_eq!(
            check_transition(Todo, InProgress, BoardActor::User, unapproved)
                .unwrap_err()
                .code,
            "plan_not_approved"
        );
        let goal = TransitionContext {
            mode: "goal",
            plan_approved: false,
        };
        assert!(check_transition(Todo, InProgress, BoardActor::User, goal).is_err());
        assert!(check_transition(Todo, InProgress, BoardActor::User, approved).is_ok());
    }

    #[test]
    fn agent_may_only_submit_for_review() {
        assert!(check_transition(InProgress, InReview, BoardActor::Agent, AGENT_MODE).is_ok());
        for (from, to) in [
            (InReview, Done),
            (InProgress, Done),
            (Todo, InProgress),
            (Backlog, Todo),
        ] {
            assert_eq!(
                check_transition(from, to, BoardActor::Agent, AGENT_MODE)
                    .unwrap_err()
                    .code,
                "agent_review_gate"
            );
        }
        assert_eq!(
            check_review(InReview, BoardActor::Agent).unwrap_err().code,
            "agent_review_gate"
        );
        assert!(check_review(InReview, BoardActor::User).is_ok());
        assert_eq!(
            check_review(InProgress, BoardActor::User).unwrap_err().code,
            "not_in_review"
        );
    }

    #[test]
    fn labels_are_trimmed_deduplicated_and_bounded() {
        assert_eq!(
            validate_labels(&[" a ".into(), "a".into(), "b".into()]).unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        assert!(validate_labels(&["".into()]).is_err());
        assert!(validate_labels(&vec!["x".to_string(); BOARD_MAX_LABELS + 1]).is_err());
    }
}
