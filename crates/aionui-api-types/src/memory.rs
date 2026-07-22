use aionui_common::{PaginatedResult, TimestampMs};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemorySettings {
    pub enabled: bool,
    pub default_capture: bool,
    pub default_recall: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_version: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consented_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemorySettingsRequest {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub default_capture: Option<bool>,
    #[serde(default)]
    pub default_recall: Option<bool>,
    #[serde(default)]
    pub consent_version: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryStatus {
    pub settings: MemorySettings,
    pub jobs: Vec<MemoryJobHealthSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMemoryPolicy {
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recall_enabled: Option<bool>,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateConversationMemoryPolicyRequest {
    #[serde(default)]
    pub capture_enabled: Option<bool>,
    #[serde(default)]
    pub recall_enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEntryKind {
    Decision,
    Outcome,
    Artifact,
    Issue,
    NextStep,
    WorkConstraint,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryEntryState {
    Active,
    Superseded,
    Conflict,
    Deleted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntryResponse {
    pub id: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
    pub kind: MemoryEntryKind,
    pub stable_key: String,
    pub fingerprint: String,
    pub content: String,
    pub state: MemoryEntryState,
    pub pinned: bool,
    pub user_edited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conflict_group_id: Option<String>,
    pub schema_version: u32,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListMemoryEntriesQuery {
    #[serde(default)]
    pub kind: Option<MemoryEntryKind>,
    #[serde(default)]
    pub state: Option<MemoryEntryState>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

pub type MemoryEntryListResponse = PaginatedResult<MemoryEntryResponse>;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemoryEntryRequest {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub pinned: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteMemoryEntryResponse {
    pub id: String,
    pub state: MemoryEntryState,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ResolveMemoryEntryConflictRequest {
    pub selected_entry_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveMemoryEntryConflictResponse {
    pub entry: MemoryEntryResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryChangeSetResponse {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub through_turn_id: String,
    pub job_id: String,
    pub added_ids: Vec<String>,
    pub refined_ids: Vec<String>,
    pub superseded_ids: Vec<String>,
    pub conflict_ids: Vec<String>,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListMemoryChangeSetsQuery {
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

pub type MemoryChangeSetListResponse = PaginatedResult<MemoryChangeSetResponse>;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateMemoryRetrievalRequest {
    pub conversation_id: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryRetrievalEntrySummary {
    pub id: String,
    pub kind: MemoryEntryKind,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub source_conversation_ids: Vec<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryRetrievalPreview {
    pub retrieval_id: String,
    pub conversation_id: String,
    pub prompt_hash: String,
    pub entries: Vec<MemoryRetrievalEntrySummary>,
    pub estimated_tokens: u32,
    pub expires_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryJobState {
    Pending,
    Running,
    RetryWait,
    Blocked,
    Succeeded,
    Failed,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryJobResponse {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_turn_id: Option<String>,
    pub through_turn_id: String,
    pub operation_version: String,
    pub input_hash: String,
    pub expected_revision: u64,
    pub state: MemoryJobState,
    pub attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_attempt_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<TimestampMs>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_code: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryJobHealthSummary {
    pub state: MemoryJobState,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RetryMemoryJobResponse {
    pub job: MemoryJobResponse,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClaimMemoryJobRequest {
    pub worker_id: String,
    pub lease_duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClaimMemoryJobResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<MemoryJobResponse>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenewMemoryJobLeaseRequest {
    pub worker_id: String,
    pub lease_duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RenewMemoryJobLeaseResponse {
    pub lease_expires_at: TimestampMs,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseMemoryJobLeaseRequest {
    pub worker_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseMemoryJobLeaseResponse {
    pub released: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemorySummary {
    pub goal: String,
    pub current_state: Vec<String>,
    pub decisions: Vec<String>,
    pub artifacts: Vec<String>,
    pub issues: Vec<String>,
    pub next_steps: Vec<String>,
    pub work_constraints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryUpdateConversationInput {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExistingMemoryEntryInput {
    pub id: String,
    pub kind: MemoryEntryKind,
    pub stable_key: String,
    pub content: String,
    pub pinned: bool,
    pub user_edited: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemorySourceMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemorySourceMessageInput {
    pub message_id: String,
    pub role: MemorySourceMessageRole,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemorySourceTurnInput {
    pub turn_id: String,
    pub messages: Vec<MemorySourceMessageInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryUpdateInput {
    pub conversation: MemoryUpdateConversationInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_summary: Option<MemorySummary>,
    pub existing_entries: Vec<ExistingMemoryEntryInput>,
    pub source_turns: Vec<MemorySourceTurnInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum MemoryCandidateMutation {
    Create {
        kind: MemoryEntryKind,
        stable_key: String,
        content: String,
        source_turn_ids: Vec<String>,
    },
    Refine {
        target_entry_id: String,
        kind: MemoryEntryKind,
        stable_key: String,
        content: String,
        source_turn_ids: Vec<String>,
    },
    Supersede {
        target_entry_id: String,
        kind: MemoryEntryKind,
        stable_key: String,
        content: String,
        source_turn_ids: Vec<String>,
    },
    Conflict {
        target_entry_id: String,
        kind: MemoryEntryKind,
        stable_key: String,
        content: String,
        source_turn_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryUpdateOutput {
    pub summary: MemorySummary,
    pub mutations: Vec<MemoryCandidateMutation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryJobEvidenceResponse {
    pub job: MemoryJobResponse,
    pub input: MemoryUpdateInput,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CompleteMemoryJobRequest {
    pub expected_revision: u64,
    pub output: MemoryUpdateOutput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryJobFailureCode {
    NotConfigured,
    ModelUnavailable,
    ProviderAuthFailed,
    QueueFull,
    Timeout,
    RateLimited,
    ProviderRequestFailed,
    InvalidOutput,
    InvalidInput,
    Canceled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NormalizedMemoryJobFailure {
    pub code: MemoryJobFailureCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecordMemoryJobFailureRequest {
    pub failure: NormalizedMemoryJobFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordMemoryJobFailureResponse {
    pub job: MemoryJobResponse,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{CompleteMemoryJobRequest, MemoryEntryKind, MemoryEntryResponse, MemorySettings, SendMessageRequest};

    #[test]
    fn settings_serialize_with_snake_case_fields_and_omit_absent_values() {
        let settings = MemorySettings {
            enabled: true,
            default_capture: true,
            default_recall: false,
            consent_version: None,
            consented_at: None,
            reset_at: None,
        };

        assert_eq!(
            serde_json::to_value(settings).unwrap(),
            json!({
                "enabled": true,
                "default_capture": true,
                "default_recall": false,
            }),
        );
    }

    #[test]
    fn entry_response_rejects_unknown_kind() {
        let value = json!({
            "id": "mem_1",
            "user_id": "user_1",
            "kind": "unsupported",
            "stable_key": "decision:one",
            "fingerprint": "fp_1",
            "content": "Use the established plan.",
            "state": "active",
            "pinned": false,
            "user_edited": false,
            "schema_version": 1,
            "created_at": 1,
            "updated_at": 1,
        });

        assert!(serde_json::from_value::<MemoryEntryResponse>(value).is_err());
    }

    #[test]
    fn send_fields_are_additive_and_optional() {
        let request: SendMessageRequest = serde_json::from_value(json!({
            "content": "continue",
            "memory_retrieval_id": "ret_1",
            "excluded_memory_ids": ["mem_2"]
        }))
        .unwrap();
        assert_eq!(request.memory_retrieval_id.as_deref(), Some("ret_1"));
        assert_eq!(request.excluded_memory_ids, vec!["mem_2"]);
    }

    #[test]
    fn worker_submission_rejects_provider_selection_fields() {
        let value = json!({ "expected_revision": 1, "output": {}, "provider_id": "p" });
        assert!(serde_json::from_value::<CompleteMemoryJobRequest>(value).is_err());
    }

    #[test]
    fn memory_entry_kind_uses_snake_case() {
        assert_eq!(
            serde_json::to_value(MemoryEntryKind::NextStep).unwrap(),
            json!("next_step"),
        );
    }
}
