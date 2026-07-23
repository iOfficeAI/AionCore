use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::DbError;
use crate::models::{
    ConversationMemoryPolicyRow, ConversationMemoryRow, ConversationRow, EffectiveMemoryPolicyRow, MemoryChangeSetRow,
    MemoryEntryRow, MemoryImportStateRow, MemoryJobHealthRow, MemoryJobRow, MemoryJobTurnRow, MemoryRetrievalRow,
    MemorySettingsRow, MessageRow,
};

/// Maximum accepted messages in one bounded Memory evidence batch.
pub const MEMORY_EVIDENCE_MAX_MESSAGES: usize = 128;
/// Maximum accepted UTF-8 content bytes in one bounded Memory evidence batch.
pub const MEMORY_EVIDENCE_MAX_BYTES: usize = 64 * 1024;
pub const MEMORY_SUMMARY_SELECTION_PREFIX: &str = "memory-summary:";

pub fn memory_summary_selection_id(conversation_id: &str) -> String {
    format!("{MEMORY_SUMMARY_SELECTION_PREFIX}{conversation_id}")
}

pub fn memory_summary_conversation_id(selection_id: &str) -> Option<&str> {
    selection_id
        .strip_prefix(MEMORY_SUMMARY_SELECTION_PREFIX)
        .filter(|value| !value.is_empty())
}

/// Canonical message families that may contribute text to Memory evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryEvidenceMessageKind {
    Text,
    Artifact,
    ToolResultSummary,
}

impl MemoryEvidenceMessageKind {
    /// Classifies a persisted message type using the canonical Memory allowlist.
    pub fn from_db_type(message_type: &str) -> Option<Self> {
        match message_type {
            "text" => Some(Self::Text),
            "artifact" => Some(Self::Artifact),
            "tool_result_summary" => Some(Self::ToolResultSummary),
            _ => None,
        }
    }

    /// Returns the JSON string field that contains accepted evidence text.
    pub fn content_field(self) -> &'static str {
        match self {
            Self::Text | Self::Artifact => "content",
            Self::ToolResultSummary => "summary",
        }
    }
}

