use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use aionui_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
use aionui_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};
use aionui_ai_agent::protocol::events::{AgentStreamEvent, ErrorEventData, FinishEventData, TextEventData};
use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{
    AcpError, AgentAvailabilityFeedbackPort, AgentError, AgentSendError, AgentSessionKind, IWorkerTaskManager,
};

use crate::response_middleware::{CronCommandResult, CronCreateParams, CronUpdateParams, ICronService};
use aionui_api_types::{
    AcpConfigOptionDto, AgentErrorCode, AgentModeResponse, ConfigOptionConfirmation, ConversationArtifactKind,
    ConversationResponse, GetConfigOptionsResponse, GetModelInfoResponse, ModelInfoEntry, ModelInfoPayload,
    SetConfigOptionRequest, SetConfigOptionResponse,
};
use aionui_api_types::{
    CloneConversationRequest, CreateConversationRequest, ListConversationsQuery, SearchMessagesQuery,
    SendMessageRequest, UpdateConversationRequest, WebSocketMessage,
};
use aionui_common::{
    AgentKillReason, AgentType, Confirmation, ConversationSource, ConversationStatus, PaginatedResult,
    ProviderWithModel, TimestampMs,
};
use aionui_db::models::{
    AcpSessionRow, AgentMetadataRow, ConversationArtifactRow, ConversationAssistantSnapshotRow, ConversationRow,
    MessageRow, UpdateAgentHandshakeParams, UpsertAgentMetadataParams,
};
use aionui_db::{
    ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, DbError, IAcpSessionRepository,
    IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantPreferenceRepository, IConversationRepository, MessageRowUpdate, MessageSearchRow, PersistedSessionState,
    SaveRuntimeStateParams, SqliteAssistantDefinitionRepository, SqliteAssistantOverlayRepository,
    SqliteAssistantPreferenceRepository, UpdateAgentAvailabilitySnapshotParams, UpsertAssistantDefinitionParams,
    UpsertAssistantOverlayParams, UpsertAssistantPreferenceParams, UpsertConversationAssistantSnapshotParams,
    init_database_memory,
};
use aionui_db::{MessagePageCursor, MessagePageDirection, MessagePageParams, MessagePageResult};
use aionui_extension::{AssistantRuleDispatcher, ExtensionError};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::{Notify, broadcast};

use crate::service::ConversationService;
use crate::skill_resolver::{FixedSkillResolver, ResolvedAgentSkill, SkillResolver};
use crate::{ConversationAgentTurnRequest, ConversationAgentTurnStatus, ConversationError};

#[path = "service_test/acp_error_recovery_test.rs"]
mod acp_error_recovery_test;

#[derive(Clone, Debug)]
struct SkillLinkCall {
    workspace: PathBuf,
    rel_dirs: Vec<String>,
    skill_names: Vec<String>,
}

struct RecordingSkillResolver {
    names: Vec<String>,
    links: Arc<Mutex<Vec<SkillLinkCall>>>,
}

struct StaticAssistantDispatcher {
    rules: std::collections::HashMap<String, String>,
}

#[async_trait::async_trait]
impl AssistantRuleDispatcher for StaticAssistantDispatcher {
    async fn read_rule(&self, id: &str, _locale: Option<&str>) -> Result<String, ExtensionError> {
        Ok(self.rules.get(id).cloned().unwrap_or_default())
    }

    async fn write_rule(&self, _id: &str, _locale: Option<&str>, _content: &str) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn delete_rule(&self, _id: &str) -> Result<bool, ExtensionError> {
        Ok(true)
    }

    async fn read_skill(&self, _id: &str, _locale: Option<&str>) -> Result<String, ExtensionError> {
        Ok(String::new())
    }

    async fn write_skill(&self, _id: &str, _locale: Option<&str>, _content: &str) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn delete_skill(&self, _id: &str) -> Result<bool, ExtensionError> {
        Ok(true)
    }
}

impl RecordingSkillResolver {
    fn new(names: Vec<String>) -> Self {
        Self {
            names,
            links: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait::async_trait]
impl SkillResolver for RecordingSkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        self.names.clone()
    }

    async fn resolve_skills(&self, names: &[String]) -> Vec<ResolvedAgentSkill> {
        names
            .iter()
            .map(|name| ResolvedAgentSkill {
                name: name.clone(),
                source_path: std::env::temp_dir().join(format!("skill-source-{name}")),
            })
            .collect()
    }

    async fn link_workspace_skills(&self, workspace: &Path, rel_dirs: &[&str], skills: &[ResolvedAgentSkill]) -> usize {
        self.links.lock().unwrap().push(SkillLinkCall {
            workspace: workspace.to_path_buf(),
            rel_dirs: rel_dirs.iter().map(|s| (*s).to_owned()).collect(),
            skill_names: skills.iter().map(|skill| skill.name.clone()).collect(),
        });

        let mut linked = 0;
        for rel_dir in rel_dirs {
            let target_dir = workspace.join(rel_dir);
            if std::fs::create_dir_all(&target_dir).is_err() {
                continue;
            }
            for skill in skills {
                if std::fs::create_dir_all(target_dir.join(&skill.name)).is_ok() {
                    linked += 1;
                }
            }
        }
        linked
    }
}

// ── Mock EventBroadcaster ──────────────────────────────────────────

struct MockBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl MockBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(vec![]),
        }
    }

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        std::mem::take(&mut self.events.lock().unwrap())
    }
}

impl EventBroadcaster for MockBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedAvailabilityFailure {
    agent_id: String,
    code: String,
    message: String,
}

#[derive(Default)]
struct RecordingAvailabilityFeedback {
    successes: Mutex<Vec<String>>,
    failures: Mutex<Vec<RecordedAvailabilityFailure>>,
}

#[async_trait::async_trait]
impl AgentAvailabilityFeedbackPort for RecordingAvailabilityFeedback {
    async fn record_session_success(&self, agent_id: &str) -> Result<(), AgentError> {
        self.successes.lock().unwrap().push(agent_id.to_owned());
        Ok(())
    }

    async fn record_session_failure(&self, agent_id: &str, code: &str, message: &str) -> Result<(), AgentError> {
        self.failures.lock().unwrap().push(RecordedAvailabilityFailure {
            agent_id: agent_id.to_owned(),
            code: code.to_owned(),
            message: message.to_owned(),
        });
        Ok(())
    }
}

// ── Mock Repository ────────────────────────────────────────────────

struct MockRepo {
    rows: Mutex<Vec<ConversationRow>>,
    messages: Mutex<Vec<MessageRow>>,
    artifacts: Mutex<Vec<ConversationArtifactRow>>,
    assistant_snapshots: Mutex<Vec<ConversationAssistantSnapshotRow>>,
}

impl MockRepo {
    fn new() -> Self {
        Self {
            rows: Mutex::new(vec![]),
            messages: Mutex::new(vec![]),
            artifacts: Mutex::new(vec![]),
            assistant_snapshots: Mutex::new(vec![]),
        }
    }
}

fn message_is_before_cursor(message: &MessageRow, cursor: &MessagePageCursor) -> bool {
    message.created_at < cursor.created_at || (message.created_at == cursor.created_at && message.id < cursor.id)
}

fn message_is_after_cursor(message: &MessageRow, cursor: &MessagePageCursor) -> bool {
    message.created_at > cursor.created_at || (message.created_at == cursor.created_at && message.id > cursor.id)
}

async fn repo_messages_asc(repo: &Arc<MockRepo>, conv_id: &str, limit: u32) -> Vec<MessageRow> {
    repo.list_messages_page(
        conv_id,
        &MessagePageParams {
            limit,
            direction: MessagePageDirection::InitialLatest,
        },
    )
    .await
    .unwrap()
    .items
}

#[async_trait::async_trait]
impl IConversationRepository for MockRepo {
    async fn get(&self, id: &str) -> Result<Option<ConversationRow>, aionui_db::DbError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows.iter().find(|r| r.id == id).cloned())
    }

    async fn create(&self, row: &ConversationRow) -> Result<(), aionui_db::DbError> {
        self.rows.lock().unwrap().push(row.clone());
        Ok(())
    }

    async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), aionui_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| aionui_db::DbError::NotFound(format!("Conversation {id}")))?;

        if let Some(name) = &updates.name {
            row.name = name.clone();
        }
        if let Some(pinned) = updates.pinned {
            row.pinned = pinned;
        }
        if let Some(pinned_at) = &updates.pinned_at {
            row.pinned_at = *pinned_at;
        }
        if let Some(model) = &updates.model {
            row.model = model.clone();
        }
        if let Some(extra) = &updates.extra {
            row.extra = extra.clone();
        }
        if let Some(status) = &updates.status {
            row.status = Some(status.clone());
        }
        if let Some(updated_at) = updates.updated_at {
            row.updated_at = updated_at;
        }
        Ok(())
    }

    async fn delete(&self, id: &str) -> Result<(), aionui_db::DbError> {
        let mut rows = self.rows.lock().unwrap();
        let len_before = rows.len();
        rows.retain(|r| r.id != id);
        if rows.len() == len_before {
            return Err(aionui_db::DbError::NotFound(format!("Conversation {id}")));
        }
        Ok(())
    }

    async fn list_paginated(
        &self,
        user_id: &str,
        filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, aionui_db::DbError> {
        let rows = self.rows.lock().unwrap();
        let matched: Vec<_> = rows
            .iter()
            .filter(|r| r.user_id == user_id)
            .filter(|r| {
                filters
                    .source
                    .as_ref()
                    .is_none_or(|s| r.source.as_deref() == Some(s.as_str()))
            })
            .filter(|r| filters.pinned.as_ref().is_none_or(|&p| r.pinned == p))
            .cloned()
            .collect();
        let total = matched.len() as u64;
        let limit = filters.effective_limit() as usize;
        let items: Vec<_> = matched.into_iter().take(limit).collect();
        let has_more = (total as usize) > limit;
        Ok(PaginatedResult { items, total, has_more })
    }

    async fn find_by_source_and_chat(
        &self,
        _user_id: &str,
        _source: &str,
        _chat_id: &str,
        _agent_type: &str,
    ) -> Result<Option<ConversationRow>, aionui_db::DbError> {
        Ok(None)
    }

    async fn list_by_cron_job(
        &self,
        _user_id: &str,
        _cron_job_id: &str,
    ) -> Result<Vec<ConversationRow>, aionui_db::DbError> {
        Ok(vec![])
    }

    async fn list_associated(
        &self,
        _user_id: &str,
        _conversation_id: &str,
    ) -> Result<Vec<ConversationRow>, aionui_db::DbError> {
        Ok(vec![])
    }

    async fn get_assistant_snapshot(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, aionui_db::DbError> {
        let rows = self.assistant_snapshots.lock().unwrap();
        Ok(rows.iter().find(|row| row.conversation_id == conversation_id).cloned())
    }

    async fn upsert_assistant_snapshot(
        &self,
        params: &UpsertConversationAssistantSnapshotParams<'_>,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, aionui_db::DbError> {
        let row = ConversationAssistantSnapshotRow {
            conversation_id: params.conversation_id.to_owned(),
            assistant_definition_id: params.assistant_definition_id.to_owned(),
            assistant_id: params.assistant_id.to_owned(),
            assistant_source: params.assistant_source.to_owned(),
            assistant_name: params.assistant_name.to_owned(),
            assistant_avatar_type: params.assistant_avatar_type.to_owned(),
            assistant_avatar_value: params.assistant_avatar_value.map(ToOwned::to_owned),
            agent_id: params.agent_id.to_owned(),
            rules_content: params.rules_content.to_owned(),
            default_model_mode: params.default_model_mode.to_owned(),
            resolved_model_id: params.resolved_model_id.map(ToOwned::to_owned),
            default_permission_mode: params.default_permission_mode.to_owned(),
            resolved_permission_value: params.resolved_permission_value.map(ToOwned::to_owned),
            default_skills_mode: params.default_skills_mode.to_owned(),
            resolved_skill_ids: params.resolved_skill_ids.to_owned(),
            resolved_disabled_builtin_skill_ids: params.resolved_disabled_builtin_skill_ids.to_owned(),
            default_mcps_mode: params.default_mcps_mode.to_owned(),
            resolved_mcp_ids: params.resolved_mcp_ids.to_owned(),
            created_at: 1,
            updated_at: 1,
        };
        let mut rows = self.assistant_snapshots.lock().unwrap();
        rows.retain(|existing| existing.conversation_id != params.conversation_id);
        rows.push(row.clone());
        Ok(Some(row))
    }

    async fn delete_assistant_snapshot(&self, conversation_id: &str) -> Result<bool, aionui_db::DbError> {
        let mut rows = self.assistant_snapshots.lock().unwrap();
        let before = rows.len();
        rows.retain(|row| row.conversation_id != conversation_id);
        Ok(rows.len() != before)
    }

    async fn list_messages_page(
        &self,
        conv_id: &str,
        params: &MessagePageParams,
    ) -> Result<MessagePageResult, aionui_db::DbError> {
        let messages = self.messages.lock().unwrap();
        let mut matched: Vec<_> = messages
            .iter()
            .filter(|message| message.conversation_id == conv_id)
            .filter(|message| !matches!(message.r#type.as_str(), "cron_trigger" | "skill_suggest"))
            .cloned()
            .collect();
        matched.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));

        let limit = params.limit.max(1) as usize;
        let items = match &params.direction {
            MessagePageDirection::InitialLatest => {
                let start = matched.len().saturating_sub(limit);
                matched[start..].to_vec()
            }
            MessagePageDirection::Before { cursor } => matched
                .iter()
                .filter(|message| message_is_before_cursor(message, cursor))
                .rev()
                .take(limit)
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            MessagePageDirection::After { cursor } => matched
                .iter()
                .filter(|message| message_is_after_cursor(message, cursor))
                .take(limit)
                .cloned()
                .collect(),
            MessagePageDirection::Anchor { message_id } => {
                let anchor = matched
                    .iter()
                    .find(|message| message.id == *message_id)
                    .cloned()
                    .ok_or_else(|| aionui_db::DbError::NotFound(format!("Message {message_id}")))?;
                let before_take = (limit.saturating_sub(1)) / 2;
                let anchor_cursor = MessagePageCursor::from(&anchor);
                let before = matched
                    .iter()
                    .filter(|message| message_is_before_cursor(message, &anchor_cursor))
                    .rev()
                    .take(before_take)
                    .cloned()
                    .collect::<Vec<_>>();
                let after = matched
                    .iter()
                    .filter(|message| message_is_after_cursor(message, &anchor_cursor))
                    .take(limit.saturating_sub(1 + before.len()))
                    .cloned()
                    .collect::<Vec<_>>();
                before
                    .into_iter()
                    .rev()
                    .chain(std::iter::once(anchor))
                    .chain(after)
                    .collect()
            }
        };
        let has_more_before = items.first().is_some_and(|first| {
            let cursor = MessagePageCursor::from(first);
            matched.iter().any(|message| message_is_before_cursor(message, &cursor))
        });
        let has_more_after = items.last().is_some_and(|last| {
            let cursor = MessagePageCursor::from(last);
            matched.iter().any(|message| message_is_after_cursor(message, &cursor))
        });

        Ok(MessagePageResult {
            items,
            has_more_before,
            has_more_after,
        })
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), aionui_db::DbError> {
        let mut messages = self.messages.lock().unwrap();
        messages.push(message.clone());
        Ok(())
    }

    async fn update_message(&self, id: &str, updates: &MessageRowUpdate) -> Result<(), aionui_db::DbError> {
        let mut messages = self.messages.lock().unwrap();
        let message = messages
            .iter_mut()
            .find(|message| message.id == id)
            .ok_or_else(|| aionui_db::DbError::NotFound(format!("Message {id}")))?;

        if let Some(content) = &updates.content {
            message.content = content.clone();
        }
        if let Some(status) = &updates.status {
            message.status = status.clone();
        }
        if let Some(hidden) = updates.hidden {
            message.hidden = hidden;
        }
        Ok(())
    }

    async fn delete_messages_by_conversation(&self, conv_id: &str) -> Result<(), aionui_db::DbError> {
        self.messages
            .lock()
            .unwrap()
            .retain(|message| message.conversation_id != conv_id);
        Ok(())
    }

    async fn get_message_by_msg_id(
        &self,
        conv_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, aionui_db::DbError> {
        let messages = self.messages.lock().unwrap();
        Ok(messages
            .iter()
            .find(|message| {
                message.conversation_id == conv_id
                    && message.msg_id.as_deref() == Some(msg_id)
                    && message.r#type == msg_type
            })
            .cloned())
    }

    async fn list_stale_runtime_messages(&self) -> Result<Vec<MessageRow>, aionui_db::DbError> {
        let messages = self.messages.lock().unwrap();
        Ok(messages
            .iter()
            .filter(|message| {
                message.position.as_deref() == Some("left")
                    && matches!(message.status.as_deref(), Some("work" | "pending"))
                    && matches!(message.r#type.as_str(), "text" | "thinking")
            })
            .cloned()
            .collect())
    }

    async fn search_messages(
        &self,
        _user_id: &str,
        _keyword: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, aionui_db::DbError> {
        Ok(PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }

    async fn list_artifacts(&self, conversation_id: &str) -> Result<Vec<ConversationArtifactRow>, aionui_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .filter(|artifact| artifact.conversation_id == conversation_id)
            .cloned()
            .collect())
    }

    async fn get_artifact(
        &self,
        conversation_id: &str,
        artifact_id: &str,
    ) -> Result<Option<ConversationArtifactRow>, aionui_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .find(|artifact| artifact.conversation_id == conversation_id && artifact.id == artifact_id)
            .cloned())
    }

    async fn upsert_artifact(
        &self,
        artifact: &ConversationArtifactRow,
    ) -> Result<ConversationArtifactRow, aionui_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        if let Some(existing) = artifacts.iter_mut().find(|row| row.id == artifact.id) {
            *existing = artifact.clone();
            return Ok(existing.clone());
        }
        artifacts.push(artifact.clone());
        Ok(artifact.clone())
    }

    async fn update_artifact_status(
        &self,
        conversation_id: &str,
        artifact_id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<Option<ConversationArtifactRow>, aionui_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        let Some(existing) = artifacts
            .iter_mut()
            .find(|artifact| artifact.conversation_id == conversation_id && artifact.id == artifact_id)
        else {
            return Ok(None);
        };
        existing.status = status.to_owned();
        existing.updated_at = updated_at;
        Ok(Some(existing.clone()))
    }

    async fn mark_skill_suggest_artifacts_saved(
        &self,
        cron_job_id: &str,
        updated_at: TimestampMs,
    ) -> Result<Vec<ConversationArtifactRow>, aionui_db::DbError> {
        let mut artifacts = self.artifacts.lock().unwrap();
        let mut updated = Vec::new();
        for artifact in artifacts
            .iter_mut()
            .filter(|artifact| artifact.cron_job_id.as_deref() == Some(cron_job_id))
        {
            artifact.status = "saved".into();
            artifact.updated_at = updated_at;
            updated.push(artifact.clone());
        }
        Ok(updated)
    }

    async fn delete_artifacts_by_conversation(&self, conversation_id: &str) -> Result<(), aionui_db::DbError> {
        self.artifacts
            .lock()
            .unwrap()
            .retain(|artifact| artifact.conversation_id != conversation_id);
        Ok(())
    }

    async fn list_legacy_cron_trigger_messages(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<MessageRow>, aionui_db::DbError> {
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.conversation_id == conversation_id && message.r#type == "cron_trigger")
            .cloned()
            .collect())
    }
}

