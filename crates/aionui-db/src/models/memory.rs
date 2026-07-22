use aionui_common::TimestampMs;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemorySettingsRow {
    pub user_id: String,
    pub enabled: bool,
    pub default_capture: bool,
    pub default_recall: bool,
    pub consent_version: Option<i64>,
    pub consented_at: Option<TimestampMs>,
    pub reset_at: Option<TimestampMs>,
    pub lifecycle_epoch: i64,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveMemoryPolicyRow {
    pub user_id: String,
    pub conversation_id: String,
    pub enabled: bool,
    pub capture_enabled: bool,
    pub recall_enabled: bool,
    pub capture_override: Option<bool>,
    pub recall_override: Option<bool>,
    pub consent_version: Option<i64>,
    pub consented_at: Option<TimestampMs>,
    pub reset_at: Option<TimestampMs>,
    pub global_epoch: i64,
    pub conversation_epoch: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ConversationMemoryRow {
    pub user_id: String,
    pub conversation_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub summary_json: String,
    pub through_turn_id: String,
    pub revision: i64,
    pub source: String,
    pub schema_version: i64,
    pub prompt_version: Option<String>,
    pub writer_provider_id: Option<String>,
    pub writer_model_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemorySourceRow {
    pub memory_entry_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub message_ids_json: String,
    pub first_observed_at: TimestampMs,
    pub last_observed_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryEntryDbRow {
    pub id: String,
    pub user_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub kind: String,
    pub stable_key: String,
    pub fingerprint: String,
    pub content: Option<String>,
    pub state: String,
    pub pinned: bool,
    pub user_edited: bool,
    pub revision: i64,
    pub supersedes_id: Option<String>,
    pub conflict_group_id: Option<String>,
    pub schema_version: i64,
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntryRow {
    pub id: String,
    pub user_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub kind: String,
    pub stable_key: String,
    pub fingerprint: String,
    pub content: Option<String>,
    pub state: String,
    pub pinned: bool,
    pub user_edited: bool,
    pub revision: i64,
    pub supersedes_id: Option<String>,
    pub conflict_group_id: Option<String>,
    pub schema_version: i64,
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub sources: Vec<MemorySourceRow>,
}

impl MemoryEntryDbRow {
    pub(crate) fn with_sources(self, sources: Vec<MemorySourceRow>) -> MemoryEntryRow {
        MemoryEntryRow {
            id: self.id,
            user_id: self.user_id,
            project_id: self.project_id,
            workspace_key: self.workspace_key,
            kind: self.kind,
            stable_key: self.stable_key,
            fingerprint: self.fingerprint,
            content: self.content,
            state: self.state,
            pinned: self.pinned,
            user_edited: self.user_edited,
            revision: self.revision,
            supersedes_id: self.supersedes_id,
            conflict_group_id: self.conflict_group_id,
            schema_version: self.schema_version,
            deleted_at: self.deleted_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
            sources,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryJobRow {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub from_turn_id: Option<String>,
    pub through_turn_id: String,
    pub operation_version: String,
    pub global_epoch: i64,
    pub conversation_epoch: i64,
    pub turn_count: i64,
    pub queue_digest: String,
    pub input_hash: String,
    pub expected_revision: i64,
    pub state: String,
    pub attempt_count: i64,
    pub next_attempt_at: Option<TimestampMs>,
    pub lease_owner: Option<String>,
    pub lease_token: Option<String>,
    pub lease_expires_at: Option<TimestampMs>,
    pub invalid_output_count: i64,
    pub last_error_code: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryJobTurnRow {
    pub job_id: String,
    pub position: i64,
    pub turn_id: String,
    pub turn_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryChangeSetRow {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub through_turn_id: String,
    pub job_id: String,
    pub added_ids_json: String,
    pub refined_ids_json: String,
    pub superseded_ids_json: String,
    pub conflict_ids_json: String,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryRetrievalRow {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub prompt_hash: String,
    pub selected_ids_json: String,
    pub estimated_tokens: i64,
    pub budget_tokens: i64,
    pub retrieval_version: String,
    pub created_at: TimestampMs,
    pub expires_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MemoryImportStateRow {
    pub user_id: String,
    pub cursor: Option<String>,
    pub completed: bool,
    pub started_at: Option<TimestampMs>,
    pub completed_at: Option<TimestampMs>,
    pub updated_at: TimestampMs,
}
