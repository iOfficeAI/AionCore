use aionui_common::TimestampMs;
use sqlx::{SqliteConnection, SqlitePool};

struct InsertEntryOptions<'a> {
    state: &'a str,
    supersedes_id: Option<&'a str>,
    conflict_group_id: Option<&'a str>,
}

use crate::DbError;
use crate::models::{
    ConversationMemoryRow, EffectiveMemoryPolicyRow, MemoryChangeSetRow, MemoryEntryDbRow, MemoryEntryRow,
    MemoryImportStateRow, MemoryJobRow, MemoryRetrievalRow, MemorySettingsRow, MemorySourceRow,
};
use crate::repository::memory::{
    ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow,
    CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow, IMemoryRepository, MemoryCandidateQueryRow,
    MemoryEntryQueryRow, ReleaseMemoryLeaseRow, RenewMemoryLeaseRow, SplitMemoryJobRow, TransitionMemoryJobRow,
    UpdateConversationMemoryPolicyRow, UpdateMemoryEntryRow, UpdateMemorySettingsRow,
};

const MAX_MEMORY_CANDIDATES: u32 = 200;

#[derive(Clone, Debug)]
pub struct SqliteMemoryRepository {
    pool: SqlitePool,
}

impl SqliteMemoryRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn ensure_user(&self, user_id: &str) -> Result<(), DbError> {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = ?)")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        if exists {
            Ok(())
        } else {
            Err(DbError::NotFound(format!("User '{user_id}' not found")))
        }
    }

    async fn ensure_conversation(&self, user_id: &str, conversation_id: &str) -> Result<(), DbError> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ? AND user_id = ?)")
                .bind(conversation_id)
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        if exists {
            Ok(())
        } else {
            Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found for user"
            )))
        }
    }

    async fn ensure_conversation_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<(), DbError> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM conversations WHERE id = ? AND user_id = ?)")
                .bind(conversation_id)
                .bind(user_id)
                .fetch_one(&mut *connection)
                .await?;
        if exists {
            Ok(())
        } else {
            Err(DbError::NotFound(format!(
                "Conversation '{conversation_id}' not found for user"
            )))
        }
    }

    async fn entry_with_sources(&self, row: MemoryEntryDbRow) -> Result<MemoryEntryRow, DbError> {
        let sources = sqlx::query_as::<_, MemorySourceRow>(
            "SELECT * FROM memory_sources WHERE memory_entry_id = ? ORDER BY first_observed_at, conversation_id, turn_id",
        )
        .bind(&row.id)
        .fetch_all(&self.pool)
        .await?;
        Ok(row.with_sources(sources))
    }

    async fn entry_with_sources_on(
        connection: &mut SqliteConnection,
        row: MemoryEntryDbRow,
    ) -> Result<MemoryEntryRow, DbError> {
        let sources = sqlx::query_as::<_, MemorySourceRow>(
            "SELECT * FROM memory_sources WHERE memory_entry_id = ? ORDER BY first_observed_at, conversation_id, turn_id",
        )
        .bind(&row.id)
        .fetch_all(&mut *connection)
        .await?;
        Ok(row.with_sources(sources))
    }

    async fn entry_rows_with_sources(&self, rows: Vec<MemoryEntryDbRow>) -> Result<Vec<MemoryEntryRow>, DbError> {
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(self.entry_with_sources(row).await?);
        }
        Ok(entries)
    }

    async fn entry_target_protection_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        entry_id: &str,
    ) -> Result<(bool, bool), DbError> {
        let protection: Option<(bool, bool)> = sqlx::query_as(
            "SELECT pinned, user_edited FROM memory_entries
             WHERE id = ? AND user_id = ? AND state <> 'deleted'",
        )
        .bind(entry_id)
        .bind(user_id)
        .fetch_optional(&mut *connection)
        .await?;
        protection.ok_or_else(|| DbError::NotFound(format!("Memory entry '{entry_id}' not found")))
    }

    async fn ensure_automatic_target_mutable_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        entry_id: &str,
    ) -> Result<(), DbError> {
        let (pinned, user_edited) = Self::entry_target_protection_on(connection, user_id, entry_id).await?;
        if pinned || user_edited {
            Err(DbError::Conflict(format!(
                "Protected Memory entry '{entry_id}' cannot be changed automatically"
            )))
        } else {
            Ok(())
        }
    }

    async fn validate_sources_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        sources: &[CommitMemorySourceRow],
    ) -> Result<(), DbError> {
        for source in sources {
            Self::ensure_conversation_on(connection, user_id, &source.conversation_id).await?;
            let turn_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(
                    SELECT 1 FROM messages
                    WHERE conversation_id = ? AND turn_id = ?
                )",
            )
            .bind(&source.conversation_id)
            .bind(&source.turn_id)
            .fetch_one(&mut *connection)
            .await?;
            if !turn_exists {
                return Err(DbError::NotFound(format!(
                    "Memory source turn '{}' not found",
                    source.turn_id
                )));
            }
            let message_ids: Vec<String> = serde_json::from_str(&source.message_ids_json)
                .map_err(|error| DbError::Conflict(format!("Invalid Memory source message IDs: {error}")))?;
            if message_ids.is_empty() {
                return Err(DbError::Conflict(
                    "Memory source must reference at least one message".into(),
                ));
            }
            for message_id in message_ids {
                let message_exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS(
                        SELECT 1 FROM messages
                        WHERE id = ? AND conversation_id = ? AND turn_id = ?
                    )",
                )
                .bind(&message_id)
                .bind(&source.conversation_id)
                .bind(&source.turn_id)
                .fetch_one(&mut *connection)
                .await?;
                if !message_exists {
                    return Err(DbError::NotFound(format!(
                        "Memory source message '{message_id}' not found"
                    )));
                }
            }
        }
        Ok(())
    }

    async fn insert_entry_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        entry: &CommitMemoryEntryRow,
        options: InsertEntryOptions<'_>,
        schema_version: i64,
        now: i64,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, project_id, workspace_key, kind, stable_key, fingerprint, content, state,
                 pinned, user_edited, supersedes_id, conflict_group_id, schema_version, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, ?, ?, ?, ?, ?)",
        )
        .bind(&entry.id)
        .bind(user_id)
        .bind(&entry.project_id)
        .bind(&entry.workspace_key)
        .bind(&entry.kind)
        .bind(&entry.stable_key)
        .bind(&entry.fingerprint)
        .bind(&entry.content)
        .bind(options.state)
        .bind(options.supersedes_id)
        .bind(options.conflict_group_id)
        .bind(schema_version)
        .bind(now)
        .bind(now)
        .execute(&mut *connection)
        .await?;
        Ok(())
    }

    async fn upsert_sources_on(
        connection: &mut SqliteConnection,
        entry_id: &str,
        sources: &[CommitMemorySourceRow],
        now: i64,
    ) -> Result<(), DbError> {
        for source in sources {
            sqlx::query(
                "INSERT INTO memory_sources
                    (memory_entry_id, conversation_id, turn_id, message_ids_json, first_observed_at, last_observed_at)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT(memory_entry_id, conversation_id, turn_id) DO UPDATE SET
                    message_ids_json = excluded.message_ids_json,
                    last_observed_at = excluded.last_observed_at",
            )
            .bind(entry_id)
            .bind(&source.conversation_id)
            .bind(&source.turn_id)
            .bind(&source.message_ids_json)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
        }
        Ok(())
    }

    #[cfg(test)]
    async fn count_jobs(&self, user_id: &str, conversation_id: &str, state: &str) -> Result<i64, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_jobs WHERE user_id = ? AND conversation_id = ? AND state = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .bind(state)
        .fetch_one(&self.pool)
        .await?)
    }
}

