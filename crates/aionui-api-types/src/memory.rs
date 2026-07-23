use aionui_common::{PaginatedResult, TimestampMs};
use serde::{Deserialize, Serialize};

use crate::system::{AppOperationsModelHealth, AppOperationsModelReasonCode};

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
    pub app_operations_readiness: MemoryAppOperationsReadiness,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_successful_update_at: Option<TimestampMs>,
    pub jobs: Vec<MemoryJobHealthSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryAppOperationsReadiness {
    pub health: AppOperationsModelHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<AppOperationsModelReasonCode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<TimestampMs>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntryResponse {
    pub id: String,
    pub user_id: String,
    pub project_id: Option<String>,
    pub workspace_key: Option<String>,
    pub kind: MemoryEntryKind,
    pub stable_key: Option<String>,
    pub fingerprint: String,
    pub content: Option<String>,
    pub state: MemoryEntryState,
    pub pinned: bool,
    pub user_edited: bool,
    pub sources: Vec<MemoryEntrySourceResponse>,
    pub supersedes_id: Option<String>,
    pub conflict_group_id: Option<String>,
    pub schema_version: u32,
    pub deleted_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Deserialize)]
struct MemoryEntryResponseWire {
    id: String,
    user_id: String,
    #[serde(default)]
    project_id: Option<String>,
    #[serde(default)]
    workspace_key: Option<String>,
    kind: MemoryEntryKind,
    #[serde(default)]
    stable_key: Option<String>,
    fingerprint: String,
    #[serde(default)]
    content: Option<String>,
    state: MemoryEntryState,
    pinned: bool,
    user_edited: bool,
    #[serde(default)]
    sources: Vec<MemoryEntrySourceResponse>,
    #[serde(default)]
    supersedes_id: Option<String>,
    #[serde(default)]
    conflict_group_id: Option<String>,
    schema_version: u32,
    #[serde(default)]
    deleted_at: Option<TimestampMs>,
    created_at: TimestampMs,
    updated_at: TimestampMs,
}

impl<'de> Deserialize<'de> for MemoryEntryResponse {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut wire = MemoryEntryResponseWire::deserialize(deserializer)?;
        match &wire.state {
            MemoryEntryState::Deleted => {
                if wire.deleted_at.is_none() {
                    return Err(serde::de::Error::custom("deleted Memory entry requires deleted_at"));
                }
                wire.stable_key = None;
                wire.content = None;
                wire.sources.clear();
            }
            _ => {
                if wire.stable_key.is_none() || wire.content.is_none() {
                    return Err(serde::de::Error::custom(
                        "non-deleted Memory entry requires stable_key and content",
                    ));
                }
                if wire.deleted_at.is_some() {
                    return Err(serde::de::Error::custom(
                        "non-deleted Memory entry cannot include deleted_at",
                    ));
                }
            }
        }
        Ok(Self {
            id: wire.id,
            user_id: wire.user_id,
            project_id: wire.project_id,
            workspace_key: wire.workspace_key,
            kind: wire.kind,
            stable_key: wire.stable_key,
            fingerprint: wire.fingerprint,
            content: wire.content,
            state: wire.state,
            pinned: wire.pinned,
            user_edited: wire.user_edited,
            sources: wire.sources,
            supersedes_id: wire.supersedes_id,
            conflict_group_id: wire.conflict_group_id,
            schema_version: wire.schema_version,
            deleted_at: wire.deleted_at,
            created_at: wire.created_at,
            updated_at: wire.updated_at,
        })
    }
}