// ── Helpers ────────────────────────────────────────────────────────

/// Stub repository for tests. Builtin rows mirror the ids used by the
/// migration seed so assistant id-resolution tests exercise the same
/// boundary as the SQLite repository.
struct StubAgentMetadataRepo;

fn stub_agent_metadata_rows() -> Vec<AgentMetadataRow> {
    [
        ("2d23ff1c", Some("claude"), "acp", "Claude Code", 100),
        ("8e1acf31", Some("codex"), "acp", "Codex CLI", 110),
        ("cc126dd5", Some("gemini"), "acp", "Gemini CLI", 120),
        ("632f31d2", None, "aionrs", "Aion CLI", 200),
    ]
    .into_iter()
    .map(|(id, backend, agent_type, name, sort_order)| AgentMetadataRow {
        id: id.to_owned(),
        icon: None,
        name: name.to_owned(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: backend.map(ToOwned::to_owned),
        agent_type: agent_type.to_owned(),
        agent_source: "builtin".to_owned(),
        agent_source_info: None,
        enabled: true,
        command: backend.map(ToOwned::to_owned),
        args: Some("[]".to_owned()),
        env: Some("[]".to_owned()),
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order,
        last_check_status: None,
        last_check_kind: None,
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        command_override: None,
        env_override: None,
        created_at: 1,
        updated_at: 1,
    })
    .collect()
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for StubAgentMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        Ok(stub_agent_metadata_rows())
    }
    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(stub_agent_metadata_rows().into_iter().find(|row| row.id == id))
    }
    async fn find_by_source_and_name(
        &self,
        _agent_source: &str,
        _name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(stub_agent_metadata_rows()
            .into_iter()
            .find(|row| row.agent_source == "builtin" && row.backend.as_deref() == Some(backend)))
    }
    async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        Err(DbError::Init("stub".into()))
    }
    async fn apply_handshake(
        &self,
        _id: &str,
        _params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn update_availability_snapshot(
        &self,
        _id: &str,
        _params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn update_agent_overrides(
        &self,
        _id: &str,
        _command_override: Option<&str>,
        _env_override: Option<&str>,
    ) -> Result<(), DbError> {
        Ok(())
    }
    async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn delete(&self, _id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
}

/// Metadata repo used by custom-workspace skill-link tests. It models the
/// builtin Claude ACP row whose native skill directory is `.claude/skills`.
struct ClaudeNativeSkillMetadataRepo;

fn claude_metadata_row() -> AgentMetadataRow {
    AgentMetadataRow {
        id: "agent-claude".into(),
        icon: None,
        name: "Claude Code".into(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some("claude".into()),
        agent_type: "acp".into(),
        agent_source: "builtin".into(),
        agent_source_info: Some("{}".into()),
        enabled: true,
        command: Some("claude".into()),
        args: Some("[]".into()),
        env: Some("[]".into()),
        native_skills_dirs: Some(r#"[".claude/skills"]"#.into()),
        behavior_policy: Some("{}".into()),
        yolo_id: Some("bypassPermissions".into()),
        agent_capabilities: None,
        auth_methods: None,
        config_options: None,
        available_modes: None,
        available_models: None,
        available_commands: None,
        sort_order: 0,
        last_check_status: None,
        last_check_kind: None,
        last_check_error_code: None,
        last_check_error_message: None,
        last_check_guidance: None,
        last_check_latency_ms: None,
        last_check_at: None,
        last_success_at: None,
        last_failure_at: None,
        command_override: None,
        env_override: None,
        created_at: 1,
        updated_at: 1,
    }
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for ClaudeNativeSkillMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        Ok(vec![claude_metadata_row()])
    }
    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok((id == "agent-claude").then(claude_metadata_row))
    }
    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok((agent_source == "builtin" && name == "Claude Code").then(claude_metadata_row))
    }
    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok((backend == "claude").then(claude_metadata_row))
    }
    async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        Err(DbError::Init("stub".into()))
    }
    async fn apply_handshake(
        &self,
        _id: &str,
        _params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn update_availability_snapshot(
        &self,
        _id: &str,
        _params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn update_agent_overrides(
        &self,
        _id: &str,
        _command_override: Option<&str>,
        _env_override: Option<&str>,
    ) -> Result<(), DbError> {
        Ok(())
    }
    async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn delete(&self, _id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeStateSaveCall {
    conversation_id: String,
    current_mode_id: Option<Option<String>>,
    current_model_id: Option<Option<String>>,
}

#[derive(Default)]
struct StubAcpSessionRepo {
    create_calls: Mutex<Vec<CreateAcpSessionCall>>,
    runtime_state_saves: Mutex<Vec<RuntimeStateSaveCall>>,
    session_id: Mutex<Option<String>>,
}

impl StubAcpSessionRepo {
    fn create_calls(&self) -> Vec<CreateAcpSessionCall> {
        self.create_calls.lock().unwrap().clone()
    }

    fn with_session_id(session_id: impl Into<String>) -> Self {
        Self {
            create_calls: Mutex::new(Vec::new()),
            runtime_state_saves: Mutex::new(Vec::new()),
            session_id: Mutex::new(Some(session_id.into())),
        }
    }

    fn runtime_state_saves(&self) -> Vec<RuntimeStateSaveCall> {
        self.runtime_state_saves.lock().unwrap().clone()
    }

    fn row_for(&self, conversation_id: &str) -> AcpSessionRow {
        AcpSessionRow {
            conversation_id: conversation_id.to_owned(),
            agent_source: "builtin".into(),
            agent_id: "codex".into(),
            session_id: self.session_id.lock().unwrap().clone(),
            session_status: "idle".into(),
            session_config: "{}".into(),
            last_active_at: None,
            suspended_at: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateAcpSessionCall {
    conversation_id: String,
    agent_source: String,
    agent_id: String,
}

#[async_trait::async_trait]
impl IAcpSessionRepository for StubAcpSessionRepo {
    async fn get(&self, conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError> {
        Ok(Some(self.row_for(conversation_id)))
    }
    async fn create(&self, params: &CreateAcpSessionParams<'_>) -> Result<AcpSessionRow, DbError> {
        self.create_calls.lock().unwrap().push(CreateAcpSessionCall {
            conversation_id: params.conversation_id.to_owned(),
            agent_source: params.agent_source.to_owned(),
            agent_id: params.agent_id.to_owned(),
        });
        // Return a synthetic row so `ConversationService::create` can
        // succeed for ACP conversations in unit tests.
        Ok(AcpSessionRow {
            conversation_id: params.conversation_id.to_owned(),
            agent_source: params.agent_source.to_owned(),
            agent_id: params.agent_id.to_owned(),
            session_id: self.session_id.lock().unwrap().clone(),
            session_status: "idle".into(),
            session_config: "{}".into(),
            last_active_at: None,
            suspended_at: None,
        })
    }
    async fn update_session_id(&self, _conversation_id: &str, session_id: &str) -> Result<bool, DbError> {
        *self.session_id.lock().unwrap() = Some(session_id.to_owned());
        Ok(true)
    }
    async fn delete(&self, _conversation_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn load_runtime_state(&self, _conversation_id: &str) -> Result<Option<PersistedSessionState>, DbError> {
        Ok(Some(PersistedSessionState {
            current_model_id: Some("deepseek-v4-pro".to_owned()),
            ..Default::default()
        }))
    }
    async fn save_runtime_state(
        &self,
        conversation_id: &str,
        params: &SaveRuntimeStateParams<'_>,
    ) -> Result<bool, DbError> {
        self.runtime_state_saves.lock().unwrap().push(RuntimeStateSaveCall {
            conversation_id: conversation_id.to_owned(),
            current_mode_id: params.current_mode_id.map(|outer| outer.map(ToOwned::to_owned)),
            current_model_id: params.current_model_id.map(|outer| outer.map(ToOwned::to_owned)),
        });
        Ok(true)
    }
}

fn make_service() -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn IWorkerTaskManager>,
) {
    make_service_with_resolver(Arc::new(FixedSkillResolver { names: vec![] }))
}

fn make_service_with_resolver(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn IWorkerTaskManager>,
) {
    make_service_with_resolver_and_acp_session_repo(skill_resolver, Arc::new(StubAcpSessionRepo::default()))
}

fn make_service_with_resolver_and_acp_session_repo(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
    acp_session_repo: Arc<dyn IAcpSessionRepository>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn IWorkerTaskManager>,
) {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo);
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        skill_resolver,
        task_mgr.clone(),
        repo.clone(),
        agent_metadata_repo,
        acp_session_repo,
    );
    (svc, broadcaster, repo, task_mgr)
}

fn make_service_with_resolver_and_agent_metadata_repo(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<dyn IWorkerTaskManager>,
) {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        skill_resolver,
        task_mgr.clone(),
        repo.clone(),
        agent_metadata_repo,
        Arc::new(StubAcpSessionRepo::default()),
    );
    (svc, broadcaster, repo, task_mgr)
}

fn make_service_with_mock_task_manager(
    task_mgr: Arc<MockTaskManager>,
) -> (ConversationService, Arc<MockBroadcaster>, Arc<MockRepo>) {
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo);
    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr;
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(FixedSkillResolver { names: vec![] }),
        task_mgr_dyn,
        repo.clone(),
        agent_metadata_repo,
        Arc::new(StubAcpSessionRepo::default()),
    );
    (svc, broadcaster, repo)
}

async fn make_service_with_mock_task_manager_and_assistant_support(
    task_mgr: Arc<MockTaskManager>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<SqliteAssistantDefinitionRepository>,
    Arc<SqliteAssistantOverlayRepository>,
    Arc<dyn IAssistantPreferenceRepository>,
) {
    let (svc, broadcaster, repo) = make_service_with_mock_task_manager(task_mgr);
    let db = init_database_memory().await.unwrap();
    let definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(db.pool().clone()));
    let overlay_repo = Arc::new(SqliteAssistantOverlayRepository::new(db.pool().clone()));
    let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(db.pool().clone()));

    svc.with_assistant_definition_repo(definition_repo.clone());
    svc.with_assistant_state_repo(overlay_repo.clone());
    svc.with_assistant_preference_repo(preference_repo.clone());

    (svc, broadcaster, repo, definition_repo, overlay_repo, preference_repo)
}

async fn make_service_with_assistant_support(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
    dispatcher: Arc<dyn AssistantRuleDispatcher>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<SqliteAssistantDefinitionRepository>,
    Arc<SqliteAssistantOverlayRepository>,
    Arc<dyn IAssistantPreferenceRepository>,
) {
    let (svc, broadcaster, repo, _task_mgr) = make_service_with_resolver(skill_resolver);
    let db = init_database_memory().await.unwrap();
    let definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(db.pool().clone()));
    let state_repo = Arc::new(SqliteAssistantOverlayRepository::new(db.pool().clone()));
    let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(db.pool().clone()));

    svc.with_assistant_definition_repo(definition_repo.clone());
    svc.with_assistant_state_repo(state_repo.clone());
    svc.with_assistant_preference_repo(preference_repo.clone());
    svc.with_assistant_dispatcher(dispatcher);

    (svc, broadcaster, repo, definition_repo, state_repo, preference_repo)
}

async fn make_service_with_assistant_support_and_acp_session_repo(
    skill_resolver: Arc<dyn crate::skill_resolver::SkillResolver>,
    dispatcher: Arc<dyn AssistantRuleDispatcher>,
    acp_session_repo: Arc<StubAcpSessionRepo>,
) -> (
    ConversationService,
    Arc<MockBroadcaster>,
    Arc<MockRepo>,
    Arc<SqliteAssistantDefinitionRepository>,
    Arc<SqliteAssistantOverlayRepository>,
    Arc<dyn IAssistantPreferenceRepository>,
    Arc<StubAcpSessionRepo>,
) {
    let (svc, broadcaster, repo, _task_mgr) =
        make_service_with_resolver_and_acp_session_repo(skill_resolver, acp_session_repo.clone());
    let db = init_database_memory().await.unwrap();
    let definition_repo = Arc::new(SqliteAssistantDefinitionRepository::new(db.pool().clone()));
    let state_repo = Arc::new(SqliteAssistantOverlayRepository::new(db.pool().clone()));
    let preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(db.pool().clone()));

    svc.with_assistant_definition_repo(definition_repo.clone());
    svc.with_assistant_state_repo(state_repo.clone());
    svc.with_assistant_preference_repo(preference_repo.clone());
    svc.with_assistant_dispatcher(dispatcher);

    (
        svc,
        broadcaster,
        repo,
        definition_repo,
        state_repo,
        preference_repo,
        acp_session_repo,
    )
}

fn make_create_req() -> CreateConversationRequest {
    let workspace = ensure_test_workspace_path();
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace }
    }))
    .unwrap()
}

fn make_create_req_with_backend(backend: &str) -> CreateConversationRequest {
    let workspace = ensure_test_workspace_path();
    serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "workspace": workspace,
            "custom_workspace": true,
            "backend": backend
        }
    }))
    .unwrap()
}

fn ensure_test_workspace_path() -> String {
    let workspace = std::env::temp_dir().join("aionui-conversation-service-test-project");
    std::fs::create_dir_all(&workspace).unwrap();
    workspace.to_string_lossy().to_string()
}

fn unique_test_workspace_path(label: &str) -> PathBuf {
    let workspace = std::env::temp_dir()
        .join(format!("aionui-conversation-service-test-{label}"))
        .join(ConversationService::mint_msg_id());
    std::fs::create_dir_all(&workspace).unwrap();
    workspace
}

async fn upsert_test_assistant_definition(
    repo: &SqliteAssistantDefinitionRepository,
    definition_id: &str,
    assistant_id: &str,
    agent_id: &str,
    default_model_mode: &str,
    default_permission_mode: &str,
) {
    repo.upsert(&UpsertAssistantDefinitionParams {
        id: definition_id,
        assistant_id,
        source: "builtin",
        owner_type: "system",
        source_ref: Some(assistant_id),
        source_version: None,
        source_hash: None,
        name: assistant_id,
        name_i18n: "{}",
        description: Some("desc"),
        description_i18n: "{}",
        avatar_type: "emoji",
        avatar_value: Some("🤖"),
        agent_id,
        rule_resource_type: "builtin_asset",
        rule_resource_ref: Some(assistant_id),
        rule_inline_content: None,
        recommended_prompts: "[]",
        recommended_prompts_i18n: "{}",
        default_model_mode,
        default_model_value: None,
        default_permission_mode,
        default_permission_value: None,
        default_skills_mode: "auto",
        default_skill_ids: "[]",
        custom_skill_names: "[]",
        default_disabled_builtin_skill_ids: "[]",
        default_mcps_mode: "auto",
        default_mcp_ids: "[]",
    })
    .await
    .unwrap();
}

async fn create_assistant_backed_conversation(
    svc: &ConversationService,
    user_id: &str,
    conversation_type: Option<&str>,
    backend: &str,
    assistant_id: &str,
) -> ConversationResponse {
    let workspace = ensure_test_workspace_path();
    let mut payload = json!({
        "name": "assistant conversation",
        "assistant": {
            "id": assistant_id,
            "locale": "en-US"
        },
        "extra": {
            "workspace": workspace,
            "backend": backend
        }
    });

    if let Some(conversation_type) = conversation_type {
        payload["type"] = json!(conversation_type);
    }

    if conversation_type == Some("aionrs") {
        payload["model"] = json!({
            "provider_id": "provider-1",
            "model": "model-a",
            "use_model": "model-a"
        });
    }

    let req: CreateConversationRequest = serde_json::from_value(payload).unwrap();
    svc.create(user_id, req).await.unwrap()
}

async fn insert_conversation_with_type(repo: &Arc<MockRepo>, user_id: &str, agent_type: AgentType) -> ConversationRow {
    let id = format!(
        "legacy-{}-{}",
        agent_type.serde_name(),
        aionui_common::generate_short_id()
    );
    let row = ConversationRow {
        id,
        user_id: user_id.to_owned(),
        name: format!("legacy {}", agent_type.serde_name()),
        r#type: agent_type.serde_name().to_owned(),
        extra: json!({
            "workspace": ensure_test_workspace_path()
        })
        .to_string(),
        model: None,
        status: Some("finished".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
    };
    repo.create(&row).await.unwrap();
    row
}

// ── Create tests ───────────────────────────────────────────────────

#[tokio::test]
async fn create_returns_conversation_with_defaults() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    let resp = svc.create("user_1", make_create_req()).await.unwrap();

    assert!(!resp.id.is_empty());
    assert_eq!(resp.r#type, AgentType::Acp);
    assert_eq!(resp.status, ConversationStatus::Pending);
    assert_eq!(resp.source, Some(ConversationSource::Aionui));
    assert!(!resp.pinned);
    assert!(resp.pinned_at.is_none());
    assert_eq!(resp.extra["workspace"], workspace);
    assert!(resp.created_at > 0);
    assert_eq!(resp.created_at, resp.modified_at);

    // Should have broadcast a listChanged(created) event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "conversation.listChanged");
    assert_eq!(events[0].data["action"], "created");
    assert_eq!(events[0].data["conversation_id"], resp.id);
    assert_eq!(events[0].data["source"], "aionui");
}

#[tokio::test]
async fn create_rejects_deprecated_agent_types_for_new_conversations() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();

    for agent_type in [
        AgentType::Gemini,
        AgentType::Codex,
        AgentType::OpenclawGateway,
        AgentType::Nanobot,
        AgentType::Remote,
    ] {
        let mut req = make_create_req();
        req.r#type = Some(agent_type);
        req.model = None;
        req.extra = json!({
            "workspace": ensure_test_workspace_path()
        });

        let err = svc.create("user_1", req).await.unwrap_err();
        assert_eq!(err.error_code(), "BAD_REQUEST");
        assert!(
            err.to_string()
                .contains("This agent type is no longer supported for new conversations."),
            "unexpected error for {}: {err}",
            agent_type.serde_name()
        );
    }
}

#[tokio::test]
async fn create_rejects_unavailable_workspace_with_trailing_whitespace_in_request() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let dir = std::env::temp_dir().join(format!("aionui-test-{}", aionui_common::generate_short_id()));
    std::fs::create_dir(&dir).unwrap();
    let workspace = dir.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace_with_trailing_space = format!("{} ", workspace.to_string_lossy());

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace_with_trailing_space }
    }))
    .unwrap();
    let err = svc.create("user_1", req).await.unwrap_err();
    assert!(matches!(
        err,
        ConversationError::WorkspacePathUnavailable { path }
            if path == workspace_with_trailing_space
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_accepts_existing_workspace_with_trailing_whitespace_in_name() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let dir = std::env::temp_dir().join(format!("aionui-test-{}", aionui_common::generate_short_id()));
    std::fs::create_dir(&dir).unwrap();
    let workspace = dir.join("workspace ");
    std::fs::create_dir(&workspace).unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace.to_string_lossy() }
    }))
    .unwrap();
    let resp = svc.create("user_1", req).await.unwrap();
    assert_eq!(resp.extra["workspace"], workspace.to_string_lossy().to_string());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_accepts_workspace_with_whitespace_in_any_path_segment() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let dir = std::env::temp_dir().join(format!("aionui-test-{}", aionui_common::generate_short_id()));
    std::fs::create_dir(&dir).unwrap();
    let workspace = dir.join("my project").join("workspace");
    std::fs::create_dir(dir.join("my project")).unwrap();
    std::fs::create_dir(&workspace).unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace.to_string_lossy() }
    }))
    .unwrap();
    let resp = svc.create("user_1", req).await.unwrap();
    assert_eq!(resp.extra["workspace"], workspace.to_string_lossy().to_string());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn create_with_custom_name_and_source() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "Custom Name",
        "source": "telegram",
        "channel_chat_id": "chat:123",
        "extra": {}
    }))
    .unwrap();

    let resp = svc.create("user_1", req).await.unwrap();

    assert_eq!(resp.name, "Custom Name");
    assert_eq!(resp.r#type, AgentType::Acp);
    assert_eq!(resp.source, Some(ConversationSource::Telegram));
    assert_eq!(resp.channel_chat_id.as_deref(), Some("chat:123"));
}