#[async_trait::async_trait]
impl IMemoryRepository for SqliteMemoryRepository {
    async fn get_settings(&self, user_id: &str) -> Result<MemorySettingsRow, DbError> {
        self.ensure_user(user_id).await?;
        sqlx::query("INSERT INTO memory_settings (user_id, updated_at) VALUES (?, 0) ON CONFLICT(user_id) DO NOTHING")
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(sqlx::query_as("SELECT * FROM memory_settings WHERE user_id = ?")
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn update_settings(&self, command: UpdateMemorySettingsRow) -> Result<MemorySettingsRow, DbError> {
        self.get_settings(&command.user_id).await?;
        sqlx::query(
            "UPDATE memory_settings SET
                enabled = COALESCE(?, enabled),
                default_capture = COALESCE(?, default_capture),
                default_recall = COALESCE(?, default_recall),
                consent_version = COALESCE(?, consent_version),
                consented_at = CASE WHEN ? IS NULL THEN consented_at ELSE ? END,
                updated_at = ?
             WHERE user_id = ?",
        )
        .bind(command.enabled)
        .bind(command.default_capture)
        .bind(command.default_recall)
        .bind(command.consent_version)
        .bind(command.consent_version)
        .bind(command.now)
        .bind(command.now)
        .bind(&command.user_id)
        .execute(&self.pool)
        .await?;
        self.get_settings(&command.user_id).await
    }

    async fn effective_policy(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<EffectiveMemoryPolicyRow, DbError> {
        self.ensure_conversation(user_id, conversation_id).await?;
        let settings = self.get_settings(user_id).await?;
        let policy: Option<(Option<bool>, Option<bool>, Option<i64>)> = sqlx::query_as(
            "SELECT capture_enabled, recall_enabled, reset_at
             FROM conversation_memory_policies WHERE user_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        let (capture_override, recall_override, conversation_reset) = policy.unwrap_or((None, None, None));
        Ok(EffectiveMemoryPolicyRow {
            user_id: user_id.into(),
            conversation_id: conversation_id.into(),
            enabled: settings.enabled,
            capture_enabled: capture_override.unwrap_or(settings.default_capture),
            recall_enabled: recall_override.unwrap_or(settings.default_recall),
            capture_override,
            recall_override,
            consent_version: settings.consent_version,
            consented_at: settings.consented_at,
            reset_at: match (settings.reset_at, conversation_reset) {
                (Some(global), Some(conversation)) => Some(global.max(conversation)),
                (global, conversation) => global.or(conversation),
            },
        })
    }

    async fn update_conversation_policy(
        &self,
        command: UpdateConversationMemoryPolicyRow,
    ) -> Result<EffectiveMemoryPolicyRow, DbError> {
        self.ensure_conversation(&command.user_id, &command.conversation_id)
            .await?;
        sqlx::query(
            "INSERT INTO conversation_memory_policies
                (user_id, conversation_id, capture_enabled, recall_enabled, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(user_id, conversation_id) DO UPDATE SET
                capture_enabled = excluded.capture_enabled,
                recall_enabled = excluded.recall_enabled,
                updated_at = excluded.updated_at",
        )
        .bind(&command.user_id)
        .bind(&command.conversation_id)
        .bind(command.capture_enabled)
        .bind(command.recall_enabled)
        .bind(command.now)
        .execute(&self.pool)
        .await?;
        self.effective_policy(&command.user_id, &command.conversation_id).await
    }

    async fn enqueue_completed_turn(&self, input: EnqueueMemoryTurnRow) -> Result<Option<MemoryJobRow>, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            Self::ensure_conversation_on(&mut connection, &input.user_id, &input.conversation_id).await?;
            let queued_turn_ids: Vec<String> = serde_json::from_str(&input.turn_ids_json)
                .map_err(|_| DbError::Conflict("Invalid Memory job turn queue".into()))?;
            if queued_turn_ids.as_slice() != [input.through_turn_id.as_str()] {
                return Err(DbError::Conflict("Enqueue must contain exactly its completed turn".into()));
            }
            let duplicate: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM memory_jobs jobs, json_each(jobs.turn_ids_json) turns
                 WHERE jobs.user_id = ? AND jobs.conversation_id = ? AND jobs.operation_version = ?
                   AND turns.value = ?)",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&input.operation_version)
            .bind(&input.through_turn_id)
            .fetch_one(&mut *connection)
            .await?;
            if duplicate {
                return Ok(None);
            }

            let pending = sqlx::query_as::<_, MemoryJobRow>(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state IN ('pending', 'retry_wait', 'blocked') LIMIT 1",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            if let Some(pending) = pending {
                sqlx::query(
                    "UPDATE memory_jobs SET turn_ids_json = json_insert(turn_ids_json, '$[#]', ?),
                     through_turn_id = ?, operation_version = ?, input_hash = ?,
                     expected_revision = ?, state = 'pending', next_attempt_at = NULL, last_error_code = NULL, updated_at = ?
                     WHERE id = ? AND user_id = ?",
                )
                .bind(&input.through_turn_id)
                .bind(&input.through_turn_id)
                .bind(&input.operation_version)
                .bind(&input.input_hash)
                .bind(input.expected_revision)
                .bind(input.now)
                .bind(&pending.id)
                .bind(&input.user_id)
                .execute(&mut *connection)
                .await?;
                return Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                    .bind(&pending.id)
                    .fetch_optional(&mut *connection)
                    .await?);
            }

            sqlx::query(
                "INSERT INTO memory_jobs
                    (id, user_id, conversation_id, from_turn_id, turn_ids_json, through_turn_id, operation_version, input_hash,
                     expected_revision, state, attempt_count, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', 0, ?, ?)",
            )
            .bind(&input.id)
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&input.from_turn_id)
            .bind(&input.turn_ids_json)
            .bind(&input.through_turn_id)
            .bind(&input.operation_version)
            .bind(&input.input_hash)
            .bind(input.expected_revision)
            .bind(input.now)
            .bind(input.now)
            .execute(&mut *connection)
            .await?;
            Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                .bind(&input.id)
                .fetch_optional(&mut *connection)
                .await?)
        }
        .await;
        match result {
            Ok(value) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(value)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn claim_next_job(&self, input: ClaimMemoryJobRow) -> Result<Option<MemoryJobRow>, DbError> {
        self.ensure_user(&input.user_id).await?;
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let candidate: Option<String> = sqlx::query_scalar(
                "SELECT jobs.id FROM memory_jobs jobs
                 WHERE jobs.user_id = ? AND (
                    (jobs.state = 'running' AND jobs.lease_expires_at <= ?)
                    OR (jobs.state IN ('pending', 'retry_wait') AND COALESCE(jobs.next_attempt_at, 0) <= ?
                        AND NOT EXISTS (
                            SELECT 1 FROM memory_jobs active
                            WHERE active.user_id = jobs.user_id AND active.conversation_id = jobs.conversation_id
                              AND active.state = 'running' AND active.lease_expires_at > ?
                        ))
                 )
                 ORDER BY CASE WHEN jobs.state = 'running' THEN 0 ELSE 1 END,
                          COALESCE(jobs.next_attempt_at, jobs.created_at), jobs.created_at, jobs.id
                 LIMIT 1",
            )
            .bind(&input.user_id)
            .bind(input.now)
            .bind(input.now)
            .bind(input.now)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(job_id) = candidate else {
                return Ok(None);
            };
            sqlx::query(
                "UPDATE memory_jobs SET state = 'running',
                 expected_revision = COALESCE((
                    SELECT revision FROM conversation_memories
                    WHERE user_id = memory_jobs.user_id AND conversation_id = memory_jobs.conversation_id
                 ), 0),
                 lease_owner = ?, lease_token = ?, lease_expires_at = ?, next_attempt_at = NULL, updated_at = ?
                 WHERE id = ? AND user_id = ?",
            )
            .bind(&input.worker_id)
            .bind(&input.lease_token)
            .bind(input.now + input.lease_duration_ms)
            .bind(input.now)
            .bind(&job_id)
            .bind(&input.user_id)
            .execute(&mut *connection)
            .await?;
            Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                .bind(job_id)
                .fetch_optional(&mut *connection)
                .await?)
        }
        .await;
        match result {
            Ok(value) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(value)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn split_claimed_job(&self, input: SplitMemoryJobRow) -> Result<bool, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let running: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE id = ? AND user_id = ? AND state = 'running'
                 AND lease_token = ? AND lease_expires_at > ?",
            )
            .bind(&input.job_id).bind(&input.user_id).bind(&input.lease_token).bind(input.now)
            .fetch_optional(&mut *connection).await?;
            let Some(running) = running else { return Ok(false); };
            sqlx::query(
                "UPDATE memory_jobs SET turn_ids_json = ?, through_turn_id = ?, input_hash = ?, updated_at = ?
                 WHERE id = ? AND user_id = ? AND state = 'running' AND lease_token = ? AND lease_expires_at > ?",
            )
            .bind(&input.running_turn_ids_json).bind(&input.running_through_turn_id)
            .bind(&input.running_input_hash).bind(input.now).bind(&input.job_id).bind(&input.user_id)
            .bind(&input.lease_token).bind(input.now).execute(&mut *connection).await?;
            let existing: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state IN ('pending','retry_wait','blocked') LIMIT 1",
            ).bind(&input.user_id).bind(&running.conversation_id).fetch_optional(&mut *connection).await?;
            if let Some(existing) = existing {
                let mut remainder: Vec<String> = serde_json::from_str(&input.pending_turn_ids_json)
                    .map_err(|_| DbError::Conflict("Invalid pending Memory turn queue".into()))?;
                let newer: Vec<String> = serde_json::from_str(&existing.turn_ids_json)
                    .map_err(|_| DbError::Conflict("Invalid existing Memory turn queue".into()))?;
                for turn_id in newer { if !remainder.contains(&turn_id) { remainder.push(turn_id); } }
                let through = remainder.last().cloned().ok_or_else(|| DbError::Conflict("Empty pending queue".into()))?;
                let remainder_json =
                    serde_json::to_string(&remainder).map_err(|error| DbError::Init(error.to_string()))?;
                sqlx::query(
                    "UPDATE memory_jobs SET from_turn_id = ?, turn_ids_json = ?, through_turn_id = ?, input_hash = ?,
                     state = 'pending', next_attempt_at = NULL, updated_at = ? WHERE id = ?",
                ).bind(&input.running_through_turn_id).bind(remainder_json)
                .bind(through).bind(&input.pending_input_hash).bind(input.now).bind(existing.id)
                .execute(&mut *connection).await?;
            } else {
                sqlx::query(
                    "INSERT INTO memory_jobs
                     (id,user_id,conversation_id,from_turn_id,turn_ids_json,through_turn_id,operation_version,input_hash,
                      expected_revision,state,attempt_count,created_at,updated_at)
                     VALUES (?,?,?,?,?,?,?,?,?,'pending',0,?,?)",
                ).bind(&input.pending_job_id).bind(&input.user_id).bind(&running.conversation_id)
                .bind(&input.running_through_turn_id).bind(&input.pending_turn_ids_json)
                .bind(&input.pending_through_turn_id).bind(&running.operation_version).bind(&input.pending_input_hash)
                .bind(running.expected_revision).bind(input.now).bind(input.now).execute(&mut *connection).await?;
            }
            Ok(true)
        }.await;
        match result {
            Ok(value) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(value)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn update_job_input_hash(
        &self,
        user_id: &str,
        job_id: &str,
        expected_turn_ids_json: &str,
        input_hash: &str,
        now: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET input_hash = ?, updated_at = ? WHERE id = ? AND user_id = ? AND turn_ids_json = ?",
        )
        .bind(input_hash)
        .bind(now)
        .bind(job_id)
        .bind(user_id)
        .bind(expected_turn_ids_json)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn validate_lease(
        &self,
        user_id: &str,
        job_id: &str,
        lease_token: &str,
        now: TimestampMs,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM memory_jobs WHERE id = ? AND user_id = ? AND state = 'running'
             AND lease_token = ? AND lease_expires_at > ?)",
        )
        .bind(job_id)
        .bind(user_id)
        .bind(lease_token)
        .bind(now)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn block_jobs(&self, user_id: &str, now: TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET state = 'blocked', next_attempt_at = NULL, updated_at = ?
             WHERE user_id = ? AND state IN ('pending','retry_wait')",
        )
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn renew_lease(&self, input: RenewMemoryLeaseRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET lease_expires_at = ?, updated_at = ?
             WHERE id = ? AND user_id = ? AND state = 'running' AND lease_owner = ? AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(input.now + input.lease_duration_ms)
        .bind(input.now)
        .bind(&input.job_id)
        .bind(&input.user_id)
        .bind(&input.worker_id)
        .bind(&input.lease_token)
        .bind(input.now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn release_lease(&self, input: ReleaseMemoryLeaseRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET state = 'pending', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
             next_attempt_at = NULL, updated_at = ?
             WHERE id = ? AND user_id = ? AND state = 'running' AND lease_owner = ? AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(input.now)
        .bind(&input.job_id)
        .bind(&input.user_id)
        .bind(&input.worker_id)
        .bind(&input.lease_token)
        .bind(input.now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    async fn transition_running_job(&self, input: TransitionMemoryJobRow) -> Result<Option<MemoryJobRow>, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET state = ?, next_attempt_at = ?, last_error_code = ?,
             attempt_count = attempt_count + ?, invalid_output_count = invalid_output_count + ?,
             lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, updated_at = ?
             WHERE id = ? AND user_id = ? AND state = 'running' AND lease_owner = ? AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(&input.state)
        .bind(input.next_attempt_at)
        .bind(&input.error_code)
        .bind(input.increment_attempt)
        .bind(input.increment_invalid_output)
        .bind(input.now)
        .bind(&input.job_id)
        .bind(&input.user_id)
        .bind(&input.worker_id)
        .bind(&input.lease_token)
        .bind(input.now)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Ok(None);
        }
        self.get_job(&input.user_id, &input.job_id).await
    }

    async fn cancel_jobs(
        &self,
        user_id: &str,
        conversation_id: Option<&str>,
        now: TimestampMs,
    ) -> Result<u64, DbError> {
        let result = match conversation_id {
            Some(conversation_id) => {
                sqlx::query(
                    "UPDATE memory_jobs SET state = 'canceled', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
                     next_attempt_at = NULL, last_error_code = 'canceled', updated_at = ?
                     WHERE user_id = ? AND conversation_id = ? AND state IN ('pending','running','retry_wait','blocked')",
                )
                .bind(now)
                .bind(user_id)
                .bind(conversation_id)
                .execute(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "UPDATE memory_jobs SET state = 'canceled', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
                     next_attempt_at = NULL, last_error_code = 'canceled', updated_at = ?
                     WHERE user_id = ? AND state IN ('pending','running','retry_wait','blocked')",
                )
                .bind(now)
                .bind(user_id)
                .execute(&self.pool)
                .await?
            }
        };
        Ok(result.rows_affected())
    }

    async fn unblock_jobs(&self, user_id: &str, now: TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET state = 'pending', next_attempt_at = NULL, last_error_code = NULL, updated_at = ?
             WHERE user_id = ? AND state = 'blocked'",
        )
        .bind(now)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn recover_expired_jobs(&self, now: TimestampMs) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET state = 'pending', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, updated_at = ?
             WHERE state = 'running' AND lease_expires_at <= ?",
        )
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn get_job(&self, user_id: &str, job_id: &str) -> Result<Option<MemoryJobRow>, DbError> {
        let owner: Option<String> = sqlx::query_scalar("SELECT user_id FROM memory_jobs WHERE id = ?")
            .bind(job_id)
            .fetch_optional(&self.pool)
            .await?;
        match owner {
            Some(owner) if owner != user_id => Err(DbError::NotFound(format!("Memory job '{job_id}' not found"))),
            None => Ok(None),
            Some(_) => Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ? AND user_id = ?")
                .bind(job_id)
                .bind(user_id)
                .fetch_optional(&self.pool)
                .await?),
        }
    }

    async fn commit_update(&self, input: CommitMemoryUpdateRow) -> Result<CommitMemoryUpdateResult, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            Self::ensure_conversation_on(&mut connection, &input.user_id, &input.conversation_id).await?;
            let job: Option<(String, String, i64, String, Option<String>, Option<String>, Option<i64>, i64)> = sqlx::query_as(
                "SELECT conversation_id, state, expected_revision, through_turn_id, lease_owner,
                        lease_token, lease_expires_at, attempt_count
                 FROM memory_jobs WHERE id = ? AND user_id = ?",
            )
            .bind(&input.job_id)
            .bind(&input.user_id)
            .fetch_optional(&mut *connection)
            .await?;
            let Some((
                job_conversation_id,
                job_state,
                job_expected_revision,
                job_through_turn_id,
                job_lease_owner,
                job_lease_token,
                job_lease_expires_at,
                job_attempt_count,
            )) = job
            else {
                return Err(DbError::NotFound(format!("Memory job '{}' not found", input.job_id)));
            };
            let valid_fence = job_conversation_id == input.conversation_id
                && job_state == "running"
                && job_expected_revision == input.expected_revision
                && job_through_turn_id == input.through_turn_id
                && job_lease_owner.as_deref() == Some(input.lease_owner.as_str())
                && job_lease_token.as_deref() == Some(input.lease_token.as_str())
                && job_lease_expires_at.is_some_and(|expires_at| expires_at > input.now)
                && job_attempt_count == input.expected_attempt_count;
            if !valid_fence {
                return Err(DbError::Conflict(format!(
                    "Memory job '{}' lease or cursor changed",
                    input.job_id
                )));
            }

            let current_revision: Option<i64> = sqlx::query_scalar(
                "SELECT revision FROM conversation_memories WHERE user_id = ? AND conversation_id = ?",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            if current_revision.unwrap_or(0) != input.expected_revision {
                return Ok(CommitMemoryUpdateResult::StaleRevision {
                    current_revision: current_revision.unwrap_or(0),
                });
            }

            let revision = input.expected_revision + 1;
            if current_revision.is_some() {
                let updated = sqlx::query(
                    "UPDATE conversation_memories SET
                        project_id = ?, workspace_key = ?, summary_json = ?, through_turn_id = ?,
                        revision = revision + 1, source = 'memory_update', schema_version = ?, prompt_version = ?,
                        writer_provider_id = ?, writer_model_id = ?, updated_at = ?
                     WHERE user_id = ? AND conversation_id = ? AND revision = ?",
                )
                .bind(&input.project_id)
                .bind(&input.workspace_key)
                .bind(&input.summary_json)
                .bind(&input.through_turn_id)
                .bind(input.schema_version)
                .bind(&input.prompt_version)
                .bind(&input.writer_provider_id)
                .bind(&input.writer_model_id)
                .bind(input.now)
                .bind(&input.user_id)
                .bind(&input.conversation_id)
                .bind(input.expected_revision)
                .execute(&mut *connection)
                .await?;
                if updated.rows_affected() == 0 {
                    let revision = sqlx::query_scalar(
                        "SELECT revision FROM conversation_memories WHERE user_id = ? AND conversation_id = ?",
                    )
                    .bind(&input.user_id)
                    .bind(&input.conversation_id)
                    .fetch_one(&mut *connection)
                    .await?;
                    return Ok(CommitMemoryUpdateResult::StaleRevision {
                        current_revision: revision,
                    });
                }
            } else {
                sqlx::query(
                    "INSERT INTO conversation_memories
                        (user_id, conversation_id, project_id, workspace_key, summary_json, through_turn_id, revision,
                         source, schema_version, prompt_version, writer_provider_id, writer_model_id, created_at, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, 1, 'memory_update', ?, ?, ?, ?, ?, ?)",
                )
                .bind(&input.user_id)
                .bind(&input.conversation_id)
                .bind(&input.project_id)
                .bind(&input.workspace_key)
                .bind(&input.summary_json)
                .bind(&input.through_turn_id)
                .bind(input.schema_version)
                .bind(&input.prompt_version)
                .bind(&input.writer_provider_id)
                .bind(&input.writer_model_id)
                .bind(input.now)
                .bind(input.now)
                .execute(&mut *connection)
                .await?;
            }

            let mut added_ids = Vec::new();
            let mut refined_ids = Vec::new();
            let mut superseded_ids = Vec::new();
            let mut conflict_ids = Vec::new();
            for entry in &input.entries {
                let tombstoned: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM memory_entries WHERE user_id = ? AND fingerprint = ? AND state = 'deleted')",
                )
                .bind(&input.user_id)
                .bind(&entry.fingerprint)
                .fetch_one(&mut *connection)
                .await?;
                if tombstoned {
                    continue;
                }

                Self::validate_sources_on(&mut connection, &input.user_id, &entry.sources).await?;
                let entry_id = match &entry.transition {
                    CommitMemoryEntryTransition::Create => {
                        Self::insert_entry_on(
                            &mut connection,
                            &input.user_id,
                            entry,
                            InsertEntryOptions {
                                state: "active",
                                supersedes_id: None,
                                conflict_group_id: None,
                            },
                            input.schema_version,
                            input.now,
                        )
                        .await?;
                        added_ids.push(entry.id.clone());
                        entry.id.clone()
                    }
                    CommitMemoryEntryTransition::Refine { target_entry_id } => {
                        Self::ensure_automatic_target_mutable_on(&mut connection, &input.user_id, target_entry_id)
                            .await?;
                        sqlx::query(
                            "UPDATE memory_entries SET project_id = ?, workspace_key = ?, kind = ?, stable_key = ?,
                             fingerprint = ?, content = ?, updated_at = ?
                             WHERE id = ? AND user_id = ? AND state <> 'deleted'",
                        )
                        .bind(&entry.project_id)
                        .bind(&entry.workspace_key)
                        .bind(&entry.kind)
                        .bind(&entry.stable_key)
                        .bind(&entry.fingerprint)
                        .bind(&entry.content)
                        .bind(input.now)
                        .bind(target_entry_id)
                        .bind(&input.user_id)
                        .execute(&mut *connection)
                        .await?;
                        refined_ids.push(target_entry_id.clone());
                        target_entry_id.clone()
                    }
                    CommitMemoryEntryTransition::Supersede { target_entry_id } => {
                        Self::ensure_automatic_target_mutable_on(&mut connection, &input.user_id, target_entry_id)
                            .await?;
                        sqlx::query(
                            "UPDATE memory_entries SET state = 'superseded', updated_at = ?
                             WHERE id = ? AND user_id = ? AND state <> 'deleted'",
                        )
                        .bind(input.now)
                        .bind(target_entry_id)
                        .bind(&input.user_id)
                        .execute(&mut *connection)
                        .await?;
                        Self::insert_entry_on(
                            &mut connection,
                            &input.user_id,
                            entry,
                            InsertEntryOptions {
                                state: "active",
                                supersedes_id: Some(target_entry_id),
                                conflict_group_id: None,
                            },
                            input.schema_version,
                            input.now,
                        )
                        .await?;
                        added_ids.push(entry.id.clone());
                        superseded_ids.push(target_entry_id.clone());
                        entry.id.clone()
                    }
                    CommitMemoryEntryTransition::Conflict {
                        target_entry_id,
                        conflict_group_id,
                    } => {
                        let (pinned, user_edited) =
                            Self::entry_target_protection_on(&mut connection, &input.user_id, target_entry_id).await?;
                        if !pinned && !user_edited {
                            sqlx::query(
                                "UPDATE memory_entries SET state = 'conflict', conflict_group_id = ?, updated_at = ?
                                 WHERE id = ? AND user_id = ? AND state <> 'deleted'",
                            )
                            .bind(conflict_group_id)
                            .bind(input.now)
                            .bind(target_entry_id)
                            .bind(&input.user_id)
                            .execute(&mut *connection)
                            .await?;
                            conflict_ids.push(target_entry_id.clone());
                        }
                        Self::insert_entry_on(
                            &mut connection,
                            &input.user_id,
                            entry,
                            InsertEntryOptions {
                                state: "conflict",
                                supersedes_id: None,
                                conflict_group_id: Some(conflict_group_id),
                            },
                            input.schema_version,
                            input.now,
                        )
                        .await?;
                        conflict_ids.push(entry.id.clone());
                        entry.id.clone()
                    }
                };
                Self::upsert_sources_on(&mut connection, &entry_id, &entry.sources, input.now).await?;
            }

            sqlx::query(
                "INSERT INTO memory_change_sets
                    (id, user_id, conversation_id, through_turn_id, job_id, added_ids_json, refined_ids_json,
                     superseded_ids_json, conflict_ids_json, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&input.change_set_id)
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&input.through_turn_id)
            .bind(&input.job_id)
            .bind(serde_json::to_string(&added_ids).map_err(|error| DbError::Init(error.to_string()))?)
            .bind(serde_json::to_string(&refined_ids).map_err(|error| DbError::Init(error.to_string()))?)
            .bind(serde_json::to_string(&superseded_ids).map_err(|error| DbError::Init(error.to_string()))?)
            .bind(serde_json::to_string(&conflict_ids).map_err(|error| DbError::Init(error.to_string()))?)
            .bind(input.now)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "UPDATE memory_jobs SET state = 'succeeded', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, updated_at = ?
                 WHERE id = ? AND user_id = ? AND state = 'running'",
            )
            .bind(input.now)
            .bind(&input.job_id)
            .bind(&input.user_id)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "UPDATE memory_jobs SET from_turn_id = ?, expected_revision = ?, updated_at = ?
                 WHERE user_id = ? AND conversation_id = ? AND state IN ('pending','retry_wait','blocked')",
            )
            .bind(&input.through_turn_id)
            .bind(revision)
            .bind(input.now)
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .execute(&mut *connection)
            .await?;

            Ok(CommitMemoryUpdateResult::Committed {
                revision,
                added_ids,
                refined_ids,
                superseded_ids,
                conflict_ids,
            })
        }
        .await;

        match result {
            Ok(CommitMemoryUpdateResult::StaleRevision { current_revision }) => {
                sqlx::query("ROLLBACK").execute(&mut *connection).await?;
                Ok(CommitMemoryUpdateResult::StaleRevision { current_revision })
            }
            Ok(value) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(value)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn get_conversation_memory(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationMemoryRow>, DbError> {
        self.ensure_conversation(user_id, conversation_id).await?;
        Ok(
            sqlx::query_as("SELECT * FROM conversation_memories WHERE user_id = ? AND conversation_id = ?")
                .bind(user_id)
                .bind(conversation_id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    async fn list_entries(&self, user_id: &str) -> Result<Vec<MemoryEntryRow>, DbError> {
        self.query_entries(user_id, MemoryEntryQueryRow::default()).await
    }

    async fn query_entries(&self, user_id: &str, query: MemoryEntryQueryRow) -> Result<Vec<MemoryEntryRow>, DbError> {
        self.ensure_user(user_id).await?;
        let limit = if query.limit == 0 {
            MAX_MEMORY_CANDIDATES
        } else {
            query.limit.min(MAX_MEMORY_CANDIDATES)
        };
        let rows = sqlx::query_as::<_, MemoryEntryDbRow>(
            "SELECT DISTINCT entries.* FROM memory_entries entries
             LEFT JOIN memory_sources sources ON sources.memory_entry_id = entries.id
             WHERE entries.user_id = ?
               AND (? IS NULL OR entries.kind = ?)
               AND (? IS NULL OR entries.state = ?)
               AND (? IS NULL OR entries.project_id = ?)
               AND (? IS NULL OR entries.workspace_key = ?)
               AND (? IS NULL OR sources.conversation_id = ?)
               AND (? IS NULL OR entries.created_at >= ?)
               AND (? IS NULL OR entries.created_at <= ?)
               AND (? IS NULL OR lower(COALESCE(entries.content, '')) LIKE '%' || lower(?) || '%')
             ORDER BY entries.pinned DESC, entries.user_edited DESC, entries.updated_at DESC, entries.id
             LIMIT ?",
        )
        .bind(user_id)
        .bind(&query.kind)
        .bind(&query.kind)
        .bind(&query.state)
        .bind(&query.state)
        .bind(&query.project_id)
        .bind(&query.project_id)
        .bind(&query.workspace_key)
        .bind(&query.workspace_key)
        .bind(&query.source_conversation_id)
        .bind(&query.source_conversation_id)
        .bind(query.created_after)
        .bind(query.created_after)
        .bind(query.created_before)
        .bind(query.created_before)
        .bind(&query.search)
        .bind(&query.search)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        self.entry_rows_with_sources(rows).await
    }

    async fn get_entry(&self, user_id: &str, entry_id: &str) -> Result<Option<MemoryEntryRow>, DbError> {
        let row = sqlx::query_as::<_, MemoryEntryDbRow>("SELECT * FROM memory_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) if row.user_id != user_id => {
                Err(DbError::NotFound(format!("Memory entry '{entry_id}' not found")))
            }
            None => Ok(None),
            Some(row) => Ok(Some(self.entry_with_sources(row).await?)),
        }
    }

    async fn update_entry(&self, input: UpdateMemoryEntryRow) -> Result<MemoryEntryRow, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let project_present = input.project_id.is_some();
        let project_id = input.project_id.flatten();
        let workspace_present = input.workspace_key.is_some();
        let workspace_key = input.workspace_key.flatten();
        let result = async {
            let updated = sqlx::query(
                "UPDATE memory_entries SET
                    content = COALESCE(?, content),
                    user_edited = CASE WHEN ? IS NULL THEN user_edited ELSE 1 END,
                    pinned = COALESCE(?, pinned),
                    project_id = CASE WHEN ? THEN ? ELSE project_id END,
                    workspace_key = CASE WHEN ? THEN ? ELSE workspace_key END,
                    updated_at = ?
                 WHERE id = ? AND user_id = ? AND state <> 'deleted'",
            )
            .bind(&input.content)
            .bind(&input.content)
            .bind(input.pinned)
            .bind(project_present)
            .bind(project_id)
            .bind(workspace_present)
            .bind(workspace_key)
            .bind(input.now)
            .bind(&input.id)
            .bind(&input.user_id)
            .execute(&mut *connection)
            .await?;
            if updated.rows_affected() == 0 {
                let state: Option<String> =
                    sqlx::query_scalar("SELECT state FROM memory_entries WHERE id = ? AND user_id = ?")
                        .bind(&input.id)
                        .bind(&input.user_id)
                        .fetch_optional(&mut *connection)
                        .await?;
                return match state.as_deref() {
                    Some("deleted") => Err(DbError::Conflict(format!(
                        "Deleted Memory entry '{}' cannot be updated",
                        input.id
                    ))),
                    _ => Err(DbError::NotFound(format!("Memory entry '{}' not found", input.id))),
                };
            }
            let row = sqlx::query_as::<_, MemoryEntryDbRow>(
                "SELECT * FROM memory_entries WHERE id = ? AND user_id = ? AND state <> 'deleted'",
            )
            .bind(&input.id)
            .bind(&input.user_id)
            .fetch_one(&mut *connection)
            .await?;
            Self::entry_with_sources_on(&mut connection, row).await
        }
        .await;
        match result {
            Ok(entry) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(entry)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn delete_entry(&self, user_id: &str, entry_id: &str, now: i64) -> Result<(), DbError> {
        self.get_entry(user_id, entry_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Memory entry '{entry_id}' not found")))?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query("DELETE FROM memory_sources WHERE memory_entry_id = ?")
            .bind(entry_id)
            .execute(&mut *transaction)
            .await?;
        sqlx::query(
            "UPDATE memory_entries SET content = NULL, state = 'deleted', pinned = 0, user_edited = 0,
             supersedes_id = NULL, conflict_group_id = NULL, deleted_at = ?, updated_at = ?
             WHERE id = ? AND user_id = ?",
        )
        .bind(now)
        .bind(now)
        .bind(entry_id)
        .bind(user_id)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn list_change_sets(&self, user_id: &str, limit: u32) -> Result<Vec<MemoryChangeSetRow>, DbError> {
        self.ensure_user(user_id).await?;
        Ok(sqlx::query_as(
            "SELECT * FROM memory_change_sets WHERE user_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(limit.min(MAX_MEMORY_CANDIDATES))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn delete_conversation_memory(&self, user_id: &str, conversation_id: &str, now: i64) -> Result<(), DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            Self::ensure_conversation_on(&mut connection, user_id, conversation_id).await?;
            let exclusive_ids: Vec<String> = sqlx::query_scalar(
                "SELECT entries.id FROM memory_entries entries
                 JOIN memory_sources source ON source.memory_entry_id = entries.id
                 WHERE entries.user_id = ? AND source.conversation_id = ?
                   AND entries.pinned = 0 AND entries.user_edited = 0 AND entries.state <> 'deleted'
                   AND NOT EXISTS (
                        SELECT 1 FROM memory_sources other_source
                        WHERE other_source.memory_entry_id = entries.id
                          AND other_source.conversation_id <> ?
                   )",
            )
            .bind(user_id)
            .bind(conversation_id)
            .bind(conversation_id)
            .fetch_all(&mut *connection)
            .await?;
            sqlx::query("DELETE FROM memory_sources WHERE conversation_id = ?")
                .bind(conversation_id)
                .execute(&mut *connection)
                .await?;
            for entry_id in exclusive_ids {
                sqlx::query("DELETE FROM memory_entries WHERE id = ? AND user_id = ?")
                    .bind(entry_id)
                    .bind(user_id)
                    .execute(&mut *connection)
                    .await?;
            }
            sqlx::query("DELETE FROM conversation_memories WHERE user_id = ? AND conversation_id = ?")
                .bind(user_id)
                .bind(conversation_id)
                .execute(&mut *connection)
                .await?;
            sqlx::query("DELETE FROM memory_change_sets WHERE user_id = ? AND conversation_id = ?")
                .bind(user_id)
                .bind(conversation_id)
                .execute(&mut *connection)
                .await?;
            sqlx::query("DELETE FROM memory_retrievals WHERE user_id = ? AND conversation_id = ?")
                .bind(user_id)
                .bind(conversation_id)
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "UPDATE memory_jobs SET state = 'canceled', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL, updated_at = ?
                 WHERE user_id = ? AND conversation_id = ? AND state NOT IN ('succeeded', 'failed', 'canceled')",
            )
            .bind(now)
            .bind(user_id)
            .bind(conversation_id)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO conversation_memory_policies (user_id, conversation_id, reset_at, updated_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(user_id, conversation_id) DO UPDATE SET reset_at = excluded.reset_at, updated_at = excluded.updated_at",
            )
            .bind(user_id)
            .bind(conversation_id)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn clear_memory(&self, user_id: &str, now: i64) -> Result<(), DbError> {
        self.ensure_user(user_id).await?;
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            sqlx::query(
                "INSERT INTO memory_settings (user_id, reset_at, updated_at) VALUES (?, ?, ?)
                 ON CONFLICT(user_id) DO UPDATE SET reset_at = excluded.reset_at, updated_at = excluded.updated_at",
            )
            .bind(user_id)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
            for table in [
                "memory_retrievals",
                "memory_change_sets",
                "conversation_memories",
                "conversation_memory_policies",
                "memory_entries",
                "memory_import_state",
                "memory_jobs",
            ] {
                sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ?"))
                    .bind(user_id)
                    .execute(&mut *connection)
                    .await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(())
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn retrieval_candidates(&self, query: MemoryCandidateQueryRow) -> Result<Vec<MemoryEntryRow>, DbError> {
        self.ensure_user(&query.user_id).await?;
        let rows = sqlx::query_as::<_, MemoryEntryDbRow>(
            "SELECT * FROM memory_entries
             WHERE user_id = ? AND state = 'active'
               AND ((? IS NULL AND project_id IS NULL)
                    OR (? IS NOT NULL AND (project_id IS NULL OR project_id = ?)))
               AND ((? IS NULL AND workspace_key IS NULL)
                    OR (? IS NOT NULL AND (workspace_key IS NULL OR workspace_key = ?)))
             ORDER BY
               CASE WHEN project_id = ? THEN 0 WHEN workspace_key = ? THEN 1 ELSE 2 END,
               pinned DESC, user_edited DESC, updated_at DESC, id
             LIMIT ?",
        )
        .bind(&query.user_id)
        .bind(&query.project_id)
        .bind(&query.project_id)
        .bind(&query.project_id)
        .bind(&query.workspace_key)
        .bind(&query.workspace_key)
        .bind(&query.workspace_key)
        .bind(&query.project_id)
        .bind(&query.workspace_key)
        .bind(query.limit.clamp(1, MAX_MEMORY_CANDIDATES))
        .fetch_all(&self.pool)
        .await?;
        self.entry_rows_with_sources(rows).await
    }

    async fn create_retrieval(&self, retrieval: MemoryRetrievalRow) -> Result<MemoryRetrievalRow, DbError> {
        self.ensure_conversation(&retrieval.user_id, &retrieval.conversation_id)
            .await?;
        let selected_ids: Vec<String> = serde_json::from_str(&retrieval.selected_ids_json)
            .map_err(|error| DbError::Conflict(format!("Invalid selected Memory IDs: {error}")))?;
        for entry_id in selected_ids {
            self.get_entry(&retrieval.user_id, &entry_id)
                .await?
                .ok_or_else(|| DbError::NotFound(format!("Memory entry '{entry_id}' not found")))?;
        }
        sqlx::query(
            "INSERT INTO memory_retrievals
                (id, user_id, conversation_id, prompt_hash, selected_ids_json, estimated_tokens, budget_tokens,
                 retrieval_version, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&retrieval.id)
        .bind(&retrieval.user_id)
        .bind(&retrieval.conversation_id)
        .bind(&retrieval.prompt_hash)
        .bind(&retrieval.selected_ids_json)
        .bind(retrieval.estimated_tokens)
        .bind(retrieval.budget_tokens)
        .bind(&retrieval.retrieval_version)
        .bind(retrieval.created_at)
        .bind(retrieval.expires_at)
        .execute(&self.pool)
        .await?;
        Ok(retrieval)
    }

    async fn get_retrieval(&self, user_id: &str, retrieval_id: &str) -> Result<Option<MemoryRetrievalRow>, DbError> {
        let row = sqlx::query_as::<_, MemoryRetrievalRow>("SELECT * FROM memory_retrievals WHERE id = ?")
            .bind(retrieval_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) if row.user_id != user_id => Err(DbError::NotFound(format!(
                "Memory retrieval '{retrieval_id}' not found"
            ))),
            row => Ok(row),
        }
    }

    async fn get_import_state(&self, user_id: &str) -> Result<Option<MemoryImportStateRow>, DbError> {
        self.ensure_user(user_id).await?;
        Ok(sqlx::query_as("SELECT * FROM memory_import_state WHERE user_id = ?")
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn upsert_import_state(&self, state: MemoryImportStateRow) -> Result<MemoryImportStateRow, DbError> {
        self.ensure_user(&state.user_id).await?;
        sqlx::query(
            "INSERT INTO memory_import_state
                (user_id, cursor, completed, started_at, completed_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(user_id) DO UPDATE SET cursor = excluded.cursor, completed = excluded.completed,
                started_at = excluded.started_at, completed_at = excluded.completed_at, updated_at = excluded.updated_at",
        )
        .bind(&state.user_id)
        .bind(&state.cursor)
        .bind(state.completed)
        .bind(state.started_at)
        .bind(state.completed_at)
        .bind(state.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteMemoryRepository;
    use crate::models::{ConversationRow, MessageRow};
    use crate::repository::memory::{
        ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow,
        CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow, MemoryCandidateQueryRow,
        RenewMemoryLeaseRow, UpdateConversationMemoryPolicyRow, UpdateMemoryEntryRow, UpdateMemorySettingsRow,
    };
    use crate::repository::{IConversationRepository, IMemoryRepository, SqliteConversationRepository};
    use crate::{DbError, init_database_memory};

    const USER_A: &str = "system_default_user";
    const USER_B: &str = "user_b";

    async fn setup() -> (SqliteMemoryRepository, SqliteConversationRepository, crate::Database) {
        let db = init_database_memory().await.unwrap();
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, created_at, updated_at)
             VALUES (?, ?, ?, '', 1, 1)",
        )
        .bind(USER_B)
        .bind(USER_B)
        .bind("user-b@example.com")
        .execute(db.pool())
        .await
        .unwrap();
        let conversations = SqliteConversationRepository::new(db.pool().clone());
        for (id, user_id) in [("conv_a", USER_A), ("conv_a2", USER_A), ("conv_b", USER_B)] {
            conversations.create(&conversation(id, user_id)).await.unwrap();
        }
        (SqliteMemoryRepository::new(db.pool().clone()), conversations, db)
    }

    fn conversation(id: &str, user_id: &str) -> ConversationRow {
        ConversationRow {
            id: id.into(),
            user_id: user_id.into(),
            name: id.into(),
            r#type: "gemini".into(),
            extra: "{}".into(),
            model: None,
            status: Some("finished".into()),
            source: Some("aionui".into()),
            channel_chat_id: None,
            pinned: false,
            pinned_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn enqueue(id: &str, conversation_id: &str, through_turn_id: &str, now: i64) -> EnqueueMemoryTurnRow {
        EnqueueMemoryTurnRow {
            id: id.into(),
            user_id: USER_A.into(),
            conversation_id: conversation_id.into(),
            from_turn_id: None,
            turn_ids_json: format!(r#"["{through_turn_id}"]"#),
            through_turn_id: through_turn_id.into(),
            operation_version: "memory-operation-v1".into(),
            input_hash: format!("hash-{through_turn_id}"),
            expected_revision: 0,
            now,
        }
    }

    fn claim(user_id: &str, worker_id: &str, now: i64) -> ClaimMemoryJobRow {
        ClaimMemoryJobRow {
            user_id: user_id.into(),
            worker_id: worker_id.into(),
            lease_token: format!("lease-{worker_id}-{now}"),
            now,
            lease_duration_ms: 10,
        }
    }

    fn source(conversation_id: &str, turn_id: &str) -> CommitMemorySourceRow {
        CommitMemorySourceRow {
            conversation_id: conversation_id.into(),
            turn_id: turn_id.into(),
            message_ids_json: format!(r#"["msg-{turn_id}"]"#),
        }
    }

    fn entry(id: &str, fingerprint: &str, sources: Vec<CommitMemorySourceRow>) -> CommitMemoryEntryRow {
        CommitMemoryEntryRow {
            id: id.into(),
            project_id: None,
            workspace_key: None,
            kind: "decision".into(),
            stable_key: format!("decision:{id}"),
            fingerprint: fingerprint.into(),
            content: format!("content for {id}"),
            transition: CommitMemoryEntryTransition::Create,
            sources,
        }
    }

    fn commit(
        job_id: &str,
        conversation_id: &str,
        through_turn_id: &str,
        expected_revision: i64,
        entries: Vec<CommitMemoryEntryRow>,
        now: i64,
    ) -> CommitMemoryUpdateRow {
        CommitMemoryUpdateRow {
            user_id: USER_A.into(),
            job_id: job_id.into(),
            conversation_id: conversation_id.into(),
            expected_revision,
            through_turn_id: through_turn_id.into(),
            project_id: None,
            workspace_key: None,
            summary_json: r#"{"goal":"ship memory"}"#.into(),
            schema_version: 1,
            prompt_version: Some("memory-v1".into()),
            writer_provider_id: Some("provider-result".into()),
            writer_model_id: Some("model-result".into()),
            lease_owner: "worker".into(),
            lease_token: "lease-worker-11".into(),
            expected_attempt_count: 0,
            entries,
            change_set_id: format!("changes-{job_id}"),
            now,
        }
    }

    async fn claimed_job(repo: &SqliteMemoryRepository, job_id: &str, conversation_id: &str, turn_id: &str) {
        sqlx::query(
            "INSERT OR IGNORE INTO messages
                (id, conversation_id, turn_id, type, content, position, status, hidden, created_at)
             VALUES (?, ?, ?, 'text', '{}', 'right', 'finish', 0, 10)",
        )
        .bind(format!("msg-{turn_id}"))
        .bind(conversation_id)
        .bind(turn_id)
        .execute(&repo.pool)
        .await
        .unwrap();
        repo.enqueue_completed_turn(enqueue(job_id, conversation_id, turn_id, 10))
            .await
            .unwrap();
        let claimed = repo
            .claim_next_job(ClaimMemoryJobRow {
                user_id: USER_A.into(),
                worker_id: "worker".into(),
                lease_token: "lease-worker-11".into(),
                now: 11,
                lease_duration_ms: 100,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, job_id);
    }

    #[tokio::test]
    async fn sqlite_memory_defaults_consent_and_reset_boundaries_are_user_scoped() {
        let (repo, _, _db) = setup().await;
        let defaults = repo.get_settings(USER_A).await.unwrap();
        assert!(!defaults.enabled);
        assert!(defaults.default_capture);
        assert!(defaults.default_recall);
        assert_eq!(defaults.consent_version, None);
        assert_eq!(defaults.reset_at, None);

        repo.update_settings(UpdateMemorySettingsRow {
            user_id: USER_A.into(),
            enabled: Some(true),
            default_capture: None,
            default_recall: None,
            consent_version: Some(1),
            now: 20,
        })
        .await
        .unwrap();
        repo.clear_memory(USER_A, 30).await.unwrap();

        let user_a = repo.get_settings(USER_A).await.unwrap();
        let user_b = repo.get_settings(USER_B).await.unwrap();
        assert_eq!(user_a.consent_version, Some(1));
        assert_eq!(user_a.consented_at, Some(20));
        assert_eq!(user_a.reset_at, Some(30));
        assert_eq!(user_b.consent_version, None);
        assert_eq!(user_b.reset_at, None);
    }

    #[tokio::test]
    async fn sqlite_memory_nullable_policy_overrides_inherit_user_defaults() {
        let (repo, _, _db) = setup().await;
        repo.update_settings(UpdateMemorySettingsRow {
            user_id: USER_A.into(),
            enabled: Some(true),
            default_capture: Some(true),
            default_recall: Some(false),
            consent_version: Some(1),
            now: 10,
        })
        .await
        .unwrap();
        repo.update_conversation_policy(UpdateConversationMemoryPolicyRow {
            user_id: USER_A.into(),
            conversation_id: "conv_a".into(),
            capture_enabled: Some(false),
            recall_enabled: None,
            now: 11,
        })
        .await
        .unwrap();

        let effective = repo.effective_policy(USER_A, "conv_a").await.unwrap();
        assert!(effective.enabled);
        assert!(!effective.capture_enabled);
        assert!(!effective.recall_enabled);
        assert_eq!(effective.capture_override, Some(false));
        assert_eq!(effective.recall_override, None);
    }

    #[tokio::test]
    async fn sqlite_memory_duplicate_enqueue_coalesces_and_running_job_has_one_pending_successor() {
        let (repo, _, _db) = setup().await;
        let first = repo
            .enqueue_completed_turn(enqueue("job-1", "conv_a", "turn-1", 10))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.id, "job-1");
        assert!(
            repo.enqueue_completed_turn(enqueue("duplicate", "conv_a", "turn-1", 11))
                .await
                .unwrap()
                .is_none()
        );

        let coalesced = repo
            .enqueue_completed_turn(enqueue("job-2", "conv_a", "turn-2", 12))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(coalesced.id, "job-1");
        assert_eq!(coalesced.through_turn_id, "turn-2");
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&coalesced.turn_ids_json).unwrap(),
            ["turn-1", "turn-2"],
        );
        assert!(
            repo.enqueue_completed_turn(enqueue("delayed-old", "conv_a", "turn-1", 13))
                .await
                .unwrap()
                .is_none()
        );
        let still_monotonic = repo.get_job(USER_A, "job-1").await.unwrap().unwrap();
        assert_eq!(still_monotonic.through_turn_id, "turn-2");
        assert_eq!(repo.count_jobs(USER_A, "conv_a", "pending").await.unwrap(), 1);

        repo.claim_next_job(claim(USER_A, "worker", 13)).await.unwrap().unwrap();
        let pending = repo
            .enqueue_completed_turn(enqueue("job-next", "conv_a", "turn-3", 14))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.id, "job-next");
        let pending = repo
            .enqueue_completed_turn(enqueue("ignored-id", "conv_a", "turn-4", 15))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.id, "job-next");
        assert_eq!(pending.through_turn_id, "turn-4");
        assert_eq!(repo.count_jobs(USER_A, "conv_a", "running").await.unwrap(), 1);
        assert_eq!(repo.count_jobs(USER_A, "conv_a", "pending").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sqlite_memory_expired_lease_is_claimable_again() {
        let (repo, _, _db) = setup().await;
        repo.enqueue_completed_turn(enqueue("job-lease", "conv_a", "turn-1", 10))
            .await
            .unwrap();
        let first = repo
            .claim_next_job(claim(USER_A, "worker-a", 20))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.lease_expires_at, Some(30));
        assert_eq!(first.attempt_count, 0, "claiming is not a durable failed attempt");
        let first_token = first.lease_token.clone().expect("server lease token");
        assert!(
            repo.claim_next_job(claim(USER_A, "worker-b", 29))
                .await
                .unwrap()
                .is_none()
        );
        let reclaimed = repo
            .claim_next_job(claim(USER_A, "worker-b", 31))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.id, "job-lease");
        assert_eq!(reclaimed.lease_owner.as_deref(), Some("worker-b"));
        assert_eq!(reclaimed.attempt_count, 0);
        assert_ne!(reclaimed.lease_token.as_deref(), Some(first_token.as_str()));
        assert!(
            repo.renew_lease(RenewMemoryLeaseRow {
                user_id: USER_A.into(),
                job_id: "job-lease".into(),
                worker_id: "worker-b".into(),
                lease_token: reclaimed.lease_token.expect("reclaimed token"),
                now: 32,
                lease_duration_ms: 10,
            })
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn sqlite_memory_reclaimed_job_rejects_the_expired_workers_commit() {
        let (repo, _, _db) = setup().await;
        sqlx::query(
            "INSERT INTO messages
                (id, conversation_id, turn_id, type, content, position, status, hidden, created_at)
             VALUES ('msg-turn-lease', 'conv_a', 'turn-lease', 'text', '{}', 'right', 'finish', 0, 10)",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        repo.enqueue_completed_turn(enqueue("job-fenced", "conv_a", "turn-lease", 10))
            .await
            .unwrap();
        repo.claim_next_job(claim(USER_A, "worker-old", 20))
            .await
            .unwrap()
            .unwrap();
        let reclaimed = repo
            .claim_next_job(claim(USER_A, "worker-new", 31))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.attempt_count, 0);

        let mut old_commit = commit(
            "job-fenced",
            "conv_a",
            "turn-lease",
            0,
            vec![entry(
                "stale-worker-entry",
                "fp-stale-worker",
                vec![source("conv_a", "turn-lease")],
            )],
            32,
        );
        old_commit.lease_owner = "worker-old".into();
        old_commit.lease_token = "lease-worker-old-20".into();
        old_commit.expected_attempt_count = 0;
        assert!(matches!(
            repo.commit_update(old_commit).await,
            Err(DbError::Conflict(_))
        ));
        assert!(repo.get_entry(USER_A, "stale-worker-entry").await.unwrap().is_none());

        let mut current_commit = commit(
            "job-fenced",
            "conv_a",
            "turn-lease",
            0,
            vec![entry(
                "current-worker-entry",
                "fp-current-worker",
                vec![source("conv_a", "turn-lease")],
            )],
            32,
        );
        current_commit.lease_owner = "worker-new".into();
        current_commit.lease_token = "lease-worker-new-31".into();
        current_commit.expected_attempt_count = 0;
        assert!(matches!(
            repo.commit_update(current_commit).await.unwrap(),
            CommitMemoryUpdateResult::Committed { .. }
        ));
    }

    #[tokio::test]
    async fn sqlite_memory_expected_revision_rejects_stale_transaction_without_partial_writes() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-1", "conv_a", "turn-1").await;
        let result = repo
            .commit_update(commit(
                "job-1",
                "conv_a",
                "turn-1",
                0,
                vec![entry("entry-1", "fp-1", vec![source("conv_a", "turn-1")])],
                20,
            ))
            .await
            .unwrap();
        assert!(matches!(
            result,
            CommitMemoryUpdateResult::Committed { revision: 1, .. }
        ));

        claimed_job(&repo, "job-2", "conv_a", "turn-2").await;
        sqlx::query("UPDATE conversation_memories SET revision = 2 WHERE user_id = ? AND conversation_id = 'conv_a'")
            .bind(USER_A)
            .execute(&repo.pool)
            .await
            .unwrap();
        let stale = repo
            .commit_update(commit(
                "job-2",
                "conv_a",
                "turn-2",
                1,
                vec![entry("stale-entry", "fp-stale", vec![source("conv_a", "turn-2")])],
                30,
            ))
            .await
            .unwrap();
        assert_eq!(stale, CommitMemoryUpdateResult::StaleRevision { current_revision: 2 });
        assert!(repo.get_entry(USER_A, "stale-entry").await.unwrap().is_none());
        assert_eq!(repo.get_job(USER_A, "job-2").await.unwrap().unwrap().state, "running");
    }

    #[tokio::test]
    async fn sqlite_memory_source_deletion_removes_exclusive_automatic_entries_only() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-source", "conv_a", "turn-1").await;
        sqlx::query(
            "INSERT INTO messages
                (id, conversation_id, turn_id, type, content, position, status, hidden, created_at)
             VALUES ('msg-turn-2', 'conv_a', 'turn-2', 'text', '{}', 'right', 'finish', 0, 11),
                    ('msg-shared-turn', 'conv_a2', 'shared-turn', 'text', '{}', 'right', 'finish', 0, 11)",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        repo.commit_update(commit(
            "job-source",
            "conv_a",
            "turn-1",
            0,
            vec![
                entry("exclusive", "fp-exclusive", vec![source("conv_a", "turn-1")]),
                entry(
                    "same-conversation-multi-turn",
                    "fp-same-conversation",
                    vec![source("conv_a", "turn-1"), source("conv_a", "turn-2")],
                ),
                entry(
                    "shared",
                    "fp-shared",
                    vec![source("conv_a", "turn-1"), source("conv_a2", "shared-turn")],
                ),
            ],
            20,
        ))
        .await
        .unwrap();

        repo.delete_conversation_memory(USER_A, "conv_a", 30).await.unwrap();
        assert!(repo.get_entry(USER_A, "exclusive").await.unwrap().is_none());
        assert!(
            repo.get_entry(USER_A, "same-conversation-multi-turn")
                .await
                .unwrap()
                .is_none()
        );
        let shared = repo.get_entry(USER_A, "shared").await.unwrap().unwrap();
        assert_eq!(shared.sources.len(), 1);
        assert_eq!(shared.sources[0].conversation_id, "conv_a2");
    }

    #[tokio::test]
    async fn sqlite_memory_tombstones_are_content_free_and_block_matching_fingerprints() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-delete", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-delete",
            "conv_a",
            "turn-1",
            0,
            vec![entry("entry-delete", "fp-deleted", vec![source("conv_a", "turn-1")])],
            20,
        ))
        .await
        .unwrap();
        repo.delete_entry(USER_A, "entry-delete", 25).await.unwrap();
        let tombstone = repo.get_entry(USER_A, "entry-delete").await.unwrap().unwrap();
        assert_eq!(tombstone.state, "deleted");
        assert_eq!(tombstone.content, None);
        assert!(tombstone.sources.is_empty());
        assert!(matches!(
            repo.update_entry(UpdateMemoryEntryRow {
                user_id: USER_A.into(),
                id: "entry-delete".into(),
                content: Some("must stay deleted".into()),
                pinned: None,
                project_id: None,
                workspace_key: None,
                now: 26,
            })
            .await,
            Err(DbError::Conflict(_))
        ));

        claimed_job(&repo, "job-readd", "conv_a", "turn-2").await;
        let result = repo
            .commit_update(commit(
                "job-readd",
                "conv_a",
                "turn-2",
                1,
                vec![entry("new-id", "fp-deleted", vec![source("conv_a", "turn-2")])],
                30,
            ))
            .await
            .unwrap();
        assert!(matches!(result, CommitMemoryUpdateResult::Committed { ref added_ids, .. } if added_ids.is_empty()));
        assert!(repo.get_entry(USER_A, "new-id").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_memory_update_entry_rejects_zero_row_cas_after_tombstone() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-update-race", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-update-race",
            "conv_a",
            "turn-1",
            0,
            vec![entry(
                "entry-update-race",
                "fp-update-race",
                vec![source("conv_a", "turn-1")],
            )],
            20,
        ))
        .await
        .unwrap();
        sqlx::query(
            "CREATE TRIGGER tombstone_before_memory_update
             BEFORE UPDATE ON memory_entries
             WHEN OLD.id = 'entry-update-race' AND OLD.state = 'active' AND NEW.state <> 'deleted'
             BEGIN
                 DELETE FROM memory_sources WHERE memory_entry_id = OLD.id;
                 UPDATE memory_entries SET content = NULL, state = 'deleted', pinned = 0, user_edited = 0,
                     supersedes_id = NULL, conflict_group_id = NULL, deleted_at = 25, updated_at = 25
                     WHERE id = OLD.id;
                 SELECT RAISE(IGNORE);
             END",
        )
        .execute(&repo.pool)
        .await
        .unwrap();

        assert!(matches!(
            repo.update_entry(UpdateMemoryEntryRow {
                user_id: USER_A.into(),
                id: "entry-update-race".into(),
                content: Some("must not report success".into()),
                pinned: None,
                project_id: None,
                workspace_key: None,
                now: 26,
            })
            .await,
            Err(DbError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn sqlite_memory_global_clear_removes_content_and_advances_reset_atomically() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-clear", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-clear",
            "conv_a",
            "turn-1",
            0,
            vec![entry("entry-clear", "fp-clear", vec![source("conv_a", "turn-1")])],
            20,
        ))
        .await
        .unwrap();
        repo.delete_entry(USER_A, "entry-clear", 21).await.unwrap();
        repo.enqueue_completed_turn(enqueue("job-pending", "conv_a2", "turn-2", 22))
            .await
            .unwrap();

        repo.clear_memory(USER_A, 50).await.unwrap();
        assert_eq!(repo.get_settings(USER_A).await.unwrap().reset_at, Some(50));
        assert!(repo.list_entries(USER_A).await.unwrap().is_empty());
        assert!(repo.get_conversation_memory(USER_A, "conv_a").await.unwrap().is_none());
        assert!(repo.get_job(USER_A, "job-clear").await.unwrap().is_none());
        assert!(repo.get_job(USER_A, "job-pending").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_memory_applies_explicit_supersede_and_conflict_transitions_to_change_set() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-base", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-base",
            "conv_a",
            "turn-1",
            0,
            vec![
                entry("old-decision", "fp-old-decision", vec![source("conv_a", "turn-1")]),
                entry("old-issue", "fp-old-issue", vec![source("conv_a", "turn-1")]),
            ],
            20,
        ))
        .await
        .unwrap();

        claimed_job(&repo, "job-transition", "conv_a", "turn-2").await;
        let mut replacement = entry("new-decision", "fp-new-decision", vec![source("conv_a", "turn-2")]);
        replacement.transition = CommitMemoryEntryTransition::Supersede {
            target_entry_id: "old-decision".into(),
        };
        let mut contradiction = entry("new-issue", "fp-new-issue", vec![source("conv_a", "turn-2")]);
        contradiction.transition = CommitMemoryEntryTransition::Conflict {
            target_entry_id: "old-issue".into(),
            conflict_group_id: "conflict-1".into(),
        };
        repo.commit_update(commit(
            "job-transition",
            "conv_a",
            "turn-2",
            1,
            vec![replacement, contradiction],
            30,
        ))
        .await
        .unwrap();

        let old_decision = repo.get_entry(USER_A, "old-decision").await.unwrap().unwrap();
        let new_decision = repo.get_entry(USER_A, "new-decision").await.unwrap().unwrap();
        assert_eq!(old_decision.state, "superseded");
        assert_eq!(new_decision.supersedes_id.as_deref(), Some("old-decision"));
        let old_issue = repo.get_entry(USER_A, "old-issue").await.unwrap().unwrap();
        let new_issue = repo.get_entry(USER_A, "new-issue").await.unwrap().unwrap();
        assert_eq!(old_issue.state, "conflict");
        assert_eq!(new_issue.state, "conflict");
        assert_eq!(new_issue.conflict_group_id.as_deref(), Some("conflict-1"));

        let changes = repo.list_change_sets(USER_A, 10).await.unwrap();
        let changes = changes.iter().find(|row| row.id == "changes-job-transition").unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&changes.superseded_ids_json).unwrap(),
            ["old-decision"]
        );
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&changes.conflict_ids_json).unwrap(),
            ["old-issue", "new-issue"]
        );
    }

    #[tokio::test]
    async fn sqlite_memory_protected_targets_reject_mutation_and_keep_conflicts_active() {
        for (protection, pinned, edited_content) in [
            ("pinned", Some(true), None),
            ("user-edited", None, Some("protected user content")),
        ] {
            for transition in ["refine", "supersede", "conflict"] {
                let (repo, _, _db) = setup().await;
                let target_id = format!("{protection}-{transition}-target");
                let target_fingerprint = format!("fp-{target_id}");
                claimed_job(&repo, "job-protected-base", "conv_a", "turn-1").await;
                repo.commit_update(commit(
                    "job-protected-base",
                    "conv_a",
                    "turn-1",
                    0,
                    vec![entry(&target_id, &target_fingerprint, vec![source("conv_a", "turn-1")])],
                    20,
                ))
                .await
                .unwrap();
                let protected = repo
                    .update_entry(UpdateMemoryEntryRow {
                        user_id: USER_A.into(),
                        id: target_id.clone(),
                        content: edited_content.map(str::to_owned),
                        pinned,
                        project_id: None,
                        workspace_key: None,
                        now: 21,
                    })
                    .await
                    .unwrap();
                let protected_content = protected.content.clone();

                let job_id = format!("job-{protection}-{transition}");
                claimed_job(&repo, &job_id, "conv_a", "turn-2").await;
                let candidate_id = format!("{protection}-{transition}-candidate");
                let mut candidate = entry(
                    &candidate_id,
                    &format!("fp-{candidate_id}"),
                    vec![source("conv_a", "turn-2")],
                );
                candidate.content = format!("automatic {transition} content");
                candidate.transition = match transition {
                    "refine" => CommitMemoryEntryTransition::Refine {
                        target_entry_id: target_id.clone(),
                    },
                    "supersede" => CommitMemoryEntryTransition::Supersede {
                        target_entry_id: target_id.clone(),
                    },
                    "conflict" => CommitMemoryEntryTransition::Conflict {
                        target_entry_id: target_id.clone(),
                        conflict_group_id: format!("group-{protection}"),
                    },
                    _ => unreachable!(),
                };
                let result = repo
                    .commit_update(commit(&job_id, "conv_a", "turn-2", 1, vec![candidate], 30))
                    .await;

                let target = repo.get_entry(USER_A, &target_id).await.unwrap().unwrap();
                assert_eq!(target.state, "active", "{protection} {transition}");
                assert_eq!(target.content, protected_content, "{protection} {transition}");
                assert_eq!(target.pinned, pinned == Some(true), "{protection} {transition}");
                assert_eq!(
                    target.user_edited,
                    edited_content.is_some(),
                    "{protection} {transition}"
                );
                assert_eq!(target.conflict_group_id, None, "{protection} {transition}");

                if transition == "conflict" {
                    let committed = result.unwrap();
                    assert!(matches!(
                        committed,
                        CommitMemoryUpdateResult::Committed {
                            ref conflict_ids,
                            ..
                        } if conflict_ids == std::slice::from_ref(&candidate_id)
                    ));
                    let candidate = repo.get_entry(USER_A, &candidate_id).await.unwrap().unwrap();
                    assert_eq!(candidate.state, "conflict");
                    assert_eq!(
                        candidate.conflict_group_id.as_deref(),
                        Some(format!("group-{protection}").as_str())
                    );
                    assert_eq!(
                        repo.get_conversation_memory(USER_A, "conv_a")
                            .await
                            .unwrap()
                            .unwrap()
                            .revision,
                        2
                    );
                } else {
                    assert!(matches!(result, Err(DbError::Conflict(_))), "{protection} {transition}");
                    assert!(repo.get_entry(USER_A, &candidate_id).await.unwrap().is_none());
                    assert_eq!(
                        repo.get_conversation_memory(USER_A, "conv_a")
                            .await
                            .unwrap()
                            .unwrap()
                            .revision,
                        1
                    );
                    assert_eq!(repo.get_job(USER_A, &job_id).await.unwrap().unwrap().state, "running");
                    assert_eq!(repo.list_change_sets(USER_A, 10).await.unwrap().len(), 1);
                }
            }
        }
    }

    #[tokio::test]
    async fn sqlite_memory_rejects_foreign_transition_targets_and_noncanonical_sources_atomically() {
        let (repo, _, db) = setup().await;
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                 schema_version, created_at, updated_at)
             VALUES ('foreign-entry', ?, 'decision', 'foreign', 'fp-foreign', 'foreign', 'active', 0, 0, 1, 1, 1)",
        )
        .bind(USER_B)
        .execute(db.pool())
        .await
        .unwrap();

        claimed_job(&repo, "job-foreign-target", "conv_a", "turn-1").await;
        let mut foreign_target = entry("candidate", "fp-candidate", vec![source("conv_a", "turn-1")]);
        foreign_target.transition = CommitMemoryEntryTransition::Supersede {
            target_entry_id: "foreign-entry".into(),
        };
        assert!(matches!(
            repo.commit_update(commit(
                "job-foreign-target",
                "conv_a",
                "turn-1",
                0,
                vec![foreign_target],
                20,
            ))
            .await,
            Err(DbError::NotFound(_))
        ));
        assert!(repo.get_conversation_memory(USER_A, "conv_a").await.unwrap().is_none());

        let mut bad_source = entry("bad-source", "fp-bad-source", vec![source("conv_b", "turn-1")]);
        bad_source.sources[0].message_ids_json = r#"["missing-message"]"#.into();
        assert!(matches!(
            repo.commit_update(commit(
                "job-foreign-target",
                "conv_a",
                "turn-1",
                0,
                vec![bad_source],
                21,
            ))
            .await,
            Err(DbError::NotFound(_))
        ));
        assert!(repo.get_entry(USER_A, "bad-source").await.unwrap().is_none());

        let missing_turn = entry(
            "missing-turn-source",
            "fp-missing-turn",
            vec![source("conv_a", "missing-turn")],
        );
        assert!(matches!(
            repo.commit_update(commit(
                "job-foreign-target",
                "conv_a",
                "turn-1",
                0,
                vec![missing_turn],
                22,
            ))
            .await,
            Err(DbError::NotFound(_))
        ));

        sqlx::query(
            "INSERT INTO messages
                (id, conversation_id, turn_id, type, content, position, status, hidden, created_at)
             VALUES ('foreign-message', 'conv_b', 'foreign-turn', 'text', '{}', 'right', 'finish', 0, 10)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let mut foreign_message = entry(
            "foreign-message-source",
            "fp-foreign-message",
            vec![source("conv_a", "turn-1")],
        );
        foreign_message.sources[0].message_ids_json = r#"["foreign-message"]"#.into();
        assert!(matches!(
            repo.commit_update(commit(
                "job-foreign-target",
                "conv_a",
                "turn-1",
                0,
                vec![foreign_message],
                23,
            ))
            .await,
            Err(DbError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn sqlite_memory_candidate_query_returns_sql_ordered_bounded_window() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-candidates", "conv_a", "turn-1").await;
        let mut exact_project = entry("exact-project", "fp-project", vec![source("conv_a", "turn-1")]);
        exact_project.project_id = Some("project-1".into());
        exact_project.content = "no shared prompt tokens".into();
        let mut exact_workspace = entry("exact-workspace", "fp-workspace", vec![source("conv_a", "turn-1")]);
        exact_workspace.workspace_key = Some("workspace-1".into());
        exact_workspace.content = "needle needle needle".into();
        let mut global = entry("global", "fp-global", vec![source("conv_a", "turn-1")]);
        global.content = "needle".into();
        repo.commit_update(commit(
            "job-candidates",
            "conv_a",
            "turn-1",
            0,
            vec![global, exact_workspace, exact_project],
            20,
        ))
        .await
        .unwrap();

        let candidates = repo
            .retrieval_candidates(MemoryCandidateQueryRow {
                user_id: USER_A.into(),
                project_id: Some("project-1".into()),
                workspace_key: Some("workspace-1".into()),
                limit: 2,
            })
            .await
            .unwrap();
        assert_eq!(
            candidates.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["exact-project", "exact-workspace"]
        );
    }

    #[tokio::test]
    async fn sqlite_memory_rejects_cross_user_resource_ids() {
        let (repo, _, _db) = setup().await;
        assert!(matches!(
            repo.effective_policy(USER_A, "conv_b").await,
            Err(DbError::NotFound(_))
        ));
        let mut other_enqueue = enqueue("foreign-job", "conv_b", "turn-1", 10);
        other_enqueue.user_id = USER_A.into();
        assert!(matches!(
            repo.enqueue_completed_turn(other_enqueue).await,
            Err(DbError::NotFound(_))
        ));

        claimed_job(&repo, "owned-job", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "owned-job",
            "conv_a",
            "turn-1",
            0,
            vec![entry("owned-entry", "fp-owned", vec![source("conv_a", "turn-1")])],
            20,
        ))
        .await
        .unwrap();
        assert!(matches!(
            repo.get_entry(USER_B, "owned-entry").await,
            Err(DbError::NotFound(_))
        ));
        assert!(matches!(
            repo.delete_entry(USER_B, "owned-entry", 30).await,
            Err(DbError::NotFound(_))
        ));
        assert!(matches!(
            repo.get_job(USER_B, "owned-job").await,
            Err(DbError::NotFound(_))
        ));
        assert!(matches!(
            repo.delete_conversation_memory(USER_B, "conv_a", 30).await,
            Err(DbError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn sqlite_memory_messages_query_exact_turn_id_and_legacy_null_rows_remain_valid() {
        let (_memory, conversations, _db) = setup().await;
        for (id, turn_id) in [("msg-1", Some("turn-1")), ("msg-2", Some("turn-2")), ("legacy", None)] {
            conversations
                .insert_message(&MessageRow {
                    id: id.into(),
                    conversation_id: "conv_a".into(),
                    turn_id: turn_id.map(str::to_owned),
                    msg_id: None,
                    r#type: "text".into(),
                    content: "{}".into(),
                    position: Some("right".into()),
                    status: Some("finish".into()),
                    hidden: false,
                    created_at: 10,
                })
                .await
                .unwrap();
        }

        let exact = conversations
            .list_messages_by_turn(USER_A, "conv_a", "turn-1")
            .await
            .unwrap();
        assert_eq!(exact.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), ["msg-1"]);
        let legacy = conversations.get_message("conv_a", "legacy").await.unwrap().unwrap();
        assert_eq!(legacy.turn_id, None);
        assert!(matches!(
            conversations.list_messages_by_turn(USER_B, "conv_a", "turn-1").await,
            Err(DbError::NotFound(_))
        ));
    }
}