impl Serialize for MemoryEntryResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Wire<'a> {
            id: &'a str,
            user_id: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            project_id: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            workspace_key: Option<&'a str>,
            kind: &'a MemoryEntryKind,
            #[serde(skip_serializing_if = "Option::is_none")]
            stable_key: Option<&'a str>,
            fingerprint: &'a str,
            #[serde(skip_serializing_if = "Option::is_none")]
            content: Option<&'a str>,
            state: &'a MemoryEntryState,
            pinned: bool,
            user_edited: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            sources: Option<&'a [MemoryEntrySourceResponse]>,
            #[serde(skip_serializing_if = "Option::is_none")]
            supersedes_id: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            conflict_group_id: Option<&'a str>,
            schema_version: u32,
            #[serde(skip_serializing_if = "Option::is_none")]
            deleted_at: Option<TimestampMs>,
            created_at: TimestampMs,
            updated_at: TimestampMs,
        }

        let (stable_key, content, sources, deleted_at) = match &self.state {
            MemoryEntryState::Deleted => {
                let deleted_at = self
                    .deleted_at
                    .ok_or_else(|| serde::ser::Error::custom("deleted Memory entry requires deleted_at"))?;
                (None, None, None, Some(deleted_at))
            }
            _ => {
                let stable_key = self
                    .stable_key
                    .as_deref()
                    .ok_or_else(|| serde::ser::Error::custom("non-deleted Memory entry requires stable_key"))?;
                let content = self
                    .content
                    .as_deref()
                    .ok_or_else(|| serde::ser::Error::custom("non-deleted Memory entry requires content"))?;
                (Some(stable_key), Some(content), Some(self.sources.as_slice()), None)
            }
        };

        Wire {
            id: &self.id,
            user_id: &self.user_id,
            project_id: self.project_id.as_deref(),
            workspace_key: self.workspace_key.as_deref(),
            kind: &self.kind,
            stable_key,
            fingerprint: &self.fingerprint,
            content,
            state: &self.state,
            pinned: self.pinned,
            user_edited: self.user_edited,
            sources,
            supersedes_id: self.supersedes_id.as_deref(),
            conflict_group_id: self.conflict_group_id.as_deref(),
            schema_version: self.schema_version,
            deleted_at,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryEntrySourceResponse {
    pub memory_entry_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub message_ids: Vec<String>,
    pub first_observed_at: TimestampMs,
    pub last_observed_at: TimestampMs,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ListMemoryEntriesQuery {
    #[serde(default)]
    pub search: Option<String>,
    #[serde(default)]
    pub kind: Option<MemoryEntryKind>,
    #[serde(default)]
    pub state: Option<MemoryEntryState>,
    #[serde(default)]
    pub project_id: Option<String>,
    #[serde(default)]
    pub workspace_key: Option<String>,
    #[serde(default)]
    pub source_conversation_id: Option<String>,
    #[serde(default)]
    pub created_after: Option<TimestampMs>,
    #[serde(default)]
    pub created_before: Option<TimestampMs>,
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
    /// `Some(Some(value))` sets scope, `Some(None)` clears it, and `None` keeps it unchanged.
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub project_id: Option<Option<String>>,
    /// `Some(Some(value))` sets scope, `Some(None)` clears it, and `None` keeps it unchanged.
    #[serde(default, deserialize_with = "deserialize_optional_nullable")]
    pub workspace_key: Option<Option<String>>,
}

/// Deserialize a nullable patch field while retaining whether it was supplied.
fn deserialize_optional_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Some(Option::deserialize(deserializer)?))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteMemoryEntryResponse {
    pub id: String,
    pub state: MemoryEntryState,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolveMemoryEntryConflictRequest {
    Select { selected_entry_id: String },
    Merge { content: String },
    KeepSeparate,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolveMemoryEntryConflictResponse {
    pub entries: Vec<MemoryEntryResponse>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RenewMemoryJobLeaseRequest {
    pub worker_id: String,
    pub lease_token: String,
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
    pub lease_token: String,
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
    pub lease_token: String,
    pub expected_revision: u64,
    pub output: MemoryUpdateOutput,
    pub task_result_provenance: MemoryTaskResultProvenance,
}

/// Provenance copied from a completed App Operations task result, never caller model selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MemoryTaskResultProvenance {
    pub provider_id: String,
    pub model_id: String,
    pub prompt_version: String,
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
    pub lease_token: String,
    pub failure: NormalizedMemoryJobFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecordMemoryJobFailureResponse {
    pub job: MemoryJobResponse,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        CompleteMemoryJobRequest, ListMemoryEntriesQuery, MemoryEntryKind, MemoryEntryResponse, MemorySettings,
        MemoryStatus, ResolveMemoryEntryConflictRequest, SendMessageRequest, UpdateMemoryEntryRequest,
    };

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
        let valid = json!({
            "id": "mem_1",
            "user_id": "user_1",
            "kind": "decision",
            "stable_key": "decision:one",
            "fingerprint": "fp_1",
            "content": "Use the established plan.",
            "state": "active",
            "pinned": false,
            "user_edited": false,
            "sources": [],
            "schema_version": 1,
            "created_at": 1,
            "updated_at": 1,
        });

        assert!(serde_json::from_value::<MemoryEntryResponse>(valid.clone()).is_ok());
        assert_eq!(
            serde_json::to_value(serde_json::from_value::<MemoryEntryResponse>(valid.clone()).unwrap()).unwrap(),
            valid,
        );

        let mut unsupported_kind = valid;
        unsupported_kind["kind"] = json!("unsupported");
        assert!(serde_json::from_value::<MemoryEntryResponse>(unsupported_kind).is_err());
    }

    #[test]
    fn deleted_entry_response_omits_scrubbed_fields_and_retains_tombstone_identity() {
        let tombstone = json!({
            "id": "mem_deleted",
            "user_id": "user_1",
            "project_id": "project_1",
            "workspace_key": "workspace_1",
            "kind": "decision",
            "fingerprint": "fp_deleted",
            "state": "deleted",
            "pinned": false,
            "user_edited": false,
            "schema_version": 1,
            "deleted_at": 42,
            "created_at": 1,
            "updated_at": 42,
        });

        let response: MemoryEntryResponse = serde_json::from_value(tombstone.clone()).unwrap();

        assert_eq!(serde_json::to_value(response).unwrap(), tombstone);
    }

    #[test]
    fn deleted_entry_response_normalizes_hostile_content_and_requires_deletion_time() {
        let dirty = json!({
            "id": "mem_deleted",
            "user_id": "user_1",
            "kind": "decision",
            "stable_key": "secret identity",
            "fingerprint": "fp_deleted",
            "content": "secret content",
            "state": "deleted",
            "pinned": false,
            "user_edited": false,
            "sources": [{
                "memory_entry_id": "mem_deleted",
                "conversation_id": "conv_1",
                "turn_id": "turn_1",
                "message_ids": ["msg_1"],
                "first_observed_at": 1,
                "last_observed_at": 2
            }],
            "schema_version": 1,
            "deleted_at": 42,
            "created_at": 1,
            "updated_at": 42,
        });

        let response: MemoryEntryResponse = serde_json::from_value(dirty).unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({
                "id": "mem_deleted",
                "user_id": "user_1",
                "kind": "decision",
                "fingerprint": "fp_deleted",
                "state": "deleted",
                "pinned": false,
                "user_edited": false,
                "schema_version": 1,
                "deleted_at": 42,
                "created_at": 1,
                "updated_at": 42,
            }),
        );

        let missing_deleted_at = json!({
            "id": "mem_deleted",
            "user_id": "user_1",
            "kind": "decision",
            "fingerprint": "fp_deleted",
            "state": "deleted",
            "pinned": false,
            "user_edited": false,
            "schema_version": 1,
            "created_at": 1,
            "updated_at": 42,
        });
        assert!(serde_json::from_value::<MemoryEntryResponse>(missing_deleted_at).is_err());
    }

    #[test]
    fn active_entry_response_requires_content_identity_and_never_emits_deleted_at() {
        let valid = json!({
            "id": "mem_1",
            "user_id": "user_1",
            "kind": "decision",
            "stable_key": "decision:one",
            "fingerprint": "fp_1",
            "content": "Use the established plan.",
            "state": "active",
            "pinned": false,
            "user_edited": false,
            "sources": [],
            "schema_version": 1,
            "created_at": 1,
            "updated_at": 1,
        });

        for missing in ["stable_key", "content"] {
            let mut malformed = valid.clone();
            malformed.as_object_mut().unwrap().remove(missing);
            assert!(
                serde_json::from_value::<MemoryEntryResponse>(malformed).is_err(),
                "active response accepted missing {missing}",
            );
        }

        let mut dirty_active: MemoryEntryResponse = serde_json::from_value(valid.clone()).unwrap();
        dirty_active.deleted_at = Some(99);
        assert_eq!(serde_json::to_value(&dirty_active).unwrap(), valid);
        dirty_active.content = None;
        assert!(serde_json::to_value(dirty_active).is_err());
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
    fn worker_submission_accepts_result_provenance_but_rejects_provider_selection_fields() {
        let valid = json!({
            "lease_token": "opaque-token",
            "expected_revision": 1,
            "output": {
                "summary": {
                    "goal": "Continue the plan.",
                    "current_state": [],
                    "decisions": [],
                    "artifacts": [],
                    "issues": [],
                    "next_steps": [],
                    "work_constraints": []
                },
                "mutations": []
            },
            "task_result_provenance": {
                "provider_id": "provider_1",
                "model_id": "model_1",
                "prompt_version": "memory-v1"
            }
        });
        assert!(serde_json::from_value::<CompleteMemoryJobRequest>(valid.clone()).is_ok());

        let mut with_provider = valid.clone();
        with_provider["provider_id"] = json!("provider_2");
        assert!(serde_json::from_value::<CompleteMemoryJobRequest>(with_provider).is_err());

        let mut with_model = valid;
        with_model["model_id"] = json!("model_2");
        assert!(serde_json::from_value::<CompleteMemoryJobRequest>(with_model).is_err());
    }

    #[test]
    fn entries_support_source_provenance_filters_and_scope_edits() {
        let entry = json!({
            "id": "mem_1",
            "user_id": "user_1",
            "kind": "decision",
            "stable_key": "decision:one",
            "fingerprint": "fp_1",
            "content": "Use the established plan.",
            "state": "active",
            "pinned": false,
            "user_edited": false,
            "schema_version": 1,
            "created_at": 1,
            "updated_at": 1,
            "sources": [{
                "memory_entry_id": "mem_1",
                "conversation_id": "conv_1",
                "turn_id": "turn_1",
                "message_ids": ["msg_1"],
                "first_observed_at": 1,
                "last_observed_at": 2
            }]
        });
        let entry: MemoryEntryResponse = serde_json::from_value(entry).unwrap();
        assert_eq!(serde_json::to_value(entry).unwrap()["sources"][0]["turn_id"], "turn_1");

        let query: ListMemoryEntriesQuery = serde_json::from_value(json!({
            "search": "established plan",
            "source_conversation_id": "conv_1",
            "created_after": 1,
            "created_before": 2
        }))
        .unwrap();
        assert_eq!(query.search.as_deref(), Some("established plan"));
        assert_eq!(query.source_conversation_id.as_deref(), Some("conv_1"));

        let request: UpdateMemoryEntryRequest = serde_json::from_value(json!({
            "project_id": null,
            "workspace_key": "workspace_1"
        }))
        .unwrap();
        assert_eq!(request.project_id, Some(None));
        assert_eq!(request.workspace_key, Some(Some("workspace_1".into())));
    }

    #[test]
    fn conflict_resolution_supports_select_merge_and_keep_separate_actions() {
        for action in [
            json!({ "action": "select", "selected_entry_id": "mem_1" }),
            json!({ "action": "merge", "content": "Merged protected content." }),
            json!({ "action": "keep_separate" }),
        ] {
            assert!(serde_json::from_value::<ResolveMemoryEntryConflictRequest>(action).is_ok());
        }
    }

    #[test]
    fn memory_status_exposes_content_free_readiness_and_last_success() {
        let status: MemoryStatus = serde_json::from_value(json!({
            "settings": {
                "enabled": true,
                "default_capture": true,
                "default_recall": true
            },
            "jobs": [],
            "app_operations_readiness": {
                "health": "ready",
                "checked_at": 5
            },
            "last_successful_update_at": 4
        }))
        .unwrap();
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["app_operations_readiness"]["health"], "ready");
        assert_eq!(value["last_successful_update_at"], 4);
    }

    #[test]
    fn content_only_send_request_has_empty_memory_fields() {
        let request: SendMessageRequest = serde_json::from_value(json!({ "content": "continue" })).unwrap();
        assert_eq!(request.memory_retrieval_id, None);
        assert!(request.excluded_memory_ids.is_empty());
    }

    #[test]
    fn memory_entry_kind_uses_snake_case() {
        assert_eq!(
            serde_json::to_value(MemoryEntryKind::NextStep).unwrap(),
            json!("next_step"),
        );
    }
}