#[tokio::test]
async fn create_stores_model_as_json() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    // Top-level model is only valid for aionrs conversations.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "aionrs",
        "model": { "provider_id": "p1", "model": "m1" },
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let resp = svc.create("user_1", req).await.unwrap();

    let model = resp.model.unwrap();
    assert_eq!(model.provider_id, "p1");
    assert_eq!(model.model, "m1");
}

#[tokio::test]
async fn create_derives_aionrs_type_from_assistant_backend_when_type_is_missing() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::new(),
    });
    let (svc, _broadcaster, repo, definition_repo, overlay_repo, _preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_aionrs_missing_type",
        "assistant-aionrs-missing-type",
        "aionrs",
        "auto",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_aionrs_missing_type",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let workspace = ensure_test_workspace_path();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "assistant": {
            "id": "assistant-aionrs-missing-type",
            "locale": "en-US"
        },
        "model": {
            "provider_id": "provider-1",
            "model": "model-a",
            "use_model": "model-a"
        },
        "extra": {
            "workspace": workspace
        }
    }))
    .unwrap();

    let resp = svc.create("user_1", req).await.unwrap();
    assert_eq!(resp.r#type, AgentType::Aionrs);
    assert!(repo.get_assistant_snapshot(&resp.id).await.unwrap().is_some());
}

#[tokio::test]
async fn create_derives_acp_type_from_assistant_backend_when_type_is_missing() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::new(),
    });
    let (svc, _broadcaster, repo, definition_repo, overlay_repo, _preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_acp_missing_type",
        "assistant-acp-missing-type",
        "codex",
        "auto",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_acp_missing_type",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let workspace = ensure_test_workspace_path();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "assistant": {
            "id": "assistant-acp-missing-type",
            "locale": "en-US"
        },
        "extra": {
            "workspace": workspace
        }
    }))
    .unwrap();

    let resp = svc.create("user_1", req).await.unwrap();
    assert_eq!(resp.r#type, AgentType::Acp);
    assert!(repo.get_assistant_snapshot(&resp.id).await.unwrap().is_some());
}

// ── Get tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn get_existing_conversation() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let created = svc.create("user_1", make_create_req()).await.unwrap();

    let fetched = svc.get("user_1", &created.id).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, created.name);
    assert!(fetched.runtime.is_some());
}

#[tokio::test]
async fn get_reports_idle_runtime_when_only_persisted_status_is_running() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let created = svc.create("user_1", make_create_req()).await.unwrap();
    repo.update(
        &created.id,
        &ConversationRowUpdate {
            status: Some("running".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let fetched = svc.get("user_1", &created.id).await.unwrap();
    let runtime = fetched.runtime.expect("runtime summary should be present");

    assert_eq!(fetched.status, ConversationStatus::Running);
    assert_eq!(runtime.state, aionui_api_types::ConversationRuntimeStateKind::Idle);
    assert!(runtime.can_send_message);
}

#[tokio::test]
async fn get_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let err = svc.get("user_1", "non-existent").await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── List tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn list_empty() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let result = svc.list("user_1", ListConversationsQuery::default()).await.unwrap();
    assert!(result.items.is_empty());
    assert_eq!(result.total, 0);
    assert!(!result.has_more);
}

#[tokio::test]
async fn list_returns_created_conversations() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    svc.create("user_1", make_create_req()).await.unwrap();
    svc.create("user_1", make_create_req()).await.unwrap();

    let result = svc.list("user_1", ListConversationsQuery::default()).await.unwrap();
    assert_eq!(result.items.len(), 2);
    assert_eq!(result.total, 2);
}

#[tokio::test]
async fn list_filters_by_user() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    svc.create("user_1", make_create_req()).await.unwrap();
    svc.create("user_2", make_create_req()).await.unwrap();

    let result = svc.list("user_1", ListConversationsQuery::default()).await.unwrap();
    assert_eq!(result.items.len(), 1);
}

#[tokio::test]
async fn list_with_source_filter() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    svc.create("user_1", make_create_req()).await.unwrap();

    let telegram_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "source": "telegram",
        "extra": {}
    }))
    .unwrap();
    svc.create("user_1", telegram_req).await.unwrap();

    let query = ListConversationsQuery {
        source: Some("telegram".into()),
        ..Default::default()
    };
    let result = svc.list("user_1", query).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].source, Some(ConversationSource::Telegram));
}

#[tokio::test]
async fn list_with_pinned_filter() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    svc.create("user_1", make_create_req()).await.unwrap();

    // Pin the first one
    let update_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    svc.update("user_1", &conv.id, update_req, &task_mgr).await.unwrap();

    let query = ListConversationsQuery {
        pinned: Some(true),
        ..Default::default()
    };
    let result = svc.list("user_1", query).await.unwrap();
    assert_eq!(result.items.len(), 1);
    assert!(result.items[0].pinned);
}

// ── Update tests ───────────────────────────────────────────────────

#[tokio::test]
async fn update_name() {
    let (svc, broadcaster, _repo, task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events(); // clear create event

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "New Name" })).unwrap();
    let updated = svc.update("user_1", &conv.id, req, &task_mgr).await.unwrap();

    assert_eq!(updated.name, "New Name");
    assert!(updated.modified_at >= conv.modified_at);

    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "updated");
}

#[tokio::test]
async fn update_pin() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    assert!(!conv.pinned);

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let updated = svc.update("user_1", &conv.id, req, &task_mgr).await.unwrap();
    assert!(updated.pinned);
    assert!(updated.pinned_at.is_some());
}

#[tokio::test]
async fn update_unpin_clears_pinned_at() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    // Pin first
    let pin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": true })).unwrap();
    let pinned = svc.update("user_1", &conv.id, pin_req, &task_mgr).await.unwrap();
    assert!(pinned.pinned);
    assert!(pinned.pinned_at.is_some());

    // Unpin
    let unpin_req: UpdateConversationRequest = serde_json::from_value(json!({ "pinned": false })).unwrap();
    let unpinned = svc.update("user_1", &conv.id, unpin_req, &task_mgr).await.unwrap();
    assert!(!unpinned.pinned);
    assert!(unpinned.pinned_at.is_none());
}

#[tokio::test]
async fn update_extra_merge() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let dir = std::env::temp_dir().join(format!(
        "aionui-conversation-update-extra-merge-{}",
        aionui_common::generate_short_id()
    ));
    let old_workspace = dir.join("old-workspace");
    let new_workspace = dir.join("new-workspace");
    std::fs::create_dir_all(&old_workspace).unwrap();
    std::fs::create_dir_all(&new_workspace).unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": old_workspace.to_string_lossy(), "contextFileName": "ctx.md" }
    }))
    .unwrap();
    let conv = svc.create("user_1", req).await.unwrap();

    // Update only workspace — contextFileName should be preserved
    let update_req: UpdateConversationRequest =
        serde_json::from_value(json!({ "extra": { "workspace": new_workspace.to_string_lossy() } })).unwrap();
    let updated = svc.update("user_1", &conv.id, update_req, &task_mgr).await.unwrap();

    assert_eq!(updated.extra["workspace"], new_workspace.to_string_lossy().to_string());
    assert_eq!(updated.extra["contextFileName"], "ctx.md");
}

#[tokio::test]
async fn update_model() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    // Top-level model updates are only valid on aionrs conversations
    // (Task 8 enforces the aionrs-only rule in update).
    let create_req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "aionrs",
        "model": { "provider_id": "p1", "model": "m1" },
        "extra": { "workspace": workspace }
    }))
    .unwrap();
    let conv = svc.create("user_1", create_req).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({
        "model": { "provider_id": "p2", "model": "new-model" }
    }))
    .unwrap();
    let updated = svc.update("user_1", &conv.id, req, &task_mgr).await.unwrap();

    let model = updated.model.unwrap();
    assert_eq!(model.provider_id, "p2");
    assert_eq!(model.model, "new-model");
}

#[tokio::test]
async fn update_not_found() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "x" })).unwrap();
    let err = svc.update("user_1", "non-existent", req, &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── Delete tests ───────────────────────────────────────────────────

#[tokio::test]
async fn delete_conversation() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events();

    svc.delete("user_1", &conv.id).await.unwrap();

    // Should be gone
    let err = svc.get("user_1", &conv.id).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));

    // Should broadcast deleted
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "deleted");
    assert_eq!(events[0].data["conversation_id"], conv.id);
}

#[tokio::test]
async fn delete_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let err = svc.delete("user_1", "non-existent").await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn delete_invokes_registered_hook() {
    use aionui_common::OnConversationDelete;

    struct RecordingHook(Mutex<Vec<String>>);
    #[async_trait::async_trait]
    impl OnConversationDelete for RecordingHook {
        async fn on_conversation_deleted(&self, conversation_id: &str) {
            self.0.lock().unwrap().push(conversation_id.to_owned());
        }
    }

    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let hook = Arc::new(RecordingHook(Mutex::new(vec![])));
    svc.with_delete_hook(hook.clone());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    svc.delete("user_1", &conv.id).await.unwrap();

    let calls = hook.0.lock().unwrap();
    assert_eq!(calls.as_slice(), &[conv.id]);
}

#[tokio::test]
async fn delete_invokes_registered_hook_before_row_delete() {
    use aionui_common::OnConversationDelete;

    struct RowVisibleHook {
        repo: Arc<MockRepo>,
        observations: Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl OnConversationDelete for RowVisibleHook {
        async fn on_conversation_deleted(&self, conversation_id: &str) {
            let exists = self.repo.get(conversation_id).await.unwrap().is_some();
            self.observations.lock().unwrap().push(exists);
        }
    }

    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let hook = Arc::new(RowVisibleHook {
        repo: repo.clone(),
        observations: Mutex::new(vec![]),
    });
    svc.with_delete_hook(hook.clone());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    svc.delete("user_1", &conv.id).await.unwrap();

    {
        let observations = hook.observations.lock().unwrap();
        assert_eq!(observations.as_slice(), &[true]);
    }
    assert!(repo.get(&conv.id).await.unwrap().is_none());
}

// ── Broadcast payload tests ────────────────────────────────────────

#[tokio::test]
async fn broadcast_includes_source_on_delete() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "source": "telegram",
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create("user_1", req).await.unwrap();
    broadcaster.take_events();

    svc.delete("user_1", &conv.id).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["source"], "telegram");
}

#[tokio::test]
async fn all_crud_operations_broadcast() {
    let (svc, broadcaster, _repo, task_mgr) = make_service();

    // Create
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "created");

    // Update
    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "x" })).unwrap();
    svc.update("user_1", &conv.id, req, &task_mgr).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "updated");

    // Delete
    svc.delete("user_1", &conv.id).await.unwrap();
    let events = broadcaster.take_events();
    assert_eq!(events[0].data["action"], "deleted");
}

// ── Ownership tests (M-3) ─────────────────────────────────────────

#[tokio::test]
async fn get_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let err = svc.get("user_2", &conv.id).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn update_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let req: UpdateConversationRequest = serde_json::from_value(json!({ "name": "hacked" })).unwrap();
    let err = svc.update("user_2", &conv.id, req, &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));

    // Original should be unchanged
    let original = svc.get("user_1", &conv.id).await.unwrap();
    assert_ne!(original.name, "hacked");
}

#[tokio::test]
async fn delete_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let err = svc.delete("user_2", &conv.id).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));

    // Should still exist
    let still_exists = svc.get("user_1", &conv.id).await.unwrap();
    assert_eq!(still_exists.id, conv.id);
}

// ── Clone tests ───────────────────────────────────────────────────

#[tokio::test]
async fn clone_without_source_creates_new() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    let req: CloneConversationRequest = serde_json::from_value(json!({
        "conversation": {
            "type": "acp",
            "name": "Cloned",
            "extra": { "workspace": workspace }
        }
    }))
    .unwrap();

    let resp = svc.clone_create("user_1", req).await.unwrap();
    assert_eq!(resp.name, "Cloned");
    assert_eq!(resp.extra["workspace"], workspace);

    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].data["action"], "created");
}

// ── Reset tests ───────────────────────────────────────────────────

#[tokio::test]
async fn reset_sets_status_to_pending() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    svc.reset("user_1", &conv.id).await.unwrap();

    let fetched = svc.get("user_1", &conv.id).await.unwrap();
    assert_eq!(fetched.status, ConversationStatus::Pending);
}

#[tokio::test]
async fn reset_clears_conversation_artifacts() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    repo.upsert_artifact(&ConversationArtifactRow {
        id: format!("{}:skill_suggest:cron_1", conv.id),
        conversation_id: conv.id.clone(),
        cron_job_id: Some("cron_1".into()),
        kind: "skill_suggest".into(),
        status: "pending".into(),
        payload: json!({ "cron_job_id": "cron_1", "name": "daily-report" }).to_string(),
        created_at: 1000,
        updated_at: 1000,
    })
    .await
    .unwrap();

    svc.reset("user_1", &conv.id).await.unwrap();

    let artifacts = repo.list_artifacts(&conv.id).await.unwrap();
    assert!(artifacts.is_empty());
}

#[tokio::test]
async fn list_artifacts_includes_legacy_cron_trigger_messages() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    repo.insert_message(&MessageRow {
        id: "legacy-msg-1".into(),
        conversation_id: conv.id.clone(),
        msg_id: Some("legacy-trigger-1".into()),
        r#type: "cron_trigger".into(),
        content: json!({
            "cron_job_id": "cron_1",
            "cron_job_name": "Daily Report",
            "triggered_at": 1234
        })
        .to_string(),
        position: Some("center".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1234,
    })
    .await
    .unwrap();

    let artifacts = svc.list_artifacts("user_1", &conv.id).await.unwrap();

    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].kind, ConversationArtifactKind::CronTrigger);
    assert_eq!(artifacts[0].payload["cron_job_id"], "cron_1");
    assert_eq!(artifacts[0].payload["cron_job_name"], "Daily Report");
}

#[tokio::test]
async fn reset_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let err = svc.reset("user_1", "no-such-id").await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn reset_wrong_user() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let err = svc.reset("user_2", &conv.id).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── Search validation tests ───────────────────────────────────────

#[tokio::test]
async fn search_messages_empty_keyword_returns_bad_request() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();

    let query = SearchMessagesQuery {
        keyword: "".into(),
        page: None,
        page_size: None,
    };
    let err = svc.search_messages("user_1", query).await.unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }));
}

#[tokio::test]
async fn search_messages_whitespace_keyword_returns_bad_request() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();

    let query = SearchMessagesQuery {
        keyword: "   ".into(),
        page: None,
        page_size: None,
    };
    let err = svc.search_messages("user_1", query).await.unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }));
}

// ── Mock Agent ───────────────────────────────────────────────────

struct MockAgent {
    conversation_id: String,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    stopped: Mutex<bool>,
    mode: Mutex<String>,
    model_id: Mutex<String>,
    config_options: Arc<Mutex<Vec<AcpConfigOptionDto>>>,
    set_config_option_calls: Arc<Mutex<Vec<(String, String)>>>,
    set_config_option_error: Arc<Mutex<Option<AgentError>>>,
    set_config_option_response: Arc<Mutex<Option<SetConfigOptionResponse>>>,
    confirmations: Mutex<Vec<Confirmation>>,
    approval_memory: Mutex<std::collections::HashMap<String, bool>>,
    allow_direct_confirm: bool,
    /// Optional workspace override; falls back to "/tmp/test" when `None`.
    workspace_override: Option<String>,
}

impl MockAgent {
    fn build_model_response(current: &str) -> GetModelInfoResponse {
        GetModelInfoResponse {
            model_info: Some(ModelInfoPayload {
                current_model_id: Some(current.to_owned()),
                current_model_label: Some(current.to_owned()),
                available_models: vec![
                    ModelInfoEntry {
                        id: "model-a".to_owned(),
                        label: "Model A".to_owned(),
                    },
                    ModelInfoEntry {
                        id: "model-b".to_owned(),
                        label: "Model B".to_owned(),
                    },
                ],
            }),
        }
    }

    fn new(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            mode: Mutex::new("default".to_owned()),
            model_id: Mutex::new("model-a".to_owned()),
            config_options: Arc::new(Mutex::new(Vec::new())),
            set_config_option_calls: Arc::new(Mutex::new(Vec::new())),
            set_config_option_error: Arc::new(Mutex::new(None)),
            set_config_option_response: Arc::new(Mutex::new(None)),
            confirmations: Mutex::new(vec![]),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: false,
            workspace_override: None,
        }
    }

    fn with_confirmations(conversation_id: &str, confirmations: Vec<Confirmation>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            mode: Mutex::new("default".to_owned()),
            model_id: Mutex::new("model-a".to_owned()),
            config_options: Arc::new(Mutex::new(Vec::new())),
            set_config_option_calls: Arc::new(Mutex::new(Vec::new())),
            set_config_option_error: Arc::new(Mutex::new(None)),
            set_config_option_response: Arc::new(Mutex::new(None)),
            confirmations: Mutex::new(confirmations),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: false,
            workspace_override: None,
        }
    }

    fn with_direct_confirm(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            mode: Mutex::new("default".to_owned()),
            model_id: Mutex::new("model-a".to_owned()),
            config_options: Arc::new(Mutex::new(Vec::new())),
            set_config_option_calls: Arc::new(Mutex::new(Vec::new())),
            set_config_option_error: Arc::new(Mutex::new(None)),
            set_config_option_response: Arc::new(Mutex::new(None)),
            confirmations: Mutex::new(vec![]),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: true,
            workspace_override: None,
        }
    }

    fn with_config_options(self, options: Vec<AcpConfigOptionDto>) -> Self {
        *self.config_options.lock().unwrap() = options;
        self
    }

    fn with_set_config_option_response(self, response: SetConfigOptionResponse) -> Self {
        *self.set_config_option_response.lock().unwrap() = Some(response);
        self
    }

    fn with_set_config_option_error(self, error: AgentError) -> Self {
        *self.set_config_option_error.lock().unwrap() = Some(error);
        self
    }
}

