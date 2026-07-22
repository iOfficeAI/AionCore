use std::cmp::Reverse;
use std::collections::HashSet;

use sqlx::{SqliteConnection, SqlitePool};
use unicode_normalization::UnicodeNormalization;

use crate::DbError;
use crate::models::{
    ConversationMemoryRow, EffectiveMemoryPolicyRow, MemoryChangeSetRow, MemoryEntryDbRow, MemoryEntryRow,
    MemoryImportStateRow, MemoryJobRow, MemoryRetrievalRow, MemorySettingsRow, MemorySourceRow,
};
use crate::repository::memory::{
    ClaimMemoryJobRow, CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow, IMemoryRepository,
    MemoryCandidateQueryRow, MemoryEntryQueryRow, RenewMemoryLeaseRow, UpdateConversationMemoryPolicyRow,
    UpdateMemoryEntryRow, UpdateMemorySettingsRow,
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

    async fn entry_rows_with_sources(&self, rows: Vec<MemoryEntryDbRow>) -> Result<Vec<MemoryEntryRow>, DbError> {
        let mut entries = Vec::with_capacity(rows.len());
        for row in rows {
            entries.push(self.entry_with_sources(row).await?);
        }
        Ok(entries)
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

fn normalized_tokens(value: &str) -> HashSet<String> {
    value
        .nfkc()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_owned)
        .collect()
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
            let duplicate: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM memory_jobs
                 WHERE user_id = ? AND conversation_id = ? AND through_turn_id = ? AND operation_version = ?)",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&input.through_turn_id)
            .bind(&input.operation_version)
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
                    "UPDATE memory_jobs SET through_turn_id = ?, operation_version = ?, input_hash = ?,
                     expected_revision = ?, state = 'pending', next_attempt_at = NULL, last_error_code = NULL, updated_at = ?
                     WHERE id = ? AND user_id = ?",
                )
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
                    (id, user_id, conversation_id, from_turn_id, through_turn_id, operation_version, input_hash,
                     expected_revision, state, attempt_count, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', 0, ?, ?)",
            )
            .bind(&input.id)
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&input.from_turn_id)
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
                "UPDATE memory_jobs SET state = 'running', attempt_count = attempt_count + 1,
                 expected_revision = COALESCE((
                    SELECT revision FROM conversation_memories
                    WHERE user_id = memory_jobs.user_id AND conversation_id = memory_jobs.conversation_id
                 ), 0),
                 lease_owner = ?, lease_expires_at = ?, next_attempt_at = NULL, updated_at = ?
                 WHERE id = ? AND user_id = ?",
            )
            .bind(&input.worker_id)
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

    async fn renew_lease(&self, input: RenewMemoryLeaseRow) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE memory_jobs SET lease_expires_at = ?, updated_at = ?
             WHERE id = ? AND user_id = ? AND state = 'running' AND lease_owner = ? AND lease_expires_at > ?",
        )
        .bind(input.now + input.lease_duration_ms)
        .bind(input.now)
        .bind(&input.job_id)
        .bind(&input.user_id)
        .bind(&input.worker_id)
        .bind(input.now)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
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
            let job: Option<(String, String)> = sqlx::query_as(
                "SELECT conversation_id, state FROM memory_jobs WHERE id = ? AND user_id = ?",
            )
            .bind(&input.job_id)
            .bind(&input.user_id)
            .fetch_optional(&mut *connection)
            .await?;
            if !matches!(job, Some((ref conversation_id, ref state)) if conversation_id == &input.conversation_id && state == "running") {
                return Err(DbError::NotFound(format!("Running Memory job '{}' not found", input.job_id)));
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

                let existing: Option<(String, bool, bool)> = sqlx::query_as(
                    "SELECT id, pinned, user_edited FROM memory_entries
                     WHERE user_id = ? AND fingerprint = ? AND state = 'active' ORDER BY created_at LIMIT 1",
                )
                .bind(&input.user_id)
                .bind(&entry.fingerprint)
                .fetch_optional(&mut *connection)
                .await?;
                let entry_id = if let Some((existing_id, pinned, user_edited)) = existing {
                    if !pinned && !user_edited {
                        sqlx::query(
                            "UPDATE memory_entries SET project_id = ?, workspace_key = ?, kind = ?, stable_key = ?,
                             content = ?, supersedes_id = ?, conflict_group_id = ?, updated_at = ?
                             WHERE id = ? AND user_id = ?",
                        )
                        .bind(&entry.project_id)
                        .bind(&entry.workspace_key)
                        .bind(&entry.kind)
                        .bind(&entry.stable_key)
                        .bind(&entry.content)
                        .bind(&entry.supersedes_id)
                        .bind(&entry.conflict_group_id)
                        .bind(input.now)
                        .bind(&existing_id)
                        .bind(&input.user_id)
                        .execute(&mut *connection)
                        .await?;
                    }
                    refined_ids.push(existing_id.clone());
                    existing_id
                } else {
                    sqlx::query(
                        "INSERT INTO memory_entries
                            (id, user_id, project_id, workspace_key, kind, stable_key, fingerprint, content, state,
                             pinned, user_edited, supersedes_id, conflict_group_id, schema_version, created_at, updated_at)
                         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'active', 0, 0, ?, ?, ?, ?, ?)",
                    )
                    .bind(&entry.id)
                    .bind(&input.user_id)
                    .bind(&entry.project_id)
                    .bind(&entry.workspace_key)
                    .bind(&entry.kind)
                    .bind(&entry.stable_key)
                    .bind(&entry.fingerprint)
                    .bind(&entry.content)
                    .bind(&entry.supersedes_id)
                    .bind(&entry.conflict_group_id)
                    .bind(input.schema_version)
                    .bind(input.now)
                    .bind(input.now)
                    .execute(&mut *connection)
                    .await?;
                    added_ids.push(entry.id.clone());
                    entry.id.clone()
                };

                for source in &entry.sources {
                    Self::ensure_conversation_on(&mut connection, &input.user_id, &source.conversation_id).await?;
                    sqlx::query(
                        "INSERT INTO memory_sources
                            (memory_entry_id, conversation_id, turn_id, message_ids_json, first_observed_at, last_observed_at)
                         VALUES (?, ?, ?, ?, ?, ?)
                         ON CONFLICT(memory_entry_id, conversation_id, turn_id) DO UPDATE SET
                            message_ids_json = excluded.message_ids_json,
                            last_observed_at = excluded.last_observed_at",
                    )
                    .bind(&entry_id)
                    .bind(&source.conversation_id)
                    .bind(&source.turn_id)
                    .bind(&source.message_ids_json)
                    .bind(input.now)
                    .bind(input.now)
                    .execute(&mut *connection)
                    .await?;
                }
            }

            sqlx::query(
                "INSERT INTO memory_change_sets
                    (id, user_id, conversation_id, through_turn_id, job_id, added_ids_json, refined_ids_json,
                     superseded_ids_json, conflict_ids_json, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, '[]', '[]', ?)",
            )
            .bind(&input.change_set_id)
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&input.through_turn_id)
            .bind(&input.job_id)
            .bind(serde_json::to_string(&added_ids).map_err(|error| DbError::Init(error.to_string()))?)
            .bind(serde_json::to_string(&refined_ids).map_err(|error| DbError::Init(error.to_string()))?)
            .bind(input.now)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "UPDATE memory_jobs SET state = 'succeeded', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
                 WHERE id = ? AND user_id = ? AND state = 'running'",
            )
            .bind(input.now)
            .bind(&input.job_id)
            .bind(&input.user_id)
            .execute(&mut *connection)
            .await?;

            Ok(CommitMemoryUpdateResult::Committed {
                revision,
                added_ids,
                refined_ids,
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
        self.get_entry(&input.user_id, &input.id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Memory entry '{}' not found", input.id)))?;
        let project_present = input.project_id.is_some();
        let project_id = input.project_id.flatten();
        let workspace_present = input.workspace_key.is_some();
        let workspace_key = input.workspace_key.flatten();
        sqlx::query(
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
        .execute(&self.pool)
        .await?;
        self.get_entry(&input.user_id, &input.id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Memory entry '{}' not found", input.id)))
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
                   AND (SELECT COUNT(*) FROM memory_sources all_sources
                        WHERE all_sources.memory_entry_id = entries.id) = 1",
            )
            .bind(user_id)
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
                "UPDATE memory_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
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
            ] {
                sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ?"))
                    .bind(user_id)
                    .execute(&mut *connection)
                    .await?;
            }
            sqlx::query(
                "UPDATE memory_jobs SET state = 'canceled', lease_owner = NULL, lease_expires_at = NULL, updated_at = ?
                 WHERE user_id = ? AND state NOT IN ('succeeded', 'failed', 'canceled')",
            )
            .bind(now)
            .bind(user_id)
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
        .bind(MAX_MEMORY_CANDIDATES)
        .fetch_all(&self.pool)
        .await?;
        let prompt_tokens = normalized_tokens(&query.prompt);
        let mut scored = rows
            .into_iter()
            .enumerate()
            .map(|(position, row)| {
                let entry_tokens = normalized_tokens(row.content.as_deref().unwrap_or_default());
                let overlap = prompt_tokens.intersection(&entry_tokens).count();
                (Reverse(overlap), position, row)
            })
            .collect::<Vec<_>>();
        scored.sort_by_key(|(score, position, _)| (*score, *position));
        let rows = scored
            .into_iter()
            .take(query.limit.clamp(1, MAX_MEMORY_CANDIDATES) as usize)
            .map(|(_, _, row)| row)
            .collect();
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
        ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemorySourceRow, CommitMemoryUpdateResult,
        CommitMemoryUpdateRow, EnqueueMemoryTurnRow, RenewMemoryLeaseRow, UpdateConversationMemoryPolicyRow,
        UpdateMemorySettingsRow,
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
            through_turn_id: through_turn_id.into(),
            operation_version: "memory-v1".into(),
            input_hash: format!("hash-{through_turn_id}"),
            expected_revision: 0,
            now,
        }
    }

    fn claim(user_id: &str, worker_id: &str, now: i64) -> ClaimMemoryJobRow {
        ClaimMemoryJobRow {
            user_id: user_id.into(),
            worker_id: worker_id.into(),
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
            supersedes_id: None,
            conflict_group_id: None,
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
            entries,
            change_set_id: format!("changes-{job_id}"),
            now,
        }
    }

    async fn claimed_job(repo: &SqliteMemoryRepository, job_id: &str, conversation_id: &str, turn_id: &str) {
        repo.enqueue_completed_turn(enqueue(job_id, conversation_id, turn_id, 10))
            .await
            .unwrap();
        let claimed = repo.claim_next_job(claim(USER_A, "worker", 11)).await.unwrap().unwrap();
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
        assert_eq!(reclaimed.attempt_count, 2);
        assert!(
            repo.renew_lease(RenewMemoryLeaseRow {
                user_id: USER_A.into(),
                job_id: "job-lease".into(),
                worker_id: "worker-b".into(),
                now: 32,
                lease_duration_ms: 10,
            })
            .await
            .unwrap()
        );
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
        let stale = repo
            .commit_update(commit(
                "job-2",
                "conv_a",
                "turn-2",
                0,
                vec![entry("stale-entry", "fp-stale", vec![source("conv_a", "turn-2")])],
                30,
            ))
            .await
            .unwrap();
        assert_eq!(stale, CommitMemoryUpdateResult::StaleRevision { current_revision: 1 });
        assert!(repo.get_entry(USER_A, "stale-entry").await.unwrap().is_none());
        assert_eq!(repo.get_job(USER_A, "job-2").await.unwrap().unwrap().state, "running");
    }

    #[tokio::test]
    async fn sqlite_memory_source_deletion_removes_exclusive_automatic_entries_only() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-source", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-source",
            "conv_a",
            "turn-1",
            0,
            vec![
                entry("exclusive", "fp-exclusive", vec![source("conv_a", "turn-1")]),
                entry(
                    "shared",
                    "fp-shared",
                    vec![source("conv_a", "turn-1"), source("conv_a2", "turn-2")],
                ),
            ],
            20,
        ))
        .await
        .unwrap();

        repo.delete_conversation_memory(USER_A, "conv_a", 30).await.unwrap();
        assert!(repo.get_entry(USER_A, "exclusive").await.unwrap().is_none());
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
        assert_eq!(
            repo.get_job(USER_A, "job-pending").await.unwrap().unwrap().state,
            "canceled"
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
