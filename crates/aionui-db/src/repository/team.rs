use crate::error::DbError;
use crate::models::{MailboxMessageRow, TeamRow, TeamTaskRow};

/// Parameters for updating a team record.
#[derive(Debug, Clone, Default)]
pub struct UpdateTeamParams {
    pub name: Option<String>,
    pub agents: Option<String>,
    pub lead_agent_id: Option<String>,
}

/// Parameters for updating a task record.
#[derive(Debug, Clone, Default)]
pub struct UpdateTaskParams {
    pub status: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub blocked_by: Option<String>,
    pub metadata: Option<String>,
}

/// Data access abstraction for team collaboration tables.
///
/// Covers three tables: `teams`, `mailbox`, and `team_tasks`.
///
/// Object-safe via `async_trait` to support `Arc<dyn ITeamRepository>`.
#[async_trait::async_trait]
pub trait ITeamRepository: Send + Sync {
    // ── Team CRUD ────────────────────────────────────────────────────

    /// Inserts a new team record.
    async fn create_team(&self, row: &TeamRow) -> Result<(), DbError>;

    /// Returns all teams ordered by creation time ascending.
    async fn list_teams(&self) -> Result<Vec<TeamRow>, DbError>;

    /// Returns teams owned by `user_id`, ordered by creation time ascending.
    async fn list_teams_by_user(&self, user_id: &str) -> Result<Vec<TeamRow>, DbError>;

    /// Returns a single team by id, or `None` if not found.
    async fn get_team(&self, team_id: &str) -> Result<Option<TeamRow>, DbError>;

    /// Updates a team by id with the provided fields.
    /// Returns `DbError::NotFound` if absent.
    async fn update_team(&self, team_id: &str, params: &UpdateTeamParams) -> Result<(), DbError>;

    /// Deletes a team by id. Returns `DbError::NotFound` if absent.
    async fn delete_team(&self, team_id: &str) -> Result<(), DbError>;

    // ── Mailbox ──────────────────────────────────────────────────────

    /// Writes a message to the mailbox.
    async fn write_message(&self, row: &MailboxMessageRow) -> Result<(), DbError>;

    /// Atomically reads all unread messages for `to_agent_id` in a team
    /// and marks them as read. Uses `BEGIN IMMEDIATE` for atomicity.
    async fn read_unread_and_mark(&self, team_id: &str, to_agent_id: &str) -> Result<Vec<MailboxMessageRow>, DbError>;

    /// Reads all unread messages for `to_agent_id` without marking them as read.
    async fn peek_unread(&self, team_id: &str, to_agent_id: &str) -> Result<Vec<MailboxMessageRow>, DbError>;

    /// Marks the given message IDs as read. IDs that don't exist are silently ignored.
    async fn mark_read_batch(&self, ids: &[String]) -> Result<(), DbError>;

    /// Returns message history for an agent, optionally limited.
    /// Messages are ordered by `created_at` ascending.
    async fn get_history(
        &self,
        team_id: &str,
        to_agent_id: &str,
        limit: Option<i64>,
    ) -> Result<Vec<MailboxMessageRow>, DbError>;

    /// Deletes all mailbox messages belonging to a team.
    async fn delete_mailbox_by_team(&self, team_id: &str) -> Result<(), DbError>;

    // ── Tasks ────────────────────────────────────────────────────────

    /// Creates a new task.
    async fn create_task(&self, row: &TeamTaskRow) -> Result<(), DbError>;

    /// Finds a task by exact id within a team.
    async fn find_task_by_id(&self, team_id: &str, task_id: &str) -> Result<Option<TeamTaskRow>, DbError>;

    /// Updates a task by id with the provided fields.
    /// Returns `DbError::NotFound` if absent.
    async fn update_task(&self, task_id: &str, params: &UpdateTaskParams) -> Result<(), DbError>;

    /// Returns all tasks for a team, ordered by `created_at` ascending.
    async fn list_tasks(&self, team_id: &str) -> Result<Vec<TeamTaskRow>, DbError>;

    /// Appends `blocked_task_id` to the `blocks` JSON array of `task_id`.
    /// This is a transactional JSON array append operation.
    async fn append_to_blocks(&self, task_id: &str, blocked_task_id: &str) -> Result<(), DbError>;

    /// Removes `unblocked_task_id` from the `blocked_by` JSON array of `task_id`.
    /// This is a transactional JSON array removal operation.
    async fn remove_from_blocked_by(&self, task_id: &str, unblocked_task_id: &str) -> Result<(), DbError>;

    /// Deletes all tasks belonging to a team.
    async fn delete_tasks_by_team(&self, team_id: &str) -> Result<(), DbError>;
}