#[async_trait::async_trait]
impl IAgentTask for MockAgent {
    fn agent_type(&self) -> AgentType {
        AgentType::Acp
    }
    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }
    fn workspace(&self) -> &str {
        self.workspace_override.as_deref().unwrap_or("/tmp/test")
    }
    fn status(&self) -> Option<ConversationStatus> {
        None
    }
    fn last_activity_at(&self) -> TimestampMs {
        0
    }
    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }
    async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
        // Emit finish event so the relay task completes
        let _ = self.event_tx.send(AgentStreamEvent::Finish(
            aionui_ai_agent::protocol::events::FinishEventData::default(),
        ));
        Ok(())
    }
    async fn cancel(&self) -> Result<(), AgentError> {
        *self.stopped.lock().unwrap() = true;
        Ok(())
    }
    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl IMockAgent for MockAgent {
    fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.lock().unwrap().clone()
    }
    fn check_approval(&self, action: &str, command_type: Option<&str>) -> bool {
        let key = match command_type {
            Some(ct) => format!("{action}:{ct}"),
            None => action.to_owned(),
        };
        self.approval_memory.lock().unwrap().get(&key).copied().unwrap_or(false)
    }
    fn confirm(
        &self,
        _msg_id: &str,
        call_id: &str,
        _data: serde_json::Value,
        always_allow: bool,
    ) -> Result<(), AgentError> {
        let mut confs = self.confirmations.lock().unwrap();
        let existed = confs.iter().any(|c| c.call_id == call_id);
        if !existed && !self.allow_direct_confirm {
            return Err(AgentError::not_found(format!("Confirmation {call_id} not found")));
        }
        if always_allow && let Some(conf) = confs.iter().find(|c| c.call_id == call_id) {
            let key = match (conf.action.as_deref(), conf.command_type.as_deref()) {
                (Some(a), Some(ct)) => format!("{a}:{ct}"),
                (Some(a), None) => a.to_owned(),
                _ => String::new(),
            };
            self.approval_memory.lock().unwrap().insert(key, true);
        }
        confs.retain(|c| c.call_id != call_id);
        Ok(())
    }

    async fn mode(&self) -> Result<AgentModeResponse, AgentError> {
        Ok(AgentModeResponse {
            mode: self.mode.lock().unwrap().clone(),
            initialized: true,
        })
    }

    async fn get_model(&self) -> Result<GetModelInfoResponse, AgentError> {
        let current = self.model_id.lock().unwrap().clone();
        Ok(Self::build_model_response(&current))
    }

    async fn get_config_options(&self) -> Result<GetConfigOptionsResponse, AgentError> {
        Ok(GetConfigOptionsResponse {
            config_options: self.config_options.lock().unwrap().clone(),
        })
    }

    async fn set_config_option(&self, option_id: &str, value: &str) -> Result<SetConfigOptionResponse, AgentError> {
        self.set_config_option_calls
            .lock()
            .unwrap()
            .push((option_id.to_owned(), value.to_owned()));
        if let Some(error) = self.set_config_option_error.lock().unwrap().take() {
            return Err(error);
        }
        if let Some(response) = self.set_config_option_response.lock().unwrap().clone() {
            return Ok(response);
        }
        Ok(SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::Observed,
            config_options: Some(self.config_options.lock().unwrap().clone()),
        })
    }
}

struct BlockingCancelAgent {
    conversation_id: String,
    agent_type: AgentType,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    send_started: Notify,
    finish_notify: Notify,
    cancel_count: AtomicUsize,
    cancel_error: bool,
}

impl BlockingCancelAgent {
    fn new(conversation_id: &str) -> Self {
        Self::new_with_type(conversation_id, AgentType::Acp)
    }

    fn new_with_type(conversation_id: &str, agent_type: AgentType) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            agent_type,
            event_tx,
            send_started: Notify::new(),
            finish_notify: Notify::new(),
            cancel_count: AtomicUsize::new(0),
            cancel_error: false,
        }
    }

    fn new_with_cancel_error(conversation_id: &str) -> Self {
        let mut agent = Self::new(conversation_id);
        agent.cancel_error = true;
        agent
    }

    async fn wait_until_send_started(&self) {
        self.send_started.notified().await;
    }

    fn release_finish(&self) {
        self.finish_notify.notify_waiters();
    }
}

#[async_trait::async_trait]
impl IAgentTask for BlockingCancelAgent {
    fn agent_type(&self) -> AgentType {
        self.agent_type
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        "/tmp/test"
    }

    fn status(&self) -> Option<ConversationStatus> {
        Some(ConversationStatus::Running)
    }

    fn last_activity_at(&self) -> TimestampMs {
        0
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
        self.send_started.notify_waiters();
        self.finish_notify.notified().await;
        let _ = self.event_tx.send(AgentStreamEvent::Finish(FinishEventData::default()));
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.cancel_count.fetch_add(1, Ordering::SeqCst);
        if self.cancel_error {
            return Err(AgentError::bad_gateway("cancel failed"));
        }
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

impl IMockAgent for BlockingCancelAgent {}

// ── Mock WorkerTaskManager ──────────────────────────────────────

struct MockTaskManager {
    agents: Mutex<std::collections::HashMap<String, AgentInstance>>,
    kill_records: Mutex<Vec<(String, Option<AgentKillReason>)>>,
    kill_count: AtomicUsize,
}

impl MockTaskManager {
    fn new() -> Self {
        Self {
            agents: Mutex::new(std::collections::HashMap::new()),
            kill_records: Mutex::new(Vec::new()),
            kill_count: AtomicUsize::new(0),
        }
    }

    fn insert_agent(&self, conversation_id: &str, agent: AgentInstance) {
        self.agents.lock().unwrap().insert(conversation_id.to_owned(), agent);
    }

    fn kill_count(&self) -> usize {
        self.kill_count.load(Ordering::SeqCst)
    }

    fn kill_records(&self) -> Vec<(String, Option<AgentKillReason>)> {
        self.kill_records.lock().unwrap().clone()
    }
}

struct RebuildingScriptedTaskManager {
    agents: Mutex<VecDeque<AgentInstance>>,
    build_count: AtomicUsize,
    kill_count: AtomicUsize,
    captured_options: Mutex<Vec<BuildTaskOptions>>,
}

impl RebuildingScriptedTaskManager {
    fn new(agents: Vec<AgentInstance>) -> Self {
        Self {
            agents: Mutex::new(agents.into()),
            build_count: AtomicUsize::new(0),
            kill_count: AtomicUsize::new(0),
            captured_options: Mutex::new(Vec::new()),
        }
    }

    fn build_count(&self) -> usize {
        self.build_count.load(Ordering::SeqCst)
    }

    fn kill_count(&self) -> usize {
        self.kill_count.load(Ordering::SeqCst)
    }

    fn captured_options(&self) -> Vec<BuildTaskOptions> {
        self.captured_options.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for RebuildingScriptedTaskManager {
    fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
        None
    }

    async fn get_or_build_task(
        &self,
        _conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        self.build_count.fetch_add(1, Ordering::SeqCst);
        self.captured_options.lock().unwrap().push(options);
        self.agents
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| AgentError::bad_gateway("no scripted agent left"))
    }

    fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.kill_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        0
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        Vec::new()
    }
}

struct FailingBuildTaskManager {
    error: String,
}

impl FailingBuildTaskManager {
    fn new(error: impl Into<String>) -> Self {
        Self { error: error.into() }
    }
}

struct AgentErrorFailingBuildTaskManager {
    error: Mutex<Option<AgentError>>,
}

impl AgentErrorFailingBuildTaskManager {
    fn new(error: AgentError) -> Self {
        Self {
            error: Mutex::new(Some(error)),
        }
    }
}

struct DelayedFailingBuildTaskManager {
    delay: Duration,
    error: String,
}

impl DelayedFailingBuildTaskManager {
    fn new(delay: Duration, error: impl Into<String>) -> Self {
        Self {
            delay,
            error: error.into(),
        }
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for AgentErrorFailingBuildTaskManager {
    fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
        None
    }

    async fn get_or_build_task(
        &self,
        _conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        Err(self
            .error
            .lock()
            .unwrap()
            .take()
            .expect("test build error should be consumed once"))
    }

    fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        0
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for DelayedFailingBuildTaskManager {
    fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
        None
    }

    async fn get_or_build_task(
        &self,
        _conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        tokio::time::sleep(self.delay).await;
        Err(AgentError::bad_gateway(self.error.clone()))
    }

    fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        0
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for FailingBuildTaskManager {
    fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
        None
    }

    async fn get_or_build_task(
        &self,
        _conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        Err(AgentError::bad_gateway(self.error.clone()))
    }

    fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        0
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for MockTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let instance = AgentInstance::Mock(Arc::new(MockAgent::new(conversation_id)));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.kill_count.fetch_add(1, Ordering::SeqCst);
        self.kill_records
            .lock()
            .unwrap()
            .push((conversation_id.to_owned(), _reason));
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct SlowBuildTaskManager {
    delay: Duration,
    built: AtomicBool,
}

impl SlowBuildTaskManager {
    fn new(delay: Duration) -> Self {
        Self {
            delay,
            built: AtomicBool::new(false),
        }
    }

    fn was_built(&self) -> bool {
        self.built.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for SlowBuildTaskManager {
    fn get_task(&self, _conversation_id: &str) -> Option<AgentInstance> {
        None
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        tokio::time::sleep(self.delay).await;
        self.built.store(true, Ordering::SeqCst);
        Ok(AgentInstance::Mock(Arc::new(MockAgent::new(conversation_id))))
    }

    fn kill(&self, _conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill_and_wait(
        &self,
        _conversation_id: &str,
        _reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {}

    fn active_count(&self) -> usize {
        usize::from(self.was_built())
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

/// A variant of MockTaskManager that always builds agents with a specific workspace.
struct MockTaskManagerWithWorkspace {
    workspace: String,
    agents: Mutex<std::collections::HashMap<String, AgentInstance>>,
}

impl MockTaskManagerWithWorkspace {
    fn new(workspace: &str) -> Self {
        Self {
            workspace: workspace.to_owned(),
            agents: Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for MockTaskManagerWithWorkspace {
    fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
        self.agents.lock().unwrap().get(conversation_id).cloned()
    }

    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        _options: BuildTaskOptions,
    ) -> Result<AgentInstance, AgentError> {
        let workspace = self.workspace.clone();
        let mut agents = self.agents.lock().unwrap();
        if let Some(existing) = agents.get(conversation_id) {
            return Ok(existing.clone());
        }
        let mut agent = MockAgent::new(conversation_id);
        agent.workspace_override = Some(workspace);
        let instance = AgentInstance::Mock(Arc::new(agent));
        agents.insert(conversation_id.to_owned(), instance.clone());
        Ok(instance)
    }

    fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.agents.lock().unwrap().remove(conversation_id);
        Ok(())
    }

    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
    }

    async fn clear(&self) {
        self.agents.lock().unwrap().clear();
    }

    fn active_count(&self) -> usize {
        self.agents.lock().unwrap().len()
    }

    fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct ScriptedAgent {
    conversation_id: String,
    agent_type: AgentType,
    status: Option<ConversationStatus>,
    event_tx: broadcast::Sender<AgentStreamEvent>,
    scripts: Mutex<VecDeque<Vec<AgentStreamEvent>>>,
    sent_contents: Mutex<Vec<String>>,
    send_error: Option<AgentSendError>,
}

impl ScriptedAgent {
    fn new(conversation_id: &str, scripts: Vec<Vec<AgentStreamEvent>>) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            agent_type: AgentType::Acp,
            status: Some(ConversationStatus::Finished),
            event_tx,
            scripts: Mutex::new(VecDeque::from(scripts)),
            sent_contents: Mutex::new(vec![]),
            send_error: None,
        }
    }

    fn with_agent_type(mut self, agent_type: AgentType) -> Self {
        self.agent_type = agent_type;
        self
    }

    fn with_status(mut self, status: Option<ConversationStatus>) -> Self {
        self.status = status;
        self
    }

    fn with_send_error(mut self, error: AgentSendError) -> Self {
        self.send_error = Some(error);
        self
    }

    fn sent_contents(&self) -> Vec<String> {
        self.sent_contents.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl IAgentTask for ScriptedAgent {
    fn agent_type(&self) -> AgentType {
        self.agent_type
    }

    fn conversation_id(&self) -> &str {
        &self.conversation_id
    }

    fn workspace(&self) -> &str {
        "/tmp/test"
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.status
    }

    fn last_activity_at(&self) -> TimestampMs {
        0
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.event_tx.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        self.sent_contents.lock().unwrap().push(data.content);
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| vec![AgentStreamEvent::Finish(FinishEventData::default())]);
        for event in script {
            let _ = self.event_tx.send(event);
        }
        if let Some(error) = &self.send_error {
            return Err(error.clone());
        }
        Ok(())
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        Ok(())
    }

    fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
}

impl IMockAgent for ScriptedAgent {}

struct MockCronContinuationService;

#[async_trait::async_trait]
impl ICronService for MockCronContinuationService {
    async fn create_job(&self, _user_id: &str, _conversation_id: &str, params: &CronCreateParams) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: format!("Created cron job '{}'", params.name),
        }
    }

    async fn update_job(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _params: &CronUpdateParams,
    ) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: "Updated cron job".into(),
        }
    }

    async fn list_jobs(&self, _user_id: &str, _conversation_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: "No scheduled tasks".into(),
        }
    }

    async fn delete_job(&self, _user_id: &str, _job_id: &str) -> CronCommandResult {
        CronCommandResult {
            success: true,
            message: "Deleted cron job".into(),
        }
    }
}

// ── send_message tests ──────────────────────────────────────────

fn make_send_req() -> SendMessageRequest {
    serde_json::from_value(json!({
        "content": "Hello"
    }))
    .unwrap()
}

async fn wait_for_turn_released(svc: &ConversationService, conversation_id: &str) {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !svc.runtime_state().is_claimed(conversation_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn should release runtime claim");
}

#[tokio::test]
async fn send_message_returns_accepted() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let response = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    assert!(!response.msg_id.is_empty(), "msg_id must be non-empty");
    assert_eq!(response.msg_id.len(), 8, "msg_id should be an 8-char short hex ID");
    assert!(response.turn_id.starts_with("turn_"), "turn_id must use turn_ prefix");
    assert_ne!(response.msg_id, response.turn_id, "turn_id must not reuse msg_id");
}

#[tokio::test]
async fn send_message_returns_msg_id_and_turn_id_and_summary_tracks_turn() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let slow_task_mgr = Arc::new(SlowBuildTaskManager::new(Duration::from_millis(500)));
    let task_mgr: Arc<dyn IWorkerTaskManager> = slow_task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let response = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    assert!(!response.msg_id.is_empty(), "msg_id must be non-empty");
    assert!(response.turn_id.starts_with("turn_"), "turn_id must use turn_ prefix");
    assert_ne!(response.msg_id, response.turn_id, "turn_id must not reuse msg_id");
    assert_eq!(
        response.runtime.turn_id.as_deref(),
        Some(response.turn_id.as_str()),
        "send response runtime must identify the accepted turn"
    );
    assert!(response.runtime.is_processing);
    assert!(!response.runtime.can_send_message);

    let runtime = svc.runtime_summary_for(&conv.id).await;
    assert_eq!(response.runtime, runtime);
    assert_eq!(runtime.turn_id.as_deref(), Some(response.turn_id.as_str()));
    assert!(runtime.is_processing);
    assert!(!runtime.can_send_message);
}

#[tokio::test]
async fn send_message_rejects_legacy_runtime_conversations_as_archived() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    for agent_type in [
        AgentType::Gemini,
        AgentType::Codex,
        AgentType::OpenclawGateway,
        AgentType::Nanobot,
        AgentType::Remote,
    ] {
        let conv = insert_conversation_with_type(&repo, "user_1", agent_type).await;

        let err = svc
            .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
            .await
            .unwrap_err();

        assert_eq!(err.error_code(), "CONVERSATION_ARCHIVED");
        assert!(
            err.to_string()
                .contains("This historical conversation can no longer be continued. Please start a new conversation."),
            "unexpected archived message for {}: {err}",
            agent_type.serde_name()
        );
    }
}

#[tokio::test]
async fn get_config_options_returns_active_agent_snapshot() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = MockAgent::new(&conv.id).with_config_options(vec![AcpConfigOptionDto {
        id: "model".to_owned(),
        name: Some("Model".to_owned()),
        label: None,
        description: None,
        category: Some("model".to_owned()),
        option_type: "select".to_owned(),
        current_value: Some("gpt-5.5".to_owned()),
        options: Vec::new(),
    }]);
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(Arc::new(agent)));

    let result = svc.get_config_options(&conv.id).await.unwrap();

    assert_eq!(result.config_options[0].id, "model");
    assert_eq!(result.config_options[0].current_value.as_deref(), Some("gpt-5.5"));
}

#[tokio::test]
async fn set_config_option_returns_observed_confirmation() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = Arc::new(MockAgent::new(&conv.id).with_config_options(vec![AcpConfigOptionDto {
        id: "reasoning_effort".to_owned(),
        name: Some("Reasoning Effort".to_owned()),
        label: None,
        description: None,
        category: Some("thought_level".to_owned()),
        option_type: "select".to_owned(),
        current_value: Some("high".to_owned()),
        options: Vec::new(),
    }]));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent.clone()));

    let result = svc
        .set_config_option(
            &conv.id,
            "reasoning_effort",
            SetConfigOptionRequest {
                value: "high".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(result.confirmation, ConfigOptionConfirmation::Observed);
    assert_eq!(
        agent.set_config_option_calls.lock().unwrap().as_slice(),
        &[("reasoning_effort".to_owned(), "high".to_owned())]
    );
}

#[tokio::test]
async fn set_config_option_evicts_task_when_acp_protocol_is_not_connected() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent =
        Arc::new(MockAgent::new(&conv.id).with_set_config_option_error(AgentError::Acp(AcpError::NotConnected)));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    let err = svc
        .set_config_option(
            &conv.id,
            "model",
            SetConfigOptionRequest {
                value: "gpt-5".to_owned(),
            },
        )
        .await
        .expect_err("set_config_option must surface ACP NotConnected");

    assert!(
        matches!(err, ConversationError::Acp(AcpError::NotConnected)),
        "expected ACP NotConnected, got {err:?}"
    );
    assert_eq!(
        task_mgr.kill_records(),
        vec![(conv.id.clone(), Some(AgentKillReason::AgentErrorRecovery))]
    );
}

