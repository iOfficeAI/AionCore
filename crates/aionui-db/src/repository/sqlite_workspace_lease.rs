use aionui_common::now_ms;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::AgentWorkspaceLeaseRow;
use crate::repository::workspace_lease::{AgentWorkspaceLeaseUpdate, IAgentWorkspaceLeaseRepository};

#[derive(Clone, Debug)]
pub struct SqliteAgentWorkspaceLeaseRepository {
    pool: SqlitePool,
}

impl SqliteAgentWorkspaceLeaseRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn map_write_error(error: sqlx::Error) -> DbError {
    if error
        .as_database_error()
        .and_then(|db| db.code())
        .is_some_and(|code| code == "2067")
    {
        DbError::Conflict("workspace lease already exists for this slot, path, or branch".into())
    } else {
        DbError::Query(error)
    }
}

#[async_trait::async_trait]
impl IAgentWorkspaceLeaseRepository for SqliteAgentWorkspaceLeaseRepository {
    async fn create(&self, row: &AgentWorkspaceLeaseRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO agent_workspace_leases \
             (id, team_id, user_id, slot_id, workspace_mode, repository_path, worktree_path, branch_name, \
              base_commit, allowed_paths, lease_status, cleanup_status, conflict_files, last_error, \
              created_at, updated_at, released_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.team_id)
        .bind(&row.user_id)
        .bind(&row.slot_id)
        .bind(&row.workspace_mode)
        .bind(&row.repository_path)
        .bind(&row.worktree_path)
        .bind(&row.branch_name)
        .bind(&row.base_commit)
        .bind(&row.allowed_paths)
        .bind(&row.lease_status)
        .bind(&row.cleanup_status)
        .bind(&row.conflict_files)
        .bind(&row.last_error)
        .bind(row.created_at)
        .bind(row.updated_at)
        .bind(row.released_at)
        .execute(&self.pool)
        .await
        .map_err(map_write_error)?;
        Ok(())
    }

    async fn get(&self, lease_id: &str) -> Result<Option<AgentWorkspaceLeaseRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM agent_workspace_leases WHERE id = ?")
            .bind(lease_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn get_for_team_slot(&self, team_id: &str, slot_id: &str) -> Result<Option<AgentWorkspaceLeaseRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM agent_workspace_leases WHERE team_id = ? AND slot_id = ?")
                .bind(team_id)
                .bind(slot_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_for_team(&self, team_id: &str) -> Result<Vec<AgentWorkspaceLeaseRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM agent_workspace_leases WHERE team_id = ? ORDER BY slot_id ASC, created_at ASC",
        )
        .bind(team_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn list_reconcile_candidates(&self) -> Result<Vec<AgentWorkspaceLeaseRow>, DbError> {
        Ok(sqlx::query_as(
            "SELECT * FROM agent_workspace_leases \
             WHERE lease_status IN ('provisioning', 'active', 'cleanup_pending', 'conflict') \
             ORDER BY team_id ASC, slot_id ASC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    async fn update(&self, lease_id: &str, update: &AgentWorkspaceLeaseUpdate) -> Result<(), DbError> {
        let existing = self
            .get(lease_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("workspace lease {lease_id}")))?;
        let lease_status = update.lease_status.as_ref().unwrap_or(&existing.lease_status);
        let cleanup_status = update.cleanup_status.as_ref().unwrap_or(&existing.cleanup_status);
        let conflict_files = update.conflict_files.as_ref().unwrap_or(&existing.conflict_files);
        let last_error = update.last_error.clone().unwrap_or(existing.last_error);
        let released_at = update.released_at.unwrap_or(existing.released_at);
        sqlx::query(
            "UPDATE agent_workspace_leases SET lease_status = ?, cleanup_status = ?, conflict_files = ?, \
             last_error = ?, released_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(lease_status)
        .bind(cleanup_status)
        .bind(conflict_files)
        .bind(last_error)
        .bind(released_at)
        .bind(now_ms())
        .bind(lease_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
