use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;

use aionui_ai_agent::agent_task::{AgentInstance, IAgentTask, IMockAgent};
use aionui_ai_agent::protocol::events::{AgentStreamEvent, ErrorEventData, FinishEventData, TextEventData};
use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{AgentError, AgentSendError, IWorkerTaskManager};

use crate::response_middleware::{CronCommandResult, CronCreateParams, CronUpdateParams, ICronService};
use aionui_api_types::{
    AgentErrorCode, AgentModeResponse, ConversationArtifactKind, GetModelInfoResponse, ModelInfoEntry,
    ModelInfoPayload, SetModeRequest, SetModelRequest,
};
use aionui_api_types::{
    CloneConversationRequest, CreateConversationRequest, ListConversationsQuery, SearchMessagesQuery,
    SendMessageRequest, UpdateConversationRequest, WebSocketMessage,
};
use aionui_common::{
    AgentKillReason, AgentType, Confirmation, ConversationSource, ConversationStatus, PaginatedResult, TimestampMs,
};
use aionui_db::models::{
    AcpSessionRow, AgentMetadataRow, ConversationArtifactRow, ConversationRow, MessageRow, UpdateAgentHandshakeParams,
    UpsertAgentMetadataParams,
};
use aionui_db::{
    ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, DbError, IAcpSessionRepository,
    IAgentMetadataRepository, IConversationRepository, MessageRowUpdate, MessageSearchRow, PersistedSessionState,
    SaveRuntimeStateParams, SortOrder,
};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::broadcast;

use crate::ConversationError;
use crate::service::ConversationService;
use crate::skill_resolver::{FixedSkillResolver, ResolvedAgentSkill, SkillResolver};

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

// ── Mock Repository ────────────────────────────────────────────────

struct MockRepo {
    rows: Mutex<Vec<ConversationRow>>,
    messages: Mutex<Vec<MessageRow>>,
    artifacts: Mutex<Vec<ConversationArtifactRow>>,
}

impl MockRepo {
    fn new() -> Self {
        Self {
            rows: Mutex::new(vec![]),
            messages: Mutex::new(vec![]),
            artifacts: Mutex::new(vec![]),
        }
    }
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

    async fn get_messages(
        &self,
        conv_id: &str,
        page: u32,
        page_size: u32,
        order: SortOrder,
    ) -> Result<PaginatedResult<MessageRow>, aionui_db::DbError> {
        let messages = self.messages.lock().unwrap();
        let mut matched: Vec<_> = messages
            .iter()
            .filter(|message| message.conversation_id == conv_id)
            .cloned()
            .collect();
        matched.sort_by_key(|message| message.created_at);
        if matches!(order, SortOrder::Desc) {
            matched.reverse();
        }

        let start = page.saturating_sub(1) as usize * page_size as usize;
        let end = (start + page_size as usize).min(matched.len());
        let items = if start < matched.len() {
            matched[start..end].to_vec()
        } else {
            Vec::new()
        };
        Ok(PaginatedResult {
            items,
            total: matched.len() as u64,
            has_more: end < matched.len(),
        })
    }