#[tokio::test]
async fn command_ack_does_not_persist_assistant_preference_in_core_service() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = Arc::new(
        MockAgent::new(&conv.id).with_set_config_option_response(SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::CommandAck,
            config_options: None,
        }),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    let result = svc
        .set_config_option(
            &conv.id,
            "model",
            SetConfigOptionRequest {
                value: "gpt-5.5".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(result.confirmation, ConfigOptionConfirmation::CommandAck);
    let refreshed = svc.get("user_1", &conv.id).await.unwrap();
    assert!(refreshed.extra.get("current_model_id").is_none());
}

#[tokio::test]
async fn set_config_option_persists_runtime_model_into_assistant_preference_when_observed() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, repo, definition_repo, overlay_repo, preference_repo) =
        make_service_with_mock_task_manager_and_assistant_support(task_mgr.clone()).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_acp_auto",
        "assistant-acp-auto",
        "codex",
        "auto",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_acp_auto",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_acp_auto",
            last_model_id: Some("legacy-acp-model"),
            last_permission_value: Some("legacy-mode"),
            last_skill_ids: "[]",
            last_disabled_builtin_skill_ids: "[]",
            last_mcp_ids: "[]",
        })
        .await
        .unwrap();

    let conv = create_assistant_backed_conversation(&svc, "user_1", Some("acp"), "codex", "assistant-acp-auto").await;

    let agent = Arc::new(MockAgent::new(&conv.id));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    let result = svc
        .set_config_option(
            &conv.id,
            "model",
            SetConfigOptionRequest {
                value: "gpt-5.5".to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(result.confirmation, ConfigOptionConfirmation::Observed);

    let pref_after_model = preference_repo.get("asstdef_acp_auto").await.unwrap().unwrap();
    assert_eq!(pref_after_model.last_model_id.as_deref(), Some("gpt-5.5"));
    assert_eq!(pref_after_model.last_permission_value.as_deref(), Some("legacy-mode"));
    let snapshot_after_model = repo.get_assistant_snapshot(&conv.id).await.unwrap().unwrap();
    assert_eq!(snapshot_after_model.resolved_model_id.as_deref(), Some("gpt-5.5"));

    svc.set_config_option(
        &conv.id,
        "mode",
        SetConfigOptionRequest {
            value: "plan".to_owned(),
        },
    )
    .await
    .unwrap();
    let pref_after_mode = preference_repo.get("asstdef_acp_auto").await.unwrap().unwrap();
    assert_eq!(pref_after_mode.last_model_id.as_deref(), Some("gpt-5.5"));
    assert_eq!(pref_after_mode.last_permission_value.as_deref(), Some("plan"));
    let snapshot_after_mode = repo.get_assistant_snapshot(&conv.id).await.unwrap().unwrap();
    assert_eq!(snapshot_after_mode.resolved_permission_value.as_deref(), Some("plan"));

    // Unrelated option ids must not touch preferences.
    svc.set_config_option(
        &conv.id,
        "thought_level",
        SetConfigOptionRequest {
            value: "high".to_owned(),
        },
    )
    .await
    .unwrap();
    let pref_after_thought = preference_repo.get("asstdef_acp_auto").await.unwrap().unwrap();
    assert_eq!(pref_after_thought.last_model_id.as_deref(), Some("gpt-5.5"));
    assert_eq!(pref_after_thought.last_permission_value.as_deref(), Some("plan"));
}

#[tokio::test]
async fn set_config_option_skips_preference_write_back_when_default_mode_is_fixed() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, repo, definition_repo, overlay_repo, preference_repo) =
        make_service_with_mock_task_manager_and_assistant_support(task_mgr.clone()).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_acp_fixed",
        "assistant-acp-fixed",
        "codex",
        "fixed",
        "fixed",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_acp_fixed",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_acp_fixed",
            last_model_id: Some("legacy-fixed-model"),
            last_permission_value: Some("legacy-fixed-mode"),
            last_skill_ids: "[]",
            last_disabled_builtin_skill_ids: "[]",
            last_mcp_ids: "[]",
        })
        .await
        .unwrap();

    let conv = create_assistant_backed_conversation(&svc, "user_1", Some("acp"), "codex", "assistant-acp-fixed").await;
    let agent = Arc::new(MockAgent::new(&conv.id));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    svc.set_config_option(
        &conv.id,
        "model",
        SetConfigOptionRequest {
            value: "transient-model".to_owned(),
        },
    )
    .await
    .unwrap();
    svc.set_config_option(
        &conv.id,
        "mode",
        SetConfigOptionRequest {
            value: "transient-mode".to_owned(),
        },
    )
    .await
    .unwrap();

    let pref = preference_repo.get("asstdef_acp_fixed").await.unwrap().unwrap();
    assert_eq!(pref.last_model_id.as_deref(), Some("legacy-fixed-model"));
    assert_eq!(pref.last_permission_value.as_deref(), Some("legacy-fixed-mode"));
    // The snapshot still tracks the runtime override so the active session reflects it,
    // even though the persisted assistant preference must not change for fixed defaults.
    let snapshot = repo.get_assistant_snapshot(&conv.id).await.unwrap().unwrap();
    assert_eq!(snapshot.resolved_model_id.as_deref(), Some("transient-model"));
    assert_eq!(snapshot.resolved_permission_value.as_deref(), Some("transient-mode"));
}

#[tokio::test]
async fn set_config_option_command_ack_does_not_persist_assistant_preference() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, repo, definition_repo, overlay_repo, preference_repo) =
        make_service_with_mock_task_manager_and_assistant_support(task_mgr.clone()).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_acp_ack",
        "assistant-acp-ack",
        "codex",
        "auto",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_acp_ack",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_acp_ack",
            last_model_id: Some("legacy-ack-model"),
            last_permission_value: Some("legacy-ack-mode"),
            last_skill_ids: "[]",
            last_disabled_builtin_skill_ids: "[]",
            last_mcp_ids: "[]",
        })
        .await
        .unwrap();

    let conv = create_assistant_backed_conversation(&svc, "user_1", Some("acp"), "codex", "assistant-acp-ack").await;
    let agent = Arc::new(
        MockAgent::new(&conv.id).with_set_config_option_response(SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::CommandAck,
            config_options: None,
        }),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    svc.set_config_option(
        &conv.id,
        "model",
        SetConfigOptionRequest {
            value: "ack-only-model".to_owned(),
        },
    )
    .await
    .unwrap();

    let pref = preference_repo.get("asstdef_acp_ack").await.unwrap().unwrap();
    assert_eq!(pref.last_model_id.as_deref(), Some("legacy-ack-model"));
    assert_eq!(pref.last_permission_value.as_deref(), Some("legacy-ack-mode"));
    let snapshot = repo.get_assistant_snapshot(&conv.id).await.unwrap().unwrap();
    assert_eq!(snapshot.resolved_model_id.as_deref(), Some("legacy-ack-model"));
}

#[tokio::test]
async fn update_aionrs_model_updates_assistant_preference_only_when_snapshot_model_mode_is_auto() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, repo, definition_repo, overlay_repo, preference_repo) =
        make_service_with_mock_task_manager_and_assistant_support(task_mgr.clone()).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_aionrs_auto",
        "assistant-aionrs-auto",
        "aionrs",
        "auto",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_aionrs_auto",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_aionrs_auto",
            last_model_id: Some("legacy-aionrs-model"),
            last_permission_value: None,
            last_skill_ids: "[]",
            last_disabled_builtin_skill_ids: "[]",
            last_mcp_ids: "[]",
        })
        .await
        .unwrap();

    let auto_conv =
        create_assistant_backed_conversation(&svc, "user_1", Some("aionrs"), "aionrs", "assistant-aionrs-auto").await;
    let updated = svc
        .update(
            "user_1",
            &auto_conv.id,
            UpdateConversationRequest {
                model: Some(ProviderWithModel {
                    provider_id: "provider-2".to_owned(),
                    model: "model-z".to_owned(),
                    use_model: Some("model-z".to_owned()),
                }),
                name: None,
                extra: None,
                pinned: None,
            },
            &(task_mgr.clone() as Arc<dyn IWorkerTaskManager>),
        )
        .await
        .unwrap();

    assert_eq!(
        updated.model.as_ref().and_then(|model| model.use_model.as_deref()),
        Some("model-z")
    );
    let auto_pref = preference_repo.get("asstdef_aionrs_auto").await.unwrap().unwrap();
    assert_eq!(auto_pref.last_model_id.as_deref(), Some("model-z"));
    let auto_snapshot = repo.get_assistant_snapshot(&auto_conv.id).await.unwrap().unwrap();
    assert_eq!(auto_snapshot.resolved_model_id.as_deref(), Some("model-z"));

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_aionrs_fixed",
        "assistant-aionrs-fixed",
        "aionrs",
        "fixed",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_aionrs_fixed",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_aionrs_fixed",
            last_model_id: Some("legacy-aionrs-fixed-model"),
            last_permission_value: None,
            last_skill_ids: "[]",
            last_disabled_builtin_skill_ids: "[]",
            last_mcp_ids: "[]",
        })
        .await
        .unwrap();

    let fixed_conv =
        create_assistant_backed_conversation(&svc, "user_1", Some("aionrs"), "aionrs", "assistant-aionrs-fixed").await;
    let _ = svc
        .update(
            "user_1",
            &fixed_conv.id,
            UpdateConversationRequest {
                model: Some(ProviderWithModel {
                    provider_id: "provider-3".to_owned(),
                    model: "model-y".to_owned(),
                    use_model: Some("model-y".to_owned()),
                }),
                name: None,
                extra: None,
                pinned: None,
            },
            &(task_mgr as Arc<dyn IWorkerTaskManager>),
        )
        .await
        .unwrap();

    let fixed_pref = preference_repo.get("asstdef_aionrs_fixed").await.unwrap().unwrap();
    assert_eq!(fixed_pref.last_model_id.as_deref(), Some("legacy-aionrs-fixed-model"));
    let fixed_snapshot = repo.get_assistant_snapshot(&fixed_conv.id).await.unwrap().unwrap();
    assert_eq!(fixed_snapshot.resolved_model_id.as_deref(), Some("model-y"));

    // Ensure the update path used the repository row, not a no-op.
    let updated_row = repo.get(&fixed_conv.id).await.unwrap().unwrap();
    assert!(
        updated_row
            .model
            .as_deref()
            .is_some_and(|model| model.contains("model-y"))
    );
}

#[tokio::test]
async fn send_message_missing_workspace_persists_message_and_failure_tip() {
    let (svc, broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events();
    let legacy_workspace = format!("/tmp/does-not-exist-{}", aionui_common::generate_short_id());
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": legacy_workspace }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let response = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();
    assert!(
        !response.msg_id.is_empty(),
        "msg_id must still be returned when runtime workspace validation fails"
    );

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo
                .list_messages_page(
                    &conv.id,
                    &MessagePageParams {
                        limit: 20,
                        direction: MessagePageDirection::InitialLatest,
                    },
                )
                .await
                .unwrap()
                .items;
            if messages.iter().any(|message| message.r#type == "tips")
                && messages.iter().any(|message| message.r#type == "text")
            {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("missing workspace failure should persist a user message and error tip");

    let user_message = messages
        .iter()
        .find(|message| message.r#type == "text")
        .expect("missing workspace failure should persist the user message");
    assert_eq!(user_message.msg_id.as_deref(), Some(response.msg_id.as_str()));

    let error_tip = messages
        .iter()
        .find(|message| message.r#type == "tips")
        .expect("missing workspace failure should persist an error tips message");
    let content: serde_json::Value = serde_json::from_str(&error_tip.content).unwrap();
    assert_eq!(content["code"], "WORKSPACE_PATH_RUNTIME_UNAVAILABLE");
    assert_eq!(content["details"]["workspace_path"], legacy_workspace);
    assert_eq!(content["error"]["code"], "WORKSPACE_PATH_RUNTIME_UNAVAILABLE");
    assert_eq!(content["error"]["workspacePath"], legacy_workspace);

    let events = broadcaster.take_events();
    let error_tip_event = events
        .iter()
        .find(|event| event.name == "message.stream" && event.data["type"] == "tips")
        .expect("missing workspace failure should broadcast the error tips message");
    assert_eq!(error_tip_event.data["status"], "error");
    assert_eq!(
        error_tip_event.data["data"]["code"],
        "WORKSPACE_PATH_RUNTIME_UNAVAILABLE"
    );
    assert_eq!(error_tip_event.data["turn_id"], response.turn_id);

    let turn_event = events
        .iter()
        .find(|event| event.name == "turn.completed")
        .expect("missing workspace failure should complete the turn");
    assert_eq!(turn_event.data["turn_id"], response.turn_id);
    assert_eq!(turn_event.data["runtime"]["is_processing"], false);
    assert_eq!(turn_event.data["runtime"]["can_send_message"], true);
}

#[tokio::test]
async fn send_message_broadcasts_user_created_event() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    // Clear events from create
    broadcaster.take_events();

    let response = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");

    assert_eq!(user_created.data["conversation_id"], conv.id);
    assert_eq!(user_created.data["msg_id"], response.msg_id);
    assert_eq!(user_created.data["content"], "Hello");
    assert_eq!(user_created.data["position"], "right");
}

#[tokio::test]
async fn send_message_returns_before_cold_agent_build_completes() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let slow_task_mgr = Arc::new(SlowBuildTaskManager::new(Duration::from_millis(500)));
    let task_mgr: Arc<dyn IWorkerTaskManager> = slow_task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let response = tokio::time::timeout(
        Duration::from_millis(50),
        svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr),
    )
    .await
    .expect("send_message should return before cold agent build finishes")
    .unwrap();

    assert!(!response.msg_id.is_empty(), "msg_id must be non-empty");
    assert!(response.turn_id.starts_with("turn_"));
    assert!(
        !slow_task_mgr.was_built(),
        "cold agent build should continue in the background after send_message returns"
    );

    let updated = repo.get(&conv.id).await.unwrap().unwrap();
    assert_ne!(updated.status.as_deref(), Some("running"));
    assert!(
        svc.runtime_state().is_claimed(&conv.id),
        "runtime claim must cover the cold agent build window"
    );
}

#[tokio::test]
async fn send_message_persists_hidden_user_message_when_requested() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Hidden cron prompt",
        "hidden": true
    }))
    .unwrap();

    svc.send_message("user_1", &conv.id, req, &task_mgr).await.unwrap();

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    // The user message is the only hidden text row written by the service.
    let user_message = messages
        .iter()
        .find(|message| message.r#type == "text" && message.position.as_deref() == Some("right"))
        .expect("user message should be persisted");
    assert!(user_message.hidden);
    // msg_id is server-generated and must be non-empty for frontend routing.
    assert!(user_message.msg_id.as_deref().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn send_message_persists_error_tip_when_agent_build_fails() {
    let (svc, broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> =
        Arc::new(FailingBuildTaskManager::new("ACP init failed: config file is invalid"));

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events();

    let response = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    assert!(!response.msg_id.is_empty(), "msg_id must be non-empty");
    assert!(response.turn_id.starts_with("turn_"));

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo
                .list_messages_page(
                    &conv.id,
                    &MessagePageParams {
                        limit: 20,
                        direction: MessagePageDirection::InitialLatest,
                    },
                )
                .await
                .unwrap()
                .items;
            if messages.iter().any(|message| message.r#type == "tips") {
                return messages;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("agent build failure should persist an error tip");
    assert_eq!(messages.len(), 2, "user message and error tip should be persisted");

    let error_tip = messages
        .iter()
        .find(|message| message.r#type == "tips")
        .expect("agent build failure should persist an error tips message");
    assert_eq!(error_tip.status.as_deref(), Some("error"));
    assert_eq!(error_tip.position.as_deref(), Some("center"));

    let content: serde_json::Value = serde_json::from_str(&error_tip.content).unwrap();
    assert_eq!(content["type"], "error");
    assert_eq!(content["source"], "send_failed");
    assert_eq!(content["code"], "BAD_GATEWAY");
    assert_eq!(content["error"]["code"], "UNKNOWN_UPSTREAM_ERROR");
    assert_eq!(content["error"]["ownership"], "unknown_upstream");
    assert_eq!(content["error"]["retryable"], true);
    assert_eq!(content["error"]["feedback_recommended"], false);
    assert_eq!(content["error"]["detail"], "ACP init failed: config file is invalid");
    assert_eq!(
        content["content"],
        "The upstream Agent failed while handling the request"
    );

    let updated = repo.get(&conv.id).await.unwrap().unwrap();
    assert_eq!(updated.status.as_deref(), Some("finished"));
    assert!(
        !svc.runtime_state().is_claimed(&conv.id),
        "runtime claim must be released after failed turn"
    );

    let events = broadcaster.take_events();
    let error_tip_event = events
        .iter()
        .find(|event| event.name == "message.stream" && event.data["type"] == "tips")
        .expect("agent build failure should broadcast the error tips message");
    assert_eq!(error_tip_event.data["status"], "error");
    assert_eq!(error_tip_event.data["data"]["code"], "BAD_GATEWAY");
    assert_eq!(error_tip_event.data["turn_id"], response.turn_id);
}

#[tokio::test]
async fn run_agent_turn_returns_error_message_when_agent_build_fails() {
    let task_mgr: Arc<dyn IWorkerTaskManager> =
        Arc::new(FailingBuildTaskManager::new("ACP init failed: config file is invalid"));
    let repo = Arc::new(MockRepo::new());
    let broadcaster = Arc::new(MockBroadcaster::new());
    let service = ConversationService::new(
        std::env::temp_dir(),
        broadcaster,
        Arc::new(FixedSkillResolver { names: vec![] }),
        task_mgr,
        repo,
        Arc::new(StubAgentMetadataRepo),
        Arc::new(StubAcpSessionRepo::default()),
    );

    let conv = service.create("user_1", make_create_req()).await.unwrap();
    let outcome = service
        .run_agent_turn(ConversationAgentTurnRequest {
            user_id: "user_1".into(),
            conversation_id: conv.id,
            content: "run scheduled task".into(),
            files: Vec::new(),
            inject_skills: Vec::new(),
            persist_user_message: true,
            user_message_hidden: true,
            on_started: None,
        })
        .await
        .unwrap();

    assert_eq!(outcome.status, ConversationAgentTurnStatus::Failed);
    assert_eq!(
        outcome.error_message.as_deref(),
        Some("ACP init failed: config file is invalid")
    );
}

#[tokio::test]
async fn latest_conversation_error_message_prefers_error_detail() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    repo.insert_message(&MessageRow {
        id: "msg_error".into(),
        conversation_id: conv.id.clone(),
        msg_id: None,
        r#type: "tips".into(),
        content: serde_json::json!({
            "content": "Current agent failed",
            "type": "error",
            "details": {
                "detail": "Cannot resume Claude session with Codex assistant"
            },
            "error": {
                "message": "Current agent failed",
                "detail": "Codex cannot resume a conversation created by Claude"
            }
        })
        .to_string(),
        position: Some("center".into()),
        status: Some("error".into()),
        hidden: false,
        created_at: 10,
    })
    .await
    .unwrap();

    let message = svc.latest_conversation_error_message(&conv.id).await.unwrap();

    assert_eq!(
        message.as_deref(),
        Some("Codex cannot resume a conversation created by Claude")
    );
}

#[tokio::test]
async fn send_message_persists_openclaw_gateway_unreachable_tip_when_turn_build_fails() {
    let (svc, broadcaster, repo, _default_task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(AgentErrorFailingBuildTaskManager::new(AgentError::from(
        AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "ACP bridge failed: connect ECONNREFUSED 127.0.0.1:18789".into(),
        },
    )));

    let conv = svc
        .create("user_1", make_create_req_with_backend("openclaw"))
        .await
        .unwrap();
    broadcaster.take_events();

    let response = svc
        .send_message(
            "user_1",
            &conv.id,
            SendMessageRequest {
                content: "hello".into(),
                hidden: false,
                files: vec![],
                inject_skills: vec![],
            },
            &task_mgr,
        )
        .await
        .unwrap();

    wait_for_turn_released(&svc, &conv.id).await;

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    let tip = messages
        .iter()
        .find(|row| row.r#type == "tips" && row.status.as_deref() == Some("error"))
        .expect("send failure tip should be persisted");
    let content: serde_json::Value = serde_json::from_str(&tip.content).unwrap();

    assert_eq!(content["source"], "send_failed");
    assert_eq!(content["error"]["code"], "USER_AGENT_OPENCLAW_GATEWAY_UNREACHABLE");
    assert_eq!(content["error"]["ownership"], "user_agent");
    assert!(
        content["error"]["detail"]
            .as_str()
            .unwrap()
            .contains("openclaw gateway start")
    );

    let events = broadcaster.take_events();
    let error_tip_event = events
        .iter()
        .find(|event| event.name == "message.stream" && event.data["type"] == "tips")
        .expect("OpenClaw Gateway build failure should broadcast the error tips message");
    assert_eq!(error_tip_event.data["status"], "error");
    assert_eq!(
        error_tip_event.data["data"]["code"],
        "USER_AGENT_OPENCLAW_GATEWAY_UNREACHABLE"
    );
    assert_eq!(
        error_tip_event.data["data"]["error"]["code"],
        "USER_AGENT_OPENCLAW_GATEWAY_UNREACHABLE"
    );
    assert_eq!(error_tip_event.data["turn_id"], response.turn_id);
}

#[tokio::test]
async fn send_message_empty_content_returns_bad_request() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": ""
    }))
    .unwrap();

    let err = svc.send_message("user_1", &conv.id, req, &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }));
}

