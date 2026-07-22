use aionui_common::TimestampMs;

use crate::DbError;
use crate::models::{
    ConversationMemoryRow, EffectiveMemoryPolicyRow, MemoryChangeSetRow, MemoryEntryRow, MemoryImportStateRow,
    MemoryJobRow, MemoryRetrievalRow, MemorySettingsRow,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMemorySettingsRow {
    pub user_id: String,
    pub enabled: Option<bool>,
    pub default_capture: Option<bool>,
    pub default_recall: Option<bool>,
    pub consent_version: Option<i64>,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateConversationMemoryPolicyRow {
    pub user_id: String,
    pub conversation_id: String,
    pub capture_enabled: Option<bool>,
    pub recall_enabled: Option<bool>,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnqueueMemoryTurnRow {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub from_turn_id: Option<String>,
    pub through_turn_id: String,
    pub operation_version: String,
    pub input_hash: String,
    pub expected_revision: i64,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimMemoryJobRow {
    pub user_id: String,
    pub worker_id: String,
    pub now: TimestampMs,
    pub lease_duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewMemoryLeaseRow {
    pub user_id: String,
    pub job_id: String,
    pub worker_id: String,
    pub now: TimestampMs,
    pub lease_duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseMemoryLeaseRow {
    pub user_id: String,
    pub job_id: String,
    pub worker_id: String,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionMemoryJobRow {
    pub user_id: String,
    pub job_id: String,
    pub worker_id: String,
    pub state: String,
    pub next_attempt_at: Option<TimestampMs>,
    pub error_code: Option<String>,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMemorySourceRow {
    pub conversation_id: String,
    pub turn_id: String,
    pub message_ids_json: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMemoryEntryRow {
    pub id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub kind: String,
    pub stable_key: String,
    pub fingerprint: String,
    pub content: String,
    pub transition: CommitMemoryEntryTransition,
    pub sources: Vec<CommitMemorySourceRow>,
}

/// Validated lifecycle transition derived by AionCore business logic.
/// The repository applies it atomically but does not classify model output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitMemoryEntryTransition {
    Create,
    Refine {
        target_entry_id: String,
    },
    Supersede {
        target_entry_id: String,
    },
    Conflict {
        target_entry_id: String,
        conflict_group_id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMemoryUpdateRow {
    pub user_id: String,
    pub job_id: String,
    pub conversation_id: String,
    pub expected_revision: i64,
    pub through_turn_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub summary_json: String,
    pub schema_version: i64,
    pub prompt_version: Option<String>,
    pub writer_provider_id: Option<String>,
    pub writer_model_id: Option<String>,
    pub lease_owner: String,
    pub expected_attempt_count: i64,
    pub entries: Vec<CommitMemoryEntryRow>,
    pub change_set_id: String,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitMemoryUpdateResult {
    Committed {
        revision: i64,
        added_ids: Vec<String>,
        refined_ids: Vec<String>,
        superseded_ids: Vec<String>,
        conflict_ids: Vec<String>,
    },
    StaleRevision {
        current_revision: i64,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryEntryQueryRow {
    pub search: Option<String>,
    pub kind: Option<String>,
    pub state: Option<String>,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub source_conversation_id: Option<String>,
    pub created_after: Option<TimestampMs>,
    pub created_before: Option<TimestampMs>,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMemoryEntryRow {
    pub user_id: String,
    pub id: String,
    pub content: Option<String>,
    pub pinned: Option<bool>,
    pub project_id: Option<Option<String>>,
    pub workspace_key: Option<Option<String>>,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidateQueryRow {
    pub user_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub limit: u32,
}

#[async_trait::async_trait]
pub trait IMemoryRepository: Send + Sync {
    async fn get_settings(&self, user_id: &str) -> Result<MemorySettingsRow, DbError>;
    async fn update_settings(&self, command: UpdateMemorySettingsRow) -> Result<MemorySettingsRow, DbError>;
    async fn effective_policy(&self, user_id: &str, conversation_id: &str)
    -> Result<EffectiveMemoryPolicyRow, DbError>;
    async fn update_conversation_policy(
        &self,
        command: UpdateConversationMemoryPolicyRow,
    ) -> Result<EffectiveMemoryPolicyRow, DbError>;
    async fn enqueue_completed_turn(&self, input: EnqueueMemoryTurnRow) -> Result<Option<MemoryJobRow>, DbError>;
    async fn claim_next_job(&self, input: ClaimMemoryJobRow) -> Result<Option<MemoryJobRow>, DbError>;
    async fn renew_lease(&self, input: RenewMemoryLeaseRow) -> Result<bool, DbError>;
    async fn release_lease(&self, input: ReleaseMemoryLeaseRow) -> Result<bool, DbError>;
    async fn transition_running_job(&self, input: TransitionMemoryJobRow) -> Result<Option<MemoryJobRow>, DbError>;
    async fn cancel_jobs(&self, user_id: &str, conversation_id: Option<&str>, now: TimestampMs)
    -> Result<u64, DbError>;
    async fn unblock_jobs(&self, user_id: &str, now: TimestampMs) -> Result<u64, DbError>;
    async fn recover_expired_jobs(&self, now: TimestampMs) -> Result<u64, DbError>;
    async fn get_job(&self, user_id: &str, job_id: &str) -> Result<Option<MemoryJobRow>, DbError>;
    async fn commit_update(&self, input: CommitMemoryUpdateRow) -> Result<CommitMemoryUpdateResult, DbError>;
    async fn get_conversation_memory(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<Option<ConversationMemoryRow>, DbError>;
    async fn list_entries(&self, user_id: &str) -> Result<Vec<MemoryEntryRow>, DbError>;
    async fn query_entries(&self, user_id: &str, query: MemoryEntryQueryRow) -> Result<Vec<MemoryEntryRow>, DbError>;
    async fn get_entry(&self, user_id: &str, entry_id: &str) -> Result<Option<MemoryEntryRow>, DbError>;
    async fn update_entry(&self, input: UpdateMemoryEntryRow) -> Result<MemoryEntryRow, DbError>;
    async fn delete_entry(&self, user_id: &str, entry_id: &str, now: TimestampMs) -> Result<(), DbError>;
    async fn list_change_sets(&self, user_id: &str, limit: u32) -> Result<Vec<MemoryChangeSetRow>, DbError>;
    async fn delete_conversation_memory(
        &self,
        user_id: &str,
        conversation_id: &str,
        now: TimestampMs,
    ) -> Result<(), DbError>;
    async fn clear_memory(&self, user_id: &str, now: TimestampMs) -> Result<(), DbError>;
    async fn retrieval_candidates(&self, query: MemoryCandidateQueryRow) -> Result<Vec<MemoryEntryRow>, DbError>;
    async fn create_retrieval(&self, retrieval: MemoryRetrievalRow) -> Result<MemoryRetrievalRow, DbError>;
    async fn get_retrieval(&self, user_id: &str, retrieval_id: &str) -> Result<Option<MemoryRetrievalRow>, DbError>;
    async fn get_import_state(&self, user_id: &str) -> Result<Option<MemoryImportStateRow>, DbError>;
    async fn upsert_import_state(&self, state: MemoryImportStateRow) -> Result<MemoryImportStateRow, DbError>;
}
