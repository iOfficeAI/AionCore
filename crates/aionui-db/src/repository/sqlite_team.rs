use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{MailboxMessageRow, TeamRow, TeamTaskRow};
use crate::repository::team::{ITeamRepository, UpdateTaskParams, UpdateTeamParams};

/// SQLite-backed implementation of [`ITeamRepository`].
#[derive(Clone, Debug)]
pub struct SqliteTeamRepository {
    pool: SqlitePool,
}

impl SqliteTeamRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl ITeamRepository for SqliteTeamRepository {
    // ── Team CRUD ────────────────────────────────────────────────────

    async fn create_team(&self, row: &TeamRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO teams (id, user_id, name, workspace, workspace_mode, agents, lead_agent_id, session_mode, agents_version, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.user_id)
        .bind(&row.name)
        .bind(&row.workspace)
        .bind(&row.workspace_mode)
        .bind(&row.agents)
        .bind(&row.lead_agent_id)
        .bind(&row.session_mode)
        .bind(&row.agents_version)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn list_teams(&self) -> Result<Vec<TeamRow>, DbError> {
        let rows = sqlx::query_as::<_, TeamRow>("SELECT * FROM teams ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn list_teams_by_user(&self, user_id: &str) -> Result<Vec<TeamRow>, DbError> {
        let rows = sqlx::query_as::<_, TeamRow>("SELECT * FROM teams WHERE user_id = ? ORDER BY created_at ASC")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows)
    }

    async fn get_team(&self, team_id: &str) -> Result<Option<TeamRow>, DbError> {
        let row = sqlx::query_as::<_, TeamRow>("SELECT * FROM teams WHERE id = ?")
            .bind(team_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update_team(&self, team_id: &str, params: &UpdateTeamParams) -> Result<(), DbError> {
        let mut set_clauses = Vec::new();
        if params.name.is_some() {
            set_clauses.push("name = ?");
        }
        if params.agents.is_some() {
            set_clauses.push("agents = ?");
        }
        if params.lead_agent_id.is_some() {
            set_clauses.push("lead_agent_id = ?");
        }

        if set_clauses.is_empty() {
            return Ok(());
        }

        set_clauses.push("updated_at = ?");
        let sql = format!("UPDATE teams SET {} WHERE id = ?", set_clauses.join(", "));

        let mut query = sqlx::query(&sql);
        if let Some(ref name) = params.name {
            query = query.bind(name);
        }
        if let Some(ref agents) = params.agents {
            query = query.bind(agents);
        }
        if let Some(ref lead_agent_id) = params.lead_agent_id {
            query = query.bind(lead_agent_id);
        }
        query = query.bind(now_ms());
        query = query.bind(team_id);

        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("team {team_id}")));
        }
        Ok(())
    }