#[tokio::test]
async fn send_message_whitespace_content_returns_bad_request() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "   "
    }))
    .unwrap();

    let err = svc.send_message("user_1", &conv.id, req, &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }));
}

#[tokio::test]
async fn send_message_conversation_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let err = svc
        .send_message("user_1", "no-such-id", make_send_req(), &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn send_message_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let err = svc
        .send_message("user_2", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn send_message_allows_stale_db_running_without_runtime_claim() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    // Manually set status to running
    let update = ConversationRowUpdate {
        status: Some("running".into()),
        ..Default::default()
    };
    repo.update(&conv.id, &update).await.unwrap();

    let result = svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr).await;
    assert!(result.is_ok(), "stale DB running must not block sending");
}

#[tokio::test]
async fn send_message_rejects_active_runtime_claim() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let _claim = svc
        .runtime_state()
        .try_claim_turn(&conv.id, "turn-test")
        .expect("test claim should be created");

    let err = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::Busy { .. }));
}

#[tokio::test]
async fn send_message_rejects_when_runtime_is_shutting_down() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    svc.runtime_state().mark_shutting_down();

    let err = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::Busy { .. }));

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    assert!(
        messages.is_empty(),
        "shutdown rejection must not persist a user message"
    );
}

#[tokio::test]
async fn send_message_build_failure_while_deleting_skips_failure_tip_and_completion() {
    let (svc, broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(DelayedFailingBuildTaskManager::new(
        Duration::from_millis(100),
        "delayed build failure",
    ));

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events();

    let response = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();
    assert!(!response.msg_id.is_empty());

    svc.runtime_state().mark_deleting(&conv.id);
    wait_for_turn_released(&svc, &conv.id).await;

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    assert!(
        messages.iter().all(|message| message.r#type != "tips"),
        "deleting conversation must skip build-failure tips"
    );

    let events = broadcaster.take_events();
    assert!(
        events.iter().all(|event| event.name != "turn.completed"),
        "deleting conversation must not publish turn.completed"
    );
}

#[tokio::test]
async fn send_message_persists_factory_resolved_workspace() {
    // Conversation created with no workspace → create() auto-assigns one.
    // Factory resolves a *different* temp dir (simulating legacy-conv fallback).
    // After send_message, conversation.extra.workspace must match what the
    // agent reports.
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let auto_workspace = "/tmp/factory-resolved";
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManagerWithWorkspace::new(auto_workspace));

    // Create a conversation with an empty workspace to simulate legacy case.
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {}
    }))
    .unwrap();
    let conv = svc.create("user_1", req).await.unwrap();

    // Inject an empty workspace directly into the repo to mimic legacy state.
    let empty_ws_update = ConversationRowUpdate {
        extra: Some(r#"{"workspace":""}"#.to_owned()),
        ..Default::default()
    };
    repo.update(&conv.id, &empty_ws_update).await.unwrap();

    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    // Verify the workspace was written back.
    let updated = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let updated = svc.get("user_1", &conv.id).await.unwrap();
            if updated.extra["workspace"] == auto_workspace {
                return updated;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("factory-resolved workspace should be persisted in the background");
    assert_eq!(updated.extra["workspace"], auto_workspace);
}

#[tokio::test]
async fn startup_recovery_closes_stale_runtime_messages_without_failure_tip() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    repo.insert_message(&MessageRow {
        id: "visible-stale".into(),
        conversation_id: conv.id.clone(),
        msg_id: Some("visible-stale".into()),
        r#type: "text".into(),
        content: json!({ "content": "partial output" }).to_string(),
        position: Some("left".into()),
        status: Some("work".into()),
        hidden: false,
        created_at: 1,
    })
    .await
    .unwrap();
    repo.insert_message(&MessageRow {
        id: "empty-stale".into(),
        conversation_id: conv.id.clone(),
        msg_id: Some("empty-stale".into()),
        r#type: "thinking".into(),
        content: json!({ "content": "" }).to_string(),
        position: Some("left".into()),
        status: Some("pending".into()),
        hidden: false,
        created_at: 2,
    })
    .await
    .unwrap();

    svc.recover_stale_runtime_state_on_startup().await;

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    let visible = messages.iter().find(|message| message.id == "visible-stale").unwrap();
    assert_eq!(visible.status.as_deref(), Some("finish"));
    assert!(!visible.hidden);

    let empty = messages.iter().find(|message| message.id == "empty-stale").unwrap();
    assert_eq!(empty.status.as_deref(), Some("finish"));
    assert!(empty.hidden);

    assert!(
        messages.iter().all(|message| message.r#type != "tips"),
        "startup recovery must not write failure tips"
    );
}

#[tokio::test]
async fn send_message_continues_cron_system_responses() {
    let (svc, broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "I'll check. [CRON_LIST]".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "[CRON_CREATE]\nname: Daily Greeting\nschedule: 0 9 * * *\nschedule_description: Daily at 9:00 AM\nmessage: Say good morning\n[/CRON_CREATE]".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
            vec![
                AgentStreamEvent::Text(TextEventData {
                    content: "Done. The task is scheduled.".into(),
                }),
                AgentStreamEvent::Finish(FinishEventData::default()),
            ],
        ],
    ));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent.clone()));
    svc.with_cron_service(Some(Arc::new(MockCronContinuationService)));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    let req: SendMessageRequest = serde_json::from_value(json!({
        "content": "Create the task now"
    }))
    .unwrap();

    svc.send_message("user_1", &conv.id, req, &task_mgr_dyn).await.unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if scripted_agent.sent_contents().len() >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    let sends = scripted_agent.sent_contents();
    assert_eq!(sends.len(), 3);
    assert_eq!(sends[0], "Create the task now");
    assert_eq!(sends[1], "[System: No scheduled tasks]");
    assert_eq!(sends[2], "[System: Created cron job 'Daily Greeting']");

    let finished = svc.get("user_1", &conv.id).await.unwrap();
    assert_eq!(finished.status, ConversationStatus::Finished);

    let events = broadcaster.take_events();
    let turn_events: Vec<_> = events.iter().filter(|evt| evt.name == "turn.completed").collect();
    assert_eq!(turn_events.len(), 1);
    assert_eq!(turn_events[0].data["runtime"]["is_processing"], false);
    assert_eq!(turn_events[0].data["runtime"]["can_send_message"], true);
}

#[tokio::test]
async fn send_message_keeps_acp_task_after_normal_finish() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.kill_count(), 0);
    assert_eq!(task_mgr.active_count(), 1);
}

#[tokio::test]
async fn send_message_does_not_evict_non_acp_task_after_terminal_error() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(
        ScriptedAgent::new(
            &conv.id,
            vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                "aionrs terminal error",
                Some(AgentErrorCode::UnknownUpstreamError),
            ))]],
        )
        .with_agent_type(AgentType::Aionrs),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.kill_count(), 0);
    assert_eq!(task_mgr.active_count(), 1);
}

#[tokio::test]
async fn send_message_does_not_inject_send_error_when_runtime_terminal_exists() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(
        ScriptedAgent::new(
            &conv.id,
            vec![vec![AgentStreamEvent::Error(ErrorEventData::legacy(
                "runtime already emitted",
                Some(AgentErrorCode::UnknownUpstreamError),
            ))]],
        )
        .with_send_error(AgentSendError::from_agent_error(AgentError::bad_gateway(
            "fallback should not render",
        ))),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert_eq!(tips.len(), 1);
    let content: serde_json::Value = serde_json::from_str(&tips[0].content).unwrap();
    assert_eq!(content["content"], "runtime already emitted");
}

#[tokio::test]
async fn send_message_injects_send_error_when_runtime_terminal_missing() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(
        ScriptedAgent::new(&conv.id, vec![vec![]])
            .with_status(None)
            .with_send_error(AgentSendError::from_agent_error(AgentError::bad_gateway(
                "provider returned 401 invalid api key",
            ))),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let messages = repo
        .list_messages_page(
            &conv.id,
            &MessagePageParams {
                limit: 20,
                direction: MessagePageDirection::InitialLatest,
            },
        )
        .await
        .unwrap()
        .items;
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert_eq!(tips.len(), 1);
    let content: serde_json::Value = serde_json::from_str(&tips[0].content).unwrap();
    assert_eq!(content["type"], "error");
    assert_eq!(content["error"]["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
}

#[tokio::test]
async fn send_message_records_agent_availability_feedback_on_send_failure() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let feedback = Arc::new(RecordingAvailabilityFeedback::default());
    svc.with_agent_availability_feedback(feedback.clone());

    let mut create_req = make_create_req();
    create_req.name = Some("Feedback Conversation".into());
    create_req.source = Some(ConversationSource::Aionui);
    create_req.extra = json!({
        "backend": "claude",
        "agent_id": "agent-feedback-1",
        "agent_source": "custom",
        "workspace": ensure_test_workspace_path()
    });

    let conv = svc.create("user_1", create_req).await.unwrap();

    let scripted_agent = Arc::new(
        ScriptedAgent::new(&conv.id, vec![vec![]])
            .with_status(None)
            .with_send_error(AgentSendError::from_agent_error(AgentError::bad_gateway(
                "provider returned 401 invalid api key",
            ))),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let failures = feedback.failures.lock().unwrap().clone();
    assert_eq!(
        failures,
        vec![RecordedAvailabilityFailure {
            agent_id: "agent-feedback-1".into(),
            code: "session_send_failed".into(),
            message: "provider returned 401 invalid api key".into(),
        }]
    );
}

#[tokio::test]
async fn send_message_records_agent_availability_feedback_on_send_success() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let feedback = Arc::new(RecordingAvailabilityFeedback::default());
    svc.with_agent_availability_feedback(feedback.clone());

    let mut create_req = make_create_req();
    create_req.name = Some("Feedback Success Conversation".into());
    create_req.source = Some(ConversationSource::Aionui);
    create_req.extra = json!({
        "backend": "claude",
        "agent_id": "agent-feedback-success",
        "agent_source": "custom",
        "workspace": ensure_test_workspace_path()
    });

    let conv = svc.create("user_1", create_req).await.unwrap();

    let scripted_agent = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let successes = feedback.successes.lock().unwrap().clone();
    assert_eq!(successes, vec!["agent-feedback-success".to_owned()]);
    assert!(feedback.failures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn send_message_recovers_when_finished_task_has_no_runtime_terminal() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let scripted_agent = Arc::new(
        ScriptedAgent::new(&conv.id, vec![vec![]])
            .with_status(Some(ConversationStatus::Finished))
            .with_send_error(AgentSendError::from_agent_error(AgentError::bad_gateway(
                "acp protocol not connected",
            ))),
    );
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(scripted_agent));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.kill_count(), 1);
    assert_eq!(task_mgr.active_count(), 1);

    let messages = repo_messages_asc(&repo, &conv.id, 20).await;
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert!(tips.is_empty());
}

#[tokio::test]
async fn send_message_auto_replays_clean_retryable_acp_error_once() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        })]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![
            AgentStreamEvent::Text(TextEventData { content: "done".into() }),
            AgentStreamEvent::Finish(FinishEventData::default()),
        ]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first.clone()),
        AgentInstance::Mock(second.clone()),
    ]));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.build_count(), 2);
    assert_eq!(task_mgr.kill_count(), 1);
    assert_eq!(first.sent_contents(), vec!["Hello"]);
    assert_eq!(second.sent_contents(), vec!["Hello"]);

    let messages = repo_messages_asc(&repo, &conv.id, 20).await;
    let users: Vec<_> = messages
        .iter()
        .filter(|msg| msg.r#type == "text" && msg.position.as_deref() == Some("right"))
        .collect();
    let assistants: Vec<_> = messages
        .iter()
        .filter(|msg| msg.r#type == "text" && msg.position.as_deref() == Some("left"))
        .collect();
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert_eq!(users.len(), 1, "auto replay must not insert the user message again");
    assert_eq!(assistants.len(), 1);
    assert_eq!(
        tips.len(),
        0,
        "first clean retryable error stays hidden when replay succeeds"
    );
}

#[tokio::test]
async fn auto_replay_rebuild_keeps_existing_acp_session_id_in_build_options() {
    let acp_session_repo = Arc::new(StubAcpSessionRepo::with_session_id("sess-existing"));
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service_with_resolver_and_acp_session_repo(
        Arc::new(FixedSkillResolver { names: vec![] }),
        acp_session_repo,
    );
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        })]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first),
        AgentInstance::Mock(second),
    ]));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    let options = task_mgr.captured_options();
    assert_eq!(options.len(), 2);
    for options in options {
        match options.context.kind {
            AgentSessionKind::Acp(ctx) => {
                assert_eq!(ctx.session_id.as_deref(), Some("sess-existing"));
            }
            AgentSessionKind::Aionrs(_) => panic!("test conversation should build ACP options"),
        }
    }
}

#[tokio::test]
async fn send_message_does_not_auto_replay_after_visible_output() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![
            AgentStreamEvent::Text(TextEventData {
                content: "partial".into(),
            }),
            AgentStreamEvent::Error(ErrorEventData {
                message: "temporary provider failure".into(),
                code: Some(AgentErrorCode::UnknownUpstreamError),
                ownership: None,
                detail: None,
                workspace_path: None,
                retryable: Some(true),
                feedback_recommended: None,
                resolution: None,
            }),
        ]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first),
        AgentInstance::Mock(second),
    ]));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.build_count(), 1);
    assert_eq!(task_mgr.kill_count(), 1);
    let messages = repo_messages_asc(&repo, &conv.id, 20).await;
    let assistants: Vec<_> = messages
        .iter()
        .filter(|msg| msg.r#type == "text" && msg.position.as_deref() == Some("left"))
        .collect();
    assert_eq!(assistants.len(), 1);
    assert!(assistants[0].content.contains("partial"));
}

#[tokio::test]
async fn send_message_does_not_auto_replay_after_tool_side_effect() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![
            AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "call-1".into(),
                name: "write_file".into(),
                args: serde_json::json!({ "path": "a.txt" }),
                status: ToolCallStatus::Running,
                input: None,
                output: None,
                description: None,
            }),
            AgentStreamEvent::Error(ErrorEventData {
                message: "temporary provider failure".into(),
                code: Some(AgentErrorCode::UnknownUpstreamError),
                ownership: None,
                detail: None,
                workspace_path: None,
                retryable: Some(true),
                feedback_recommended: None,
                resolution: None,
            }),
        ]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first),
        AgentInstance::Mock(second),
    ]));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.build_count(), 1);
    assert_eq!(task_mgr.kill_count(), 1);
}

#[tokio::test]
async fn send_message_does_not_auto_replay_model_not_found() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData {
            message: "model not found".into(),
            code: Some(AgentErrorCode::UserLlmProviderModelNotFound),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        })]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first),
        AgentInstance::Mock(second),
    ]));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.build_count(), 1);
    assert_eq!(task_mgr.kill_count(), 1);
    let messages = repo_messages_asc(&repo, &conv.id, 20).await;
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert_eq!(
        tips.len(),
        1,
        "model_not_found should remain visible and must not be swallowed by replay deferral"
    );
}

#[tokio::test]
async fn send_message_auto_replay_stops_after_second_retryable_failure() {
    let (svc, _broadcaster, repo, _default_task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let first = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure one".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        })]],
    ));
    let second = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure two".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        })]],
    ));
    let third = Arc::new(ScriptedAgent::new(
        &conv.id,
        vec![vec![AgentStreamEvent::Finish(FinishEventData::default())]],
    ));
    let task_mgr = Arc::new(RebuildingScriptedTaskManager::new(vec![
        AgentInstance::Mock(first),
        AgentInstance::Mock(second),
        AgentInstance::Mock(third),
    ]));

    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    wait_for_turn_released(&svc, &conv.id).await;

    assert_eq!(task_mgr.build_count(), 2);
    assert_eq!(task_mgr.kill_count(), 2);
    let messages = repo_messages_asc(&repo, &conv.id, 20).await;
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert_eq!(tips.len(), 1, "second failure is final and visible");
    let content: serde_json::Value = serde_json::from_str(&tips[0].content).unwrap();
    assert_eq!(content["content"], "temporary provider failure two");
}

// ── stop_stream tests ───────────────────────────────────────────

#[tokio::test]
async fn stop_stream_with_active_agent() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    // Build agent via send_message
    let send = svc
        .send_message(
            "user_1",
            &conv.id,
            make_send_req(),
            &(task_mgr.clone() as Arc<dyn IWorkerTaskManager>),
        )
        .await
        .unwrap();

    // Stop should succeed since agent exists
    let result = svc
        .cancel(
            "user_1",
            &conv.id,
            &send.turn_id,
            &(task_mgr as Arc<dyn IWorkerTaskManager>),
        )
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn cancel_with_mismatched_turn_id_does_not_cancel_and_returns_current_runtime() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let slow_task_mgr = Arc::new(SlowBuildTaskManager::new(Duration::from_millis(500)));
    let task_mgr: Arc<dyn IWorkerTaskManager> = slow_task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let send = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    let response = svc.cancel("user_1", &conv.id, "turn_stale", &task_mgr).await.unwrap();

    assert_eq!(response.runtime.turn_id.as_deref(), Some(send.turn_id.as_str()));
    assert!(response.runtime.is_processing);
    assert!(svc.runtime_state().is_claimed(&conv.id));
}

#[tokio::test]
async fn cancel_keeps_turn_claim_until_agent_terminal_event() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = Arc::new(BlockingCancelAgent::new(&conv.id));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent.clone()));

    let send = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    agent.wait_until_send_started().await;

    let cancel = svc
        .cancel("user_1", &conv.id, &send.turn_id, &task_mgr_dyn)
        .await
        .unwrap();

    assert_eq!(cancel.runtime.turn_id.as_deref(), Some(send.turn_id.as_str()));
    assert!(cancel.runtime.is_processing);
    assert!(!cancel.runtime.can_send_message);
    assert!(svc.runtime_state().is_claimed(&conv.id));

    let second = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap_err();
    assert!(matches!(second, ConversationError::Busy { .. }));

    agent.release_finish();
    wait_for_turn_released(&svc, &conv.id).await;
}

#[tokio::test]
async fn cancel_error_clears_cancelling_state() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = Arc::new(BlockingCancelAgent::new_with_cancel_error(&conv.id));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent.clone()));

    let send = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();
    agent.wait_until_send_started().await;

    let err = svc
        .cancel("user_1", &conv.id, &send.turn_id, &task_mgr_dyn)
        .await
        .unwrap_err();

    assert!(matches!(err, ConversationError::BadGateway { .. }));
    assert!(!svc.runtime_state().is_cancelling(&conv.id));

    agent.release_finish();
    wait_for_turn_released(&svc, &conv.id).await;
}

