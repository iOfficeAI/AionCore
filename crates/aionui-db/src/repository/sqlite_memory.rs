use std::collections::HashSet;

use aionui_common::TimestampMs;
use sha2::{Digest, Sha256};
use sqlx::{SqliteConnection, SqlitePool};

struct InsertEntryOptions<'a> {
    state: &'a str,
    supersedes_id: Option<&'a str>,
    conflict_group_id: Option<&'a str>,
}

use crate::DbError;
use crate::models::{
    ConversationMemoryPolicyRow, ConversationMemoryRow, EffectiveMemoryPolicyRow, MemoryChangeSetRow, MemoryEntryDbRow,
    MemoryEntryRow, MemoryImportStateRow, MemoryJobHealthRow, MemoryJobRow, MemoryJobTurnRow, MemoryRetrievalRow,
    MemorySettingsRow, MemorySourceRow, MessageRow,
};
use crate::repository::memory::{
    BoundedMemoryTurnMessagesRow, ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition,
    CommitMemorySourceRow, CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow,
    FinalizeMemoryJobSnapshotResult, FinalizeMemoryJobSnapshotRow, IMemoryRepository, MEMORY_EVIDENCE_MAX_BYTES,
    MEMORY_EVIDENCE_MAX_MESSAGES, MemoryCandidateQueryRow, MemoryChangeSetQueryRow, MemoryEntryQueryRow,
    MemoryReconciliationSnapshotRow, ReleaseMemoryLeaseRow, RenewMemoryLeaseRow, ResolveMemoryConflictActionRow,
    ResolveMemoryConflictRow, SplitMemoryJobRow, TransitionMemoryJobRow, UpdateConversationMemoryLifecycleRow,
    UpdateConversationMemoryPolicyRow, UpdateMemoryEntryRow, UpdateMemoryLifecycleRow, UpdateMemorySettingsRow,
    derive_memory_fingerprint, memory_entry_content_hash, memory_evidence_content,
};

const MAX_MEMORY_CANDIDATES: u32 = 200;
const QUEUE_DIGEST_MULTIPLIER: u128 = 0x100000001b3;
const TURN_SNAPSHOT_VERSION: &str = "memory-eligible-turn-snapshot-v2";
const ELIGIBLE_MESSAGES_CTE: &str = r#"
WITH candidates AS (
    SELECT id,conversation_id,turn_id,msg_id,type,position,status,hidden,created_at,
        CASE WHEN json_valid(content) THEN
            CASE WHEN type = 'tool_result_summary'
                THEN json_extract(content, '$.summary')
                ELSE json_extract(content, '$.content')
            END
        END AS accepted_content
    FROM messages
    WHERE conversation_id = ? AND turn_id = ? AND hidden = 0 AND status = 'finish'
      AND position IN ('left','right')
      AND type IN ('text','artifact','tool_result_summary')
), eligible AS (
    SELECT * FROM candidates
    WHERE typeof(accepted_content) = 'text'
      AND trim(accepted_content, char(
          9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,
          8232,8233,8239,8287,12288
      )) <> ''
)
"#;

#[derive(sqlx::FromRow)]
struct CanonicalMessageMetadataRow {
    id: String,
    msg_id: Option<String>,
    r#type: String,
    position: Option<String>,
    status: Option<String>,
    hidden: bool,
    created_at: i64,
    content_bytes: i64,
}

struct CanonicalTurnSnapshot {
    hash: String,
    message_count: i64,
    content_bytes: i64,
    earliest_all_at: Option<i64>,
    has_user_work: bool,
    has_assistant_outcome: bool,
    absolute_limit_exceeded: bool,
    messages: Vec<MessageRow>,
}

struct QueueTransition<'a> {
    state: &'a str,
    next_attempt_at: Option<TimestampMs>,
    error_code: Option<&'a str>,
    increment_attempt: bool,
    increment_invalid_output: bool,
    now: TimestampMs,
}

#[derive(Clone, Debug)]
pub struct SqliteMemoryRepository {
    pool: SqlitePool,
}