/// Extracts canonical evidence text when row metadata and JSON content are eligible.
pub fn memory_evidence_content(message: &MessageRow) -> Option<String> {
    if message.hidden
        || message.status.as_deref() != Some("finish")
        || !matches!(message.position.as_deref(), Some("left" | "right"))
    {
        return None;
    }
    let kind = MemoryEvidenceMessageKind::from_db_type(&message.r#type)?;
    let value: serde_json::Value = serde_json::from_str(&message.content).ok()?;
    value
        .get(kind.content_field())
        .and_then(serde_json::Value::as_str)
        .filter(|content| !content.trim().is_empty())
        .map(str::to_owned)
}

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
    pub through_turn_id: String,
    pub operation_version: String,
    pub expected_global_epoch: i64,
    pub expected_conversation_epoch: i64,
    pub required_consent_version: i64,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundedMemoryTurnMessagesRow {
    pub messages: Vec<MessageRow>,
    pub message_count: i64,
    pub content_bytes: i64,
    pub snapshot_hash: String,
    pub snapshot_matches: bool,
    pub limit_exceeded: bool,
    pub has_user_work: bool,
    pub has_assistant_outcome: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryTurnSnapshotExpectationRow {
    pub turn_id: String,
    pub snapshot_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeMemoryJobSnapshotRow {
    pub user_id: String,
    pub job_id: String,
    pub lease_token: String,
    pub expected_global_epoch: i64,
    pub expected_conversation_epoch: i64,
    pub turn_snapshots: Vec<MemoryTurnSnapshotExpectationRow>,
    pub reconciliation_snapshot: Option<Vec<MemoryReconciliationSnapshotRow>>,
    pub require_existing_reconciliation_snapshot: bool,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryReconciliationSnapshotRow {
    pub id: String,
    pub revision: i64,
    pub state: String,
    pub fingerprint: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub pinned: bool,
    pub user_edited: bool,
    pub content_hash: String,
}

/// Produces the content-free digest persisted in a job's reconciliation snapshot.
pub fn memory_entry_content_hash(content: Option<&str>) -> String {
    let material = serde_json::to_vec(&("memory-entry-content-v1", content))
        .expect("serializing a static tag and optional string cannot fail");
    Sha256::digest(material)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Derives the canonical identity fingerprint for an entry owner, scope, kind, and stable key.
pub fn derive_memory_fingerprint(
    user_id: &str,
    project_id: Option<&str>,
    workspace_key: Option<&str>,
    kind: &str,
    stable_key: &str,
) -> String {
    let material = serde_json::to_vec(&(
        "memory-fingerprint-v1",
        user_id,
        project_id,
        workspace_key,
        kind,
        stable_key,
    ))
    .expect("serializing Memory fingerprint fields cannot fail");
    Sha256::digest(material)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinalizeMemoryJobSnapshotResult {
    Finalized(Box<MemoryJobRow>),
    SnapshotChanged,
    ReconciliationChanged,
    FenceLost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimMemoryJobRow {
    pub user_id: String,
    pub worker_id: String,
    pub lease_token: String,
    pub now: TimestampMs,
    pub lease_duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenewMemoryLeaseRow {
    pub user_id: String,
    pub job_id: String,
    pub worker_id: String,
    pub lease_token: String,
    pub now: TimestampMs,
    pub lease_duration_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseMemoryLeaseRow {
    pub user_id: String,
    pub job_id: String,
    pub worker_id: String,
    pub lease_token: String,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionMemoryJobRow {
    pub user_id: String,
    pub job_id: String,
    pub worker_id: String,
    pub lease_token: String,
    pub state: String,
    pub next_attempt_at: Option<TimestampMs>,
    pub error_code: Option<String>,
    pub increment_attempt: bool,
    pub increment_invalid_output: bool,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedMemoryEntryRow {
    pub id: String,
    pub revision: i64,
    pub state: String,
    pub fingerprint: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub content: Option<String>,
}

/// Validated lifecycle transition derived by AionCore business logic.
/// The repository applies it atomically but does not classify model output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitMemoryEntryTransition {
    Create,
    Refine {
        target: ExpectedMemoryEntryRow,
    },
    Supersede {
        target: ExpectedMemoryEntryRow,
    },
    Conflict {
        target: ExpectedMemoryEntryRow,
        conflict_group_id: String,
    },
    AttachSource {
        target: ExpectedMemoryEntryRow,
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
    pub lease_token: String,
    pub expected_attempt_count: i64,
    pub entries: Vec<CommitMemoryEntryRow>,
    pub change_set_id: String,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitMemoryJobRow {
    pub user_id: String,
    pub job_id: String,
    pub lease_token: String,
    pub prefix_count: i64,
    pub pending_job_id: String,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMemoryLifecycleRow {
    pub user_id: String,
    pub enabled: Option<bool>,
    pub default_capture: Option<bool>,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateConversationMemoryLifecycleRow {
    pub user_id: String,
    pub conversation_id: String,
    pub capture_enabled: bool,
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
    StaleReconciliation,
    SnapshotChanged,
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
    pub offset: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemoryChangeSetQueryRow {
    pub conversation_id: Option<String>,
    pub limit: u32,
    pub offset: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateMemoryEntryRow {
    pub user_id: String,
    pub id: String,
    pub expected_revision: i64,
    pub expected_state: String,
    pub content: Option<String>,
    pub pinned: Option<bool>,
    pub project_id: Option<Option<String>>,
    pub workspace_key: Option<Option<String>>,
    pub new_fingerprint: Option<String>,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveMemoryConflictActionRow {
    Select { selected_entry_id: String },
    Merge { content: String },
    KeepSeparate { tombstone_id_prefix: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveMemoryConflictRow {
    pub user_id: String,
    pub entry_id: String,
    pub action: ResolveMemoryConflictActionRow,
    pub now: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCandidateQueryRow {
    pub user_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub limit: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryRetrievalItemRow {
    Entry(MemoryEntryRow),
    ConversationSummary(ConversationMemoryRow),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMemoryRetrievalSnapshotRow {
    pub retrieval: MemoryRetrievalRow,
    pub expected_policy: EffectiveMemoryPolicyRow,
    pub expected_conversation_updated_at: TimestampMs,
    pub items: Vec<MemoryRetrievalItemRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsumeMemoryRetrievalSnapshotRow {
    pub user_id: String,
    pub conversation_id: String,
    pub retrieval_id: String,
    pub prompt_hash: String,
    pub retrieval_version: String,
    pub expected_budget_tokens: i64,
    pub now: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct MemoryRetrievalSnapshotRow {
    pub retrieval: MemoryRetrievalRow,
    pub policy: EffectiveMemoryPolicyRow,
    pub conversation: ConversationRow,
    pub items: Vec<MemoryRetrievalItemRow>,
}

#[async_trait::async_trait]
pub trait IMemoryRepository: Send + Sync {
    async fn get_settings(&self, user_id: &str) -> Result<MemorySettingsRow, DbError>;
    async fn update_settings(&self, command: UpdateMemorySettingsRow) -> Result<MemorySettingsRow, DbError>;
    async fn effective_policy(&self, user_id: &str, conversation_id: &str)
    -> Result<EffectiveMemoryPolicyRow, DbError>;
    async fn get_conversation_policy(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationMemoryPolicyRow, DbError>;
    async fn update_conversation_policy(
        &self,
        command: UpdateConversationMemoryPolicyRow,
    ) -> Result<EffectiveMemoryPolicyRow, DbError>;
    async fn enqueue_completed_turn(&self, input: EnqueueMemoryTurnRow) -> Result<Option<MemoryJobRow>, DbError>;
    async fn retry_failed_job(
        &self,
        user_id: &str,
        job_id: &str,
        now: TimestampMs,
    ) -> Result<Option<MemoryJobRow>, DbError>;
    async fn claim_next_job(&self, input: ClaimMemoryJobRow) -> Result<Option<MemoryJobRow>, DbError>;
    async fn list_job_turns(&self, user_id: &str, job_id: &str, limit: u32) -> Result<Vec<MemoryJobTurnRow>, DbError>;
    async fn load_job_turn_messages_bounded(
        &self,
        user_id: &str,
        job_id: &str,
        turn_id: &str,
        max_messages: u32,
        max_bytes: u64,
    ) -> Result<BoundedMemoryTurnMessagesRow, DbError>;
    async fn finalize_claimed_job_snapshot(
        &self,
        input: FinalizeMemoryJobSnapshotRow,
    ) -> Result<FinalizeMemoryJobSnapshotResult, DbError>;
    async fn split_claimed_job(&self, input: SplitMemoryJobRow) -> Result<bool, DbError>;
    async fn update_memory_lifecycle(&self, input: UpdateMemoryLifecycleRow) -> Result<(), DbError>;
    async fn update_conversation_memory_lifecycle(
        &self,
        input: UpdateConversationMemoryLifecycleRow,
    ) -> Result<(), DbError>;
    async fn validate_lease(
        &self,
        user_id: &str,
        job_id: &str,
        lease_token: &str,
        now: TimestampMs,
    ) -> Result<bool, DbError>;
    async fn block_jobs(&self, user_id: &str, now: TimestampMs) -> Result<u64, DbError>;
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
    async fn count_entries(&self, user_id: &str, query: MemoryEntryQueryRow) -> Result<u64, DbError>;
    async fn get_entry(&self, user_id: &str, entry_id: &str) -> Result<Option<MemoryEntryRow>, DbError>;
    async fn update_entry(&self, input: UpdateMemoryEntryRow) -> Result<MemoryEntryRow, DbError>;
    async fn resolve_conflict(&self, input: ResolveMemoryConflictRow) -> Result<Vec<MemoryEntryRow>, DbError>;
    async fn delete_entry(&self, user_id: &str, entry_id: &str, now: TimestampMs) -> Result<(), DbError>;
    async fn list_change_sets(&self, user_id: &str, limit: u32) -> Result<Vec<MemoryChangeSetRow>, DbError>;
    async fn query_change_sets(
        &self,
        user_id: &str,
        query: MemoryChangeSetQueryRow,
    ) -> Result<(Vec<MemoryChangeSetRow>, u64), DbError>;
    async fn memory_job_health(&self, user_id: &str)
    -> Result<(Option<TimestampMs>, Vec<MemoryJobHealthRow>), DbError>;
    async fn delete_conversation_memory(
        &self,
        user_id: &str,
        conversation_id: &str,
        now: TimestampMs,
    ) -> Result<(), DbError>;
    async fn clear_memory(&self, user_id: &str, now: TimestampMs) -> Result<(), DbError>;
    async fn retrieval_candidates(&self, query: MemoryCandidateQueryRow) -> Result<Vec<MemoryEntryRow>, DbError>;
    async fn retrieval_summaries(&self, query: MemoryCandidateQueryRow) -> Result<Vec<ConversationMemoryRow>, DbError>;
    async fn reconciliation_entries(
        &self,
        user_id: &str,
        fingerprints: &[String],
        target_ids: &[String],
    ) -> Result<Vec<MemoryEntryRow>, DbError>;
    async fn create_retrieval_snapshot(
        &self,
        input: CreateMemoryRetrievalSnapshotRow,
    ) -> Result<MemoryRetrievalRow, DbError>;
    async fn consume_retrieval_snapshot(
        &self,
        input: ConsumeMemoryRetrievalSnapshotRow,
    ) -> Result<MemoryRetrievalSnapshotRow, DbError>;
    async fn get_retrieval(&self, user_id: &str, retrieval_id: &str) -> Result<Option<MemoryRetrievalRow>, DbError>;
    async fn get_import_state(&self, user_id: &str) -> Result<Option<MemoryImportStateRow>, DbError>;
    async fn upsert_import_state(&self, state: MemoryImportStateRow) -> Result<MemoryImportStateRow, DbError>;
}

#[cfg(test)]
mod tests {
    use super::MemoryEvidenceMessageKind;

    #[test]
    fn memory_evidence_message_kind_requires_exact_canonical_type_names() {
        assert_eq!(
            MemoryEvidenceMessageKind::from_db_type("text"),
            Some(MemoryEvidenceMessageKind::Text),
        );
        assert_eq!(MemoryEvidenceMessageKind::from_db_type("\ttext\t"), None);
        assert_eq!(MemoryEvidenceMessageKind::from_db_type("\u{2003}text\u{2003}"), None);
        assert_eq!(MemoryEvidenceMessageKind::from_db_type("Text"), None);
    }
}