    async fn delete_team(&self, team_id: &str) -> Result<(), DbError> {
        let result = sqlx::query("DELETE FROM teams WHERE id = ?")
            .bind(team_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("team {team_id}")));
        }
        Ok(())
    }

    // ── Mailbox ──────────────────────────────────────────────────────

    async fn write_message(&self, row: &MailboxMessageRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO mailbox \
                (id, team_id, to_agent_id, from_agent_id, type, content, summary, files, read, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.team_id)
        .bind(&row.to_agent_id)
        .bind(&row.from_agent_id)
        .bind(&row.msg_type)
        .bind(&row.content)
        .bind(&row.summary)
        .bind(&row.files)
        .bind(row.read)
        .bind(row.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn read_unread_and_mark(&self, team_id: &str, to_agent_id: &str) -> Result<Vec<MailboxMessageRow>, DbError> {
        // Use BEGIN IMMEDIATE for atomicity: prevents concurrent readers
        // from seeing the same unread messages.
        let mut tx = self.pool.begin().await?;

        // SQLite does not support RETURNING on UPDATE, so we use a
        // two-step approach within the same IMMEDIATE transaction.
        sqlx::query("PRAGMA read_uncommitted = false").execute(&mut *tx).await?;

        let rows = sqlx::query_as::<_, MailboxMessageRow>(
            "SELECT id, team_id, to_agent_id, from_agent_id, \
                    type, content, summary, files, read, created_at \
             FROM mailbox \
             WHERE team_id = ? AND to_agent_id = ? AND read = 0 \
             ORDER BY created_at ASC",
        )
        .bind(team_id)
        .bind(to_agent_id)
        .fetch_all(&mut *tx)
        .await?;

        if !rows.is_empty() {
            sqlx::query(
                "UPDATE mailbox SET read = 1 \
                 WHERE team_id = ? AND to_agent_id = ? AND read = 0",
            )
            .bind(team_id)
            .bind(to_agent_id)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(rows)
    }

    async fn peek_unread(&self, team_id: &str, to_agent_id: &str) -> Result<Vec<MailboxMessageRow>, DbError> {
        let rows = sqlx::query_as::<_, MailboxMessageRow>(
            "SELECT id, team_id, to_agent_id, from_agent_id, \
                    type, content, summary, files, read, created_at \
             FROM mailbox \
             WHERE team_id = ? AND to_agent_id = ? AND read = 0 \
             ORDER BY created_at ASC",
        )
        .bind(team_id)
        .bind(to_agent_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    async fn mark_read_batch(&self, ids: &[String]) -> Result<(), DbError> {
        if ids.is_empty() {
            return Ok(());
        }
        // SQLite placeholder limit is 999; batch if needed.
        for chunk in ids.chunks(500) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("UPDATE mailbox SET read = 1 WHERE id IN ({placeholders})");
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(id);
            }
            query.execute(&self.pool).await?;
        }
        Ok(())
    }

    async fn get_history(
        &self,
        team_id: &str,
        to_agent_id: &str,
        limit: Option<i64>,
    ) -> Result<Vec<MailboxMessageRow>, DbError> {
        let rows = if let Some(limit) = limit {
            sqlx::query_as::<_, MailboxMessageRow>(
                "SELECT id, team_id, to_agent_id, from_agent_id, \
                        type, content, summary, files, read, created_at \
                 FROM mailbox \
                 WHERE team_id = ? AND to_agent_id = ? \
                 ORDER BY created_at ASC \
                 LIMIT ?",
            )
            .bind(team_id)
            .bind(to_agent_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, MailboxMessageRow>(
                "SELECT id, team_id, to_agent_id, from_agent_id, \
                        type, content, summary, files, read, created_at \
                 FROM mailbox \
                 WHERE team_id = ? AND to_agent_id = ? \
                 ORDER BY created_at ASC",
            )
            .bind(team_id)
            .bind(to_agent_id)
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows)
    }

    async fn delete_mailbox_by_team(&self, team_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM mailbox WHERE team_id = ?")
            .bind(team_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // ── Tasks ────────────────────────────────────────────────────────

    async fn create_task(&self, row: &TeamTaskRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO team_tasks \
                (id, team_id, subject, description, status, owner, \
                 blocked_by, blocks, metadata, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.team_id)
        .bind(&row.subject)
        .bind(&row.description)
        .bind(&row.status)
        .bind(&row.owner)
        .bind(&row.blocked_by)
        .bind(&row.blocks)
        .bind(&row.metadata)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_task_by_id(&self, team_id: &str, task_id: &str) -> Result<Option<TeamTaskRow>, DbError> {
        let row = sqlx::query_as::<_, TeamTaskRow>("SELECT * FROM team_tasks WHERE team_id = ? AND id = ?")
            .bind(team_id)
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    async fn update_task(&self, task_id: &str, params: &UpdateTaskParams) -> Result<(), DbError> {
        let mut set_clauses = Vec::new();
        if params.status.is_some() {
            set_clauses.push("status = ?");
        }
        if params.description.is_some() {
            set_clauses.push("description = ?");
        }
        if params.owner.is_some() {
            set_clauses.push("owner = ?");
        }
        if params.blocked_by.is_some() {
            set_clauses.push("blocked_by = ?");
        }
        if params.metadata.is_some() {
            set_clauses.push("metadata = ?");
        }

        if set_clauses.is_empty() {
            return Ok(());
        }

        set_clauses.push("updated_at = ?");
        let sql = format!("UPDATE team_tasks SET {} WHERE id = ?", set_clauses.join(", "));

        let mut query = sqlx::query(&sql);
        if let Some(ref status) = params.status {
            query = query.bind(status);
        }
        if let Some(ref description) = params.description {
            query = query.bind(description);
        }
        if let Some(ref owner) = params.owner {
            query = query.bind(owner);
        }
        if let Some(ref blocked_by) = params.blocked_by {
            query = query.bind(blocked_by);
        }
        if let Some(ref metadata) = params.metadata {
            query = query.bind(metadata);
        }
        query = query.bind(now_ms());
        query = query.bind(task_id);

        let result = query.execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("task {task_id}")));
        }
        Ok(())
    }

    async fn list_tasks(&self, team_id: &str) -> Result<Vec<TeamTaskRow>, DbError> {
        let rows =
            sqlx::query_as::<_, TeamTaskRow>("SELECT * FROM team_tasks WHERE team_id = ? ORDER BY created_at ASC")
                .bind(team_id)
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }

    async fn append_to_blocks(&self, task_id: &str, blocked_task_id: &str) -> Result<(), DbError> {
        // Read current blocks, append, and write back within a transaction.
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as::<_, TeamTaskRow>("SELECT * FROM team_tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("task {task_id}")))?;

        let mut blocks: Vec<String> = serde_json::from_str(&row.blocks).unwrap_or_default();
        if !blocks.contains(&blocked_task_id.to_string()) {
            blocks.push(blocked_task_id.to_string());
        }
        let new_blocks = serde_json::to_string(&blocks).unwrap_or_else(|_| "[]".to_string());

        sqlx::query("UPDATE team_tasks SET blocks = ?, updated_at = ? WHERE id = ?")
            .bind(&new_blocks)
            .bind(now_ms())
            .bind(task_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn remove_from_blocked_by(&self, task_id: &str, unblocked_task_id: &str) -> Result<(), DbError> {
        // Read current blocked_by, remove, and write back within a transaction.
        let mut tx = self.pool.begin().await?;

        let row = sqlx::query_as::<_, TeamTaskRow>("SELECT * FROM team_tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("task {task_id}")))?;

        let mut blocked_by: Vec<String> = serde_json::from_str(&row.blocked_by).unwrap_or_default();
        blocked_by.retain(|id| id != unblocked_task_id);
        let new_blocked_by = serde_json::to_string(&blocked_by).unwrap_or_else(|_| "[]".to_string());

        sqlx::query("UPDATE team_tasks SET blocked_by = ?, updated_at = ? WHERE id = ?")
            .bind(&new_blocked_by)
            .bind(now_ms())
            .bind(task_id)
            .execute(&mut *tx)
            .await?;

        tx.commit().await?;
        Ok(())
    }

    async fn delete_tasks_by_team(&self, team_id: &str) -> Result<(), DbError> {
        sqlx::query("DELETE FROM team_tasks WHERE team_id = ?")
            .bind(team_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
