use aionui_common::TimestampMs;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::ApprovalRequestRow;
use crate::repository::approval::IApprovalRepository;

#[derive(Clone, Debug)]
pub struct SqliteApprovalRepository {
    pool: SqlitePool,
}

impl SqliteApprovalRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl IApprovalRepository for SqliteApprovalRepository {
    async fn create(&self, row: &ApprovalRequestRow) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO approval_requests (id, requester_user_id, project_id, run_id, task_id, conversation_id, \
             agent_id, call_id, action_type, command, working_directory, risk_level, options, status, \
             approver_user_id, source_channel, source_chat_id, source_thread_id, expires_at, consumed_at, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&row.id)
        .bind(&row.requester_user_id)
        .bind(&row.project_id)
        .bind(&row.run_id)
        .bind(&row.task_id)
        .bind(&row.conversation_id)
        .bind(&row.agent_id)
        .bind(&row.call_id)
        .bind(&row.action_type)
        .bind(&row.command)
        .bind(&row.working_directory)
        .bind(&row.risk_level)
        .bind(&row.options)
        .bind(&row.status)
        .bind(&row.approver_user_id)
        .bind(&row.source_channel)
        .bind(&row.source_chat_id)
        .bind(row.source_thread_id)
        .bind(row.expires_at)
        .bind(row.consumed_at)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<ApprovalRequestRow>, DbError> {
        Ok(sqlx::query_as("SELECT * FROM approval_requests WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn get_by_conversation_call(
        &self,
        conversation_id: &str,
        call_id: &str,
    ) -> Result<Option<ApprovalRequestRow>, DbError> {
        Ok(
            sqlx::query_as("SELECT * FROM approval_requests WHERE conversation_id = ? AND call_id = ?")
                .bind(conversation_id)
                .bind(call_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_for_user(
        &self,
        requester_user_id: &str,
        run_id: Option<&str>,
    ) -> Result<Vec<ApprovalRequestRow>, DbError> {
        let rows = match run_id {
            Some(run_id) => {
                sqlx::query_as(
                    "SELECT * FROM approval_requests WHERE requester_user_id = ? AND run_id = ? \
                 ORDER BY created_at DESC, id ASC",
                )
                .bind(requester_user_id)
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query_as(
                    "SELECT * FROM approval_requests WHERE requester_user_id = ? ORDER BY created_at DESC, id ASC",
                )
                .bind(requester_user_id)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows)
    }

    async fn consume(&self, id: &str, approver_user_id: &str, status: &str, now: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE approval_requests SET status = ?, approver_user_id = ?, consumed_at = ?, updated_at = ? \
             WHERE id = ? AND requester_user_id = ? AND status = 'pending' AND expires_at > ?",
        )
        .bind(status)
        .bind(approver_user_id)
        .bind(now)
        .bind(now)
        .bind(id)
        .bind(approver_user_id)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn cancel_consumed(&self, id: &str, approver_user_id: &str, now: TimestampMs) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE approval_requests SET status = 'cancelled', updated_at = ? \
             WHERE id = ? AND approver_user_id = ? AND status IN ('approved', 'rejected')",
        )
        .bind(now)
        .bind(id)
        .bind(approver_user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn mark_expired(&self, now: TimestampMs) -> Result<u64, DbError> {
        Ok(sqlx::query(
            "UPDATE approval_requests SET status = 'expired', updated_at = ? \
             WHERE status = 'pending' AND expires_at <= ?",
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?
        .rows_affected())
    }
}