#[tokio::test(start_paused = true)]
async fn cancel_timeout_kills_acp_task_when_turn_still_claimed() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = Arc::new(BlockingCancelAgent::new(&conv.id));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    let send = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();

    svc.cancel("user_1", &conv.id, &send.turn_id, &task_mgr_dyn)
        .await
        .unwrap();

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15) + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;

    assert_eq!(
        task_mgr.kill_records(),
        vec![(conv.id.clone(), Some(AgentKillReason::UserCancelTimeout))]
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_timeout_does_not_kill_non_acp_task() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());
    let task_mgr_dyn: Arc<dyn IWorkerTaskManager> = task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let agent = Arc::new(BlockingCancelAgent::new_with_type(&conv.id, AgentType::Aionrs));
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(agent));

    let send = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr_dyn)
        .await
        .unwrap();

    svc.cancel("user_1", &conv.id, &send.turn_id, &task_mgr_dyn)
        .await
        .unwrap();

    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(15) + Duration::from_millis(1)).await;
    tokio::task::yield_now().await;

    assert!(task_mgr.kill_records().is_empty());
}

#[tokio::test]
async fn stop_stream_conversation_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let err = svc
        .cancel("user_1", "no-such-id", "turn-test", &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn stop_stream_no_active_agent_is_idempotent() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let result = svc.cancel("user_1", &conv.id, "turn-test", &task_mgr).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn stop_stream_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let err = svc
        .cancel("user_2", &conv.id, "turn-test", &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── warmup tests ────────────────────────────────────────────────

#[tokio::test]
async fn warmup_creates_agent_task() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let result = svc
        .warmup("user_1", &conv.id, &(task_mgr.clone() as Arc<dyn IWorkerTaskManager>))
        .await;
    assert!(result.is_ok());

    // Agent should now exist
    assert!(task_mgr.get_task(&conv.id).is_some());
}

#[tokio::test]
async fn warmup_rejects_legacy_runtime_conversations_as_archived() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    for agent_type in [
        AgentType::Gemini,
        AgentType::Codex,
        AgentType::OpenclawGateway,
        AgentType::Nanobot,
        AgentType::Remote,
    ] {
        let conv = insert_conversation_with_type(&repo, "user_1", agent_type).await;

        let err = svc.warmup("user_1", &conv.id, &task_mgr).await.unwrap_err();

        assert_eq!(err.error_code(), "CONVERSATION_ARCHIVED");
        assert!(
            err.to_string()
                .contains("This historical conversation can no longer be continued. Please start a new conversation."),
            "unexpected archived message for {}: {err}",
            agent_type.serde_name()
        );
    }
}

#[tokio::test]
async fn warmup_conversation_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let err = svc.warmup("user_1", "no-such-id", &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn warmup_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let err = svc.warmup("user_2", &conv.id, &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn warmup_rejects_legacy_workspace_with_runtime_error_code() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let legacy_workspace = format!("/tmp/does-not-exist-{}", aionui_common::generate_short_id());
    repo.update(
        &conv.id,
        &ConversationRowUpdate {
            extra: Some(json!({ "workspace": legacy_workspace }).to_string()),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let err = svc.warmup("user_1", &conv.id, &task_mgr).await.unwrap_err();
    assert!(matches!(
        err,
        ConversationError::WorkspacePathRuntimeUnavailable { path: message }
            if message == legacy_workspace
    ));
}

#[tokio::test]
async fn warmup_returns_openclaw_gateway_unreachable_when_startup_stderr_matches() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(AgentErrorFailingBuildTaskManager::new(AgentError::from(
        AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "ACP bridge failed: connect ECONNREFUSED 127.0.0.1:18789".into(),
        },
    )));

    let conv = svc
        .create("user_1", make_create_req_with_backend("openclaw"))
        .await
        .unwrap();

    let err = svc.warmup("user_1", &conv.id, &task_mgr).await.unwrap_err();

    match err {
        ConversationError::OpenClawGatewayUnreachable { detail } => {
            assert!(detail.contains("127.0.0.1:18789"));
            assert!(detail.contains("openclaw gateway status"));
            assert!(detail.contains("openclaw gateway start"));
        }
        other => panic!("expected OpenClawGatewayUnreachable, got {other:?}"),
    }
}

#[tokio::test]
async fn warmup_keeps_generic_error_for_non_openclaw_gateway_signature() {
    let (svc, _broadcaster, _repo, _default_task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(AgentErrorFailingBuildTaskManager::new(AgentError::from(
        AcpError::StartupCrash {
            exit_code: Some(1),
            signal: None,
            stderr: "ACP bridge failed: connect ECONNREFUSED 127.0.0.1:18789".into(),
        },
    )));

    let conv = svc
        .create("user_1", make_create_req_with_backend("codex"))
        .await
        .unwrap();

    let err = svc.warmup("user_1", &conv.id, &task_mgr).await.unwrap_err();

    assert!(
        !matches!(err, ConversationError::OpenClawGatewayUnreachable { .. }),
        "non-OpenClaw backend must not receive the OpenClaw Gateway error"
    );
}

// ── Confirmation system tests ────────────────────────────────────

fn make_test_confirmations() -> Vec<Confirmation> {
    vec![
        Confirmation {
            id: "c1".into(),
            call_id: "call-1".into(),
            title: Some("Allow file edit".into()),
            action: Some("edit_file".into()),
            description: "Edit main.rs".into(),
            command_type: Some("bash".into()),
            options: vec![],
        },
        Confirmation {
            id: "c2".into(),
            call_id: "call-2".into(),
            title: Some("Read file".into()),
            action: Some("read_file".into()),
            description: "Read config.toml".into(),
            command_type: None,
            options: vec![],
        },
    ]
}

#[tokio::test]
async fn list_confirmations_empty_when_no_agent() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let result = svc.list_confirmations("user_1", &conv.id, &task_mgr).await.unwrap();
    assert!(result.is_empty());
}

#[tokio::test]
async fn list_confirmations_returns_items() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let agent = AgentInstance::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    task_mgr.insert_agent(&conv.id, agent);

    let result = svc
        .list_confirmations("user_1", &conv.id, &(task_mgr as Arc<dyn IWorkerTaskManager>))
        .await
        .unwrap();
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].call_id, "call-1");
    assert_eq!(result[1].call_id, "call-2");
}

#[tokio::test]
async fn list_confirmations_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let err = svc
        .list_confirmations("user_1", "no-such-id", &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn list_confirmations_wrong_user() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let err = svc.list_confirmations("user_2", &conv.id, &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn confirm_removes_confirmation_and_broadcasts() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events(); // clear create event

    let agent = AgentInstance::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    task_mgr.insert_agent(&conv.id, agent);

    let req = aionui_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    svc.confirm(
        "user_1",
        &conv.id,
        "call-1",
        req,
        &(task_mgr.clone() as Arc<dyn IWorkerTaskManager>),
    )
    .await
    .unwrap();

    // Confirmation should be removed from the agent
    let remaining = task_mgr.get_task(&conv.id).unwrap().get_confirmations();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].call_id, "call-2");

    // Should broadcast confirmation.remove event
    let events = broadcaster.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "confirmation.remove");
    assert_eq!(events[0].data["conversation_id"], conv.id);
    assert_eq!(events[0].data["id"], "c1");
}

#[tokio::test]
async fn confirm_with_always_allow_stores_approval() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let agent = AgentInstance::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    task_mgr.insert_agent(&conv.id, agent);

    let req = aionui_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: true,
    };
    let task_mgr_arc: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.confirm("user_1", &conv.id, "call-1", req, &task_mgr_arc)
        .await
        .unwrap();

    // check_approval should now return true for edit_file:bash
    let agent = task_mgr.get_task(&conv.id).unwrap();
    assert!(agent.check_approval("edit_file", Some("bash")));
    assert!(!agent.check_approval("delete_file", None));
}

#[tokio::test]
async fn confirm_nonexistent_call_id_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let agent = AgentInstance::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    task_mgr.insert_agent(&conv.id, agent);

    let req = aionui_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    let err = svc
        .confirm(
            "user_1",
            &conv.id,
            "nonexistent-call",
            req,
            &(task_mgr as Arc<dyn IWorkerTaskManager>),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFoundReason { .. }));
}

#[tokio::test]
async fn confirm_without_confirmation_state_still_calls_agent() {
    let (svc, broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    broadcaster.take_events();

    let agent = AgentInstance::Mock(Arc::new(MockAgent::with_direct_confirm(&conv.id)));
    task_mgr.insert_agent(&conv.id, agent);

    let req = aionui_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!("allow_once"),
        always_allow: false,
    };
    svc.confirm(
        "user_1",
        &conv.id,
        "call-1",
        req,
        &(task_mgr.clone() as Arc<dyn IWorkerTaskManager>),
    )
    .await
    .unwrap();

    assert!(broadcaster.take_events().is_empty());
}

#[tokio::test]
async fn confirm_no_agent_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let req = aionui_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: false,
    };
    let err = svc
        .confirm("user_1", &conv.id, "call-1", req, &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::ActiveAgentNotFound { .. }));
}

#[tokio::test]
async fn check_approval_returns_false_when_not_set() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let agent = AgentInstance::Mock(Arc::new(MockAgent::new(&conv.id)));
    task_mgr.insert_agent(&conv.id, agent);

    let result = svc
        .check_approval(
            "user_1",
            &conv.id,
            "edit_file",
            None,
            &(task_mgr as Arc<dyn IWorkerTaskManager>),
        )
        .await
        .unwrap();
    assert!(!result.approved);
}

#[tokio::test]
async fn check_approval_returns_true_after_always_allow() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let agent = AgentInstance::Mock(Arc::new(MockAgent::with_confirmations(
        &conv.id,
        make_test_confirmations(),
    )));
    task_mgr.insert_agent(&conv.id, agent);

    // Confirm with always_allow
    let req = aionui_api_types::ConfirmRequest {
        msg_id: "msg-1".into(),
        data: json!({ "value": "allow" }),
        always_allow: true,
    };
    let task_mgr_arc: Arc<dyn IWorkerTaskManager> = task_mgr.clone();
    svc.confirm("user_1", &conv.id, "call-1", req, &task_mgr_arc)
        .await
        .unwrap();

    // Now check_approval should return true
    let result = svc
        .check_approval("user_1", &conv.id, "edit_file", Some("bash"), &task_mgr_arc)
        .await
        .unwrap();
    assert!(result.approved);
}

#[tokio::test]
async fn check_approval_returns_false_when_no_agent() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let result = svc
        .check_approval("user_1", &conv.id, "edit_file", None, &task_mgr)
        .await
        .unwrap();
    assert!(!result.approved);
}

#[tokio::test]
async fn check_approval_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let err = svc
        .check_approval("user_1", "no-such-id", "edit_file", None, &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

// ── Skill snapshot tests ───────────────────────────────────────────

#[tokio::test]
async fn create_writes_extra_skills_from_auto_inject_and_preset() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let (svc, _broadcaster, _repo, _task_mgr) = make_service_with_resolver(resolver);
    let workspace = ensure_test_workspace_path();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "t",
        "extra": {
            "workspace": workspace,
            "backend": "claude",
            "preset_enabled_skills": ["pdf", "cron"],
            "exclude_auto_inject_skills": ["todo-tracker"],
        },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    assert_eq!(resp.extra["skills"], json!(["cron", "pdf"]));
    assert!(resp.extra.get("preset_enabled_skills").is_none());
    assert!(resp.extra.get("exclude_auto_inject_skills").is_none());
}

#[tokio::test]
async fn create_resolves_assistant_snapshot_and_updates_preferences() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::from([("preset-1".to_string(), "assistant rule body".to_string())]),
    });
    let (svc, _broadcaster, repo, definition_repo, state_repo, preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;
    let workspace = ensure_test_workspace_path();

    definition_repo
        .upsert(&UpsertAssistantDefinitionParams {
            id: "asstdef_preset_1",
            assistant_id: "preset-1",
            source: "builtin",
            owner_type: "system",
            source_ref: Some("preset-1"),
            source_version: None,
            source_hash: None,
            name: "Preset",
            name_i18n: "{}",
            description: Some("desc"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("🤖"),
            agent_id: "claude",
            rule_resource_type: "builtin_asset",
            rule_resource_ref: Some("preset-1"),
            rule_inline_content: None,
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "auto",
            default_skill_ids: "[]",
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        })
        .await
        .unwrap();
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_preset_1",
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex"),
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_preset_1",
            last_model_id: Some("old-model"),
            last_permission_value: Some("workspace-write"),
            last_skill_ids: r#"["legacy-skill"]"#,
            last_disabled_builtin_skill_ids: r#"["legacy-disabled"]"#,
            last_mcp_ids: r#"["legacy-mcp"]"#,
        })
        .await
        .unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "t",
        "assistant": {
            "id": "preset-1",
            "locale": "zh-CN",
            "conversation_overrides": {
                "model": "new-model",
                "skill_ids": ["pdf"],
                "disabled_builtin_skill_ids": ["todo-tracker"],
            }
        },
        "extra": {
            "workspace": workspace,
            "backend": "claude"
        },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    assert_eq!(
        resp.assistant,
        Some(aionui_api_types::ConversationAssistantIdentityResponse {
            id: "preset-1".into(),
            source: "builtin".into(),
            name: "Preset".into(),
            avatar: "🤖".into(),
            backend: "codex".into(),
        })
    );
    assert!(resp.extra.get("assistant_id").is_none());
    assert_eq!(resp.extra["agent_id"], json!("8e1acf31"));
    assert_eq!(resp.extra["agent_source"], json!("builtin"));
    assert!(resp.extra.get("preset_assistant_id").is_none());
    assert!(resp.extra.get("preset_context").is_none());
    assert!(resp.extra.get("preset_rules").is_none());
    assert_eq!(resp.extra["session_mode"], json!("workspace-write"));
    assert_eq!(resp.extra["current_mode_id"], json!("workspace-write"));
    assert_eq!(resp.extra["current_model_id"], json!("new-model"));
    assert_eq!(resp.extra["skills"], json!(["cron", "pdf"]));
    assert!(resp.extra.get("assistant_snapshot").is_none());

    let snapshot = repo.get_assistant_snapshot(&resp.id).await.unwrap().unwrap();
    assert_eq!(snapshot.assistant_definition_id, "asstdef_preset_1");
    assert_eq!(snapshot.assistant_id, "preset-1");
    assert_eq!(snapshot.agent_id, "8e1acf31");
    assert_eq!(snapshot.rules_content, "assistant rule body");
    assert_eq!(snapshot.default_model_mode, "auto");
    assert_eq!(snapshot.resolved_model_id.as_deref(), Some("new-model"));
    assert_eq!(snapshot.default_skills_mode, "auto");
    assert_eq!(snapshot.resolved_skill_ids, r#"["pdf"]"#);

    let updated_pref = preference_repo.get("asstdef_preset_1").await.unwrap().unwrap();
    assert_eq!(updated_pref.last_model_id.as_deref(), Some("new-model"));
    assert_eq!(updated_pref.last_skill_ids, r#"["pdf"]"#);
    assert_eq!(updated_pref.last_disabled_builtin_skill_ids, r#"["todo-tracker"]"#);

    let fetched = svc.get("user-1", &resp.id).await.unwrap();
    assert_eq!(
        fetched.assistant,
        Some(aionui_api_types::ConversationAssistantIdentityResponse {
            id: "preset-1".into(),
            source: "builtin".into(),
            name: "Preset".into(),
            avatar: "🤖".into(),
            backend: "codex".into(),
        })
    );

    let listed = svc
        .list(
            "user-1",
            ListConversationsQuery {
                cursor: None,
                limit: Some(20),
                source: None,
                cron_job_id: None,
                pinned: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        listed.items[0].assistant,
        Some(aionui_api_types::ConversationAssistantIdentityResponse {
            id: "preset-1".into(),
            source: "builtin".into(),
            name: "Preset".into(),
            avatar: "🤖".into(),
            backend: "codex".into(),
        })
    );
}

#[tokio::test]
async fn assistant_backed_acp_build_options_include_snapshot_rule_as_preset_context() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::from([("preset-acp-rule".to_string(), "assistant rule body".to_string())]),
    });
    let (svc, _broadcaster, repo, definition_repo, state_repo, _preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_preset_acp_rule",
        "preset-acp-rule",
        "codex",
        "auto",
        "auto",
    )
    .await;
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_preset_acp_rule",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let conv = create_assistant_backed_conversation(&svc, "user_1", Some("acp"), "codex", "preset-acp-rule").await;
    let row = repo.get(&conv.id).await.unwrap().unwrap();
    let options = svc.build_task_options(&row).await.unwrap();

    match options.context.kind {
        AgentSessionKind::Acp(ctx) => {
            assert_eq!(ctx.config.preset_context.as_deref(), Some("assistant rule body"));
        }
        AgentSessionKind::Aionrs(_) => panic!("test conversation should build ACP options"),
    }
}

#[tokio::test]
async fn assistant_backed_aionrs_build_options_include_snapshot_rule_as_preset_rules() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::from([("preset-aionrs-rule".to_string(), "assistant rule body".to_string())]),
    });
    let (svc, _broadcaster, repo, definition_repo, state_repo, _preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_preset_aionrs_rule",
        "preset-aionrs-rule",
        "aionrs",
        "auto",
        "auto",
    )
    .await;
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_preset_aionrs_rule",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let conv =
        create_assistant_backed_conversation(&svc, "user_1", Some("aionrs"), "aionrs", "preset-aionrs-rule").await;
    let row = repo.get(&conv.id).await.unwrap().unwrap();
    let options = svc.build_task_options(&row).await.unwrap();

    match options.context.kind {
        AgentSessionKind::Acp(_) => panic!("test conversation should build Aionrs options"),
        AgentSessionKind::Aionrs(ctx) => {
            assert_eq!(ctx.config.preset_rules.as_deref(), Some("assistant rule body"));
        }
    }
}

#[tokio::test]
async fn create_prefers_assistant_snapshot_over_legacy_runtime_seed_fields() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::from([("preset-1".to_string(), "assistant rule body".to_string())]),
    });
    let (svc, _broadcaster, repo, definition_repo, state_repo, preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;
    let workspace = ensure_test_workspace_path();

    definition_repo
        .upsert(&UpsertAssistantDefinitionParams {
            id: "asstdef_preset_legacy_seed",
            assistant_id: "preset-1",
            source: "builtin",
            owner_type: "system",
            source_ref: Some("preset-1"),
            source_version: None,
            source_hash: None,
            name: "Preset",
            name_i18n: "{}",
            description: Some("desc"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("🤖"),
            agent_id: "claude",
            rule_resource_type: "builtin_asset",
            rule_resource_ref: Some("preset-1"),
            rule_inline_content: None,
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "auto",
            default_skill_ids: "[]",
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        })
        .await
        .unwrap();
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_preset_legacy_seed",
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex"),
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_preset_legacy_seed",
            last_model_id: Some("preferred-model"),
            last_permission_value: Some("workspace-write"),
            last_skill_ids: r#"["legacy-skill"]"#,
            last_disabled_builtin_skill_ids: r#"["legacy-disabled"]"#,
            last_mcp_ids: r#"["legacy-mcp"]"#,
        })
        .await
        .unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "t",
        "assistant": {
            "id": "preset-1",
            "locale": "zh-CN",
            "conversation_overrides": {
                "model": "override-model"
            }
        },
        "extra": {
            "workspace": workspace,
            "backend": "claude",
            "current_model_id": "legacy-model",
            "session_mode": "legacy-mode",
            "current_mode_id": "legacy-mode"
        },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    assert_eq!(resp.extra["current_model_id"], json!("override-model"));
    assert_eq!(resp.extra["session_mode"], json!("workspace-write"));
    assert_eq!(resp.extra["current_mode_id"], json!("workspace-write"));

    let snapshot = repo.get_assistant_snapshot(&resp.id).await.unwrap().unwrap();
    assert_eq!(snapshot.agent_id, "8e1acf31");
    assert_eq!(snapshot.resolved_model_id.as_deref(), Some("override-model"));
    assert_eq!(snapshot.resolved_permission_value.as_deref(), Some("workspace-write"));
}