impl SqliteMemoryRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    fn queue_item_digest(turn_id: &str, turn_hash: &str) -> u128 {
        let digest =
            Sha256::digest(serde_json::to_vec(&(turn_id, turn_hash)).expect("serializing two strings cannot fail"));
        u128::from_be_bytes(digest[..16].try_into().expect("SHA-256 prefix is 16 bytes"))
    }

    fn parse_queue_digest(value: &str) -> Result<u128, DbError> {
        u128::from_str_radix(value, 16).map_err(|_| DbError::Conflict("Invalid Memory queue digest".into()))
    }

    fn queue_digest(value: u128) -> String {
        format!("{value:032x}")
    }

    fn queue_power(count: i64) -> Result<u128, DbError> {
        let count: u64 = count
            .try_into()
            .map_err(|_| DbError::Conflict("Invalid Memory queue length".into()))?;
        let mut base = QUEUE_DIGEST_MULTIPLIER;
        let mut exponent = count;
        let mut result = 1_u128;
        while exponent > 0 {
            if exponent & 1 == 1 {
                result = result.wrapping_mul(base);
            }
            base = base.wrapping_mul(base);
            exponent >>= 1;
        }
        Ok(result)
    }

    fn append_queue_digest(current: u128, turn_id: &str, turn_hash: &str) -> u128 {
        current
            .wrapping_mul(QUEUE_DIGEST_MULTIPLIER)
            .wrapping_add(Self::queue_item_digest(turn_id, turn_hash))
    }

    fn concat_queue_digest(left: u128, right: u128, right_count: i64) -> Result<u128, DbError> {
        Ok(left.wrapping_mul(Self::queue_power(right_count)?).wrapping_add(right))
    }

    fn input_hash(
        operation_version: &str,
        global_epoch: i64,
        conversation_epoch: i64,
        from_turn_id: Option<&str>,
        turn_count: i64,
        queue_digest: u128,
    ) -> Result<String, DbError> {
        let material = serde_json::to_vec(&serde_json::json!({
            "operation_version": operation_version,
            "global_epoch": global_epoch,
            "conversation_epoch": conversation_epoch,
            "from_turn_id": from_turn_id,
            "turn_count": turn_count,
            "queue_digest": Self::queue_digest(queue_digest),
        }))
        .map_err(|error| DbError::Init(error.to_string()))?;
        Ok(Sha256::digest(material)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect())
    }

    fn update_framed(hasher: &mut Sha256, value: &[u8]) {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value);
    }

    async fn canonical_turn_snapshot_on(
        connection: &mut SqliteConnection,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<CanonicalTurnSnapshot, DbError> {
        let metadata_sql = format!(
            "{ELIGIBLE_MESSAGES_CTE}
             SELECT id,msg_id,type,position,status,hidden,created_at,
                    length(CAST(accepted_content AS BLOB)) AS content_bytes
             FROM eligible ORDER BY created_at,id LIMIT ?",
        );
        let metadata: Vec<CanonicalMessageMetadataRow> = sqlx::query_as(&metadata_sql)
            .bind(conversation_id)
            .bind(turn_id)
            .bind((MEMORY_EVIDENCE_MAX_MESSAGES + 1) as i64)
            .fetch_all(&mut *connection)
            .await?;
        let content_bytes = metadata.iter().try_fold(0_i64, |total, row| {
            total
                .checked_add(row.content_bytes)
                .ok_or_else(|| DbError::Conflict("Message size overflow".into()))
        })?;
        let message_count: i64 = metadata
            .len()
            .try_into()
            .map_err(|_| DbError::Conflict("Message count overflow".into()))?;
        let absolute_limit_exceeded =
            metadata.len() > MEMORY_EVIDENCE_MAX_MESSAGES || content_bytes > MEMORY_EVIDENCE_MAX_BYTES as i64;
        let earliest_all_at: Option<i64> =
            sqlx::query_scalar("SELECT MIN(created_at) FROM messages WHERE conversation_id = ? AND turn_id = ?")
                .bind(conversation_id)
                .bind(turn_id)
                .fetch_one(&mut *connection)
                .await?;
        let has_user_work = absolute_limit_exceeded
            || metadata
                .iter()
                .any(|row| row.position.as_deref() == Some("right") && row.r#type == "text");
        let has_assistant_outcome =
            absolute_limit_exceeded || metadata.iter().any(|row| row.position.as_deref() == Some("left"));
        let messages = if absolute_limit_exceeded {
            Vec::new()
        } else {
            let messages_sql = format!(
                "{ELIGIBLE_MESSAGES_CTE}
                 SELECT id,conversation_id,turn_id,msg_id,type,
                        CASE WHEN type = 'tool_result_summary'
                            THEN json_object('summary',accepted_content)
                            ELSE json_object('content',accepted_content)
                        END AS content,
                        position,status,hidden,created_at
                 FROM eligible ORDER BY created_at,id LIMIT ?",
            );
            sqlx::query_as::<_, MessageRow>(&messages_sql)
                .bind(conversation_id)
                .bind(turn_id)
                .bind(MEMORY_EVIDENCE_MAX_MESSAGES as i64)
                .fetch_all(&mut *connection)
                .await?
        };
        if !absolute_limit_exceeded && messages.len() != metadata.len() {
            return Err(DbError::Conflict(
                "Canonical Memory snapshot changed while reading".into(),
            ));
        }

        let mut hasher = Sha256::new();
        Self::update_framed(&mut hasher, TURN_SNAPSHOT_VERSION.as_bytes());
        Self::update_framed(&mut hasher, conversation_id.as_bytes());
        Self::update_framed(&mut hasher, turn_id.as_bytes());
        Self::update_framed(
            &mut hasher,
            &serde_json::to_vec(&(message_count, content_bytes, absolute_limit_exceeded))
                .map_err(|error| DbError::Init(error.to_string()))?,
        );
        for (index, row) in metadata.iter().enumerate() {
            let structured = serde_json::to_vec(&(
                row.id.as_str(),
                row.msg_id.as_deref(),
                row.r#type.as_str(),
                row.position.as_deref(),
                row.status.as_deref(),
                row.hidden,
                row.created_at,
                row.content_bytes,
            ))
            .map_err(|error| DbError::Init(error.to_string()))?;
            Self::update_framed(&mut hasher, &structured);
            if let Some(message) = messages.get(index) {
                let content = memory_evidence_content(message)
                    .ok_or_else(|| DbError::Conflict("Canonical Memory evidence row became invalid".into()))?;
                Self::update_framed(&mut hasher, content.as_bytes());
            }
        }
        Ok(CanonicalTurnSnapshot {
            hash: hasher.finalize().iter().map(|byte| format!("{byte:02x}")).collect(),
            message_count,
            content_bytes,
            earliest_all_at,
            has_user_work,
            has_assistant_outcome,
            absolute_limit_exceeded,
            messages,
        })
    }

    fn conversation_is_excluded(kind: &str, source: Option<&str>, extra: &str) -> bool {
        let kind = kind.trim().to_ascii_lowercase();
        let source = source.unwrap_or_default().trim().to_ascii_lowercase();
        if matches!(
            kind.as_str(),
            "health_check" | "health-check" | "internal" | "ephemeral"
        ) || matches!(source.as_str(), "health_check" | "health-check" | "internal")
        {
            return true;
        }
        let Ok(serde_json::Value::Object(extra)) = serde_json::from_str(extra) else {
            return true;
        };
        ["health_check", "internal", "ephemeral"]
            .into_iter()
            .any(|key| extra.get(key).and_then(serde_json::Value::as_bool) == Some(true))
    }

    async fn job_snapshot_matches_on(connection: &mut SqliteConnection, job: &MemoryJobRow) -> Result<bool, DbError> {
        let turns: Vec<MemoryJobTurnRow> = sqlx::query_as(
            "SELECT job_id,position,turn_id,turn_hash FROM memory_job_turns
             WHERE job_id = ? ORDER BY position LIMIT 33",
        )
        .bind(&job.id)
        .fetch_all(&mut *connection)
        .await?;
        if turns.len() as i64 != job.turn_count || turns.len() > 32 {
            return Ok(false);
        }
        for turn in turns {
            let snapshot = Self::canonical_turn_snapshot_on(connection, &job.conversation_id, &turn.turn_id).await?;
            if snapshot.hash != turn.turn_hash {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn reconciliation_snapshot_matches_on(
        connection: &mut SqliteConnection,
        job: &MemoryJobRow,
    ) -> Result<bool, DbError> {
        let Some(snapshot_json) = job.reconciliation_snapshot_json.as_deref() else {
            return Ok(true);
        };
        let snapshot: Vec<MemoryReconciliationSnapshotRow> = serde_json::from_str(snapshot_json)
            .map_err(|error| DbError::Conflict(format!("Invalid Memory reconciliation snapshot: {error}")))?;
        if snapshot.len() > 64 {
            return Ok(false);
        }
        let mut ids = std::collections::HashSet::new();
        for expected in snapshot {
            if !ids.insert(expected.id.clone()) || expected.state != "active" {
                return Ok(false);
            }
            let current =
                sqlx::query_as::<_, MemoryEntryDbRow>("SELECT * FROM memory_entries WHERE id = ? AND user_id = ?")
                    .bind(&expected.id)
                    .bind(&job.user_id)
                    .fetch_optional(&mut *connection)
                    .await?;
            let Some(current) = current else {
                return Ok(false);
            };
            if current.revision != expected.revision
                || current.state != expected.state
                || current.fingerprint != expected.fingerprint
                || current.project_id != expected.project_id
                || current.workspace_key != expected.workspace_key
                || current.pinned != expected.pinned
                || current.user_edited != expected.user_edited
                || memory_entry_content_hash(current.content.as_deref()) != expected.content_hash
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn absorb_queued_successor_on(
        connection: &mut SqliteConnection,
        barrier: &MemoryJobRow,
        now: TimestampMs,
    ) -> Result<MemoryJobRow, DbError> {
        let mut combined = barrier.clone();
        loop {
            let successor: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ? AND id <> ?
                 AND state IN ('pending','retry_wait','blocked','running') ORDER BY created_at,id LIMIT 1",
            )
            .bind(&combined.user_id)
            .bind(&combined.conversation_id)
            .bind(&combined.id)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(successor) = successor else {
                return Ok(combined);
            };
            let turn_count = combined
                .turn_count
                .checked_add(successor.turn_count)
                .ok_or_else(|| DbError::Conflict("Memory queue length overflow".into()))?;
            let digest = Self::concat_queue_digest(
                Self::parse_queue_digest(&combined.queue_digest)?,
                Self::parse_queue_digest(&successor.queue_digest)?,
                successor.turn_count,
            )?;
            let input_hash = Self::input_hash(
                &combined.operation_version,
                combined.global_epoch,
                combined.conversation_epoch,
                combined.from_turn_id.as_deref(),
                turn_count,
                digest,
            )?;
            sqlx::query("UPDATE memory_job_turns SET job_id = ?,position = position + ? WHERE job_id = ?")
                .bind(&combined.id)
                .bind(combined.turn_count)
                .bind(&successor.id)
                .execute(&mut *connection)
                .await?;
            sqlx::query("DELETE FROM memory_jobs WHERE id = ?")
                .bind(&successor.id)
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "UPDATE memory_jobs SET through_turn_id = ?,turn_count = ?,queue_digest = ?,input_hash = ?,updated_at = ?
                 WHERE id = ?",
            )
            .bind(&successor.through_turn_id)
            .bind(turn_count)
            .bind(Self::queue_digest(digest))
            .bind(input_hash)
            .bind(now)
            .bind(&combined.id)
            .execute(&mut *connection)
            .await?;
            combined = sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                .bind(&combined.id)
                .fetch_one(&mut *connection)
                .await?;
        }
    }

    async fn transition_running_on(
        connection: &mut SqliteConnection,
        running: &MemoryJobRow,
        transition: QueueTransition<'_>,
    ) -> Result<MemoryJobRow, DbError> {
        let successor: Option<MemoryJobRow> =
            if matches!(transition.state, "pending" | "retry_wait" | "blocked" | "failed") {
                sqlx::query_as(
                    "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state IN ('pending','retry_wait','blocked') LIMIT 1",
                )
                .bind(&running.user_id)
                .bind(&running.conversation_id)
                .fetch_optional(&mut *connection)
                .await?
            } else {
                None
            };
        let attempt_count = running.attempt_count + i64::from(transition.increment_attempt);
        let invalid_output_count = running.invalid_output_count + i64::from(transition.increment_invalid_output);
        if let Some(successor) = successor {
            let combined_count = running
                .turn_count
                .checked_add(successor.turn_count)
                .ok_or_else(|| DbError::Conflict("Memory queue length overflow".into()))?;
            let digest = Self::concat_queue_digest(
                Self::parse_queue_digest(&running.queue_digest)?,
                Self::parse_queue_digest(&successor.queue_digest)?,
                successor.turn_count,
            )?;
            let input_hash = Self::input_hash(
                &running.operation_version,
                running.global_epoch,
                running.conversation_epoch,
                running.from_turn_id.as_deref(),
                combined_count,
                digest,
            )?;
            if transition.state == "failed" {
                sqlx::query("UPDATE memory_job_turns SET job_id = ?,position = position + ? WHERE job_id = ?")
                    .bind(&running.id)
                    .bind(running.turn_count)
                    .bind(&successor.id)
                    .execute(&mut *connection)
                    .await?;
                sqlx::query("DELETE FROM memory_jobs WHERE id = ?")
                    .bind(&successor.id)
                    .execute(&mut *connection)
                    .await?;
                sqlx::query(
                    "UPDATE memory_jobs SET through_turn_id = ?,turn_count = ?,queue_digest = ?,input_hash = ?,
                     state = 'failed',attempt_count = ?,invalid_output_count = ?,next_attempt_at = NULL,
                     last_error_code = ?,lease_owner = NULL,lease_token = NULL,lease_expires_at = NULL,
                     reconciliation_snapshot_json = NULL,updated_at = ?
                     WHERE id = ?",
                )
                .bind(&successor.through_turn_id)
                .bind(combined_count)
                .bind(Self::queue_digest(digest))
                .bind(input_hash)
                .bind(attempt_count)
                .bind(invalid_output_count)
                .bind(transition.error_code)
                .bind(transition.now)
                .bind(&running.id)
                .execute(&mut *connection)
                .await?;
                return Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                    .bind(&running.id)
                    .fetch_one(&mut *connection)
                    .await?);
            }
            let parking_offset = combined_count;
            sqlx::query("UPDATE memory_job_turns SET position = position + ? WHERE job_id = ?")
                .bind(parking_offset)
                .bind(&successor.id)
                .execute(&mut *connection)
                .await?;
            sqlx::query("UPDATE memory_job_turns SET job_id = ? WHERE job_id = ?")
                .bind(&successor.id)
                .bind(&running.id)
                .execute(&mut *connection)
                .await?;
            sqlx::query("UPDATE memory_job_turns SET position = position - ? + ? WHERE job_id = ? AND position >= ?")
                .bind(parking_offset)
                .bind(running.turn_count)
                .bind(&successor.id)
                .bind(parking_offset)
                .execute(&mut *connection)
                .await?;
            let turn_count = combined_count;
            sqlx::query(
                "UPDATE memory_jobs SET from_turn_id = ?, operation_version = ?, global_epoch = ?,
                 conversation_epoch = ?, turn_count = ?, queue_digest = ?, input_hash = ?, expected_revision = ?,
                 state = ?, attempt_count = ?, invalid_output_count = ?, next_attempt_at = ?, last_error_code = ?,
                 lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
                 reconciliation_snapshot_json = NULL, updated_at = ? WHERE id = ?",
            )
            .bind(&running.from_turn_id)
            .bind(&running.operation_version)
            .bind(running.global_epoch)
            .bind(running.conversation_epoch)
            .bind(turn_count)
            .bind(Self::queue_digest(digest))
            .bind(input_hash)
            .bind(running.expected_revision)
            .bind(transition.state)
            .bind(attempt_count)
            .bind(invalid_output_count)
            .bind(transition.next_attempt_at)
            .bind(transition.error_code)
            .bind(transition.now)
            .bind(&successor.id)
            .execute(&mut *connection)
            .await?;
            sqlx::query("DELETE FROM memory_jobs WHERE id = ?")
                .bind(&running.id)
                .execute(&mut *connection)
                .await?;
            return Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                .bind(&successor.id)
                .fetch_one(&mut *connection)
                .await?);
        }

        sqlx::query(
            "UPDATE memory_jobs SET state = ?, next_attempt_at = ?, last_error_code = ?, attempt_count = ?,
             invalid_output_count = ?, lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
             reconciliation_snapshot_json = NULL, updated_at = ?
             WHERE id = ?",
        )
        .bind(transition.state)
        .bind(transition.next_attempt_at)
        .bind(transition.error_code)
        .bind(attempt_count)
        .bind(invalid_output_count)
        .bind(transition.now)
        .bind(&running.id)
        .execute(&mut *connection)
        .await?;
        Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
            .bind(&running.id)
            .fetch_one(&mut *connection)
            .await?)
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

    async fn requeue_stale_reconciliation_on(
        connection: &mut SqliteConnection,
        job: &MemoryJobRow,
        now: i64,
    ) -> Result<CommitMemoryUpdateResult, DbError> {
        sqlx::query("ROLLBACK TO SAVEPOINT memory_reconciliation")
            .execute(&mut *connection)
            .await?;
        sqlx::query("RELEASE SAVEPOINT memory_reconciliation")
            .execute(&mut *connection)
            .await?;
        Self::transition_running_on(
            connection,
            job,
            QueueTransition {
                state: "pending",
                next_attempt_at: None,
                error_code: Some("stale_reconciliation"),
                increment_attempt: false,
                increment_invalid_output: false,
                now,
            },
        )
        .await?;
        Ok(CommitMemoryUpdateResult::StaleReconciliation)
    }

    async fn active_fingerprint_collision_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        fingerprint: &str,
        excluded_id: Option<&str>,
    ) -> Result<bool, DbError> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(
                SELECT 1 FROM memory_entries
                WHERE user_id = ? AND fingerprint = ? AND state = 'active'
                  AND (? IS NULL OR id <> ?)
            )",
        )
        .bind(user_id)
        .bind(fingerprint)
        .bind(excluded_id)
        .bind(excluded_id)
        .fetch_one(&mut *connection)
        .await?)
    }

    async fn ensure_transition_target_owner_on(
        connection: &mut SqliteConnection,
        user_id: &str,
        entry_id: &str,
    ) -> Result<(), DbError> {
        let owner: Option<String> = sqlx::query_scalar("SELECT user_id FROM memory_entries WHERE id = ?")
            .bind(entry_id)
            .fetch_optional(&mut *connection)
            .await?;
        if owner.as_deref() != Some(user_id) {
            return Err(DbError::NotFound(format!("Memory entry '{entry_id}' not found")));
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
        self.ensure_user(&command.user_id).await?;
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            sqlx::query(
                "INSERT INTO memory_settings (user_id,updated_at) VALUES (?,?) ON CONFLICT(user_id) DO NOTHING",
            )
            .bind(&command.user_id)
            .bind(command.now)
            .execute(&mut *connection)
            .await?;
            let current: MemorySettingsRow = sqlx::query_as("SELECT * FROM memory_settings WHERE user_id = ?")
                .bind(&command.user_id)
                .fetch_one(&mut *connection)
                .await?;
            let lifecycle_changed = command.enabled.is_some_and(|value| value != current.enabled)
                || command
                    .default_capture
                    .is_some_and(|value| value != current.default_capture)
                || command
                    .consent_version
                    .is_some_and(|value| Some(value) != current.consent_version);
            sqlx::query(
                "UPDATE memory_settings SET
                    enabled = COALESCE(?, enabled), default_capture = COALESCE(?, default_capture),
                    default_recall = COALESCE(?, default_recall), consent_version = COALESCE(?, consent_version),
                    consented_at = CASE WHEN ? IS NULL THEN consented_at ELSE ? END,
                    lifecycle_epoch = lifecycle_epoch + ?, updated_at = ? WHERE user_id = ?",
            )
            .bind(command.enabled)
            .bind(command.default_capture)
            .bind(command.default_recall)
            .bind(command.consent_version)
            .bind(command.consent_version)
            .bind(command.now)
            .bind(i64::from(lifecycle_changed))
            .bind(command.now)
            .bind(&command.user_id)
            .execute(&mut *connection)
            .await?;
            if lifecycle_changed {
                sqlx::query(
                    "UPDATE memory_jobs SET state = 'canceled',lease_owner = NULL,lease_token = NULL,
                     lease_expires_at = NULL,reconciliation_snapshot_json = NULL,next_attempt_at = NULL,
                     last_error_code = 'canceled',updated_at = ?
                     WHERE user_id = ? AND state IN ('pending','running','retry_wait','blocked','failed')",
                )
                .bind(command.now)
                .bind(&command.user_id)
                .execute(&mut *connection)
                .await?;
            }
            Ok(sqlx::query_as("SELECT * FROM memory_settings WHERE user_id = ?")
                .bind(&command.user_id)
                .fetch_one(&mut *connection)
                .await?)
        }
        .await;
        match result {
            Ok(row) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(row)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn effective_policy(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<EffectiveMemoryPolicyRow, DbError> {
        self.ensure_conversation(user_id, conversation_id).await?;
        let settings = self.get_settings(user_id).await?;
        let policy: Option<(Option<bool>, Option<bool>, Option<i64>, i64)> = sqlx::query_as(
            "SELECT capture_enabled, recall_enabled, reset_at, lifecycle_epoch
             FROM conversation_memory_policies WHERE user_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        let (capture_override, recall_override, conversation_reset, conversation_epoch) =
            policy.unwrap_or((None, None, None, 0));
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
            global_epoch: settings.lifecycle_epoch,
            conversation_epoch,
        })
    }

    async fn get_conversation_policy(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationMemoryPolicyRow, DbError> {
        self.ensure_conversation(user_id, conversation_id).await?;
        Ok(sqlx::query_as(
            "SELECT conversation_id, capture_enabled, recall_enabled, updated_at
             FROM conversation_memory_policies WHERE user_id = ? AND conversation_id = ?",
        )
        .bind(user_id)
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?
        .unwrap_or(ConversationMemoryPolicyRow {
            conversation_id: conversation_id.into(),
            capture_enabled: None,
            recall_enabled: None,
            updated_at: 0,
        }))
    }

    async fn update_conversation_policy(
        &self,
        command: UpdateConversationMemoryPolicyRow,
    ) -> Result<EffectiveMemoryPolicyRow, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            Self::ensure_conversation_on(&mut connection, &command.user_id, &command.conversation_id).await?;
            sqlx::query(
                "INSERT INTO memory_settings (user_id,updated_at) VALUES (?,?)
                 ON CONFLICT(user_id) DO NOTHING",
            )
            .bind(&command.user_id)
            .bind(command.now)
            .execute(&mut *connection)
            .await?;
            let default_capture: bool =
                sqlx::query_scalar("SELECT default_capture FROM memory_settings WHERE user_id = ?")
                    .bind(&command.user_id)
                    .fetch_one(&mut *connection)
                    .await?;
            let current_capture: Option<Option<bool>> = sqlx::query_scalar(
                "SELECT capture_enabled FROM conversation_memory_policies WHERE user_id = ? AND conversation_id = ?",
            )
            .bind(&command.user_id)
            .bind(&command.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            let previous_effective_capture = current_capture.flatten().unwrap_or(default_capture);
            let next_effective_capture = command.capture_enabled.unwrap_or(default_capture);
            let capture_changed = previous_effective_capture != next_effective_capture;
            sqlx::query(
                "INSERT INTO conversation_memory_policies
                    (user_id,conversation_id,capture_enabled,recall_enabled,lifecycle_epoch,updated_at)
                 VALUES (?,?,?,?,?,?) ON CONFLICT(user_id,conversation_id) DO UPDATE SET
                    capture_enabled = excluded.capture_enabled,
                    recall_enabled = excluded.recall_enabled,
                    lifecycle_epoch = conversation_memory_policies.lifecycle_epoch + ?,updated_at = excluded.updated_at",
            )
            .bind(&command.user_id)
            .bind(&command.conversation_id)
            .bind(command.capture_enabled)
            .bind(command.recall_enabled)
            .bind(i64::from(capture_changed))
            .bind(command.now)
            .bind(i64::from(capture_changed))
            .execute(&mut *connection)
            .await?;
            if capture_changed {
                sqlx::query(
                    "UPDATE memory_jobs SET state = 'canceled',lease_owner = NULL,lease_token = NULL,
                     lease_expires_at = NULL,reconciliation_snapshot_json = NULL,next_attempt_at = NULL,
                     last_error_code = 'canceled',updated_at = ?
                     WHERE user_id = ? AND conversation_id = ?
                     AND state IN ('pending','running','retry_wait','blocked','failed')",
                )
                .bind(command.now)
                .bind(&command.user_id)
                .bind(&command.conversation_id)
                .execute(&mut *connection)
                .await?;
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                return Err(error);
            }
        }
        drop(connection);
        self.effective_policy(&command.user_id, &command.conversation_id).await
    }

    async fn enqueue_completed_turn(&self, input: EnqueueMemoryTurnRow) -> Result<Option<MemoryJobRow>, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            Self::ensure_conversation_on(&mut connection, &input.user_id, &input.conversation_id).await?;
            let conversation: (Option<String>, String, Option<String>, String) = sqlx::query_as(
                "SELECT status,type,source,extra FROM conversations WHERE id = ? AND user_id = ?",
            )
            .bind(&input.conversation_id)
            .bind(&input.user_id)
            .fetch_one(&mut *connection)
            .await?;
            let settings: (bool, bool, Option<i64>, Option<i64>, i64) = sqlx::query_as(
                "SELECT enabled,default_capture,consent_version,reset_at,lifecycle_epoch
                 FROM memory_settings WHERE user_id = ?",
            ).bind(&input.user_id).fetch_one(&mut *connection).await?;
            let policy: Option<(Option<bool>, Option<i64>, i64)> = sqlx::query_as(
                "SELECT capture_enabled,reset_at,lifecycle_epoch FROM conversation_memory_policies
                 WHERE user_id = ? AND conversation_id = ?",
            ).bind(&input.user_id).bind(&input.conversation_id).fetch_optional(&mut *connection).await?;
            let (capture_override, conversation_reset, conversation_epoch) = policy.unwrap_or((None, None, 0));
            let reset_at = settings.3.into_iter().chain(conversation_reset).max();
            let snapshot =
                Self::canonical_turn_snapshot_on(&mut connection, &input.conversation_id, &input.through_turn_id)
                    .await?;
            if !settings.0
                || !capture_override.unwrap_or(settings.1)
                || settings.2 != Some(input.required_consent_version)
                || settings.4 != input.expected_global_epoch
                || conversation_epoch != input.expected_conversation_epoch
                || conversation.0.as_deref() != Some("finished")
                || Self::conversation_is_excluded(&conversation.1, conversation.2.as_deref(), &conversation.3)
                || snapshot.earliest_all_at.is_none()
                || !snapshot.has_user_work
                || !snapshot.has_assistant_outcome
                || reset_at.is_some_and(|reset| snapshot.earliest_all_at.is_none_or(|earliest| earliest <= reset))
            {
                return Ok(None);
            }
            let duplicate: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM memory_job_turns WHERE user_id = ? AND conversation_id = ?
                 AND operation_version = ? AND turn_id = ?)",
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
            let running = sqlx::query_as::<_, MemoryJobRow>(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ? AND state = 'running' LIMIT 1",
            ).bind(&input.user_id).bind(&input.conversation_id).fetch_optional(&mut *connection).await?;
            let pending = sqlx::query_as::<_, MemoryJobRow>(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state IN ('pending', 'retry_wait', 'blocked') LIMIT 1",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            let failed = sqlx::query_as::<_, MemoryJobRow>(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state = 'failed' ORDER BY created_at,id LIMIT 1",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            let failed = match failed {
                Some(failed) => Some(Self::absorb_queued_successor_on(&mut connection, &failed, input.now).await?),
                None => None,
            };
            let current_memory: Option<(String, i64)> = sqlx::query_as(
                "SELECT through_turn_id,revision FROM conversation_memories WHERE user_id = ? AND conversation_id = ?",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            let base_from_turn_id = failed
                .as_ref()
                .and_then(|job| job.from_turn_id.clone())
                .or_else(|| running
                .as_ref()
                .map(|job| job.through_turn_id.clone())
                .or_else(|| current_memory.as_ref().map(|memory| memory.0.clone())));
            let base_revision = failed.as_ref().map_or_else(
                || {
                    running.as_ref().map_or_else(
                        || current_memory.as_ref().map_or(0, |memory| memory.1),
                        |job| job.expected_revision,
                    )
                },
                |job| job.expected_revision,
            );
            if let Some(pending) = failed.or(pending) {
                let remains_failed = pending.state == "failed";
                let digest = Self::append_queue_digest(
                    Self::parse_queue_digest(&pending.queue_digest)?, &input.through_turn_id, &snapshot.hash,
                );
                let turn_count = pending
                    .turn_count
                    .checked_add(1)
                    .ok_or_else(|| DbError::Conflict("Memory queue length overflow".into()))?;
                let input_hash = Self::input_hash(
                    &input.operation_version, settings.4, conversation_epoch, base_from_turn_id.as_deref(),
                    turn_count, digest,
                )?;
                sqlx::query(
                    "INSERT INTO memory_job_turns
                     (job_id,user_id,conversation_id,operation_version,position,turn_id,turn_hash)
                     VALUES (?,?,?,?,?,?,?)",
                ).bind(&pending.id).bind(&input.user_id).bind(&input.conversation_id)
                .bind(&input.operation_version).bind(pending.turn_count).bind(&input.through_turn_id)
                .bind(&snapshot.hash).execute(&mut *connection).await?;
                sqlx::query(
                    "UPDATE memory_jobs SET from_turn_id = ?,through_turn_id = ?, operation_version = ?, global_epoch = ?,
                     conversation_epoch = ?, turn_count = ?, queue_digest = ?, input_hash = ?, expected_revision = ?,
                     state = ?, next_attempt_at = NULL, last_error_code = ?, updated_at = ?
                     WHERE id = ? AND user_id = ?",
                )
                .bind(&base_from_turn_id)
                .bind(&input.through_turn_id)
                .bind(&input.operation_version)
                .bind(settings.4).bind(conversation_epoch).bind(turn_count).bind(Self::queue_digest(digest))
                .bind(input_hash)
                .bind(base_revision)
                .bind(if remains_failed { "failed" } else { "pending" })
                .bind(if remains_failed { pending.last_error_code.as_deref() } else { None })
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
            let from_turn_id = base_from_turn_id;
            let expected_revision = base_revision;
            let digest = Self::append_queue_digest(0, &input.through_turn_id, &snapshot.hash);
            let input_hash = Self::input_hash(
                &input.operation_version, settings.4, conversation_epoch, from_turn_id.as_deref(), 1, digest,
            )?;
            sqlx::query(
                "INSERT INTO memory_jobs
                    (id,user_id,conversation_id,from_turn_id,through_turn_id,operation_version,global_epoch,
                     conversation_epoch,turn_count,queue_digest,input_hash,expected_revision,state,attempt_count,created_at,updated_at)
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,'pending',0,?,?)",
            )
            .bind(&input.id)
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .bind(&from_turn_id)
            .bind(&input.through_turn_id)
            .bind(&input.operation_version)
            .bind(settings.4).bind(conversation_epoch).bind(1_i64).bind(Self::queue_digest(digest))
            .bind(input_hash).bind(expected_revision)
            .bind(input.now)
            .bind(input.now)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO memory_job_turns
                 (job_id,user_id,conversation_id,operation_version,position,turn_id,turn_hash)
                 VALUES (?,?,?,?,0,?,?)",
            ).bind(&input.id).bind(&input.user_id).bind(&input.conversation_id)
            .bind(&input.operation_version).bind(&input.through_turn_id).bind(&snapshot.hash)
            .execute(&mut *connection).await?;
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

    async fn retry_failed_job(
        &self,
        user_id: &str,
        job_id: &str,
        now: TimestampMs,
    ) -> Result<Option<MemoryJobRow>, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let failed: Option<MemoryJobRow> =
                sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ? AND user_id = ? AND state = 'failed'")
                    .bind(job_id)
                    .bind(user_id)
                    .fetch_optional(&mut *connection)
                    .await?;
            let Some(failed) = failed else { return Ok(None) };
            let barrier = Self::absorb_queued_successor_on(&mut connection, &failed, now).await?;
            sqlx::query(
                "UPDATE memory_jobs SET state = 'pending',next_attempt_at = NULL,last_error_code = NULL,
                 lease_owner = NULL,lease_token = NULL,lease_expires_at = NULL,
                 reconciliation_snapshot_json = NULL,updated_at = ? WHERE id = ?",
            )
            .bind(now)
            .bind(&barrier.id)
            .execute(&mut *connection)
            .await?;
            Ok(sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                .bind(&barrier.id)
                .fetch_optional(&mut *connection)
                .await?)
        }
        .await;
        match result {
            Ok(row) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(row)
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
                 WHERE jobs.user_id = ?
                   AND NOT EXISTS (
                       SELECT 1 FROM memory_jobs barrier
                       WHERE barrier.user_id = jobs.user_id AND barrier.conversation_id = jobs.conversation_id
                         AND barrier.state = 'failed'
                   )
                   AND (
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
            let lease_expires_at = input
                .now
                .checked_add(input.lease_duration_ms)
                .ok_or_else(|| DbError::Conflict("Memory lease expiry overflow".into()))?;
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
            .bind(lease_expires_at)
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

    async fn list_job_turns(&self, user_id: &str, job_id: &str, limit: u32) -> Result<Vec<MemoryJobTurnRow>, DbError> {
        self.get_job(user_id, job_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Memory job '{job_id}' not found")))?;
        Ok(sqlx::query_as(
            "SELECT job_id,position,turn_id,turn_hash FROM memory_job_turns
             WHERE job_id = ? ORDER BY position LIMIT ?",
        )
        .bind(job_id)
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await?)
    }

    async fn load_job_turn_messages_bounded(
        &self,
        user_id: &str,
        job_id: &str,
        turn_id: &str,
        max_messages: u32,
        max_bytes: u64,
    ) -> Result<BoundedMemoryTurnMessagesRow, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let stored: Option<(String, String)> = sqlx::query_as(
                "SELECT turns.turn_hash,jobs.conversation_id FROM memory_job_turns turns
                 JOIN memory_jobs jobs ON jobs.id = turns.job_id
                 WHERE turns.job_id = ? AND turns.turn_id = ? AND jobs.user_id = ?",
            )
            .bind(job_id)
            .bind(turn_id)
            .bind(user_id)
            .fetch_optional(&mut *connection)
            .await?;
            let Some((stored_hash, conversation_id)) = stored else {
                return Err(DbError::NotFound(format!("Memory job turn '{turn_id}' not found")));
            };
            let snapshot = Self::canonical_turn_snapshot_on(&mut connection, &conversation_id, turn_id).await?;
            let max_bytes: i64 = max_bytes
                .try_into()
                .map_err(|_| DbError::Conflict("Memory evidence byte limit overflow".into()))?;
            let limit_exceeded = snapshot.absolute_limit_exceeded
                || snapshot.message_count > i64::from(max_messages)
                || snapshot.content_bytes > max_bytes;
            let messages = if limit_exceeded { Vec::new() } else { snapshot.messages };
            Ok(BoundedMemoryTurnMessagesRow {
                messages,
                message_count: snapshot.message_count,
                content_bytes: snapshot.content_bytes,
                snapshot_matches: snapshot.hash == stored_hash,
                snapshot_hash: snapshot.hash,
                limit_exceeded,
                has_user_work: snapshot.has_user_work,
                has_assistant_outcome: snapshot.has_assistant_outcome,
            })
        }
        .await;
        match result {
            Ok(row) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(row)
            }
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
    }

    async fn finalize_claimed_job_snapshot(
        &self,
        input: FinalizeMemoryJobSnapshotRow,
    ) -> Result<FinalizeMemoryJobSnapshotResult, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let job: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE id = ? AND user_id = ? AND state = 'running'
                 AND lease_token = ? AND lease_expires_at > ?",
            )
            .bind(&input.job_id)
            .bind(&input.user_id)
            .bind(&input.lease_token)
            .bind(input.now)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(job) = job else {
                return Ok(FinalizeMemoryJobSnapshotResult::FenceLost);
            };
            let global_epoch: i64 = sqlx::query_scalar("SELECT lifecycle_epoch FROM memory_settings WHERE user_id = ?")
                .bind(&input.user_id)
                .fetch_one(&mut *connection)
                .await?;
            let conversation_epoch: i64 = sqlx::query_scalar(
                "SELECT COALESCE((SELECT lifecycle_epoch FROM conversation_memory_policies
                  WHERE user_id = ? AND conversation_id = ?),0)",
            )
            .bind(&input.user_id)
            .bind(&job.conversation_id)
            .fetch_one(&mut *connection)
            .await?;
            if job.global_epoch != input.expected_global_epoch
                || job.conversation_epoch != input.expected_conversation_epoch
                || global_epoch != input.expected_global_epoch
                || conversation_epoch != input.expected_conversation_epoch
            {
                return Ok(FinalizeMemoryJobSnapshotResult::FenceLost);
            }
            let turns: Vec<MemoryJobTurnRow> = sqlx::query_as(
                "SELECT job_id,position,turn_id,turn_hash FROM memory_job_turns
                 WHERE job_id = ? ORDER BY position LIMIT 33",
            )
            .bind(&input.job_id)
            .fetch_all(&mut *connection)
            .await?;
            if turns.len() > 32
                || turns.len() as i64 != job.turn_count
                || turns.len() != input.turn_snapshots.len()
                || turns
                    .iter()
                    .zip(&input.turn_snapshots)
                    .any(|(turn, expected)| turn.turn_id != expected.turn_id)
            {
                return Ok(FinalizeMemoryJobSnapshotResult::SnapshotChanged);
            }
            let mut digest = 0_u128;
            let mut validated_hashes = Vec::with_capacity(turns.len());
            for (turn, expected) in turns.iter().zip(&input.turn_snapshots) {
                let snapshot =
                    Self::canonical_turn_snapshot_on(&mut connection, &job.conversation_id, &turn.turn_id).await?;
                if snapshot.absolute_limit_exceeded
                    || !snapshot.has_user_work
                    || !snapshot.has_assistant_outcome
                    || snapshot.hash != expected.snapshot_hash
                {
                    return Ok(FinalizeMemoryJobSnapshotResult::SnapshotChanged);
                }
                digest = Self::append_queue_digest(digest, &turn.turn_id, &snapshot.hash);
                validated_hashes.push(snapshot.hash);
            }
            for (turn, snapshot_hash) in turns.iter().zip(&validated_hashes) {
                sqlx::query("UPDATE memory_job_turns SET turn_hash = ? WHERE job_id = ? AND position = ?")
                    .bind(snapshot_hash)
                    .bind(&input.job_id)
                    .bind(turn.position)
                    .execute(&mut *connection)
                    .await?;
            }
            let reconciliation_snapshot_json = if let Some(snapshot) = &input.reconciliation_snapshot {
                if snapshot.len() > 64 {
                    return Ok(FinalizeMemoryJobSnapshotResult::ReconciliationChanged);
                }
                let mut ids = std::collections::HashSet::new();
                for expected in snapshot {
                    if !ids.insert(expected.id.as_str()) || expected.state != "active" {
                        return Ok(FinalizeMemoryJobSnapshotResult::ReconciliationChanged);
                    }
                    let current = sqlx::query_as::<_, MemoryEntryDbRow>(
                        "SELECT * FROM memory_entries WHERE id = ? AND user_id = ?",
                    )
                    .bind(&expected.id)
                    .bind(&input.user_id)
                    .fetch_optional(&mut *connection)
                    .await?;
                    let Some(current) = current else {
                        return Ok(FinalizeMemoryJobSnapshotResult::ReconciliationChanged);
                    };
                    if current.revision != expected.revision
                        || current.state != expected.state
                        || current.fingerprint != expected.fingerprint
                        || current.project_id != expected.project_id
                        || current.workspace_key != expected.workspace_key
                        || current.pinned != expected.pinned
                        || current.user_edited != expected.user_edited
                        || memory_entry_content_hash(current.content.as_deref()) != expected.content_hash
                    {
                        return Ok(FinalizeMemoryJobSnapshotResult::ReconciliationChanged);
                    }
                }
                Some(serde_json::to_string(snapshot).map_err(|error| DbError::Init(error.to_string()))?)
            } else {
                None
            };
            if input.require_existing_reconciliation_snapshot && job.reconciliation_snapshot_json.is_none() {
                return Ok(FinalizeMemoryJobSnapshotResult::ReconciliationChanged);
            }
            if let (Some(persisted), Some(current)) = (
                job.reconciliation_snapshot_json.as_deref(),
                reconciliation_snapshot_json.as_deref(),
            ) && persisted != current
            {
                return Ok(FinalizeMemoryJobSnapshotResult::ReconciliationChanged);
            }
            let input_hash = Self::input_hash(
                &job.operation_version,
                job.global_epoch,
                job.conversation_epoch,
                job.from_turn_id.as_deref(),
                job.turn_count,
                digest,
            )?;
            sqlx::query(
                "UPDATE memory_jobs SET queue_digest = ?,input_hash = ?,
                 reconciliation_snapshot_json = COALESCE(reconciliation_snapshot_json, ?),updated_at = ? WHERE id = ?",
            )
            .bind(Self::queue_digest(digest))
            .bind(input_hash)
            .bind(reconciliation_snapshot_json)
            .bind(input.now)
            .bind(&input.job_id)
            .execute(&mut *connection)
            .await?;
            let finalized = sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ?")
                .bind(&input.job_id)
                .fetch_one(&mut *connection)
                .await?;
            Ok(FinalizeMemoryJobSnapshotResult::Finalized(Box::new(finalized)))
        }
        .await;
        match result {
            Ok(result) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(result)
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
            if input.prefix_count <= 0 || input.prefix_count >= running.turn_count {
                return Err(DbError::Conflict("Invalid Memory batch split".into()));
            }
            let prefix_turns: Vec<MemoryJobTurnRow> = sqlx::query_as(
                "SELECT job_id,position,turn_id,turn_hash FROM memory_job_turns
                 WHERE job_id = ? AND position < ? ORDER BY position",
            ).bind(&running.id).bind(input.prefix_count).fetch_all(&mut *connection).await?;
            let prefix_digest = prefix_turns.iter().fold(0_u128, |digest, turn| {
                Self::append_queue_digest(digest, &turn.turn_id, &turn.turn_hash)
            });
            let running_through = prefix_turns.last().ok_or_else(|| DbError::Conflict("Empty Memory prefix".into()))?;
            let suffix_count = running.turn_count - input.prefix_count;
            let suffix_digest = Self::parse_queue_digest(&running.queue_digest)?.wrapping_sub(
                prefix_digest.wrapping_mul(Self::queue_power(suffix_count)?),
            );
            let running_input_hash = Self::input_hash(
                &running.operation_version, running.global_epoch, running.conversation_epoch,
                running.from_turn_id.as_deref(), input.prefix_count, prefix_digest,
            )?;
            sqlx::query(
                "UPDATE memory_jobs SET through_turn_id = ?,turn_count = ?,queue_digest = ?,input_hash = ?,updated_at = ?
                 WHERE id = ? AND user_id = ? AND state = 'running' AND lease_token = ? AND lease_expires_at > ?",
            ).bind(&running_through.turn_id).bind(input.prefix_count).bind(Self::queue_digest(prefix_digest))
            .bind(running_input_hash).bind(input.now).bind(&input.job_id).bind(&input.user_id)
            .bind(&input.lease_token).bind(input.now).execute(&mut *connection).await?;
            let existing: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state IN ('pending','retry_wait','blocked') LIMIT 1",
            ).bind(&input.user_id).bind(&running.conversation_id).fetch_optional(&mut *connection).await?;
            if let Some(existing) = existing {
                let parking_offset = suffix_count
                    .checked_add(existing.turn_count)
                    .ok_or_else(|| DbError::Conflict("Memory queue length overflow".into()))?;
                sqlx::query("UPDATE memory_job_turns SET position = position + ? WHERE job_id = ?")
                    .bind(parking_offset).bind(&existing.id).execute(&mut *connection).await?;
                sqlx::query("UPDATE memory_job_turns SET job_id = ?,position = position - ? WHERE job_id = ? AND position >= ?")
                    .bind(&existing.id).bind(input.prefix_count).bind(&running.id).bind(input.prefix_count)
                    .execute(&mut *connection).await?;
                sqlx::query(
                    "UPDATE memory_job_turns SET position = position - ? + ? WHERE job_id = ? AND position >= ?",
                )
                .bind(parking_offset)
                .bind(suffix_count)
                .bind(&existing.id)
                .bind(parking_offset)
                .execute(&mut *connection)
                .await?;
                let digest = Self::concat_queue_digest(
                    suffix_digest, Self::parse_queue_digest(&existing.queue_digest)?, existing.turn_count,
                )?;
                let count = parking_offset;
                let input_hash = Self::input_hash(
                    &running.operation_version, running.global_epoch, running.conversation_epoch,
                    Some(&running_through.turn_id), count, digest,
                )?;
                sqlx::query(
                    "UPDATE memory_jobs SET from_turn_id = ?,global_epoch = ?,conversation_epoch = ?,turn_count = ?,
                     queue_digest = ?,input_hash = ?,expected_revision = ?,state = 'pending',next_attempt_at = NULL,
                     updated_at = ? WHERE id = ?",
                ).bind(&running_through.turn_id).bind(running.global_epoch).bind(running.conversation_epoch)
                .bind(count).bind(Self::queue_digest(digest)).bind(input_hash).bind(running.expected_revision)
                .bind(input.now).bind(existing.id)
                .execute(&mut *connection).await?;
            } else {
                let input_hash = Self::input_hash(
                    &running.operation_version, running.global_epoch, running.conversation_epoch,
                    Some(&running_through.turn_id), suffix_count, suffix_digest,
                )?;
                sqlx::query(
                    "INSERT INTO memory_jobs
                     (id,user_id,conversation_id,from_turn_id,through_turn_id,operation_version,global_epoch,
                      conversation_epoch,turn_count,queue_digest,input_hash,expected_revision,state,attempt_count,created_at,updated_at)
                     VALUES (?,?,?,?,?,?,?,?,?,?,?,?,'pending',0,?,?)",
                ).bind(&input.pending_job_id).bind(&input.user_id).bind(&running.conversation_id)
                .bind(&running_through.turn_id).bind(&running.through_turn_id).bind(&running.operation_version)
                .bind(running.global_epoch).bind(running.conversation_epoch).bind(suffix_count)
                .bind(Self::queue_digest(suffix_digest)).bind(input_hash).bind(running.expected_revision)
                .bind(input.now).bind(input.now).execute(&mut *connection).await?;
                sqlx::query("UPDATE memory_job_turns SET job_id = ?,position = position - ? WHERE job_id = ? AND position >= ?")
                    .bind(&input.pending_job_id).bind(input.prefix_count).bind(&running.id).bind(input.prefix_count)
                    .execute(&mut *connection).await?;
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

    async fn update_memory_lifecycle(&self, input: UpdateMemoryLifecycleRow) -> Result<(), DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            sqlx::query(
                "INSERT INTO memory_settings (user_id,updated_at) VALUES (?,?) ON CONFLICT(user_id) DO NOTHING",
            )
            .bind(&input.user_id)
            .bind(input.now)
            .execute(&mut *connection)
            .await?;
            let current: (bool, bool) =
                sqlx::query_as("SELECT enabled,default_capture FROM memory_settings WHERE user_id = ?")
                    .bind(&input.user_id)
                    .fetch_one(&mut *connection)
                    .await?;
            let lifecycle_changed = input.enabled.is_some_and(|value| value != current.0)
                || input.default_capture.is_some_and(|value| value != current.1);
            sqlx::query(
                "UPDATE memory_settings SET enabled = COALESCE(?,enabled),
                 default_capture = COALESCE(?,default_capture),lifecycle_epoch = lifecycle_epoch + ?,updated_at = ?
                 WHERE user_id = ?",
            )
            .bind(input.enabled)
            .bind(input.default_capture)
            .bind(i64::from(lifecycle_changed))
            .bind(input.now)
            .bind(&input.user_id)
            .execute(&mut *connection)
            .await?;
            if lifecycle_changed {
                sqlx::query(
                    "UPDATE memory_jobs SET state = 'canceled',lease_owner = NULL,lease_token = NULL,
                     lease_expires_at = NULL,reconciliation_snapshot_json = NULL,next_attempt_at = NULL,
                     last_error_code = 'canceled',updated_at = ?
                     WHERE user_id = ? AND state IN ('pending','running','retry_wait','blocked','failed')",
                )
                .bind(input.now)
                .bind(&input.user_id)
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

    async fn update_conversation_memory_lifecycle(
        &self,
        input: UpdateConversationMemoryLifecycleRow,
    ) -> Result<(), DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            Self::ensure_conversation_on(&mut connection, &input.user_id, &input.conversation_id).await?;
            let current: Option<Option<bool>> = sqlx::query_scalar(
                "SELECT capture_enabled FROM conversation_memory_policies WHERE user_id = ? AND conversation_id = ?",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            let lifecycle_changed = current.flatten() != Some(input.capture_enabled);
            sqlx::query(
                "INSERT INTO conversation_memory_policies
                 (user_id,conversation_id,capture_enabled,lifecycle_epoch,updated_at) VALUES (?,?,?,?,?)
                 ON CONFLICT(user_id,conversation_id) DO UPDATE SET capture_enabled = excluded.capture_enabled,
                 lifecycle_epoch = conversation_memory_policies.lifecycle_epoch + ?,updated_at = excluded.updated_at",
            ).bind(&input.user_id).bind(&input.conversation_id).bind(input.capture_enabled)
            .bind(i64::from(lifecycle_changed)).bind(input.now).bind(i64::from(lifecycle_changed))
            .execute(&mut *connection).await?;
            if lifecycle_changed {
                sqlx::query(
                    "UPDATE memory_jobs SET state = 'canceled',lease_owner = NULL,lease_token = NULL,
                     lease_expires_at = NULL,reconciliation_snapshot_json = NULL,next_attempt_at = NULL,
                     last_error_code = 'canceled',updated_at = ?
                     WHERE user_id = ? AND conversation_id = ? AND state IN ('pending','running','retry_wait','blocked','failed')",
                ).bind(input.now).bind(&input.user_id).bind(&input.conversation_id)
                .execute(&mut *connection).await?;
            }
            Ok(())
        }.await;
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
        let lease_expires_at = input
            .now
            .checked_add(input.lease_duration_ms)
            .ok_or_else(|| DbError::Conflict("Memory lease expiry overflow".into()))?;
        let result = sqlx::query(
            "UPDATE memory_jobs SET lease_expires_at = ?, updated_at = ?
             WHERE id = ? AND user_id = ? AND state = 'running' AND lease_owner = ? AND lease_token = ? AND lease_expires_at > ?",
        )
        .bind(lease_expires_at)
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
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let running: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE id = ? AND user_id = ? AND state = 'running'
                 AND lease_owner = ? AND lease_token = ? AND lease_expires_at > ?",
            )
            .bind(&input.job_id)
            .bind(&input.user_id)
            .bind(&input.worker_id)
            .bind(&input.lease_token)
            .bind(input.now)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(running) = running else {
                return Ok(false);
            };
            Self::transition_running_on(
                &mut connection,
                &running,
                QueueTransition {
                    state: "pending",
                    next_attempt_at: None,
                    error_code: None,
                    increment_attempt: false,
                    increment_invalid_output: false,
                    now: input.now,
                },
            )
            .await?;
            Ok(true)
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

    async fn transition_running_job(&self, input: TransitionMemoryJobRow) -> Result<Option<MemoryJobRow>, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let running: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE id = ? AND user_id = ? AND state = 'running'
                 AND lease_owner = ? AND lease_token = ? AND lease_expires_at > ?",
            )
            .bind(&input.job_id)
            .bind(&input.user_id)
            .bind(&input.worker_id)
            .bind(&input.lease_token)
            .bind(input.now)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(running) = running else {
                return Ok(None);
            };
            Ok(Some(
                Self::transition_running_on(
                    &mut connection,
                    &running,
                    QueueTransition {
                        state: &input.state,
                        next_attempt_at: input.next_attempt_at,
                        error_code: input.error_code.as_deref(),
                        increment_attempt: input.increment_attempt,
                        increment_invalid_output: input.increment_invalid_output,
                        now: input.now,
                    },
                )
                .await?,
            ))
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
                     reconciliation_snapshot_json = NULL, next_attempt_at = NULL, last_error_code = 'canceled', updated_at = ?
                     WHERE user_id = ? AND conversation_id = ?
                       AND state IN ('pending','running','retry_wait','blocked','failed')",
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
                     reconciliation_snapshot_json = NULL, next_attempt_at = NULL, last_error_code = 'canceled', updated_at = ?
                     WHERE user_id = ? AND state IN ('pending','running','retry_wait','blocked','failed')",
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
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result =
            async {
                let expired: Vec<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE state = 'running' AND lease_expires_at <= ? ORDER BY created_at,id",
            ).bind(now).fetch_all(&mut *connection).await?;
                for running in &expired {
                    Self::transition_running_on(
                        &mut connection,
                        running,
                        QueueTransition {
                            state: "pending",
                            next_attempt_at: None,
                            error_code: running.last_error_code.as_deref(),
                            increment_attempt: false,
                            increment_invalid_output: false,
                            now,
                        },
                    )
                    .await?;
                }
                Ok(expired.len() as u64)
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
            let job: Option<MemoryJobRow> = sqlx::query_as("SELECT * FROM memory_jobs WHERE id = ? AND user_id = ?")
            .bind(&input.job_id)
            .bind(&input.user_id)
            .fetch_optional(&mut *connection)
            .await?;
            let Some(job) = job else {
                return Err(DbError::NotFound(format!("Memory job '{}' not found", input.job_id)));
            };
            let settings: (bool, bool, Option<i64>, Option<i64>, i64) = sqlx::query_as(
                "SELECT enabled,default_capture,consent_version,reset_at,lifecycle_epoch
                 FROM memory_settings WHERE user_id = ?",
            ).bind(&input.user_id).fetch_one(&mut *connection).await?;
            let policy: Option<(Option<bool>, Option<i64>, i64)> = sqlx::query_as(
                "SELECT capture_enabled,reset_at,lifecycle_epoch FROM conversation_memory_policies
                 WHERE user_id = ? AND conversation_id = ?",
            ).bind(&input.user_id).bind(&input.conversation_id).fetch_optional(&mut *connection).await?;
            let (capture_override, conversation_reset, conversation_epoch) = policy.unwrap_or((None, None, 0));
            let reset_at = settings.3.into_iter().chain(conversation_reset).max();
            let earliest_at: Option<i64> = sqlx::query_scalar(
                "SELECT MIN(messages.created_at) FROM memory_job_turns turns JOIN messages
                 ON messages.conversation_id = turns.conversation_id AND messages.turn_id = turns.turn_id
                 WHERE turns.job_id = ?",
            ).bind(&job.id).fetch_one(&mut *connection).await?;
            let valid_fence = job.conversation_id == input.conversation_id
                && job.state == "running"
                && job.expected_revision == input.expected_revision
                && job.through_turn_id == input.through_turn_id
                && job.lease_owner.as_deref() == Some(input.lease_owner.as_str())
                && job.lease_token.as_deref() == Some(input.lease_token.as_str())
                && job.lease_expires_at.is_some_and(|expires_at| expires_at > input.now)
                && job.attempt_count == input.expected_attempt_count
                && settings.0
                && capture_override.unwrap_or(settings.1)
                && settings.2.is_some()
                && settings.4 == job.global_epoch
                && conversation_epoch == job.conversation_epoch
                && earliest_at.is_some()
                && reset_at.is_none_or(|reset| earliest_at.is_some_and(|earliest| earliest > reset));
            if !valid_fence {
                return Err(DbError::Conflict(format!(
                    "Memory job '{}' lease or cursor changed",
                    input.job_id
                )));
            }
            if !Self::job_snapshot_matches_on(&mut connection, &job).await? {
                Self::transition_running_on(
                    &mut connection,
                    &job,
                    QueueTransition {
                        state: "pending",
                        next_attempt_at: None,
                        error_code: Some("snapshot_changed"),
                        increment_attempt: false,
                        increment_invalid_output: false,
                        now: input.now,
                    },
                )
                .await?;
                return Ok(CommitMemoryUpdateResult::SnapshotChanged);
            }
            let current_revision: Option<i64> = sqlx::query_scalar(
                "SELECT revision FROM conversation_memories WHERE user_id = ? AND conversation_id = ?",
            )
            .bind(&input.user_id)
            .bind(&input.conversation_id)
            .fetch_optional(&mut *connection)
            .await?;
            if current_revision.unwrap_or(0) != input.expected_revision {
                Self::transition_running_on(
                    &mut connection,
                    &job,
                    QueueTransition {
                        state: "pending",
                        next_attempt_at: None,
                        error_code: Some("stale_revision"),
                        increment_attempt: false,
                        increment_invalid_output: false,
                        now: input.now,
                    },
                )
                .await?;
                return Ok(CommitMemoryUpdateResult::StaleRevision {
                    current_revision: current_revision.unwrap_or(0),
                });
            }

            sqlx::query("SAVEPOINT memory_reconciliation")
                .execute(&mut *connection)
                .await?;
            if !Self::reconciliation_snapshot_matches_on(&mut connection, &job).await? {
                return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
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
                    Self::transition_running_on(
                        &mut connection,
                        &job,
                        QueueTransition {
                            state: "pending",
                            next_attempt_at: None,
                            error_code: Some("stale_revision"),
                            increment_attempt: false,
                            increment_invalid_output: false,
                            now: input.now,
                        },
                    )
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
                let target = match &entry.transition {
                    CommitMemoryEntryTransition::Create => None,
                    CommitMemoryEntryTransition::Refine { target }
                    | CommitMemoryEntryTransition::Supersede { target }
                    | CommitMemoryEntryTransition::Conflict { target, .. }
                    | CommitMemoryEntryTransition::AttachSource { target } => Some(target),
                };
                if let Some(target) = target {
                    Self::ensure_transition_target_owner_on(&mut connection, &input.user_id, &target.id).await?;
                    if target.state != "active" {
                        return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                    }
                }
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
                        if Self::active_fingerprint_collision_on(
                            &mut connection,
                            &input.user_id,
                            &entry.fingerprint,
                            None,
                        )
                        .await?
                        {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
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
                    CommitMemoryEntryTransition::Refine { target } => {
                        if Self::active_fingerprint_collision_on(
                            &mut connection,
                            &input.user_id,
                            &entry.fingerprint,
                            Some(&target.id),
                        )
                        .await?
                        {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
                        let updated = sqlx::query(
                            "UPDATE memory_entries SET project_id = ?, workspace_key = ?, kind = ?, stable_key = ?,
                             fingerprint = ?, content = ?, revision = revision + 1, updated_at = ?
                             WHERE id = ? AND user_id = ? AND state = 'active' AND revision = ?
                               AND fingerprint = ? AND project_id IS ? AND workspace_key IS ?
                               AND content IS ?
                               AND pinned = 0 AND user_edited = 0",
                        )
                        .bind(&entry.project_id)
                        .bind(&entry.workspace_key)
                        .bind(&entry.kind)
                        .bind(&entry.stable_key)
                        .bind(&entry.fingerprint)
                        .bind(&entry.content)
                        .bind(input.now)
                        .bind(&target.id)
                        .bind(&input.user_id)
                        .bind(target.revision)
                        .bind(&target.fingerprint)
                        .bind(&target.project_id)
                        .bind(&target.workspace_key)
                        .bind(&target.content)
                        .execute(&mut *connection)
                        .await?;
                        if updated.rows_affected() != 1 {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
                        refined_ids.push(target.id.clone());
                        target.id.clone()
                    }
                    CommitMemoryEntryTransition::Supersede { target } => {
                        if Self::active_fingerprint_collision_on(
                            &mut connection,
                            &input.user_id,
                            &entry.fingerprint,
                            Some(&target.id),
                        )
                        .await?
                        {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
                        let updated = sqlx::query(
                            "UPDATE memory_entries SET state = 'superseded', revision = revision + 1, updated_at = ?
                             WHERE id = ? AND user_id = ? AND state = 'active' AND revision = ?
                               AND fingerprint = ? AND project_id IS ? AND workspace_key IS ?
                               AND content IS ?
                               AND pinned = 0 AND user_edited = 0",
                        )
                        .bind(input.now)
                        .bind(&target.id)
                        .bind(&input.user_id)
                        .bind(target.revision)
                        .bind(&target.fingerprint)
                        .bind(&target.project_id)
                        .bind(&target.workspace_key)
                        .bind(&target.content)
                        .execute(&mut *connection)
                        .await?;
                        if updated.rows_affected() != 1 {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
                        Self::insert_entry_on(
                            &mut connection,
                            &input.user_id,
                            entry,
                            InsertEntryOptions {
                                state: "active",
                                supersedes_id: Some(&target.id),
                                conflict_group_id: None,
                            },
                            input.schema_version,
                            input.now,
                        )
                        .await?;
                        added_ids.push(entry.id.clone());
                        superseded_ids.push(target.id.clone());
                        entry.id.clone()
                    }
                    CommitMemoryEntryTransition::Conflict {
                        target,
                        conflict_group_id,
                    } => {
                        let snapshot: Option<(bool, bool)> = sqlx::query_as(
                            "SELECT pinned,user_edited FROM memory_entries
                             WHERE id = ? AND user_id = ? AND state = ? AND revision = ?
                               AND fingerprint = ? AND project_id IS ? AND workspace_key IS ?
                               AND content IS ?",
                        )
                        .bind(&target.id)
                        .bind(&input.user_id)
                        .bind(&target.state)
                        .bind(target.revision)
                        .bind(&target.fingerprint)
                        .bind(&target.project_id)
                        .bind(&target.workspace_key)
                        .bind(&target.content)
                        .fetch_optional(&mut *connection)
                        .await?;
                        let Some((pinned, user_edited)) = snapshot else {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        };
                        if !pinned && !user_edited {
                            let updated = sqlx::query(
                                "UPDATE memory_entries SET state = 'conflict', conflict_group_id = ?,
                                 revision = revision + 1, updated_at = ?
                                 WHERE id = ? AND user_id = ? AND state = ? AND revision = ?
                                   AND fingerprint = ? AND project_id IS ? AND workspace_key IS ?",
                            )
                            .bind(conflict_group_id)
                            .bind(input.now)
                            .bind(&target.id)
                            .bind(&input.user_id)
                            .bind(&target.state)
                            .bind(target.revision)
                            .bind(&target.fingerprint)
                            .bind(&target.project_id)
                            .bind(&target.workspace_key)
                            .execute(&mut *connection)
                            .await?;
                            if updated.rows_affected() != 1 {
                                return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                            }
                            conflict_ids.push(target.id.clone());
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
                    CommitMemoryEntryTransition::AttachSource { target } => {
                        if target.fingerprint != entry.fingerprint
                            || target.project_id != entry.project_id
                            || target.workspace_key != entry.workspace_key
                            || target.content.as_deref() != Some(entry.content.as_str())
                        {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
                        let matches: bool = sqlx::query_scalar(
                            "SELECT EXISTS(
                                SELECT 1 FROM memory_entries
                                WHERE id = ? AND user_id = ? AND state = ? AND revision = ?
                                  AND fingerprint = ? AND project_id IS ? AND workspace_key IS ?
                                  AND content IS ? AND (pinned = 1 OR user_edited = 1)
                            )",
                        )
                        .bind(&target.id)
                        .bind(&input.user_id)
                        .bind(&target.state)
                        .bind(target.revision)
                        .bind(&target.fingerprint)
                        .bind(&target.project_id)
                        .bind(&target.workspace_key)
                        .bind(&target.content)
                        .fetch_one(&mut *connection)
                        .await?;
                        if !matches {
                            return Self::requeue_stale_reconciliation_on(&mut connection, &job, input.now).await;
                        }
                        refined_ids.push(target.id.clone());
                        target.id.clone()
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
            sqlx::query("RELEASE SAVEPOINT memory_reconciliation")
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "UPDATE memory_jobs SET state = 'succeeded', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
                 reconciliation_snapshot_json = NULL, updated_at = ?
                 WHERE id = ? AND user_id = ? AND state = 'running'",
            )
            .bind(input.now)
            .bind(&input.job_id)
            .bind(&input.user_id)
            .execute(&mut *connection)
            .await?;
            let successor: Option<MemoryJobRow> = sqlx::query_as(
                "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = ?
                 AND state IN ('pending','retry_wait','blocked') LIMIT 1",
            ).bind(&input.user_id).bind(&input.conversation_id).fetch_optional(&mut *connection).await?;
            if let Some(successor) = successor {
                let successor_hash = Self::input_hash(
                    &successor.operation_version, successor.global_epoch, successor.conversation_epoch,
                    Some(&input.through_turn_id), successor.turn_count,
                    Self::parse_queue_digest(&successor.queue_digest)?,
                )?;
                sqlx::query(
                    "UPDATE memory_jobs SET from_turn_id = ?,expected_revision = ?,input_hash = ?,updated_at = ? WHERE id = ?",
                ).bind(&input.through_turn_id).bind(revision).bind(successor_hash).bind(input.now).bind(successor.id)
                .execute(&mut *connection).await?;
            }

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
             LIMIT ? OFFSET ?",
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
        .bind(query.offset)
        .fetch_all(&self.pool)
        .await?;
        self.entry_rows_with_sources(rows).await
    }

    async fn count_entries(&self, user_id: &str, query: MemoryEntryQueryRow) -> Result<u64, DbError> {
        self.ensure_user(user_id).await?;
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT entries.id) FROM memory_entries entries
             LEFT JOIN memory_sources sources ON sources.memory_entry_id = entries.id
             WHERE entries.user_id = ?
               AND (? IS NULL OR entries.kind = ?)
               AND (? IS NULL OR entries.state = ?)
               AND (? IS NULL OR entries.project_id = ?)
               AND (? IS NULL OR entries.workspace_key = ?)
               AND (? IS NULL OR sources.conversation_id = ?)
               AND (? IS NULL OR entries.created_at >= ?)
               AND (? IS NULL OR entries.created_at <= ?)
               AND (? IS NULL OR lower(COALESCE(entries.content, '')) LIKE '%' || lower(?) || '%')",
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
        .fetch_one(&self.pool)
        .await?;
        count
            .try_into()
            .map_err(|_| DbError::Conflict("Memory entry count overflow".into()))
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
        let result = async {
            let current =
                sqlx::query_as::<_, MemoryEntryDbRow>("SELECT * FROM memory_entries WHERE id = ? AND user_id = ?")
                    .bind(&input.id)
                    .bind(&input.user_id)
                    .fetch_optional(&mut *connection)
                    .await?
                    .ok_or_else(|| DbError::NotFound(format!("Memory entry '{}' not found", input.id)))?;
            if current.state == "deleted" {
                return Err(DbError::Conflict(format!(
                    "Deleted Memory entry '{}' cannot be updated",
                    input.id
                )));
            }
            if current.revision != input.expected_revision || current.state != input.expected_state {
                return Err(DbError::Conflict(format!(
                    "Memory entry '{}' revision or state changed",
                    input.id
                )));
            }
            let project_id = input.project_id.clone().unwrap_or_else(|| current.project_id.clone());
            let workspace_key = input
                .workspace_key
                .clone()
                .unwrap_or_else(|| current.workspace_key.clone());
            let scope_changed = project_id != current.project_id || workspace_key != current.workspace_key;
            let fingerprint = if scope_changed {
                let supplied = input
                    .new_fingerprint
                    .as_deref()
                    .ok_or_else(|| DbError::Conflict("Memory scope edits require a rederived fingerprint".into()))?;
                let derived = derive_memory_fingerprint(
                    &input.user_id,
                    project_id.as_deref(),
                    workspace_key.as_deref(),
                    &current.kind,
                    &current.stable_key,
                );
                if supplied != derived {
                    return Err(DbError::Conflict(
                        "Memory scope fingerprint does not match identity".into(),
                    ));
                }
                let blocked: bool = sqlx::query_scalar(
                    "SELECT EXISTS(
                        SELECT 1 FROM memory_entries
                        WHERE user_id = ? AND fingerprint = ? AND id <> ?
                          AND state IN ('active','deleted')
                    )",
                )
                .bind(&input.user_id)
                .bind(supplied)
                .bind(&input.id)
                .fetch_one(&mut *connection)
                .await?;
                if blocked {
                    return Err(DbError::Conflict(
                        "Memory scope identity is already active or tombstoned".into(),
                    ));
                }
                supplied.to_owned()
            } else {
                if input
                    .new_fingerprint
                    .as_deref()
                    .is_some_and(|fingerprint| fingerprint != current.fingerprint)
                {
                    return Err(DbError::Conflict(
                        "Memory fingerprint cannot change without a scope edit".into(),
                    ));
                }
                current.fingerprint.clone()
            };
            let updated = sqlx::query(
                "UPDATE memory_entries SET
                    content = COALESCE(?, content),
                    user_edited = CASE WHEN ? IS NULL AND ? = 0 THEN user_edited ELSE 1 END,
                    pinned = COALESCE(?, pinned),
                    project_id = ?, workspace_key = ?, fingerprint = ?,
                    revision = revision + 1,
                    updated_at = ?
                 WHERE id = ? AND user_id = ? AND state = ? AND revision = ? AND fingerprint = ?",
            )
            .bind(&input.content)
            .bind(&input.content)
            .bind(scope_changed)
            .bind(input.pinned)
            .bind(&project_id)
            .bind(&workspace_key)
            .bind(&fingerprint)
            .bind(input.now)
            .bind(&input.id)
            .bind(&input.user_id)
            .bind(&input.expected_state)
            .bind(input.expected_revision)
            .bind(&current.fingerprint)
            .execute(&mut *connection)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(DbError::Conflict(format!(
                    "Memory entry '{}' revision or state changed",
                    input.id
                )));
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

    async fn resolve_conflict(&self, input: ResolveMemoryConflictRow) -> Result<Vec<MemoryEntryRow>, DbError> {
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            let anchor =
                sqlx::query_as::<_, MemoryEntryDbRow>("SELECT * FROM memory_entries WHERE id = ? AND user_id = ?")
                    .bind(&input.entry_id)
                    .bind(&input.user_id)
                    .fetch_optional(&mut *connection)
                    .await?
                    .ok_or_else(|| DbError::NotFound(format!("Memory entry '{}' not found", input.entry_id)))?;
            if anchor.state != "conflict" {
                return Err(DbError::Conflict(
                    "Memory entry is not in an unresolved conflict".into(),
                ));
            }
            let group_id = anchor
                .conflict_group_id
                .clone()
                .ok_or_else(|| DbError::Conflict("Memory conflict is missing its group".into()))?;
            let members = sqlx::query_as::<_, MemoryEntryDbRow>(
                "SELECT * FROM memory_entries WHERE user_id = ? AND conflict_group_id = ? AND state = 'conflict'
                 ORDER BY created_at,id",
            )
            .bind(&input.user_id)
            .bind(&group_id)
            .fetch_all(&mut *connection)
            .await?;
            if members.len() < 2 {
                return Err(DbError::Conflict(
                    "Memory conflict no longer has multiple versions".into(),
                ));
            }
            match &input.action {
                ResolveMemoryConflictActionRow::Select { selected_entry_id } => {
                    if !members.iter().any(|entry| entry.id == *selected_entry_id) {
                        return Err(DbError::NotFound(format!(
                            "Memory entry '{selected_entry_id}' not found"
                        )));
                    }
                    sqlx::query(
                        "UPDATE memory_entries SET state = 'superseded', conflict_group_id = NULL,
                         revision = revision + 1, updated_at = ? WHERE user_id = ? AND conflict_group_id = ?",
                    )
                    .bind(input.now)
                    .bind(&input.user_id)
                    .bind(&group_id)
                    .execute(&mut *connection)
                    .await?;
                    sqlx::query(
                        "UPDATE memory_entries SET state = 'active', user_edited = 1, conflict_group_id = NULL,
                         revision = revision + 1, updated_at = ? WHERE id = ? AND user_id = ?",
                    )
                    .bind(input.now)
                    .bind(selected_entry_id)
                    .bind(&input.user_id)
                    .execute(&mut *connection)
                    .await?;
                }
                ResolveMemoryConflictActionRow::Merge { content } => {
                    sqlx::query(
                        "UPDATE memory_entries SET state = 'superseded', conflict_group_id = NULL,
                         revision = revision + 1, updated_at = ? WHERE user_id = ? AND conflict_group_id = ?",
                    )
                    .bind(input.now)
                    .bind(&input.user_id)
                    .bind(&group_id)
                    .execute(&mut *connection)
                    .await?;
                    sqlx::query(
                        "UPDATE memory_entries SET content = ?, state = 'active', user_edited = 1,
                         conflict_group_id = NULL, revision = revision + 1, updated_at = ?
                         WHERE id = ? AND user_id = ?",
                    )
                    .bind(content)
                    .bind(input.now)
                    .bind(&input.entry_id)
                    .bind(&input.user_id)
                    .execute(&mut *connection)
                    .await?;
                }
                ResolveMemoryConflictActionRow::KeepSeparate { tombstone_id_prefix } => {
                    let mut prior_identities = Vec::new();
                    let mut seen_fingerprints = HashSet::new();
                    for member in &members {
                        if seen_fingerprints.insert(member.fingerprint.clone()) {
                            prior_identities.push(member.clone());
                        }
                        let stable_key = format!(
                            "resolved-{}",
                            memory_entry_content_hash(Some(&format!("{group_id}:{}", member.id))),
                        );
                        let fingerprint = derive_memory_fingerprint(
                            &input.user_id,
                            member.project_id.as_deref(),
                            member.workspace_key.as_deref(),
                            &member.kind,
                            &stable_key,
                        );
                        sqlx::query(
                            "UPDATE memory_entries SET stable_key = ?, fingerprint = ?, state = 'active',
                             user_edited = 1, conflict_group_id = NULL, revision = revision + 1, updated_at = ?
                             WHERE id = ? AND user_id = ?",
                        )
                        .bind(stable_key)
                        .bind(fingerprint)
                        .bind(input.now)
                        .bind(&member.id)
                        .bind(&input.user_id)
                        .execute(&mut *connection)
                        .await?;
                    }
                    for (index, identity) in prior_identities.into_iter().enumerate() {
                        let already_tombstoned: bool = sqlx::query_scalar(
                            "SELECT EXISTS(SELECT 1 FROM memory_entries
                             WHERE user_id = ? AND fingerprint = ? AND state = 'deleted')",
                        )
                        .bind(&input.user_id)
                        .bind(&identity.fingerprint)
                        .fetch_one(&mut *connection)
                        .await?;
                        if already_tombstoned {
                            continue;
                        }
                        let tombstone_id = if index == 0 {
                            tombstone_id_prefix.clone()
                        } else {
                            format!("{tombstone_id_prefix}-{index}")
                        };
                        sqlx::query(
                            "INSERT INTO memory_entries
                             (id,user_id,project_id,workspace_key,kind,stable_key,fingerprint,content,state,pinned,
                              user_edited,revision,schema_version,deleted_at,created_at,updated_at)
                             VALUES (?,?,?,?,?,?,?,NULL,'deleted',0,0,0,?,?,?,?)",
                        )
                        .bind(tombstone_id)
                        .bind(&input.user_id)
                        .bind(&identity.project_id)
                        .bind(&identity.workspace_key)
                        .bind(&identity.kind)
                        .bind(&identity.stable_key)
                        .bind(&identity.fingerprint)
                        .bind(identity.schema_version)
                        .bind(input.now)
                        .bind(input.now)
                        .bind(input.now)
                        .execute(&mut *connection)
                        .await?;
                    }
                }
            }
            let mut output = Vec::with_capacity(members.len());
            for member in members {
                let row =
                    sqlx::query_as::<_, MemoryEntryDbRow>("SELECT * FROM memory_entries WHERE id = ? AND user_id = ?")
                        .bind(member.id)
                        .bind(&input.user_id)
                        .fetch_one(&mut *connection)
                        .await?;
                output.push(Self::entry_with_sources_on(&mut connection, row).await?);
            }
            Ok(output)
        }
        .await;
        match result {
            Ok(rows) => {
                sqlx::query("COMMIT").execute(&mut *connection).await?;
                Ok(rows)
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
             supersedes_id = NULL, conflict_group_id = NULL, revision = revision + 1, deleted_at = ?, updated_at = ?
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
        Ok(self
            .query_change_sets(
                user_id,
                MemoryChangeSetQueryRow {
                    conversation_id: None,
                    limit,
                    offset: 0,
                },
            )
            .await?
            .0)
    }

    async fn query_change_sets(
        &self,
        user_id: &str,
        query: MemoryChangeSetQueryRow,
    ) -> Result<(Vec<MemoryChangeSetRow>, u64), DbError> {
        self.ensure_user(user_id).await?;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_change_sets WHERE user_id = ? AND (? IS NULL OR conversation_id = ?)",
        )
        .bind(user_id)
        .bind(&query.conversation_id)
        .bind(&query.conversation_id)
        .fetch_one(&self.pool)
        .await?;
        let rows = sqlx::query_as(
            "SELECT * FROM memory_change_sets WHERE user_id = ? AND (? IS NULL OR conversation_id = ?)
             ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?",
        )
        .bind(user_id)
        .bind(&query.conversation_id)
        .bind(&query.conversation_id)
        .bind(query.limit.clamp(1, MAX_MEMORY_CANDIDATES))
        .bind(query.offset)
        .fetch_all(&self.pool)
        .await?;
        Ok((
            rows,
            total
                .try_into()
                .map_err(|_| DbError::Conflict("Memory change-set count overflow".into()))?,
        ))
    }

    async fn memory_job_health(&self, user_id: &str) -> Result<(Option<i64>, Vec<MemoryJobHealthRow>), DbError> {
        self.ensure_user(user_id).await?;
        let last_successful =
            sqlx::query_scalar("SELECT MAX(updated_at) FROM memory_jobs WHERE user_id = ? AND state = 'succeeded'")
                .bind(user_id)
                .fetch_one(&self.pool)
                .await?;
        let jobs = sqlx::query_as(
            "SELECT state, COUNT(*) AS count FROM memory_jobs WHERE user_id = ? GROUP BY state ORDER BY state",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok((last_successful, jobs))
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
                "UPDATE memory_jobs SET state = 'canceled', lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
                 reconciliation_snapshot_json = NULL, next_attempt_at = NULL, last_error_code = 'canceled', updated_at = ?
                 WHERE user_id = ? AND conversation_id = ? AND state NOT IN ('succeeded', 'canceled')",
            )
            .bind(now)
            .bind(user_id)
            .bind(conversation_id)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO conversation_memory_policies (user_id,conversation_id,reset_at,lifecycle_epoch,updated_at)
                 VALUES (?,?,?,1,?) ON CONFLICT(user_id,conversation_id) DO UPDATE SET
                 reset_at = excluded.reset_at,lifecycle_epoch = conversation_memory_policies.lifecycle_epoch + 1,
                 updated_at = excluded.updated_at",
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
                "INSERT INTO memory_settings (user_id,reset_at,lifecycle_epoch,updated_at) VALUES (?,?,1,?)
                 ON CONFLICT(user_id) DO UPDATE SET reset_at = excluded.reset_at,
                 lifecycle_epoch = memory_settings.lifecycle_epoch + 1,updated_at = excluded.updated_at",
            )
            .bind(user_id)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "UPDATE conversation_memory_policies SET reset_at = ?,
                 lifecycle_epoch = lifecycle_epoch + 1, updated_at = ? WHERE user_id = ?",
            )
            .bind(now)
            .bind(now)
            .bind(user_id)
            .execute(&mut *connection)
            .await?;
            sqlx::query(
                "INSERT INTO memory_import_state
                    (user_id,cursor,completed,started_at,completed_at,updated_at)
                 VALUES (?,NULL,1,?,?,?) ON CONFLICT(user_id) DO UPDATE SET
                    completed = 1,completed_at = excluded.completed_at,updated_at = excluded.updated_at",
            )
            .bind(user_id)
            .bind(now)
            .bind(now)
            .bind(now)
            .execute(&mut *connection)
            .await?;
            for table in [
                "memory_retrievals",
                "memory_change_sets",
                "conversation_memories",
                "memory_entries",
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
               AND ((project_id IS NULL AND workspace_key IS NULL)
                    OR (? IS NOT NULL AND project_id = ?)
                    OR (? IS NOT NULL AND workspace_key = ?))
             ORDER BY
               CASE WHEN project_id = ? THEN 0 WHEN workspace_key = ? THEN 1 ELSE 2 END,
               pinned DESC, user_edited DESC, updated_at DESC, id
             LIMIT ?",
        )
        .bind(&query.user_id)
        .bind(&query.project_id)
        .bind(&query.project_id)
        .bind(&query.workspace_key)
        .bind(&query.workspace_key)
        .bind(&query.project_id)
        .bind(&query.workspace_key)
        .bind(query.limit.clamp(1, MAX_MEMORY_CANDIDATES))
        .fetch_all(&self.pool)
        .await?;
        self.entry_rows_with_sources(rows).await
    }

    async fn reconciliation_entries(
        &self,
        user_id: &str,
        fingerprints: &[String],
        target_ids: &[String],
    ) -> Result<Vec<MemoryEntryRow>, DbError> {
        const MAX_LOOKUPS: usize = 32;
        const MAX_RESULTS: usize = MAX_LOOKUPS * 3;
        if fingerprints.len() > MAX_LOOKUPS || target_ids.len() > MAX_LOOKUPS {
            return Err(DbError::Conflict(
                "Memory reconciliation lookup exceeds its bound".into(),
            ));
        }
        self.ensure_user(user_id).await?;
        let mut rows = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for fingerprint in fingerprints {
            for state in ["active", "deleted"] {
                let row = sqlx::query_as::<_, MemoryEntryDbRow>(
                    "SELECT * FROM memory_entries
                     WHERE user_id = ? AND fingerprint = ? AND state = ?
                     ORDER BY updated_at DESC, id LIMIT 1",
                )
                .bind(user_id)
                .bind(fingerprint)
                .bind(state)
                .fetch_optional(&self.pool)
                .await?;
                let Some(row) = row else {
                    continue;
                };
                if seen.insert(row.id.clone()) {
                    rows.push(row);
                }
            }
        }
        for target_id in target_ids {
            let row =
                sqlx::query_as::<_, MemoryEntryDbRow>("SELECT * FROM memory_entries WHERE user_id = ? AND id = ?")
                    .bind(user_id)
                    .bind(target_id)
                    .fetch_optional(&self.pool)
                    .await?;
            if let Some(row) = row
                && seen.insert(row.id.clone())
            {
                rows.push(row);
            }
        }
        if rows.len() > MAX_RESULTS {
            return Err(DbError::Conflict(
                "Memory reconciliation result exceeds its bound".into(),
            ));
        }
        Ok(rows.into_iter().map(|row| row.with_sources(Vec::new())).collect())
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
        let mut connection = self.pool.acquire().await?;
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *connection).await?;
        let result = async {
            sqlx::query("DELETE FROM memory_retrievals WHERE expires_at <= ?")
                .bind(retrieval.created_at)
                .execute(&mut *connection)
                .await?;
            sqlx::query(
                "DELETE FROM memory_retrievals
                 WHERE user_id = ? AND conversation_id = ? AND prompt_hash = ? AND retrieval_version = ?",
            )
            .bind(&retrieval.user_id)
            .bind(&retrieval.conversation_id)
            .bind(&retrieval.prompt_hash)
            .bind(&retrieval.retrieval_version)
            .execute(&mut *connection)
            .await?;
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
            .execute(&mut *connection)
            .await?;
            sqlx::query("COMMIT").execute(&mut *connection).await?;
            Ok::<_, DbError>(())
        }
        .await;
        match result {
            Ok(()) => Ok(retrieval),
            Err(error) => {
                let _ = sqlx::query("ROLLBACK").execute(&mut *connection).await;
                Err(error)
            }
        }
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

    async fn delete_expired_retrievals(&self, now: TimestampMs) -> Result<u64, DbError> {
        Ok(sqlx::query("DELETE FROM memory_retrievals WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?
            .rows_affected())
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
                started_at = excluded.started_at, completed_at = excluded.completed_at, updated_at = excluded.updated_at
             WHERE memory_import_state.completed = 0",
        )
        .bind(&state.user_id)
        .bind(&state.cursor)
        .bind(state.completed)
        .bind(state.started_at)
        .bind(state.completed_at)
        .bind(state.updated_at)
        .execute(&self.pool)
        .await?;
        self.get_import_state(&state.user_id)
            .await?
            .ok_or_else(|| DbError::NotFound("Memory import state was not persisted".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::SqliteMemoryRepository;
    use crate::models::{ConversationRow, MemoryImportStateRow, MemoryRetrievalRow, MessageRow};
    use crate::repository::memory::{
        ClaimMemoryJobRow, CommitMemoryEntryRow, CommitMemoryEntryTransition, CommitMemorySourceRow,
        CommitMemoryUpdateResult, CommitMemoryUpdateRow, EnqueueMemoryTurnRow, ExpectedMemoryEntryRow,
        FinalizeMemoryJobSnapshotResult, FinalizeMemoryJobSnapshotRow, MemoryCandidateQueryRow,
        MemoryReconciliationSnapshotRow, MemoryTurnSnapshotExpectationRow, ReleaseMemoryLeaseRow, RenewMemoryLeaseRow,
        ResolveMemoryConflictActionRow, ResolveMemoryConflictRow, SplitMemoryJobRow, TransitionMemoryJobRow,
        UpdateConversationMemoryLifecycleRow, UpdateConversationMemoryPolicyRow, UpdateMemoryEntryRow,
        UpdateMemorySettingsRow, derive_memory_fingerprint, memory_entry_content_hash,
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
        sqlx::query(
            "INSERT INTO memory_settings
                (user_id, enabled, default_capture, default_recall, consent_version, consented_at, updated_at)
             VALUES (?, 1, 1, 1, 1, 1, 1)",
        )
        .bind(USER_A)
        .execute(db.pool())
        .await
        .unwrap();
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
            through_turn_id: through_turn_id.into(),
            operation_version: "memory-operation-v1".into(),
            expected_global_epoch: 0,
            expected_conversation_epoch: 0,
            required_consent_version: 1,
            now,
        }
    }

    async fn enqueue_turn(
        repo: &SqliteMemoryRepository,
        id: &str,
        conversation_id: &str,
        turn_id: &str,
        now: i64,
    ) -> Option<crate::models::MemoryJobRow> {
        for (suffix, position, content, created_at) in
            [("user", "right", "work", now), ("assistant", "left", "done", now + 1)]
        {
            sqlx::query(
                "INSERT OR IGNORE INTO messages
                    (id, conversation_id, turn_id, type, content, position, status, hidden, created_at)
                 VALUES (?, ?, ?, 'text', ?, ?, 'finish', 0, ?)",
            )
            .bind(format!("msg-{conversation_id}-{turn_id}-{suffix}"))
            .bind(conversation_id)
            .bind(turn_id)
            .bind(serde_json::json!({ "content": content }).to_string())
            .bind(position)
            .bind(created_at)
            .execute(&repo.pool)
            .await
            .unwrap();
        }
        repo.enqueue_completed_turn(enqueue(id, conversation_id, turn_id, now))
            .await
            .unwrap()
    }

    async fn insert_job_in_state(repo: &SqliteMemoryRepository, job_id: &str, state: &str) {
        sqlx::query(
            "INSERT INTO memory_jobs
                (id,user_id,conversation_id,through_turn_id,operation_version,queue_digest,input_hash,
                 expected_revision,state,attempt_count,invalid_output_count,created_at,updated_at)
             VALUES (?,?,'conv_a','turn-policy','memory-operation-v1','digest','hash',0,?,0,0,10,10)",
        )
        .bind(job_id)
        .bind(USER_A)
        .bind(state)
        .execute(&repo.pool)
        .await
        .unwrap();
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

    fn enqueued_source(conversation_id: &str, turn_id: &str) -> CommitMemorySourceRow {
        CommitMemorySourceRow {
            conversation_id: conversation_id.into(),
            turn_id: turn_id.into(),
            message_ids_json: format!(r#"["msg-{conversation_id}-{turn_id}-user"]"#),
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

    fn expected_entry(
        id: &str,
        fingerprint: &str,
        revision: i64,
        state: &str,
        content: Option<&str>,
    ) -> ExpectedMemoryEntryRow {
        ExpectedMemoryEntryRow {
            id: id.into(),
            revision,
            state: state.into(),
            fingerprint: fingerprint.into(),
            project_id: None,
            workspace_key: None,
            content: content.map(str::to_owned),
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
        enqueue_turn(repo, job_id, conversation_id, turn_id, 10).await;
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

    async fn claim_cross_conversation_jobs(
        repo: &SqliteMemoryRepository,
        suffix: &str,
    ) -> (crate::models::MemoryJobRow, crate::models::MemoryJobRow) {
        let first_turn = format!("turn-first-{suffix}");
        let second_turn = format!("turn-second-{suffix}");
        enqueue_turn(repo, &format!("job-first-{suffix}"), "conv_a", &first_turn, 30).await;
        enqueue_turn(repo, &format!("job-second-{suffix}"), "conv_a2", &second_turn, 30).await;
        let first = repo
            .claim_next_job(ClaimMemoryJobRow {
                user_id: USER_A.into(),
                worker_id: format!("worker-first-{suffix}"),
                lease_token: format!("lease-first-{suffix}"),
                now: 31,
                lease_duration_ms: 100,
            })
            .await
            .unwrap()
            .unwrap();
        let second = repo
            .claim_next_job(ClaimMemoryJobRow {
                user_id: USER_A.into(),
                worker_id: format!("worker-second-{suffix}"),
                lease_token: format!("lease-second-{suffix}"),
                now: 31,
                lease_duration_ms: 100,
            })
            .await
            .unwrap()
            .unwrap();
        (first, second)
    }

    async fn running_with_successor(
        repo: &SqliteMemoryRepository,
    ) -> (crate::models::MemoryJobRow, crate::models::MemoryJobRow) {
        enqueue_turn(repo, "job-running", "conv_a", "turn-1", 10).await;
        let running = repo
            .claim_next_job(ClaimMemoryJobRow {
                user_id: USER_A.into(),
                worker_id: "worker".into(),
                lease_token: "lease-running".into(),
                now: 20,
                lease_duration_ms: 100,
            })
            .await
            .unwrap()
            .unwrap();
        enqueue_turn(repo, "job-successor", "conv_a", "turn-2", 21).await;
        let successor = enqueue_turn(repo, "ignored", "conv_a", "turn-3", 22).await.unwrap();
        (running, successor)
    }

    async fn assert_merged_successor(
        repo: &SqliteMemoryRepository,
        old_job_id: &str,
        successor_id: &str,
        state: &str,
        attempt_count: i64,
    ) -> crate::models::MemoryJobRow {
        assert!(repo.get_job(USER_A, old_job_id).await.unwrap().is_none());
        let successor = repo.get_job(USER_A, successor_id).await.unwrap().unwrap();
        assert_eq!(successor.state, state);
        assert_eq!(successor.attempt_count, attempt_count);
        assert_eq!(successor.turn_count, 3);
        assert_eq!(successor.through_turn_id, "turn-3");
        assert_eq!(
            repo.list_job_turns(USER_A, successor_id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2", "turn-3"],
        );
        let active_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM memory_jobs WHERE user_id = ? AND conversation_id = 'conv_a'
             AND state IN ('pending','retry_wait','blocked')",
        )
        .bind(USER_A)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(active_count, 1);
        successor
    }

    #[tokio::test]
    async fn sqlite_memory_defaults_consent_and_reset_boundaries_are_user_scoped() {
        let (repo, _, _db) = setup().await;
        let defaults = repo.get_settings(USER_B).await.unwrap();
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
    async fn sqlite_memory_policy_fences_only_effective_capture_changes() {
        for direction in ["inherit-to-explicit", "explicit-to-inherit"] {
            for state in ["pending", "running", "retry_wait", "blocked", "failed"] {
                let (repo, _, _db) = setup().await;
                if direction == "explicit-to-inherit" {
                    repo.update_conversation_policy(UpdateConversationMemoryPolicyRow {
                        user_id: USER_A.into(),
                        conversation_id: "conv_a".into(),
                        capture_enabled: Some(true),
                        recall_enabled: None,
                        now: 5,
                    })
                    .await
                    .unwrap();
                }
                let before_epoch: i64 = sqlx::query_scalar(
                    "SELECT COALESCE(lifecycle_epoch,0) FROM conversation_memory_policies
                     WHERE user_id = ? AND conversation_id = 'conv_a'",
                )
                .bind(USER_A)
                .fetch_optional(&repo.pool)
                .await
                .unwrap()
                .unwrap_or(0);
                let job_id = format!("job-{direction}-{state}");
                insert_job_in_state(&repo, &job_id, state).await;

                repo.update_conversation_policy(UpdateConversationMemoryPolicyRow {
                    user_id: USER_A.into(),
                    conversation_id: "conv_a".into(),
                    capture_enabled: (direction == "inherit-to-explicit").then_some(true),
                    recall_enabled: None,
                    now: 20,
                })
                .await
                .unwrap();

                let stored = repo.get_job(USER_A, &job_id).await.unwrap().unwrap();
                assert_eq!(stored.state, state, "{direction} must preserve {state}");
                let policy = repo.effective_policy(USER_A, "conv_a").await.unwrap();
                assert!(policy.capture_enabled);
                assert_eq!(
                    policy.conversation_epoch, before_epoch,
                    "{direction} must not fence equivalent effective capture for {state}",
                );
            }
        }

        let (repo, _, _db) = setup().await;
        insert_job_in_state(&repo, "job-effective-disable", "failed").await;
        let disabled = repo
            .update_conversation_policy(UpdateConversationMemoryPolicyRow {
                user_id: USER_A.into(),
                conversation_id: "conv_a".into(),
                capture_enabled: Some(false),
                recall_enabled: None,
                now: 30,
            })
            .await
            .unwrap();
        assert!(!disabled.capture_enabled);
        assert_eq!(disabled.conversation_epoch, 1);
        let canceled = repo.get_job(USER_A, "job-effective-disable").await.unwrap().unwrap();
        assert_eq!(canceled.state, "canceled");
        assert_eq!(canceled.last_error_code.as_deref(), Some("canceled"));
    }

    #[tokio::test]
    async fn sqlite_memory_duplicate_enqueue_coalesces_and_running_job_has_one_pending_successor() {
        let (repo, _, _db) = setup().await;
        let first = enqueue_turn(&repo, "job-1", "conv_a", "turn-1", 10).await.unwrap();
        assert_eq!(first.id, "job-1");
        assert!(enqueue_turn(&repo, "duplicate", "conv_a", "turn-1", 11).await.is_none());

        let coalesced = enqueue_turn(&repo, "job-2", "conv_a", "turn-2", 12).await.unwrap();
        assert_eq!(coalesced.id, "job-1");
        assert_eq!(coalesced.through_turn_id, "turn-2");
        assert_eq!(coalesced.turn_count, 2);
        assert_eq!(
            repo.list_job_turns(USER_A, "job-1", 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2"],
        );
        assert!(
            enqueue_turn(&repo, "delayed-old", "conv_a", "turn-1", 13)
                .await
                .is_none()
        );
        let still_monotonic = repo.get_job(USER_A, "job-1").await.unwrap().unwrap();
        assert_eq!(still_monotonic.through_turn_id, "turn-2");
        assert_eq!(repo.count_jobs(USER_A, "conv_a", "pending").await.unwrap(), 1);

        repo.claim_next_job(claim(USER_A, "worker", 13)).await.unwrap().unwrap();
        let pending = enqueue_turn(&repo, "job-next", "conv_a", "turn-3", 14).await.unwrap();
        assert_eq!(pending.id, "job-next");
        let pending = enqueue_turn(&repo, "ignored-id", "conv_a", "turn-4", 15).await.unwrap();
        assert_eq!(pending.id, "job-next");
        assert_eq!(pending.through_turn_id, "turn-4");
        assert_eq!(repo.count_jobs(USER_A, "conv_a", "running").await.unwrap(), 1);
        assert_eq!(repo.count_jobs(USER_A, "conv_a", "pending").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn sqlite_memory_successor_merge_preserves_exact_order_for_release_queue_and_block_transitions() {
        for (label, state, increment_attempt) in [
            ("queue_full", "pending", false),
            ("canceled", "pending", false),
            ("retry", "retry_wait", true),
            ("blocked", "blocked", true),
        ] {
            let (repo, _, _db) = setup().await;
            let (running, successor) = running_with_successor(&repo).await;
            let original_hash = successor.input_hash.clone();
            let merged = repo
                .transition_running_job(TransitionMemoryJobRow {
                    user_id: USER_A.into(),
                    job_id: running.id.clone(),
                    worker_id: "worker".into(),
                    lease_token: "lease-running".into(),
                    state: state.into(),
                    next_attempt_at: (state == "retry_wait").then_some(90),
                    error_code: Some(label.into()),
                    increment_attempt,
                    increment_invalid_output: false,
                    now: 30,
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(merged.id, successor.id, "{label}");
            let merged =
                assert_merged_successor(&repo, &running.id, &successor.id, state, i64::from(increment_attempt)).await;
            assert_ne!(merged.input_hash, original_hash, "{label}");
            assert_eq!(merged.last_error_code.as_deref(), Some(label));
        }

        let (repo, _, _db) = setup().await;
        let (running, successor) = running_with_successor(&repo).await;
        assert!(
            repo.release_lease(ReleaseMemoryLeaseRow {
                user_id: USER_A.into(),
                job_id: running.id.clone(),
                worker_id: "worker".into(),
                lease_token: "lease-running".into(),
                now: 30,
            })
            .await
            .unwrap()
        );
        assert_merged_successor(&repo, &running.id, &successor.id, "pending", 0).await;
    }

    #[tokio::test]
    async fn sqlite_memory_terminal_failures_absorb_successor_without_skipping_predecessor_turns() {
        for error_code in ["invalid_input", "invalid_output", "exhausted_retry"] {
            let (repo, _, _db) = setup().await;
            let (running, successor) = running_with_successor(&repo).await;
            let failed = repo
                .transition_running_job(TransitionMemoryJobRow {
                    user_id: USER_A.into(),
                    job_id: running.id.clone(),
                    worker_id: "worker".into(),
                    lease_token: "lease-running".into(),
                    state: "failed".into(),
                    next_attempt_at: None,
                    error_code: Some(error_code.into()),
                    increment_attempt: true,
                    increment_invalid_output: error_code == "invalid_output",
                    now: 30,
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(failed.id, running.id, "{error_code}");
            assert_eq!(failed.state, "failed", "{error_code}");
            assert!(repo.get_job(USER_A, &successor.id).await.unwrap().is_none());
            assert_eq!(failed.turn_count, 3);
            assert_eq!(
                repo.list_job_turns(USER_A, &failed.id, 10)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|turn| turn.turn_id)
                    .collect::<Vec<_>>(),
                ["turn-1", "turn-2", "turn-3"],
                "{error_code}",
            );

            sqlx::query("UPDATE memory_jobs SET state = 'pending' WHERE id = ? AND state = 'failed'")
                .bind(&failed.id)
                .execute(&repo.pool)
                .await
                .unwrap();
            let retried = repo
                .claim_next_job(claim(USER_A, "manual-retry", 40))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(retried.id, failed.id, "{error_code}");
            assert_eq!(retried.turn_count, 3, "{error_code}");
        }
    }

    #[tokio::test]
    async fn sqlite_memory_failed_job_remains_the_barrier_for_later_enqueue() {
        let (repo, _, _db) = setup().await;
        let (running, _successor) = running_with_successor(&repo).await;
        let failed = repo
            .transition_running_job(TransitionMemoryJobRow {
                user_id: USER_A.into(),
                job_id: running.id,
                worker_id: "worker".into(),
                lease_token: "lease-running".into(),
                state: "failed".into(),
                next_attempt_at: None,
                error_code: Some("invalid_input".into()),
                increment_attempt: true,
                increment_invalid_output: false,
                now: 30,
            })
            .await
            .unwrap()
            .unwrap();

        let barrier = enqueue_turn(&repo, "must-not-become-pending", "conv_a", "turn-4", 40)
            .await
            .unwrap();
        assert_eq!(barrier.id, failed.id);
        assert_eq!(barrier.state, "failed");
        assert_eq!(barrier.turn_count, 4);
        assert_eq!(
            repo.list_job_turns(USER_A, &barrier.id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2", "turn-3", "turn-4"],
        );
        assert!(
            repo.claim_next_job(claim(USER_A, "blocked", 50))
                .await
                .unwrap()
                .is_none()
        );

        let retried = repo.retry_failed_job(USER_A, &barrier.id, 60).await.unwrap().unwrap();
        assert_eq!(retried.id, barrier.id);
        assert_eq!(retried.state, "pending");
        let claimed = repo
            .claim_next_job(claim(USER_A, "manual-retry", 70))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, barrier.id);
        assert_eq!(
            repo.list_job_turns(USER_A, &claimed.id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2", "turn-3", "turn-4"],
        );
    }

    #[tokio::test]
    async fn sqlite_memory_manual_retry_defensively_absorbs_a_legacy_successor() {
        let (repo, _, _db) = setup().await;
        let (running, _successor) = running_with_successor(&repo).await;
        let failed = repo
            .transition_running_job(TransitionMemoryJobRow {
                user_id: USER_A.into(),
                job_id: running.id,
                worker_id: "worker".into(),
                lease_token: "lease-running".into(),
                state: "failed".into(),
                next_attempt_at: None,
                error_code: Some("invalid_input".into()),
                increment_attempt: true,
                increment_invalid_output: false,
                now: 30,
            })
            .await
            .unwrap()
            .unwrap();
        sqlx::query("UPDATE memory_jobs SET state = 'canceled' WHERE id = ?")
            .bind(&failed.id)
            .execute(&repo.pool)
            .await
            .unwrap();
        let legacy_successor = enqueue_turn(&repo, "legacy-successor", "conv_a", "turn-4", 40)
            .await
            .unwrap();
        sqlx::query("UPDATE memory_jobs SET state = 'failed' WHERE id = ?")
            .bind(&failed.id)
            .execute(&repo.pool)
            .await
            .unwrap();

        assert!(
            repo.claim_next_job(claim(USER_A, "blocked", 50))
                .await
                .unwrap()
                .is_none()
        );
        let retried = repo.retry_failed_job(USER_A, &failed.id, 60).await.unwrap().unwrap();
        assert!(repo.get_job(USER_A, &legacy_successor.id).await.unwrap().is_none());
        assert_eq!(retried.state, "pending");
        assert_eq!(
            repo.list_job_turns(USER_A, &retried.id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2", "turn-3", "turn-4"],
        );
    }

    #[tokio::test]
    async fn sqlite_memory_failed_barrier_blocks_and_absorbs_an_expired_legacy_running_successor() {
        let (repo, _, _db) = setup().await;
        let (barrier, successor) = running_with_successor(&repo).await;
        sqlx::query(
            "UPDATE memory_jobs SET state = 'failed',last_error_code = 'invalid_input',
             lease_owner = NULL,lease_token = NULL,lease_expires_at = NULL WHERE id = ?",
        )
        .bind(&barrier.id)
        .execute(&repo.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE memory_jobs SET state = 'running',lease_owner = 'legacy-worker',
             lease_token = 'legacy-lease',lease_expires_at = 30 WHERE id = ?",
        )
        .bind(&successor.id)
        .execute(&repo.pool)
        .await
        .unwrap();

        assert!(
            repo.claim_next_job(claim(USER_A, "blocked", 40))
                .await
                .unwrap()
                .is_none()
        );
        let retried = repo.retry_failed_job(USER_A, &barrier.id, 50).await.unwrap().unwrap();
        assert!(repo.get_job(USER_A, &successor.id).await.unwrap().is_none());
        assert_eq!(retried.state, "pending");
        assert_eq!(
            repo.list_job_turns(USER_A, &retried.id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2", "turn-3"],
        );
    }

    #[tokio::test]
    async fn sqlite_memory_enqueue_after_commit_uses_transaction_current_cursor_and_revision() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-first", "conv_a", "turn-1").await;
        repo.commit_update(commit("job-first", "conv_a", "turn-1", 0, Vec::new(), 20))
            .await
            .unwrap();

        enqueue_turn(&repo, "job-second", "conv_a", "turn-2", 30).await;
        let second = repo.get_job(USER_A, "job-second").await.unwrap().unwrap();
        assert_eq!(second.from_turn_id.as_deref(), Some("turn-1"));
        assert_eq!(second.expected_revision, 1);
    }

    #[tokio::test]
    async fn sqlite_memory_enqueue_stores_the_repository_canonical_snapshot_hash() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "job-snapshot", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        let stored_hash: String =
            sqlx::query_scalar("SELECT turn_hash FROM memory_job_turns WHERE job_id = ? AND position = 0")
                .bind(job.id)
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_ne!(stored_hash, "turn-hash-turn-1");
    }

    #[tokio::test]
    async fn sqlite_memory_commit_snapshot_drift_requeues_without_partial_writes() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-drift", "conv_a", "turn-1").await;
        sqlx::query("UPDATE messages SET status = 'error' WHERE id = 'msg-conv_a-turn-1-assistant'")
            .execute(&repo.pool)
            .await
            .unwrap();

        assert_eq!(
            repo.commit_update(commit("job-drift", "conv_a", "turn-1", 0, Vec::new(), 20))
                .await
                .unwrap(),
            CommitMemoryUpdateResult::SnapshotChanged,
        );
        assert_eq!(
            repo.get_job(USER_A, "job-drift").await.unwrap().unwrap().state,
            "pending",
        );
        assert!(repo.get_conversation_memory(USER_A, "conv_a").await.unwrap().is_none());
        let change_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM memory_change_sets WHERE job_id = 'job-drift'")
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(change_count, 0);
    }

    #[tokio::test]
    async fn sqlite_memory_bounded_snapshot_ignores_unaccepted_tool_rows_before_counting() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "job-filtered", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        for index in 0..200 {
            let (message_type, status, hidden) = match index % 4 {
                0 => ("tool_call", "finish", false),
                1 => ("permission", "finish", false),
                2 => ("text", "error", false),
                _ => ("text", "finish", true),
            };
            sqlx::query(
                "INSERT INTO messages
                    (id,conversation_id,turn_id,type,content,position,status,hidden,created_at)
                 VALUES (?, 'conv_a', 'turn-1', ?, ?, 'left', ?, ?, ?)",
            )
            .bind(format!("excluded-row-{index:03}"))
            .bind(message_type)
            .bind(serde_json::json!({ "content": "x".repeat(1024), "raw": "x".repeat(1024) }).to_string())
            .bind(status)
            .bind(hidden)
            .bind(100 + index)
            .execute(&repo.pool)
            .await
            .unwrap();
        }
        sqlx::query("UPDATE messages SET content = ? WHERE id = 'msg-conv_a-turn-1-assistant'")
            .bind(serde_json::json!({ "content": "done", "raw": "x".repeat(1024 * 1024) }).to_string())
            .execute(&repo.pool)
            .await
            .unwrap();
        let claimed = repo
            .claim_next_job(claim(USER_A, "filtered-worker", 1_000))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, job.id);

        let bounded = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        assert!(bounded.snapshot_matches);
        assert!(!bounded.limit_exceeded);
        assert_eq!(bounded.message_count, 2);
        assert_eq!(bounded.messages.len(), 2);
        assert!(bounded.messages.iter().all(|message| message.content.len() < 100));
    }

    #[tokio::test]
    async fn sqlite_memory_bounded_snapshot_excludes_all_unicode_whitespace_before_counting() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "job-whitespace", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        for index in 0..200 {
            sqlx::query(
                "INSERT INTO messages
                    (id,conversation_id,turn_id,type,content,position,status,hidden,created_at)
                 VALUES (?, 'conv_a', 'turn-1', 'text', ?, 'left', 'finish', 0, ?)",
            )
            .bind(format!("whitespace-row-{index:03}"))
            .bind(serde_json::json!({ "content": " \t\n\r" }).to_string())
            .bind(100 + index)
            .execute(&repo.pool)
            .await
            .unwrap();
        }

        let bounded = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        assert!(bounded.snapshot_matches);
        assert!(!bounded.limit_exceeded);
        assert_eq!(bounded.message_count, 2);
        assert_eq!(bounded.messages.len(), 2);
    }

    #[tokio::test]
    async fn sqlite_memory_bounded_snapshot_excludes_padded_noncanonical_type_names() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "job-padded-types", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        for (index, message_type) in ["\ttext\t", "\u{2003}text\u{2003}"]
            .into_iter()
            .cycle()
            .take(200)
            .enumerate()
        {
            sqlx::query(
                "INSERT INTO messages
                    (id,conversation_id,turn_id,type,content,position,status,hidden,created_at)
                 VALUES (?, 'conv_a', 'turn-1', ?, ?, 'left', 'finish', 0, ?)",
            )
            .bind(format!("padded-type-row-{index:03}"))
            .bind(message_type)
            .bind(serde_json::json!({ "content": "must be excluded" }).to_string())
            .bind(100 + index as i64)
            .execute(&repo.pool)
            .await
            .unwrap();
        }

        let bounded = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        assert!(bounded.snapshot_matches);
        assert!(!bounded.limit_exceeded);
        assert_eq!(bounded.message_count, 2);
    }

    #[tokio::test]
    async fn sqlite_memory_finalize_claim_rejects_eligibility_mutation_without_blessing() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "job-gap-eligibility", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        let claimed = repo
            .claim_next_job(claim(USER_A, "gap-worker", 1_000))
            .await
            .unwrap()
            .unwrap();
        let validated = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        assert!(validated.snapshot_matches);
        sqlx::query("UPDATE messages SET status = 'error' WHERE id = 'msg-conv_a-turn-1-assistant'")
            .execute(&repo.pool)
            .await
            .unwrap();

        let finalized = repo
            .finalize_claimed_job_snapshot(FinalizeMemoryJobSnapshotRow {
                user_id: USER_A.into(),
                job_id: job.id.clone(),
                lease_token: claimed.lease_token.unwrap(),
                expected_global_epoch: claimed.global_epoch,
                expected_conversation_epoch: claimed.conversation_epoch,
                turn_snapshots: vec![MemoryTurnSnapshotExpectationRow {
                    turn_id: "turn-1".into(),
                    snapshot_hash: validated.snapshot_hash.clone(),
                }],
                reconciliation_snapshot: None,
                require_existing_reconciliation_snapshot: false,
                now: 20,
            })
            .await
            .unwrap();
        assert_eq!(finalized, FinalizeMemoryJobSnapshotResult::SnapshotChanged);
        let stored_hash: String =
            sqlx::query_scalar("SELECT turn_hash FROM memory_job_turns WHERE job_id = ? AND turn_id = 'turn-1'")
                .bind(&job.id)
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(stored_hash, validated.snapshot_hash);
    }

    #[tokio::test]
    async fn sqlite_memory_finalize_claim_rejects_oversized_mutation_without_blessing() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "job-gap-bounds", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        let claimed = repo
            .claim_next_job(claim(USER_A, "gap-worker", 1_000))
            .await
            .unwrap()
            .unwrap();
        let validated = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        for index in 0..127 {
            sqlx::query(
                "INSERT INTO messages
                    (id,conversation_id,turn_id,type,content,position,status,hidden,created_at)
                 VALUES (?, 'conv_a', 'turn-1', 'text', ?, 'left', 'finish', 0, ?)",
            )
            .bind(format!("eligible-gap-{index:03}"))
            .bind(serde_json::json!({ "content": format!("accepted-{index}") }).to_string())
            .bind(100 + index)
            .execute(&repo.pool)
            .await
            .unwrap();
        }

        assert_eq!(
            repo.finalize_claimed_job_snapshot(FinalizeMemoryJobSnapshotRow {
                user_id: USER_A.into(),
                job_id: job.id,
                lease_token: claimed.lease_token.unwrap(),
                expected_global_epoch: claimed.global_epoch,
                expected_conversation_epoch: claimed.conversation_epoch,
                turn_snapshots: vec![MemoryTurnSnapshotExpectationRow {
                    turn_id: "turn-1".into(),
                    snapshot_hash: validated.snapshot_hash,
                }],
                reconciliation_snapshot: None,
                require_existing_reconciliation_snapshot: false,
                now: 20,
            })
            .await
            .unwrap(),
            FinalizeMemoryJobSnapshotResult::SnapshotChanged,
        );
    }

    #[tokio::test]
    async fn sqlite_memory_finalizes_content_free_entry_snapshot_and_rejects_later_drift() {
        let (repo, _, _db) = setup().await;
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                 schema_version, created_at, updated_at)
             VALUES ('snapshot-entry', ?, 'decision', 'snapshot key', 'snapshot-fingerprint',
                     'private memory content', 'active', 0, 0, 1, 1, 1)",
        )
        .bind(USER_A)
        .execute(&repo.pool)
        .await
        .unwrap();
        let job = enqueue_turn(&repo, "snapshot-job", "conv_a", "turn-1", 10)
            .await
            .unwrap();
        let claimed = repo
            .claim_next_job(claim(USER_A, "snapshot-worker", 20))
            .await
            .unwrap()
            .unwrap();
        let turn = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        let entry_snapshot = MemoryReconciliationSnapshotRow {
            id: "snapshot-entry".into(),
            revision: 0,
            state: "active".into(),
            fingerprint: "snapshot-fingerprint".into(),
            project_id: None,
            workspace_key: None,
            pinned: false,
            user_edited: false,
            content_hash: memory_entry_content_hash(Some("private memory content")),
        };
        let finalize = |require_existing_reconciliation_snapshot| FinalizeMemoryJobSnapshotRow {
            user_id: USER_A.into(),
            job_id: job.id.clone(),
            lease_token: claimed.lease_token.clone().unwrap(),
            expected_global_epoch: claimed.global_epoch,
            expected_conversation_epoch: claimed.conversation_epoch,
            turn_snapshots: vec![MemoryTurnSnapshotExpectationRow {
                turn_id: "turn-1".into(),
                snapshot_hash: turn.snapshot_hash.clone(),
            }],
            reconciliation_snapshot: Some(vec![entry_snapshot.clone()]),
            require_existing_reconciliation_snapshot,
            now: 21,
        };

        let finalized = repo.finalize_claimed_job_snapshot(finalize(false)).await.unwrap();
        let FinalizeMemoryJobSnapshotResult::Finalized(finalized) = finalized else {
            panic!("expected finalized snapshot");
        };
        let persisted = finalized.reconciliation_snapshot_json.unwrap();
        assert!(!persisted.contains("private memory content"));
        assert_eq!(
            serde_json::from_str::<Vec<MemoryReconciliationSnapshotRow>>(&persisted).unwrap(),
            vec![entry_snapshot.clone()],
        );

        sqlx::query(
            "UPDATE memory_entries SET content = 'changed after evidence', revision = revision + 1
             WHERE id = 'snapshot-entry'",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        assert_eq!(
            repo.finalize_claimed_job_snapshot(finalize(true)).await.unwrap(),
            FinalizeMemoryJobSnapshotResult::ReconciliationChanged,
        );
        let unchanged: String =
            sqlx::query_scalar("SELECT reconciliation_snapshot_json FROM memory_jobs WHERE id = 'snapshot-job'")
                .fetch_one(&repo.pool)
                .await
                .unwrap();
        assert_eq!(unchanged, persisted);
    }

    #[tokio::test]
    async fn sqlite_memory_commit_requeues_entry_drift_after_snapshot_finalization() {
        let (repo, _, _db) = setup().await;
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                 schema_version, created_at, updated_at)
             VALUES ('snapshot-drift-entry', ?, 'decision', 'snapshot key', 'snapshot-drift-fingerprint',
                     'private memory content', 'active', 0, 0, 1, 1, 1)",
        )
        .bind(USER_A)
        .execute(&repo.pool)
        .await
        .unwrap();
        claimed_job(&repo, "snapshot-drift-job", "conv_a", "turn-1").await;
        let job = repo.get_job(USER_A, "snapshot-drift-job").await.unwrap().unwrap();
        let turn = repo
            .load_job_turn_messages_bounded(USER_A, &job.id, "turn-1", 128, 64 * 1024)
            .await
            .unwrap();
        let finalized = repo
            .finalize_claimed_job_snapshot(FinalizeMemoryJobSnapshotRow {
                user_id: USER_A.into(),
                job_id: job.id.clone(),
                lease_token: job.lease_token.clone().unwrap(),
                expected_global_epoch: job.global_epoch,
                expected_conversation_epoch: job.conversation_epoch,
                turn_snapshots: vec![MemoryTurnSnapshotExpectationRow {
                    turn_id: "turn-1".into(),
                    snapshot_hash: turn.snapshot_hash,
                }],
                reconciliation_snapshot: Some(vec![MemoryReconciliationSnapshotRow {
                    id: "snapshot-drift-entry".into(),
                    revision: 0,
                    state: "active".into(),
                    fingerprint: "snapshot-drift-fingerprint".into(),
                    project_id: None,
                    workspace_key: None,
                    pinned: false,
                    user_edited: false,
                    content_hash: memory_entry_content_hash(Some("private memory content")),
                }]),
                require_existing_reconciliation_snapshot: false,
                now: 12,
            })
            .await
            .unwrap();
        assert!(matches!(finalized, FinalizeMemoryJobSnapshotResult::Finalized(_)));

        sqlx::query(
            "UPDATE memory_entries SET content = 'changed after finalization', revision = revision + 1
             WHERE id = 'snapshot-drift-entry'",
        )
        .execute(&repo.pool)
        .await
        .unwrap();

        assert_eq!(
            repo.commit_update(commit("snapshot-drift-job", "conv_a", "turn-1", 0, Vec::new(), 20,))
                .await
                .unwrap(),
            CommitMemoryUpdateResult::StaleReconciliation,
        );
        let job = repo.get_job(USER_A, "snapshot-drift-job").await.unwrap().unwrap();
        assert_eq!(job.state, "pending");
        assert_eq!(job.last_error_code.as_deref(), Some("stale_reconciliation"));
        assert_eq!(job.reconciliation_snapshot_json, None);
        assert!(repo.get_conversation_memory(USER_A, "conv_a").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_memory_expired_recovery_merges_running_predecessor_before_successor() {
        let (repo, _, _db) = setup().await;
        let (running, successor) = running_with_successor(&repo).await;
        assert_eq!(repo.recover_expired_jobs(121).await.unwrap(), 1);
        assert_merged_successor(&repo, &running.id, &successor.id, "pending", 0).await;
    }

    #[tokio::test]
    async fn sqlite_memory_split_append_and_rebase_keep_canonical_hashes_and_exact_turns() {
        let (repo, _, _db) = setup().await;
        let first = enqueue_turn(&repo, "job-batch", "conv_a", "turn-1", 10).await.unwrap();
        let appended = enqueue_turn(&repo, "ignored-2", "conv_a", "turn-2", 11).await.unwrap();
        enqueue_turn(&repo, "ignored-3", "conv_a", "turn-3", 12).await;
        assert_ne!(first.input_hash, appended.input_hash);
        let running = repo.claim_next_job(claim(USER_A, "worker", 20)).await.unwrap().unwrap();
        assert!(
            repo.split_claimed_job(SplitMemoryJobRow {
                user_id: USER_A.into(),
                job_id: running.id.clone(),
                lease_token: running.lease_token.clone().unwrap(),
                prefix_count: 1,
                pending_job_id: "job-remainder".into(),
                now: 21,
            })
            .await
            .unwrap()
        );
        let prefix = repo.get_job(USER_A, &running.id).await.unwrap().unwrap();
        let remainder = repo.get_job(USER_A, "job-remainder").await.unwrap().unwrap();
        assert_ne!(prefix.input_hash, running.input_hash);
        let remainder_hash = remainder.input_hash.clone();
        let appended = enqueue_turn(&repo, "ignored-4", "conv_a", "turn-4", 22).await.unwrap();
        assert_eq!(appended.id, "job-remainder");
        assert_ne!(appended.input_hash, remainder_hash);
        assert!(
            repo.release_lease(ReleaseMemoryLeaseRow {
                user_id: USER_A.into(),
                job_id: running.id.clone(),
                worker_id: "worker".into(),
                lease_token: running.lease_token.unwrap(),
                now: 23,
            })
            .await
            .unwrap()
        );
        let merged = repo.get_job(USER_A, "job-remainder").await.unwrap().unwrap();
        assert_ne!(merged.input_hash, appended.input_hash);
        assert_eq!(
            repo.list_job_turns(USER_A, "job-remainder", 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-1", "turn-2", "turn-3", "turn-4"],
        );
    }

    #[tokio::test]
    async fn sqlite_memory_large_backlog_reads_only_bounded_prefix_with_sentinel() {
        let (repo, _, _db) = setup().await;
        for index in 0..256 {
            let turn_id = format!("turn-{index:03}");
            enqueue_turn(&repo, &format!("job-{index}"), "conv_a", &turn_id, 10 + index).await;
        }
        let job: crate::models::MemoryJobRow = sqlx::query_as(
            "SELECT * FROM memory_jobs WHERE user_id = ? AND conversation_id = 'conv_a' AND state = 'pending'",
        )
        .bind(USER_A)
        .fetch_one(&repo.pool)
        .await
        .unwrap();
        assert_eq!(job.turn_count, 256);
        let bounded = repo.list_job_turns(USER_A, &job.id, 33).await.unwrap();
        assert_eq!(bounded.len(), 33);
        assert_eq!(bounded.first().unwrap().turn_id, "turn-000");
        assert_eq!(bounded.last().unwrap().turn_id, "turn-032");
    }

    #[tokio::test]
    async fn sqlite_memory_lifecycle_epoch_fences_stale_enqueue_and_running_commit() {
        let (repo, _, _db) = setup().await;
        let old_policy = repo.effective_policy(USER_A, "conv_a").await.unwrap();
        sqlx::query(
            "INSERT INTO messages
                (id, conversation_id, turn_id, type, content, position, status, hidden, created_at)
             VALUES ('crossing-before', 'conv_a', 'turn-crossing', 'text', '{}', 'right', 'finish', 0, 10),
                    ('crossing-after', 'conv_a', 'turn-crossing', 'text', '{}', 'left', 'finish', 0, 30)",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        repo.clear_memory(USER_A, 20).await.unwrap();
        let mut stale = enqueue("stale-callback", "conv_a", "turn-crossing", 31);
        stale.expected_global_epoch = old_policy.global_epoch;
        stale.expected_conversation_epoch = old_policy.conversation_epoch;
        assert!(repo.enqueue_completed_turn(stale).await.unwrap().is_none());

        let current = repo.effective_policy(USER_A, "conv_a").await.unwrap();
        let mut crossing = enqueue("crossing", "conv_a", "turn-crossing", 32);
        crossing.expected_global_epoch = current.global_epoch;
        crossing.expected_conversation_epoch = current.conversation_epoch;
        assert!(
            repo.enqueue_completed_turn(crossing).await.unwrap().is_none(),
            "the earliest canonical message, not the latest, fences reset-crossing turns",
        );

        enqueue_turn(&repo, "stale-job-old-worker", "conv_a", "turn-new", 40).await;
        let mut current_enqueue = enqueue("job-old-worker", "conv_a", "turn-new", 40);
        current_enqueue.expected_global_epoch = current.global_epoch;
        current_enqueue.expected_conversation_epoch = current.conversation_epoch;
        repo.enqueue_completed_turn(current_enqueue).await.unwrap().unwrap();
        let running = repo.claim_next_job(claim(USER_A, "worker", 41)).await.unwrap().unwrap();
        repo.update_conversation_memory_lifecycle(UpdateConversationMemoryLifecycleRow {
            user_id: USER_A.into(),
            conversation_id: "conv_a".into(),
            capture_enabled: false,
            now: 42,
        })
        .await
        .unwrap();
        let mut stale_commit = commit(
            &running.id,
            "conv_a",
            "turn-new",
            0,
            vec![entry("stale-entry", "stale-fp", vec![source("conv_a", "turn-new")])],
            43,
        );
        stale_commit.lease_token = running.lease_token.unwrap();
        assert!(matches!(
            repo.commit_update(stale_commit).await,
            Err(DbError::Conflict(_))
        ));
        assert!(repo.get_entry(USER_A, "stale-entry").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlite_memory_reset_fence_uses_earliest_all_row_even_when_excluded_from_evidence() {
        let (repo, _, _db) = setup().await;
        repo.delete_conversation_memory(USER_A, "conv_a", 30).await.unwrap();
        let excluded_rows = [
            (
                "hidden",
                "text",
                serde_json::json!({ "content": "hidden" }).to_string(),
                "finish",
                true,
            ),
            (
                "tool",
                "tool_call",
                serde_json::json!({ "content": "raw" }).to_string(),
                "finish",
                false,
            ),
            (
                "error",
                "text",
                serde_json::json!({ "content": "failed" }).to_string(),
                "error",
                false,
            ),
            ("malformed", "text", "not-json".into(), "finish", false),
        ];
        for (index, (label, message_type, content, status, hidden)) in excluded_rows.into_iter().enumerate() {
            let turn_id = format!("turn-excluded-{label}");
            sqlx::query(
                "INSERT INTO messages
                    (id,conversation_id,turn_id,type,content,position,status,hidden,created_at)
                 VALUES (?, 'conv_a', ?, ?, ?, 'left', ?, ?, ?)",
            )
            .bind(format!("pre-reset-{label}"))
            .bind(&turn_id)
            .bind(message_type)
            .bind(content)
            .bind(status)
            .bind(hidden)
            .bind(10 + index as i64)
            .execute(&repo.pool)
            .await
            .unwrap();
            assert!(
                enqueue_turn(
                    &repo,
                    &format!("stale-epoch-{label}"),
                    "conv_a",
                    &turn_id,
                    40 + index as i64
                )
                .await
                .is_none(),
            );
            let mut current = enqueue(
                &format!("must-stay-blocked-{label}"),
                "conv_a",
                &turn_id,
                50 + index as i64,
            );
            current.expected_conversation_epoch = 1;
            assert!(
                repo.enqueue_completed_turn(current).await.unwrap().is_none(),
                "pre-reset {label} row must fence the exact turn",
            );
        }
    }

    #[tokio::test]
    async fn sqlite_memory_atomic_enqueue_rechecks_a_complete_canonical_turn() {
        let (repo, _, _db) = setup().await;
        sqlx::query(
            "INSERT INTO messages
                (id,conversation_id,turn_id,type,content,position,status,hidden,created_at)
             VALUES ('partial-user','conv_a','turn-partial','text','{\"content\":\"work\"}',
                     'right','finish',0,10)",
        )
        .execute(&repo.pool)
        .await
        .unwrap();
        assert!(
            repo.enqueue_completed_turn(enqueue("partial", "conv_a", "turn-partial", 11))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn sqlite_memory_lease_expiry_arithmetic_is_checked() {
        let (repo, _, _db) = setup().await;
        enqueue_turn(&repo, "job-overflow", "conv_a", "turn-overflow", 10).await;
        let mut overflow = claim(USER_A, "worker", i64::MAX);
        overflow.lease_duration_ms = 1;
        assert!(matches!(repo.claim_next_job(overflow).await, Err(DbError::Conflict(_))));
    }

    #[tokio::test]
    async fn sqlite_memory_expired_lease_is_claimable_again() {
        let (repo, _, _db) = setup().await;
        enqueue_turn(&repo, "job-lease", "conv_a", "turn-1", 10).await;
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
        enqueue_turn(&repo, "job-fenced", "conv_a", "turn-lease", 10).await;
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
        let retry = repo.get_job(USER_A, "job-2").await.unwrap().unwrap();
        assert_eq!(retry.state, "pending");
        assert_eq!(retry.last_error_code.as_deref(), Some("stale_revision"));
        assert_eq!(retry.attempt_count, 0);
    }

    #[tokio::test]
    async fn sqlite_memory_entry_revision_fences_cross_conversation_refine_races() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-entry-base", "conv_a", "turn-base").await;
        repo.commit_update(commit(
            "job-entry-base",
            "conv_a",
            "turn-base",
            0,
            vec![entry(
                "shared-entry",
                "fp-shared-entry",
                vec![source("conv_a", "turn-base")],
            )],
            20,
        ))
        .await
        .unwrap();

        let (first, second) = claim_cross_conversation_jobs(&repo, "refine").await;
        let mut first_entry = entry(
            "first-candidate",
            "fp-shared-entry",
            vec![enqueued_source("conv_a", "turn-first-refine")],
        );
        first_entry.content = "first refine wins".into();
        first_entry.transition = CommitMemoryEntryTransition::Refine {
            target: expected_entry(
                "shared-entry",
                "fp-shared-entry",
                0,
                "active",
                Some("content for shared-entry"),
            ),
        };
        let mut first_commit = commit(
            &first.id,
            "conv_a",
            "turn-first-refine",
            first.expected_revision,
            vec![first_entry],
            40,
        );
        first_commit.lease_owner = "worker-first-refine".into();
        first_commit.lease_token = first.lease_token.unwrap();
        repo.commit_update(first_commit).await.unwrap();

        let mut stale_entry = entry(
            "stale-candidate",
            "fp-shared-entry",
            vec![enqueued_source("conv_a2", "turn-second-refine")],
        );
        stale_entry.content = "stale refine must not win".into();
        stale_entry.transition = CommitMemoryEntryTransition::Refine {
            target: expected_entry(
                "shared-entry",
                "fp-shared-entry",
                0,
                "active",
                Some("content for shared-entry"),
            ),
        };
        let mut stale_commit = commit(
            &second.id,
            "conv_a2",
            "turn-second-refine",
            second.expected_revision,
            vec![stale_entry],
            41,
        );
        stale_commit.lease_owner = "worker-second-refine".into();
        stale_commit.lease_token = second.lease_token.unwrap();
        let result = repo.commit_update(stale_commit).await.unwrap();

        assert!(!matches!(result, CommitMemoryUpdateResult::Committed { .. }));
        let stored = repo.get_entry(USER_A, "shared-entry").await.unwrap().unwrap();
        assert_eq!(stored.content.as_deref(), Some("first refine wins"));
        assert_eq!(stored.sources.len(), 2);
        let retry = repo.get_job(USER_A, &second.id).await.unwrap().unwrap();
        assert_eq!(retry.state, "pending");
        assert_eq!(retry.attempt_count, 0);
    }

    #[tokio::test]
    async fn sqlite_memory_entry_revision_fences_supersede_then_stale_refine() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-entry-base", "conv_a", "turn-base").await;
        repo.commit_update(commit(
            "job-entry-base",
            "conv_a",
            "turn-base",
            0,
            vec![entry(
                "shared-entry",
                "fp-shared-entry",
                vec![source("conv_a", "turn-base")],
            )],
            20,
        ))
        .await
        .unwrap();

        let (first, second) = claim_cross_conversation_jobs(&repo, "supersede").await;
        let mut replacement = entry(
            "replacement-entry",
            "fp-replacement-entry",
            vec![enqueued_source("conv_a", "turn-first-supersede")],
        );
        replacement.transition = CommitMemoryEntryTransition::Supersede {
            target: expected_entry(
                "shared-entry",
                "fp-shared-entry",
                0,
                "active",
                Some("content for shared-entry"),
            ),
        };
        let mut first_commit = commit(
            &first.id,
            "conv_a",
            "turn-first-supersede",
            first.expected_revision,
            vec![replacement],
            40,
        );
        first_commit.lease_owner = "worker-first-supersede".into();
        first_commit.lease_token = first.lease_token.unwrap();
        repo.commit_update(first_commit).await.unwrap();

        let mut stale_entry = entry(
            "stale-candidate",
            "fp-shared-entry",
            vec![enqueued_source("conv_a2", "turn-second-supersede")],
        );
        stale_entry.content = "stale refine must not touch superseded target".into();
        stale_entry.transition = CommitMemoryEntryTransition::Refine {
            target: expected_entry(
                "shared-entry",
                "fp-shared-entry",
                0,
                "active",
                Some("content for shared-entry"),
            ),
        };
        let mut stale_commit = commit(
            &second.id,
            "conv_a2",
            "turn-second-supersede",
            second.expected_revision,
            vec![stale_entry],
            41,
        );
        stale_commit.lease_owner = "worker-second-supersede".into();
        stale_commit.lease_token = second.lease_token.unwrap();
        let result = repo.commit_update(stale_commit).await.unwrap();

        assert!(!matches!(result, CommitMemoryUpdateResult::Committed { .. }));
        let target = repo.get_entry(USER_A, "shared-entry").await.unwrap().unwrap();
        assert_eq!(target.state, "superseded");
        assert_ne!(
            target.content.as_deref(),
            Some("stale refine must not touch superseded target")
        );
        assert_eq!(
            repo.get_job(USER_A, &second.id).await.unwrap().unwrap().state,
            "pending"
        );
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
    async fn sqlite_memory_conversation_forget_cancels_failed_barriers_before_new_enqueue() {
        let (repo, _, _db) = setup().await;
        let job = enqueue_turn(&repo, "pre-forget", "conv_a", "turn-1", 10).await.unwrap();
        let claimed = repo
            .claim_next_job(claim(USER_A, "forget-worker", 20))
            .await
            .unwrap()
            .unwrap();
        let failed = repo
            .transition_running_job(TransitionMemoryJobRow {
                user_id: USER_A.into(),
                job_id: job.id.clone(),
                worker_id: "forget-worker".into(),
                lease_token: claimed.lease_token.unwrap(),
                state: "failed".into(),
                next_attempt_at: None,
                error_code: Some("invalid_input".into()),
                increment_attempt: true,
                increment_invalid_output: false,
                now: 21,
            })
            .await
            .unwrap()
            .unwrap();
        repo.delete_conversation_memory(USER_A, "conv_a", 30).await.unwrap();
        assert_eq!(
            repo.get_job(USER_A, &failed.id).await.unwrap().unwrap().state,
            "canceled"
        );
        assert!(repo.retry_failed_job(USER_A, &failed.id, 31).await.unwrap().is_none());

        assert!(
            enqueue_turn(&repo, "stale-epoch", "conv_a", "turn-2", 40)
                .await
                .is_none()
        );
        let mut current_epoch = enqueue("post-forget", "conv_a", "turn-2", 40);
        current_epoch.expected_conversation_epoch = 1;
        let fresh = repo.enqueue_completed_turn(current_epoch).await.unwrap().unwrap();
        assert_eq!(fresh.id, "post-forget");
        assert_eq!(fresh.turn_count, 1);
        assert_eq!(
            repo.list_job_turns(USER_A, &fresh.id, 10)
                .await
                .unwrap()
                .into_iter()
                .map(|turn| turn.turn_id)
                .collect::<Vec<_>>(),
            ["turn-2"],
        );
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
                expected_revision: 1,
                expected_state: "deleted".into(),
                content: Some("must stay deleted".into()),
                pinned: None,
                project_id: None,
                workspace_key: None,
                new_fingerprint: None,
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
    async fn sqlite_memory_keep_separate_tombstones_every_prior_identity_and_blocks_replay() {
        let (repo, _, _db) = setup().await;
        for (id, fingerprint, content) in [
            ("separate-version-a", "fp-separate-a", "Version A"),
            ("separate-version-b", "fp-separate-b", "Version B"),
        ] {
            sqlx::query(
                "INSERT INTO memory_entries
                    (id,user_id,kind,stable_key,fingerprint,content,state,pinned,user_edited,revision,
                     conflict_group_id,schema_version,created_at,updated_at)
                 VALUES (?,?,'decision',?,?,?,'conflict',0,0,0,'separate-group',1,10,10)",
            )
            .bind(id)
            .bind(USER_A)
            .bind(format!("key-{id}"))
            .bind(fingerprint)
            .bind(content)
            .execute(&repo.pool)
            .await
            .unwrap();
        }

        let resolved = repo
            .resolve_conflict(ResolveMemoryConflictRow {
                user_id: USER_A.into(),
                entry_id: "separate-version-a".into(),
                action: ResolveMemoryConflictActionRow::KeepSeparate {
                    tombstone_id_prefix: "separate-tombstone".into(),
                },
                now: 20,
            })
            .await
            .unwrap();
        assert!(
            resolved
                .iter()
                .all(|entry| entry.state == "active" && entry.user_edited)
        );
        let tombstoned_fingerprints: Vec<String> = sqlx::query_scalar(
            "SELECT fingerprint FROM memory_entries WHERE user_id = ? AND state = 'deleted'
             AND fingerprint IN ('fp-separate-a','fp-separate-b') ORDER BY fingerprint",
        )
        .bind(USER_A)
        .fetch_all(&repo.pool)
        .await
        .unwrap();
        assert_eq!(tombstoned_fingerprints, ["fp-separate-a", "fp-separate-b"]);

        claimed_job(&repo, "job-replay-separate", "conv_a", "turn-replay-separate").await;
        let replay = repo
            .commit_update(commit(
                "job-replay-separate",
                "conv_a",
                "turn-replay-separate",
                0,
                vec![
                    entry(
                        "replayed-version-a",
                        "fp-separate-a",
                        vec![source("conv_a", "turn-replay-separate")],
                    ),
                    entry(
                        "replayed-version-b",
                        "fp-separate-b",
                        vec![source("conv_a", "turn-replay-separate")],
                    ),
                ],
                30,
            ))
            .await
            .unwrap();
        assert!(matches!(replay, CommitMemoryUpdateResult::Committed { ref added_ids, .. } if added_ids.is_empty()));
        assert!(repo.get_entry(USER_A, "replayed-version-a").await.unwrap().is_none());
        assert!(repo.get_entry(USER_A, "replayed-version-b").await.unwrap().is_none());
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
                expected_revision: 0,
                expected_state: "active".into(),
                content: Some("must not report success".into()),
                pinned: None,
                project_id: None,
                workspace_key: None,
                new_fingerprint: None,
                now: 26,
            })
            .await,
            Err(DbError::Conflict(_))
        ));
    }

    #[tokio::test]
    async fn sqlite_memory_scope_edit_requires_a_rederived_fingerprint() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-scope-edit", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-scope-edit",
            "conv_a",
            "turn-1",
            0,
            vec![entry(
                "scope-entry",
                "scope-old-fingerprint",
                vec![source("conv_a", "turn-1")],
            )],
            20,
        ))
        .await
        .unwrap();

        assert!(matches!(
            repo.update_entry(UpdateMemoryEntryRow {
                user_id: USER_A.into(),
                id: "scope-entry".into(),
                expected_revision: 0,
                expected_state: "active".into(),
                content: None,
                pinned: None,
                project_id: Some(Some("project-moved".into())),
                workspace_key: None,
                new_fingerprint: None,
                now: 25,
            })
            .await,
            Err(DbError::Conflict(_))
        ));
        let unchanged = repo.get_entry(USER_A, "scope-entry").await.unwrap().unwrap();
        assert_eq!(unchanged.project_id, None);
        assert_eq!(unchanged.fingerprint, "scope-old-fingerprint");
    }

    #[tokio::test]
    async fn sqlite_memory_scope_edit_moves_the_canonical_lookup_under_revision_cas() {
        let (repo, _, db) = setup().await;
        let old_fingerprint = derive_memory_fingerprint(USER_A, None, None, "decision", "move key");
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                 schema_version, created_at, updated_at)
             VALUES ('move-entry', ?, 'decision', 'move key', ?, 'move content', 'active', 0, 0, 1, 1, 1)",
        )
        .bind(USER_A)
        .bind(&old_fingerprint)
        .execute(db.pool())
        .await
        .unwrap();
        let moved_fingerprint = derive_memory_fingerprint(USER_A, Some("project-moved"), None, "decision", "move key");

        assert!(matches!(
            repo.update_entry(UpdateMemoryEntryRow {
                user_id: USER_A.into(),
                id: "move-entry".into(),
                expected_revision: 1,
                expected_state: "active".into(),
                content: None,
                pinned: None,
                project_id: Some(Some("project-moved".into())),
                workspace_key: None,
                new_fingerprint: Some(moved_fingerprint.clone()),
                now: 2,
            })
            .await,
            Err(DbError::Conflict(_))
        ));
        let moved = repo
            .update_entry(UpdateMemoryEntryRow {
                user_id: USER_A.into(),
                id: "move-entry".into(),
                expected_revision: 0,
                expected_state: "active".into(),
                content: None,
                pinned: None,
                project_id: Some(Some("project-moved".into())),
                workspace_key: None,
                new_fingerprint: Some(moved_fingerprint.clone()),
                now: 3,
            })
            .await
            .unwrap();
        assert_eq!(moved.revision, 1);
        assert_eq!(moved.project_id.as_deref(), Some("project-moved"));
        assert_eq!(moved.fingerprint, moved_fingerprint);
        assert!(
            repo.reconciliation_entries(USER_A, &[old_fingerprint], &[])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            repo.reconciliation_entries(USER_A, &[moved.fingerprint], &[])
                .await
                .unwrap()
                .into_iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            ["move-entry"],
        );
    }

    #[tokio::test]
    async fn sqlite_memory_scope_edit_rejects_active_and_tombstoned_destination_identities() {
        let (repo, _, db) = setup().await;
        let source_fingerprint = derive_memory_fingerprint(USER_A, None, None, "decision", "source key");
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                 schema_version, created_at, updated_at)
             VALUES ('scope-source', ?, 'decision', 'source key', ?, 'source', 'active', 0, 0, 1, 1, 1)",
        )
        .bind(USER_A)
        .bind(&source_fingerprint)
        .execute(db.pool())
        .await
        .unwrap();
        for (scope, id, state, content, deleted_at) in [
            ("active-destination", "scope-active", "active", Some("active"), None),
            ("deleted-destination", "scope-deleted", "deleted", None, Some(2_i64)),
        ] {
            let fingerprint = derive_memory_fingerprint(USER_A, Some(scope), None, "decision", "source key");
            sqlx::query(
                "INSERT INTO memory_entries
                    (id, user_id, project_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                     schema_version, deleted_at, created_at, updated_at)
                 VALUES (?, ?, ?, 'decision', 'source key', ?, ?, ?, 0, 0, 1, ?, 2, 2)",
            )
            .bind(id)
            .bind(USER_A)
            .bind(scope)
            .bind(&fingerprint)
            .bind(content)
            .bind(state)
            .bind(deleted_at)
            .execute(db.pool())
            .await
            .unwrap();
            assert!(matches!(
                repo.update_entry(UpdateMemoryEntryRow {
                    user_id: USER_A.into(),
                    id: "scope-source".into(),
                    expected_revision: 0,
                    expected_state: "active".into(),
                    content: None,
                    pinned: None,
                    project_id: Some(Some(scope.into())),
                    workspace_key: None,
                    new_fingerprint: Some(fingerprint),
                    now: 3,
                })
                .await,
                Err(DbError::Conflict(_))
            ));
        }
        let source = repo.get_entry(USER_A, "scope-source").await.unwrap().unwrap();
        assert_eq!(source.revision, 0);
        assert_eq!(source.project_id, None);
        assert_eq!(source.fingerprint, source_fingerprint);
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
        enqueue_turn(&repo, "job-pending", "conv_a", "turn-2", 22).await;
        let stale_worker = repo
            .claim_next_job(ClaimMemoryJobRow {
                user_id: USER_A.into(),
                worker_id: "worker-before-clear".into(),
                lease_token: "lease-before-clear".into(),
                now: 23,
                lease_duration_ms: 100,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stale_worker.id, "job-pending");
        repo.update_conversation_policy(UpdateConversationMemoryPolicyRow {
            user_id: USER_A.into(),
            conversation_id: "conv_a2".into(),
            capture_enabled: Some(false),
            recall_enabled: Some(false),
            now: 21,
        })
        .await
        .unwrap();
        repo.upsert_import_state(MemoryImportStateRow {
            user_id: USER_A.into(),
            cursor: Some("legacy-cursor-7".into()),
            completed: false,
            started_at: Some(15),
            completed_at: None,
            updated_at: 22,
        })
        .await
        .unwrap();

        repo.clear_memory(USER_A, 50).await.unwrap();
        assert_eq!(repo.get_settings(USER_A).await.unwrap().reset_at, Some(50));
        assert!(repo.list_entries(USER_A).await.unwrap().is_empty());
        assert!(repo.get_conversation_memory(USER_A, "conv_a").await.unwrap().is_none());
        assert!(repo.get_job(USER_A, "job-clear").await.unwrap().is_none());
        assert!(repo.get_job(USER_A, "job-pending").await.unwrap().is_none());
        assert!(
            !repo
                .renew_lease(RenewMemoryLeaseRow {
                    user_id: USER_A.into(),
                    job_id: "job-pending".into(),
                    worker_id: "worker-before-clear".into(),
                    lease_token: "lease-before-clear".into(),
                    now: 51,
                    lease_duration_ms: 100,
                })
                .await
                .unwrap()
        );
        let policy = repo.effective_policy(USER_A, "conv_a2").await.unwrap();
        assert_eq!(policy.capture_override, Some(false));
        assert_eq!(policy.recall_override, Some(false));
        assert!(!policy.capture_enabled && !policy.recall_enabled);
        assert_eq!(policy.reset_at, Some(50));
        let import_state = repo.get_import_state(USER_A).await.unwrap().unwrap();
        assert_eq!(import_state.cursor.as_deref(), Some("legacy-cursor-7"));
        assert!(import_state.completed);
        assert_eq!(import_state.started_at, Some(15));
        assert_eq!(import_state.completed_at, Some(50));
        let stale_import_write = repo
            .upsert_import_state(MemoryImportStateRow {
                user_id: USER_A.into(),
                cursor: Some("stale-page-after-clear".into()),
                completed: false,
                started_at: Some(15),
                completed_at: None,
                updated_at: 55,
            })
            .await
            .unwrap();
        assert_eq!(stale_import_write.cursor.as_deref(), Some("legacy-cursor-7"));
        assert!(stale_import_write.completed);
        assert_eq!(stale_import_write.completed_at, Some(50));

        assert!(repo.get_import_state(USER_B).await.unwrap().is_none());
        repo.clear_memory(USER_B, 60).await.unwrap();
        let preimport_clear = repo.get_import_state(USER_B).await.unwrap().unwrap();
        assert_eq!(preimport_clear.cursor, None);
        assert!(preimport_clear.completed);
        assert_eq!(preimport_clear.started_at, Some(60));
        assert_eq!(preimport_clear.completed_at, Some(60));
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
            target: expected_entry(
                "old-decision",
                "fp-old-decision",
                0,
                "active",
                Some("content for old-decision"),
            ),
        };
        let mut contradiction = entry("new-issue", "fp-new-issue", vec![source("conv_a", "turn-2")]);
        contradiction.transition = CommitMemoryEntryTransition::Conflict {
            target: expected_entry("old-issue", "fp-old-issue", 0, "active", Some("content for old-issue")),
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
                        expected_revision: 0,
                        expected_state: "active".into(),
                        content: edited_content.map(str::to_owned),
                        pinned,
                        project_id: None,
                        workspace_key: None,
                        new_fingerprint: None,
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
                let expected = ExpectedMemoryEntryRow {
                    id: target_id.clone(),
                    revision: protected.revision,
                    state: protected.state.clone(),
                    fingerprint: protected.fingerprint.clone(),
                    project_id: protected.project_id.clone(),
                    workspace_key: protected.workspace_key.clone(),
                    content: protected.content.clone(),
                };
                candidate.transition = match transition {
                    "refine" => CommitMemoryEntryTransition::Refine {
                        target: expected.clone(),
                    },
                    "supersede" => CommitMemoryEntryTransition::Supersede {
                        target: expected.clone(),
                    },
                    "conflict" => CommitMemoryEntryTransition::Conflict {
                        target: expected,
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
                    assert_eq!(result.unwrap(), CommitMemoryUpdateResult::StaleReconciliation);
                    assert!(repo.get_entry(USER_A, &candidate_id).await.unwrap().is_none());
                    assert_eq!(
                        repo.get_conversation_memory(USER_A, "conv_a")
                            .await
                            .unwrap()
                            .unwrap()
                            .revision,
                        1
                    );
                    assert_eq!(repo.get_job(USER_A, &job_id).await.unwrap().unwrap().state, "pending");
                    assert_eq!(repo.list_change_sets(USER_A, 10).await.unwrap().len(), 1);
                }
            }
        }
    }

    #[tokio::test]
    async fn sqlite_memory_protected_identical_content_only_attaches_a_source() {
        let (repo, _, _db) = setup().await;
        claimed_job(&repo, "job-protected-source-base", "conv_a", "turn-1").await;
        repo.commit_update(commit(
            "job-protected-source-base",
            "conv_a",
            "turn-1",
            0,
            vec![entry(
                "protected-source-target",
                "fp-protected-source",
                vec![source("conv_a", "turn-1")],
            )],
            20,
        ))
        .await
        .unwrap();
        let protected = repo
            .update_entry(UpdateMemoryEntryRow {
                user_id: USER_A.into(),
                id: "protected-source-target".into(),
                expected_revision: 0,
                expected_state: "active".into(),
                content: None,
                pinned: Some(true),
                project_id: None,
                workspace_key: None,
                new_fingerprint: None,
                now: 21,
            })
            .await
            .unwrap();

        claimed_job(&repo, "job-protected-source", "conv_a", "turn-2").await;
        let mut candidate = entry(
            "unused-protected-candidate",
            "fp-protected-source",
            vec![source("conv_a", "turn-2")],
        );
        candidate.content = protected.content.clone().unwrap();
        candidate.transition = CommitMemoryEntryTransition::AttachSource {
            target: ExpectedMemoryEntryRow {
                id: protected.id.clone(),
                revision: protected.revision,
                state: protected.state.clone(),
                fingerprint: protected.fingerprint.clone(),
                project_id: protected.project_id.clone(),
                workspace_key: protected.workspace_key.clone(),
                content: protected.content.clone(),
            },
        };
        let result = repo
            .commit_update(commit(
                "job-protected-source",
                "conv_a",
                "turn-2",
                1,
                vec![candidate],
                30,
            ))
            .await
            .unwrap();
        assert!(matches!(
            result,
            CommitMemoryUpdateResult::Committed {
                ref refined_ids,
                ..
            } if refined_ids == &["protected-source-target"]
        ));
        let attached = repo
            .get_entry(USER_A, "protected-source-target")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attached.content, protected.content);
        assert_eq!(attached.state, "active");
        assert_eq!(attached.revision, protected.revision);
        assert_eq!(attached.sources.len(), 2);
        assert!(
            repo.get_entry(USER_A, "unused-protected-candidate")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn sqlite_memory_reconciliation_lookup_finds_rows_beyond_the_evidence_window() {
        let (repo, _, db) = setup().await;
        for index in 0..65 {
            sqlx::query(
                "INSERT INTO memory_entries
                    (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                     schema_version, created_at, updated_at)
                 VALUES (?, ?, 'decision', ?, ?, ?, 'active', 0, 0, 1, ?, ?)",
            )
            .bind(format!("lookup-entry-{index:02}"))
            .bind(USER_A)
            .bind(format!("lookup key {index:02}"))
            .bind(format!("lookup-fingerprint-{index:02}"))
            .bind(format!("lookup content {index:02}"))
            .bind(index)
            .bind(index)
            .execute(db.pool())
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO memory_entries
                (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                 schema_version, deleted_at, created_at, updated_at)
             VALUES ('lookup-tombstone', ?, 'decision', 'deleted key', 'lookup-deleted-fingerprint', NULL,
                     'deleted', 0, 0, 1, 70, 70, 70)",
        )
        .bind(USER_A)
        .execute(db.pool())
        .await
        .unwrap();

        let rows = repo
            .reconciliation_entries(
                USER_A,
                &["lookup-fingerprint-00".into(), "lookup-deleted-fingerprint".into()],
                &["lookup-entry-64".into()],
            )
            .await
            .unwrap();
        let ids = rows
            .into_iter()
            .map(|row| row.id)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(
            ids,
            ["lookup-entry-00", "lookup-entry-64", "lookup-tombstone"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        );
    }

    #[tokio::test]
    async fn sqlite_memory_reconciliation_lookup_bounds_conflicts_and_omits_sources() {
        let (repo, _, db) = setup().await;
        for (id, state, deleted_at, updated_at) in [
            ("bounded-active", "active", None, 1_i64),
            ("bounded-deleted-old", "deleted", Some(2_i64), 2),
            ("bounded-deleted-new", "deleted", Some(3_i64), 3),
        ] {
            sqlx::query(
                "INSERT INTO memory_entries
                    (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                     schema_version, deleted_at, created_at, updated_at)
                 VALUES (?, ?, 'decision', ?, 'bounded-fingerprint', ?, ?, 0, 0, 1, ?, ?, ?)",
            )
            .bind(id)
            .bind(USER_A)
            .bind(id)
            .bind((state != "deleted").then_some(id))
            .bind(state)
            .bind(deleted_at)
            .bind(updated_at)
            .bind(updated_at)
            .execute(db.pool())
            .await
            .unwrap();
        }
        for index in 0..80 {
            let id = format!("bounded-conflict-{index:03}");
            sqlx::query(
                "INSERT INTO memory_entries
                    (id, user_id, kind, stable_key, fingerprint, content, state, pinned, user_edited,
                     schema_version, created_at, updated_at)
                 VALUES (?, ?, 'decision', ?, 'bounded-fingerprint', ?, 'conflict', 0, 0, 1, ?, ?)",
            )
            .bind(&id)
            .bind(USER_A)
            .bind(&id)
            .bind(&id)
            .bind(10 + index)
            .bind(10 + index)
            .execute(db.pool())
            .await
            .unwrap();
            sqlx::query(
                "INSERT INTO memory_sources
                    (memory_entry_id, conversation_id, turn_id, message_ids_json, first_observed_at, last_observed_at)
                 VALUES (?, 'conv_a', ?, '[]', 1, 1)",
            )
            .bind(&id)
            .bind(format!("source-turn-{index:03}"))
            .execute(db.pool())
            .await
            .unwrap();
        }

        let rows = repo
            .reconciliation_entries(
                USER_A,
                &["bounded-fingerprint".into()],
                &["bounded-conflict-079".into()],
            )
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            ["bounded-active", "bounded-deleted-new", "bounded-conflict-079"],
        );
        assert!(rows.iter().all(|row| row.sources.is_empty()));
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
            target: expected_entry("foreign-entry", "fp-foreign", 0, "active", Some("foreign")),
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
    async fn sqlite_memory_retrieval_create_replaces_same_key_and_lazily_cleans_expired_rows() {
        let (repo, _, db) = setup().await;
        let retrieval = |id: &str, prompt_hash: &str, created_at: i64, expires_at: i64| MemoryRetrievalRow {
            id: id.into(),
            user_id: USER_A.into(),
            conversation_id: "conv_a".into(),
            prompt_hash: prompt_hash.into(),
            selected_ids_json: "[]".into(),
            estimated_tokens: 0,
            budget_tokens: 2_000,
            retrieval_version: "memory-retrieval-v1".into(),
            created_at,
            expires_at,
        };
        repo.create_retrieval(retrieval("first", "same", 10, 100))
            .await
            .unwrap();
        repo.create_retrieval(retrieval("replacement", "same", 20, 200))
            .await
            .unwrap();
        assert!(repo.get_retrieval(USER_A, "first").await.unwrap().is_none());
        assert!(repo.get_retrieval(USER_A, "replacement").await.unwrap().is_some());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM memory_retrievals")
                .fetch_one(db.pool())
                .await
                .unwrap(),
            1,
        );

        repo.create_retrieval(retrieval("expired", "other", 30, 31))
            .await
            .unwrap();
        assert_eq!(repo.delete_expired_retrievals(31).await.unwrap(), 1);
        assert!(repo.get_retrieval(USER_A, "expired").await.unwrap().is_none());
        assert!(matches!(
            repo.get_retrieval(USER_B, "replacement").await,
            Err(DbError::NotFound(_))
        ));
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