    async fn insert_message(&self, message: &MessageRow) -> Result<(), aionui_db::DbError> {
        self.messages.lock().unwrap().push(message.clone());
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

/// Stub repository for tests — every lookup returns `None` so the
/// service falls back to `AgentType::native_skills_dirs()` paths.
struct StubAgentMetadataRepo;

#[async_trait::async_trait]
impl IAgentMetadataRepository for StubAgentMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        Ok(Vec::new())
    }
    async fn get(&self, _id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn find_by_source_and_name(
        &self,
        _agent_source: &str,
        _name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn find_builtin_by_backend(&self, _backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
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
    current_model_id: Option<Option<String>>,
}

#[derive(Default)]
struct StubAcpSessionRepo {
    runtime_state_saves: Mutex<Vec<RuntimeStateSaveCall>>,
}

impl StubAcpSessionRepo {
    fn runtime_state_saves(&self) -> Vec<RuntimeStateSaveCall> {
        self.runtime_state_saves.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl IAcpSessionRepository for StubAcpSessionRepo {
    async fn get(&self, _conversation_id: &str) -> Result<Option<AcpSessionRow>, DbError> {
        Ok(None)
    }
    async fn create(&self, _params: &CreateAcpSessionParams<'_>) -> Result<AcpSessionRow, DbError> {
        // Return a synthetic row so `ConversationService::create` can
        // succeed for ACP conversations in unit tests.
        Ok(AcpSessionRow {
            conversation_id: "stub".into(),
            agent_backend: "stub".into(),
            agent_source: "stub".into(),
            agent_id: "stub".into(),
            session_id: None,
            session_status: "idle".into(),
            session_config: "{}".into(),
            last_active_at: None,
            suspended_at: None,
        })
    }
    async fn update_session_id(&self, _conversation_id: &str, _session_id: &str) -> Result<bool, DbError> {
        Ok(false)
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

fn make_create_req() -> CreateConversationRequest {
    let workspace = ensure_test_workspace_path();
    serde_json::from_value(json!({
        "type": "acp",
        "extra": { "workspace": workspace }
    }))
    .unwrap()
}

fn ensure_test_workspace_path() -> String {
    let workspace = std::env::temp_dir().join("aionui-conversation-service-test-project");
    std::fs::create_dir_all(&workspace).unwrap();
    workspace.to_string_lossy().to_string()
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
    confirmations: Mutex<Vec<Confirmation>>,
    approval_memory: Mutex<std::collections::HashMap<String, bool>>,
    allow_direct_confirm: bool,
    /// Optional workspace override; falls back to "/tmp/test" when `None`.
    workspace_override: Option<String>,
}

impl MockAgent {
    fn new(conversation_id: &str) -> Self {
        let (event_tx, _) = broadcast::channel(64);
        Self {
            conversation_id: conversation_id.to_owned(),
            event_tx,
            stopped: Mutex::new(false),
            mode: Mutex::new("default".to_owned()),
            model_id: Mutex::new("model-a".to_owned()),
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
            confirmations: Mutex::new(vec![]),
            approval_memory: Mutex::new(std::collections::HashMap::new()),
            allow_direct_confirm: true,
            workspace_override: None,
        }
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

    async fn set_mode(&self, mode: &str) -> Result<(), AgentError> {
        *self.mode.lock().unwrap() = mode.to_owned();
        Ok(())
    }

    async fn get_model(&self) -> Result<GetModelInfoResponse, AgentError> {
        let current = self.model_id.lock().unwrap().clone();
        Ok(GetModelInfoResponse {
            model_info: Some(ModelInfoPayload {
                current_model_id: Some(current.clone()),
                current_model_label: Some(current.clone()),
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
        })
    }

    async fn set_model(&self, model_id: &str) -> Result<(), AgentError> {
        *self.model_id.lock().unwrap() = model_id.to_owned();
        Ok(())
    }
}

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

struct FailingBuildTaskManager {
    error: String,
}

impl FailingBuildTaskManager {
    fn new(error: impl Into<String>) -> Self {
        Self { error: error.into() }
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

    fn clear(&self) {}

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

    fn clear(&self) {
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

    fn clear(&self) {}

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

    fn clear(&self) {
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
    let msg_id = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
    assert_eq!(msg_id.len(), 8, "msg_id should be an 8-char short hex ID");
}

#[tokio::test]
async fn set_mode_returns_confirmed_mode_from_active_agent() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(Arc::new(MockAgent::new(&conv.id))));

    let response = svc
        .set_mode(
            &conv.id,
            SetModeRequest {
                mode: "plan".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(response.mode, "plan");
    assert!(response.initialized);
}

#[tokio::test]
async fn set_model_returns_confirmed_model_from_active_agent() {
    let task_mgr = Arc::new(MockTaskManager::new());
    let (svc, _broadcaster, _repo) = make_service_with_mock_task_manager(task_mgr.clone());
    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    task_mgr.insert_agent(&conv.id, AgentInstance::Mock(Arc::new(MockAgent::new(&conv.id))));

    let response = svc
        .set_model(
            &conv.id,
            SetModelRequest {
                model_id: "model-b".to_owned(),
            },
        )
        .await
        .unwrap();

    let model_info = response.model_info.expect("model info should be returned");
    assert_eq!(model_info.current_model_id.as_deref(), Some("model-b"));
    assert!(model_info.available_models.iter().any(|m| m.id == "model-b"));
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

    let msg_id = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();
    assert!(
        !msg_id.is_empty(),
        "msg_id must still be returned when runtime workspace validation fails"
    );

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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
    assert_eq!(user_message.msg_id.as_deref(), Some(msg_id.as_str()));

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

    let turn_event = events
        .iter()
        .find(|event| event.name == "turn.completed")
        .expect("missing workspace failure should complete the turn");
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

    let msg_id = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    let events = broadcaster.take_events();
    let user_created = events
        .iter()
        .find(|e| e.name == "message.userCreated")
        .expect("should broadcast message.userCreated event");

    assert_eq!(user_created.data["conversation_id"], conv.id);
    assert_eq!(user_created.data["msg_id"], msg_id);
    assert_eq!(user_created.data["content"], "Hello");
    assert_eq!(user_created.data["position"], "right");
}

#[tokio::test]
async fn send_message_returns_before_cold_agent_build_completes() {
    let (svc, _broadcaster, repo, _task_mgr) = make_service();
    let slow_task_mgr = Arc::new(SlowBuildTaskManager::new(Duration::from_millis(500)));
    let task_mgr: Arc<dyn IWorkerTaskManager> = slow_task_mgr.clone();

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let msg_id = tokio::time::timeout(
        Duration::from_millis(50),
        svc.send_message("user_1", &conv.id, make_send_req(), &task_mgr),
    )
    .await
    .expect("send_message should return before cold agent build finishes")
    .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");
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

    let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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

    let msg_id = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap();

    assert!(!msg_id.is_empty(), "msg_id must be non-empty");

    let messages = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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
    assert_eq!(content["error"]["feedback_recommended"], true);
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
        .try_claim_turn(&conv.id)
        .expect("test claim should be created");

    let err = svc
        .send_message("user_1", &conv.id, make_send_req(), &task_mgr)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::Busy { .. }));
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

    let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
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

    let messages = repo.get_messages(&conv.id, 1, 20, SortOrder::Asc).await.unwrap().items;
    let tips: Vec<_> = messages.iter().filter(|msg| msg.r#type == "tips").collect();
    assert_eq!(tips.len(), 1);
    let content: serde_json::Value = serde_json::from_str(&tips[0].content).unwrap();
    assert_eq!(content["type"], "error");
    assert_eq!(content["error"]["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
}

// ── stop_stream tests ───────────────────────────────────────────

#[tokio::test]
async fn stop_stream_with_active_agent() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    // Build agent via send_message
    svc.send_message(
        "user_1",
        &conv.id,
        make_send_req(),
        &(task_mgr.clone() as Arc<dyn IWorkerTaskManager>),
    )
    .await
    .unwrap();

    // Stop should succeed since agent exists
    let result = svc
        .cancel("user_1", &conv.id, &(task_mgr as Arc<dyn IWorkerTaskManager>))
        .await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn stop_stream_conversation_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let err = svc.cancel("user_1", "no-such-id", &task_mgr).await.unwrap_err();
    assert!(matches!(err, ConversationError::NotFound { .. }));
}

#[tokio::test]
async fn stop_stream_no_active_agent_is_idempotent() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();

    let result = svc.cancel("user_1", &conv.id, &task_mgr).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn stop_stream_wrong_user_returns_not_found() {
    let (svc, _broadcaster, _repo, _task_mgr) = make_service();
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(MockTaskManager::new());

    let conv = svc.create("user_1", make_create_req()).await.unwrap();
    let err = svc.cancel("user_2", &conv.id, &task_mgr).await.unwrap_err();
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
            assert!(msg.contains("/mode or /model"), "msg = {msg:?}")
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