#[tokio::test]
async fn create_prefers_snapshot_runtime_identity_over_legacy_extra_identity() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::new(),
    });
    let acp_repo = Arc::new(StubAcpSessionRepo::default());
    let (svc, _broadcaster, _repo, definition_repo, overlay_repo, _preference_repo, acp_repo) =
        make_service_with_assistant_support_and_acp_session_repo(resolver, dispatcher, acp_repo).await;

    upsert_test_assistant_definition(
        &definition_repo,
        "asstdef_snapshot_identity",
        "preset-snapshot-identity",
        "codex",
        "auto",
        "auto",
    )
    .await;
    overlay_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_snapshot_identity",
            enabled: true,
            sort_order: 0,
            agent_id_override: None,
            last_used_at: None,
        })
        .await
        .unwrap();

    let workspace = ensure_test_workspace_path();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "assistant": {
            "id": "preset-snapshot-identity",
            "locale": "en-US"
        },
        "extra": {
            "workspace": workspace,
            "backend": "claude",
            "agent_source": "custom",
            "agent_id": "legacy-custom-agent"
        },
    }))
    .unwrap();

    let resp = svc.create("user-1", req).await.unwrap();

    assert_eq!(
        resp.assistant.as_ref().map(|assistant| assistant.backend.as_str()),
        Some("codex")
    );
    assert_eq!(resp.extra["backend"], json!("codex"));
    assert_eq!(resp.extra["agent_id"], json!("8e1acf31"));

    let create_calls = acp_repo.create_calls();
    assert_eq!(create_calls.len(), 1);
    assert_eq!(create_calls[0].agent_id, "8e1acf31");
    assert_eq!(create_calls[0].agent_source, "builtin");
    assert_eq!(create_calls[0].agent_id, "8e1acf31");
}

#[tokio::test]
async fn create_does_not_overwrite_preferences_for_fixed_skills_and_mcps() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::from([("preset-fixed".to_string(), "assistant rule body".to_string())]),
    });
    let (svc, _broadcaster, _repo, definition_repo, state_repo, preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;
    let workspace = ensure_test_workspace_path();

    definition_repo
        .upsert(&UpsertAssistantDefinitionParams {
            id: "asstdef_preset_fixed",
            assistant_id: "preset-fixed",
            source: "builtin",
            owner_type: "system",
            source_ref: Some("preset-fixed"),
            source_version: None,
            source_hash: None,
            name: "Preset Fixed",
            name_i18n: "{}",
            description: Some("desc"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("🤖"),
            agent_id: "claude",
            rule_resource_type: "builtin_asset",
            rule_resource_ref: Some("preset-fixed"),
            rule_inline_content: None,
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "fixed",
            default_skill_ids: r#"["pdf"]"#,
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: r#"["todo-tracker"]"#,
            default_mcps_mode: "fixed",
            default_mcp_ids: r#"["mcp-fixed"]"#,
        })
        .await
        .unwrap();
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_preset_fixed",
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex"),
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_preset_fixed",
            last_model_id: Some("legacy-model"),
            last_permission_value: Some("workspace-write"),
            last_skill_ids: r#"["legacy-skill"]"#,
            last_disabled_builtin_skill_ids: r#"["legacy-disabled"]"#,
            last_mcp_ids: r#"["legacy-mcp"]"#,
        })
        .await
        .unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "t",
        "assistant": {
            "id": "preset-fixed",
            "locale": "zh-CN",
            "conversation_overrides": {
                "model": "new-model",
                "permission": "workspace-read",
                "skill_ids": ["pdf", "cron"],
                "disabled_builtin_skill_ids": [],
                "mcp_ids": ["mcp-temp"]
            }
        },
        "extra": {
            "workspace": workspace,
            "backend": "claude"
        },
    }))
    .unwrap();
    let _resp = svc.create("user-1", req).await.unwrap();

    let updated_pref = preference_repo.get("asstdef_preset_fixed").await.unwrap().unwrap();
    assert_eq!(updated_pref.last_model_id.as_deref(), Some("new-model"));
    assert_eq!(updated_pref.last_permission_value.as_deref(), Some("workspace-read"));
    assert_eq!(updated_pref.last_skill_ids, r#"["legacy-skill"]"#);
    assert_eq!(updated_pref.last_disabled_builtin_skill_ids, r#"["legacy-disabled"]"#);
    assert_eq!(updated_pref.last_mcp_ids, r#"["legacy-mcp"]"#);
}

#[tokio::test]
async fn create_with_auto_builtin_defaults_without_preferences_keeps_snapshot_values_empty() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let dispatcher = Arc::new(StaticAssistantDispatcher {
        rules: std::collections::HashMap::from([("preset-auto".to_string(), "assistant rule body".to_string())]),
    });
    let (svc, _broadcaster, _repo, definition_repo, state_repo, preference_repo) =
        make_service_with_assistant_support(resolver, dispatcher).await;
    let workspace = ensure_test_workspace_path();

    definition_repo
        .upsert(&UpsertAssistantDefinitionParams {
            id: "asstdef_preset_auto",
            assistant_id: "preset-auto",
            source: "builtin",
            owner_type: "system",
            source_ref: Some("preset-auto"),
            source_version: None,
            source_hash: None,
            name: "Preset Unset",
            name_i18n: "{}",
            description: Some("desc"),
            description_i18n: "{}",
            avatar_type: "emoji",
            avatar_value: Some("🤖"),
            agent_id: "claude",
            rule_resource_type: "builtin_asset",
            rule_resource_ref: Some("preset-auto"),
            rule_inline_content: None,
            recommended_prompts: "[]",
            recommended_prompts_i18n: "{}",
            default_model_mode: "auto",
            default_model_value: None,
            default_permission_mode: "auto",
            default_permission_value: None,
            default_skills_mode: "fixed",
            default_skill_ids: r#"["pdf"]"#,
            custom_skill_names: "[]",
            default_disabled_builtin_skill_ids: "[]",
            default_mcps_mode: "auto",
            default_mcp_ids: "[]",
        })
        .await
        .unwrap();
    state_repo
        .upsert(&UpsertAssistantOverlayParams {
            assistant_definition_id: "asstdef_preset_auto",
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex"),
            last_used_at: None,
        })
        .await
        .unwrap();
    preference_repo
        .upsert(&UpsertAssistantPreferenceParams {
            assistant_definition_id: "asstdef_preset_auto",
            last_model_id: None,
            last_permission_value: None,
            last_skill_ids: "[]",
            last_disabled_builtin_skill_ids: "[]",
            last_mcp_ids: "[]",
        })
        .await
        .unwrap();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "name": "t",
        "assistant": {
            "id": "preset-auto",
            "locale": "zh-CN"
        },
        "extra": {
            "workspace": workspace,
            "backend": "claude"
        },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    assert!(resp.extra.get("current_model_id").is_none());
    assert!(resp.extra.get("permission_mode").is_none());
    assert!(resp.extra.get("assistant_snapshot").is_none());

    let updated_pref = preference_repo.get("asstdef_preset_auto").await.unwrap().unwrap();
    assert_eq!(updated_pref.last_model_id, None);
    assert_eq!(updated_pref.last_permission_value, None);
    assert_eq!(updated_pref.last_mcp_ids, "[]");
}

#[tokio::test]
async fn create_writes_empty_skills_when_no_auto_inject_and_no_preset() {
    let resolver = Arc::new(FixedSkillResolver { names: vec![] });
    let (svc, _broadcaster, _repo, _task_mgr) = make_service_with_resolver(resolver);
    let workspace = ensure_test_workspace_path();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace, "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    assert_eq!(resp.extra["skills"], json!([]));
}

#[tokio::test]
async fn create_links_skills_into_custom_workspace_for_native_acp_agent() {
    let resolver = Arc::new(RecordingSkillResolver::new(vec!["cron".into()]));
    let links = resolver.links.clone();
    let (svc, _broadcaster, _repo, _task_mgr) =
        make_service_with_resolver_and_agent_metadata_repo(resolver, Arc::new(ClaudeNativeSkillMetadataRepo));
    let workspace = unique_test_workspace_path("custom-create");

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "workspace": workspace,
            "backend": "claude"
        },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    assert_eq!(resp.extra["skills"], json!(["cron"]));
    assert!(workspace.join(".claude/skills/cron").is_dir());
    let calls = links.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workspace, workspace);
    assert_eq!(calls[0].rel_dirs, vec![".claude/skills"]);
    assert_eq!(calls[0].skill_names, vec!["cron"]);
}

#[tokio::test]
async fn warmup_restores_skill_links_for_recreated_auto_workspace() {
    let resolver = Arc::new(RecordingSkillResolver::new(vec!["cron".into()]));
    let links = resolver.links.clone();
    let (svc, _broadcaster, _repo, _task_mgr) = make_service_with_resolver(resolver);

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "aionrs",
        "extra": {},
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();
    let workspace = PathBuf::from(resp.extra["workspace"].as_str().unwrap());
    assert!(workspace.join(".aionrs/skills/cron").is_dir());

    std::fs::remove_dir_all(&workspace).unwrap();
    assert!(!workspace.exists());
    links.lock().unwrap().clear();

    let task_mgr: Arc<dyn IWorkerTaskManager> =
        Arc::new(MockTaskManagerWithWorkspace::new(workspace.to_str().unwrap()));
    svc.warmup("user-1", &resp.id, &task_mgr).await.unwrap();

    assert!(workspace.join(".aionrs/skills/cron").is_dir());
    let calls = links.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workspace, workspace);
    assert_eq!(calls[0].rel_dirs, vec![".aionrs/skills"]);
    assert_eq!(calls[0].skill_names, vec!["cron"]);
}

#[tokio::test]
async fn warmup_restores_skill_links_for_custom_workspace() {
    let resolver = Arc::new(RecordingSkillResolver::new(vec!["cron".into()]));
    let links = resolver.links.clone();
    let (svc, _broadcaster, _repo, _task_mgr) =
        make_service_with_resolver_and_agent_metadata_repo(resolver, Arc::new(ClaudeNativeSkillMetadataRepo));
    let workspace = unique_test_workspace_path("custom-warmup");

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "workspace": workspace,
            "backend": "claude"
        },
    }))
    .unwrap();
    let resp = svc.create("user-1", req).await.unwrap();

    std::fs::remove_dir_all(workspace.join(".claude")).unwrap();
    assert!(!workspace.join(".claude/skills/cron").exists());
    links.lock().unwrap().clear();

    let task_mgr: Arc<dyn IWorkerTaskManager> =
        Arc::new(MockTaskManagerWithWorkspace::new(workspace.to_str().unwrap()));
    svc.warmup("user-1", &resp.id, &task_mgr).await.unwrap();

    assert!(workspace.join(".claude/skills/cron").is_dir());
    let calls = links.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].workspace, workspace);
    assert_eq!(calls[0].rel_dirs, vec![".claude/skills"]);
    assert_eq!(calls[0].skill_names, vec!["cron"]);
}

#[tokio::test]
async fn update_rejects_extra_skills() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace, "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create("u", req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "skills": ["cron"] },
    }))
    .unwrap();
    let err = svc.update("u", &resp.id, update_req, &task_mgr).await.unwrap_err();

    match err {
        ConversationError::BadRequest { reason: msg } => assert!(msg.contains("skills"), "msg = {msg:?}"),
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn update_rejects_acp_runtime_current_extra_fields() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace, "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create("u", req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "current_model_id": "claude-3-5-sonnet", "current_mode_id": "default" },
    }))
    .unwrap();
    let err = svc.update("u", &resp.id, update_req, &task_mgr).await.unwrap_err();

    match err {
        ConversationError::BadRequest { reason: msg } => {
            assert!(msg.contains("/config-options"), "msg = {msg:?}")
        }
        other => panic!("expected BadRequest, got {other:?}"),
    }
}

#[tokio::test]
async fn update_allows_other_extra_fields() {
    let (svc, _broadcaster, _repo, task_mgr) = make_service();
    let workspace = ensure_test_workspace_path();

    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace, "backend": "claude" },
    }))
    .unwrap();
    let resp = svc.create("u", req).await.unwrap();

    let update_req: UpdateConversationRequest = serde_json::from_value(json!({
        "extra": { "display_density": "compact" },
    }))
    .unwrap();
    let updated = svc.update("u", &resp.id, update_req, &task_mgr).await.unwrap();

    assert_eq!(updated.extra["display_density"], "compact");
}

#[tokio::test]
async fn get_backfills_legacy_row_and_persists() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into(), "todo-tracker".into()],
    });
    let (svc, _broadcaster, repo, _task_mgr) = make_service_with_resolver(resolver);

    // Seed a legacy row directly via the repo — simulates a pre-migration
    // conversation that the service has never touched.
    let legacy_row = ConversationRow {
        id: "legacy-1".into(),
        user_id: "user-1".into(),
        name: "legacy".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/tmp/x",
            "enabled_skills": ["pdf"],
            "exclude_builtin_skills": ["todo-tracker"],
            "loaded_skills": [{"name": "cron", "description": "stale"}],
        }))
        .unwrap(),
        model: None,
        status: Some("finished".into()),
        source: Some("aionui".into()),
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 0,
        updated_at: 0,
    };
    repo.create(&legacy_row).await.unwrap();

    let resp = svc.get("user-1", "legacy-1").await.unwrap();
    assert_eq!(resp.extra["skills"], json!(["cron", "pdf"]));
    assert!(resp.extra.get("enabled_skills").is_none());
    assert!(resp.extra.get("exclude_builtin_skills").is_none());
    assert!(resp.extra.get("loaded_skills").is_none());

    // Second read returns the same result.
    let resp2 = svc.get("user-1", "legacy-1").await.unwrap();
    assert_eq!(resp2.extra["skills"], json!(["cron", "pdf"]));

    // Verify the row on disk was persisted with the new shape.
    let persisted = repo.get("legacy-1").await.unwrap().unwrap();
    let persisted_extra: serde_json::Value = serde_json::from_str(&persisted.extra).unwrap();
    assert_eq!(persisted_extra["skills"], json!(["cron", "pdf"]));
    assert!(persisted_extra.get("enabled_skills").is_none());
    assert!(persisted_extra.get("exclude_builtin_skills").is_none());
    assert!(persisted_extra.get("loaded_skills").is_none());
}

#[tokio::test]
async fn list_backfills_mixed_rows() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into()],
    });
    let (svc, _broadcaster, repo, _task_mgr) = make_service_with_resolver(resolver);

    // Row 1: legacy (needs backfill).
    let legacy = ConversationRow {
        id: "a".into(),
        user_id: "u".into(),
        name: "a".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/tmp/a",
            "enabled_skills": ["pdf"],
        }))
        .unwrap(),
        model: None,
        status: None,
        source: None,
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 1,
        updated_at: 1,
    };
    // Row 2: already migrated.
    let modern = ConversationRow {
        id: "b".into(),
        user_id: "u".into(),
        name: "b".into(),
        r#type: "acp".into(),
        extra: serde_json::to_string(&json!({
            "workspace": "/tmp/b",
            "skills": ["cron", "pdf"],
        }))
        .unwrap(),
        model: None,
        status: None,
        source: None,
        channel_chat_id: None,
        pinned: false,
        pinned_at: None,
        created_at: 2,
        updated_at: 2,
    };
    repo.create(&legacy).await.unwrap();
    repo.create(&modern).await.unwrap();

    let resp = svc.list("u", ListConversationsQuery::default()).await.unwrap();
    let extras: Vec<_> = resp.items.iter().map(|c| c.extra.clone()).collect();
    assert!(extras.iter().any(|e| e["skills"] == json!(["cron", "pdf"])));
}

#[tokio::test]
async fn create_honors_legacy_alias_fields_from_clone_merge() {
    let resolver = Arc::new(FixedSkillResolver {
        names: vec!["cron".into()],
    });
    let (svc, _broadcaster, _repo, _task_mgr) = make_service_with_resolver(resolver);

    // Legacy-shaped extra — what clone_create might merge in from an
    // unmigrated source conversation.
    let workspace = ensure_test_workspace_path();
    let req: CreateConversationRequest = serde_json::from_value(json!({
        "type": "acp",
        "extra": {
            "workspace": workspace,
            "backend": "claude",
            "enabled_skills": ["pdf"],
            "exclude_builtin_skills": ["cron"],
            "loaded_skills": [{"name": "cron", "description": "stale"}],
        },
    }))
    .unwrap();
    let resp = svc.create("u", req).await.unwrap();

    // Legacy enabled_skills ["pdf"] surfaces as preset; legacy exclude drops
    // cron; snapshot = {} ∪ ["pdf"] = ["pdf"].
    assert_eq!(resp.extra["skills"], json!(["pdf"]));
    assert!(resp.extra.get("enabled_skills").is_none());
    assert!(resp.extra.get("exclude_builtin_skills").is_none());
    assert!(resp.extra.get("loaded_skills").is_none());
}

// ── insert_raw_message ────────────────────────────────────────────
// Exercised by the team wake path (mirroring non-user mailbox rows into
// the target agent's conversation so the UI shows who spoke). Covers both
// the DB write and the live `message.stream` broadcast.

#[tokio::test]
async fn insert_raw_message_persists_row_and_broadcasts_stream() {
    let (svc, broadcaster, repo, _task_mgr) = make_service();
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    // Clear the create event so our assertion sees only the insert broadcast.
    let _ = broadcaster.take_events();

    let row = MessageRow {
        id: "msg-mirror-1".into(),
        conversation_id: conv.id.clone(),
        msg_id: Some("msg-mirror-1".into()),
        r#type: "text".into(),
        content: serde_json::json!({
            "content": "from teammate",
            "teammate_message": true,
            "sender_name": "Lead",
        })
        .to_string(),
        position: Some("left".into()),
        status: Some("finish".into()),
        hidden: false,
        created_at: 1234,
    };

    svc.insert_raw_message(&row).await.unwrap();

    let stored = repo.messages.lock().unwrap().clone();
    assert_eq!(stored.len(), 1, "row must be persisted via repo.insert_message");
    assert_eq!(stored[0].id, "msg-mirror-1");
    assert_eq!(stored[0].position.as_deref(), Some("left"));

    let events = broadcaster.take_events();
    let stream_events: Vec<_> = events.iter().filter(|e| e.name == "message.stream").collect();
    assert_eq!(stream_events.len(), 1, "expected exactly one message.stream event");
    let data = &stream_events[0].data;
    assert_eq!(data["conversation_id"], conv.id);
    assert_eq!(data["msg_id"], "msg-mirror-1");
    assert_eq!(data["type"], "text");
    assert_eq!(data["position"], "left");
    assert_eq!(data["data"]["content"], "from teammate");
    assert_eq!(data["data"]["teammate_message"], true);
}
