//! Black-box integration tests for `CronService`.
//!
//! Uses real SQLite (in-memory), mock broadcaster, and stubs for
//! task manager / conversation service (since integration with AI agents
//! is out of scope for this service-layer test).
//!
//! Covers test-plan items: CJ-1..CJ-12, SK-1..SK-7, SC-1..SC-8,
//! OC-1, SR-1, conversation helper API integration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_ai_agent::AgentRegistry;
use aionui_ai_agent::agent_task::AgentInstance;
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_api_types::{
    ApiResponse, CreateConversationCronRequest, CreateConversationCronResponse, CreateCronJobRequest, CronJobResponse,
    CronScheduleDto, ListCronJobsQuery, SaveCronSkillRequest, UpdateConversationCronRequest, UpdateCronJobRequest,
    WebSocketMessage,
};
use aionui_common::{PaginatedResult, ProviderWithModel, TimestampMs, now_ms};
use aionui_conversation::ConversationService;
use aionui_cron::{CronRouterState, cron_routes};
use aionui_db::{
    ConversationFilters, ConversationRowUpdate, IAcpSessionRepository, IAgentMetadataRepository,
    IAssistantDefinitionRepository, IAssistantOverlayRepository, IAssistantPreferenceRepository,
    IConversationRepository, ICronRepository, MessagePageParams, MessagePageResult, MessageRowUpdate, MessageSearchRow,
    SqliteAcpSessionRepository, SqliteAgentMetadataRepository, SqliteAssistantDefinitionRepository,
    SqliteAssistantOverlayRepository, SqliteAssistantPreferenceRepository, SqliteCronRepository,
    UpsertAgentMetadataParams, UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
    UpsertConversationAssistantSnapshotParams, init_database_memory,
    models::{ConversationAssistantSnapshotRow, CronJobRow, MessageRow},
};
use aionui_realtime::EventBroadcaster;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode};

use aionui_cron::events::CronEventEmitter;
use aionui_cron::executor::JobExecutor;
use aionui_cron::scheduler::CronScheduler;
use aionui_cron::service::{CronService, CronServiceDeps};
use aionui_cron::types::CronAgentConfig;
use aionui_cron::types::JobStatus;
use tower::ServiceExt;

// ── Test infrastructure ────────────────────────────────────────────

fn ensure_named_workspace_path(name: &str) -> String {
    let workspace = std::env::temp_dir().join(name);
    std::fs::create_dir_all(&workspace).unwrap();
    workspace.to_string_lossy().to_string()
}

struct MockBroadcaster {
    events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl MockBroadcaster {
    fn new() -> Self {
        Self {
            events: Mutex::new(Vec::new()),
        }
    }

    fn take_events(&self) -> Vec<WebSocketMessage<serde_json::Value>> {
        let mut guard = self.events.lock().unwrap();
        std::mem::take(&mut *guard)
    }
}

impl EventBroadcaster for MockBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

struct StubTaskManager;

#[async_trait::async_trait]
impl aionui_ai_agent::task_manager::IWorkerTaskManager for StubTaskManager {
    fn get_task(&self, _: &str) -> Option<AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: BuildTaskOptions,
    ) -> Result<AgentInstance, aionui_ai_agent::AgentError> {
        Err(aionui_ai_agent::AgentError::internal("stub"))
    }
    fn kill(&self, _: &str, _: Option<aionui_common::AgentKillReason>) -> Result<(), aionui_ai_agent::AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<aionui_common::AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct StubConvRepo {
    messages: Mutex<Vec<MessageRow>>,
    artifacts: Mutex<Vec<aionui_db::ConversationArtifactRow>>,
    rows: Mutex<HashMap<String, aionui_db::models::ConversationRow>>,
    assistant_snapshots: Mutex<HashMap<String, ConversationAssistantSnapshotRow>>,
    update_failures: Mutex<Vec<String>>,
}

impl StubConvRepo {
    fn new() -> Self {
        Self {
            messages: Mutex::new(Vec::new()),
            artifacts: Mutex::new(Vec::new()),
            rows: Mutex::new(HashMap::new()),
            assistant_snapshots: Mutex::new(HashMap::new()),
            update_failures: Mutex::new(Vec::new()),
        }
    }

    fn take_messages(&self) -> Vec<MessageRow> {
        let mut guard = self.messages.lock().unwrap();
        std::mem::take(&mut *guard)
    }

    fn upsert_artifact_row(&self, artifact: aionui_db::ConversationArtifactRow) {
        let mut guard = self.artifacts.lock().unwrap();
        if let Some(existing) = guard.iter_mut().find(|row| row.id == artifact.id) {
            *existing = artifact;
        } else {
            guard.push(artifact);
        }
    }

    fn artifacts(&self) -> Vec<aionui_db::ConversationArtifactRow> {
        self.artifacts.lock().unwrap().clone()
    }

    fn fail_updates_for(&self, conversation_id: &str) {
        self.update_failures.lock().unwrap().push(conversation_id.to_owned());
    }

    fn set_conversation_extra(&self, conversation_id: &str, extra: serde_json::Value) {
        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .entry(conversation_id.to_owned())
            .or_insert_with(|| aionui_db::models::ConversationRow {
                id: conversation_id.to_owned(),
                user_id: "u1".into(),
                name: "stub".into(),
                r#type: "default".into(),
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: "{}".into(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            });
        row.extra = extra.to_string();
    }
}

#[async_trait::async_trait]
impl IConversationRepository for StubConvRepo {
    async fn get(&self, id: &str) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
        let mut rows = self.rows.lock().unwrap();

        if let Some(existing) = rows.get(id) {
            return Ok(Some(existing.clone()));
        }
        if id.starts_with("missing") {
            return Ok(None);
        }

        let row = if id == "conv_mode" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Gemini Chat".into(),
                r#type: "acp".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "gemini",
                        "model": "gemini-2.5-pro",
                        "use_model": "gemini-2.5-pro"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "gemini",
                    "agent_name": "Gemini",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-gemini-workspace"),
                    "session_mode": "yolo",
                    "current_model_id": "gemini-2.5-pro"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_hermes" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Hermes Chat".into(),
                r#type: "acp".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "hermes",
                        "model": "gemini-2.5-pro",
                        "use_model": "gemini-2.5-pro"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "hermes",
                    "agent_name": "Hermes",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-hermes-workspace"),
                    "session_mode": "default",
                    "current_model_id": "gemini-2.5-pro"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_default" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Gemini Default Chat".into(),
                r#type: "acp".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "gemini",
                        "model": "gemini-2.5-pro",
                        "use_model": "gemini-2.5-pro"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "gemini",
                    "agent_name": "Gemini",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-gemini-default-workspace"),
                    "session_mode": "default",
                    "current_model_id": "gemini-2.5-pro"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_codex" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Codex Chat".into(),
                r#type: "acp".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "codex",
                        "model": "gpt-5-codex",
                        "use_model": "gpt-5-codex"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "codex",
                    "agent_name": "Codex",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-codex-workspace"),
                    "session_mode": "default",
                    "current_model_id": "gpt-5-codex"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_claude" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Claude Chat".into(),
                r#type: "acp".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "claude",
                        "model": "claude-sonnet-4-20250514",
                        "use_model": "claude-sonnet-4-20250514"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "claude",
                    "agent_name": "Claude",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-claude-workspace"),
                    "session_mode": "default",
                    "current_model_id": "claude-sonnet-4-20250514"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_stale_backend" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Gemini Stale Backend Chat".into(),
                r#type: "acp".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "gemini",
                        "model": "gemini-2.5-pro",
                        "use_model": "gemini-2.5-pro"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "claude",
                    "agent_name": "Gemini",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-stale-backend-workspace"),
                    "session_mode": "yolo",
                    "current_model_id": "gemini-2.5-pro"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_aionrs" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Aionrs Chat".into(),
                r#type: "aionrs".into(),
                model: Some(
                    serde_json::json!({
                        "provider_id": "anthropic",
                        "model": "claude-sonnet-4-20250514",
                        "use_model": "claude-sonnet-4-20250514"
                    })
                    .to_string(),
                ),
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "anthropic",
                    "agent_name": "Aion CLI",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-aionrs-workspace"),
                    "session_mode": "default",
                    "current_model_id": "claude-sonnet-4-20250514"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_assistant_stale_backend" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Assistant Stale Backend Chat".into(),
                r#type: "acp".into(),
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "assistant_id": "assistant-override",
                    "backend": "claude",
                    "agent_name": "Override Assistant",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-assistant-stale-backend-workspace"),
                    "current_model_id": "gpt-5.4"
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_missing_assistant_stale_backend" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Missing Assistant Stale Backend Chat".into(),
                r#type: "acp".into(),
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "assistant_id": "missing-assistant",
                    "backend": "claude",
                    "agent_name": "Missing Assistant",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-missing-assistant-stale-backend-workspace")
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else if id == "conv_mode_assistant_snapshot" {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "Snapshot Assistant Chat".into(),
                r#type: "acp".into(),
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: serde_json::json!({
                    "backend": "claude",
                    "agent_name": "Legacy Extra Assistant",
                    "workspace": ensure_named_workspace_path("aionui-cron-service-assistant-snapshot-workspace")
                })
                .to_string(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        } else {
            aionui_db::models::ConversationRow {
                id: id.into(),
                user_id: "u1".into(),
                name: "stub".into(),
                r#type: "default".into(),
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: "{}".into(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            }
        };

        rows.insert(id.to_owned(), row.clone());
        Ok(Some(row))
    }

    async fn get_assistant_snapshot(
        &self,
        conversation_id: &str,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, aionui_db::DbError> {
        Ok(self.assistant_snapshots.lock().unwrap().get(conversation_id).cloned())
    }

    async fn upsert_assistant_snapshot(
        &self,
        params: &UpsertConversationAssistantSnapshotParams<'_>,
    ) -> Result<Option<ConversationAssistantSnapshotRow>, aionui_db::DbError> {
        let now = now_ms();
        let row = ConversationAssistantSnapshotRow {
            conversation_id: params.conversation_id.to_owned(),
            assistant_definition_id: params.assistant_definition_id.to_owned(),
            assistant_id: params.assistant_id.to_owned(),
            assistant_source: params.assistant_source.to_owned(),
            agent_id: params.agent_id.to_owned(),
            rules_content: params.rules_content.to_owned(),
            default_model_mode: params.default_model_mode.to_owned(),
            resolved_model_id: params.resolved_model_id.map(ToOwned::to_owned),
            default_permission_mode: params.default_permission_mode.to_owned(),
            resolved_permission_value: params.resolved_permission_value.map(ToOwned::to_owned),
            default_thought_level_mode: params.default_thought_level_mode.to_owned(),
            resolved_thought_level_value: params.resolved_thought_level_value.map(ToOwned::to_owned),
            default_skills_mode: params.default_skills_mode.to_owned(),
            resolved_skill_ids: params.resolved_skill_ids.to_owned(),
            resolved_disabled_builtin_skill_ids: params.resolved_disabled_builtin_skill_ids.to_owned(),
            default_mcps_mode: params.default_mcps_mode.to_owned(),
            resolved_mcp_ids: params.resolved_mcp_ids.to_owned(),
            created_at: now,
            updated_at: now,
        };
        self.assistant_snapshots
            .lock()
            .unwrap()
            .insert(row.conversation_id.clone(), row.clone());
        Ok(Some(row))
    }

    async fn create(&self, row: &aionui_db::models::ConversationRow) -> Result<(), aionui_db::DbError> {
        self.rows.lock().unwrap().insert(row.id.clone(), row.clone());
        Ok(())
    }
    async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), aionui_db::DbError> {
        if self.update_failures.lock().unwrap().iter().any(|item| item == id) {
            return Err(aionui_db::DbError::Init(format!("forced update failure for {id}")));
        }

        let mut rows = self.rows.lock().unwrap();
        let row = rows
            .entry(id.to_owned())
            .or_insert_with(|| aionui_db::models::ConversationRow {
                id: id.to_owned(),
                user_id: "u1".into(),
                name: "stub".into(),
                r#type: "default".into(),
                model: None,
                status: Some("active".into()),
                source: None,
                channel_chat_id: None,
                extra: "{}".into(),
                pinned: false,
                pinned_at: None,
                created_at: 1000,
                updated_at: 1000,
            });
        if let Some(extra) = &updates.extra {
            row.extra = extra.clone();
        }
        if let Some(updated_at) = updates.updated_at {
            row.updated_at = updated_at;
        }
        Ok(())
    }
    async fn delete(&self, _id: &str) -> Result<(), aionui_db::DbError> {
        Ok(())
    }
    async fn list_paginated(
        &self,
        _user_id: &str,
        _filters: &ConversationFilters,
    ) -> Result<PaginatedResult<aionui_db::models::ConversationRow>, aionui_db::DbError> {
        Ok(PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }
    async fn find_by_source_and_chat(
        &self,
        _user_id: &str,
        _source: &str,
        _chat_id: &str,
        _agent_type: &str,
    ) -> Result<Option<aionui_db::models::ConversationRow>, aionui_db::DbError> {
        Ok(None)
    }
    async fn list_by_cron_job(
        &self,
        _user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
        let rows = self.rows.lock().unwrap();
        Ok(rows
            .values()
            .filter(|row| {
                let parsed = serde_json::from_str::<serde_json::Value>(&row.extra).ok();
                let bound = parsed.as_ref().and_then(|extra| {
                    extra
                        .get("cron_job_id")
                        .and_then(|value| value.as_str())
                        .or_else(|| extra.get("cronJobId").and_then(|value| value.as_str()))
                });
                bound == Some(cron_job_id)
            })
            .cloned()
            .collect())
    }
    async fn list_associated(
        &self,
        _user_id: &str,
        _conversation_id: &str,
    ) -> Result<Vec<aionui_db::models::ConversationRow>, aionui_db::DbError> {
        Ok(vec![])
    }
    async fn list_messages_page(
        &self,
        _conv_id: &str,
        _params: &MessagePageParams,
    ) -> Result<MessagePageResult, aionui_db::DbError> {
        Ok(MessagePageResult {
            items: vec![],
            has_more_before: false,
            has_more_after: false,
        })
    }
    async fn insert_message(&self, message: &aionui_db::models::MessageRow) -> Result<(), aionui_db::DbError> {
        self.messages.lock().unwrap().push(message.clone());
        Ok(())
    }
    async fn update_message(&self, _id: &str, _updates: &MessageRowUpdate) -> Result<(), aionui_db::DbError> {
        Ok(())
    }
    async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), aionui_db::DbError> {
        Ok(())
    }
    async fn get_message_by_msg_id(
        &self,
        _conv_id: &str,
        _msg_id: &str,
        _msg_type: &str,
    ) -> Result<Option<aionui_db::models::MessageRow>, aionui_db::DbError> {
        Ok(None)
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
    async fn list_artifacts(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<aionui_db::ConversationArtifactRow>, aionui_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.conversation_id == conversation_id)
            .cloned()
            .collect())
    }
    async fn get_artifact(
        &self,
        conversation_id: &str,
        artifact_id: &str,
    ) -> Result<Option<aionui_db::ConversationArtifactRow>, aionui_db::DbError> {
        Ok(self
            .artifacts
            .lock()
            .unwrap()
            .iter()
            .find(|row| row.conversation_id == conversation_id && row.id == artifact_id)
            .cloned())
    }
    async fn upsert_artifact(
        &self,
        artifact: &aionui_db::ConversationArtifactRow,
    ) -> Result<aionui_db::ConversationArtifactRow, aionui_db::DbError> {
        self.upsert_artifact_row(artifact.clone());
        Ok(artifact.clone())
    }
    async fn update_artifact_status(
        &self,
        conversation_id: &str,
        artifact_id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<Option<aionui_db::ConversationArtifactRow>, aionui_db::DbError> {
        let mut guard = self.artifacts.lock().unwrap();
        let Some(existing) = guard
            .iter_mut()
            .find(|row| row.conversation_id == conversation_id && row.id == artifact_id)
        else {
            return Ok(None);
        };
        existing.status = status.to_string();
        existing.updated_at = updated_at;
        Ok(Some(existing.clone()))
    }
    async fn mark_skill_suggest_artifacts_saved(
        &self,
        cron_job_id: &str,
        updated_at: TimestampMs,
    ) -> Result<Vec<aionui_db::ConversationArtifactRow>, aionui_db::DbError> {
        let mut guard = self.artifacts.lock().unwrap();
        let mut updated = Vec::new();
        for artifact in guard.iter_mut() {
            if artifact.kind == "skill_suggest" && artifact.cron_job_id.as_deref() == Some(cron_job_id) {
                artifact.status = "saved".into();
                artifact.updated_at = updated_at;
                updated.push(artifact.clone());
            }
        }
        Ok(updated)
    }
}

async fn setup() -> (CronService, Arc<dyn ICronRepository>, Arc<MockBroadcaster>) {
    let (svc, repo, bc, _) = setup_with_conv_repo().await;
    (svc, repo, bc)
}

async fn setup_with_conv_repo() -> (
    CronService,
    Arc<dyn ICronRepository>,
    Arc<MockBroadcaster>,
    Arc<StubConvRepo>,
) {
    let (svc, repo, bc, conv_repo, _) = setup_with_conv_runtime().await;
    (svc, repo, bc, conv_repo)
}

async fn setup_with_conv_runtime() -> (
    CronService,
    Arc<dyn ICronRepository>,
    Arc<MockBroadcaster>,
    Arc<StubConvRepo>,
    Arc<ConversationService>,
) {
    let (svc, cron_repo, bc, stub_conv_repo, conv_service, _) = setup_with_conv_runtime_and_agent_metadata().await;
    (svc, cron_repo, bc, stub_conv_repo, conv_service)
}

async fn setup_with_conv_runtime_and_agent_metadata() -> (
    CronService,
    Arc<dyn ICronRepository>,
    Arc<MockBroadcaster>,
    Arc<StubConvRepo>,
    Arc<ConversationService>,
    Arc<dyn IAgentMetadataRepository>,
) {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let cron_repo: Arc<dyn ICronRepository> = Arc::new(SqliteCronRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository> =
        Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository> =
        Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()));
    let assistant_preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let bc = Arc::new(MockBroadcaster::new());
    let data_dir = std::env::temp_dir().join(format!("aionui-cron-test-{}", now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();

    struct StubSkillResolver;
    #[async_trait::async_trait]
    impl aionui_conversation::skill_resolver::SkillResolver for StubSkillResolver {
        async fn auto_inject_names(&self) -> Vec<String> {
            Vec::new()
        }

        async fn resolve_skills(
            &self,
            _names: &[String],
        ) -> Vec<aionui_conversation::skill_resolver::ResolvedAgentSkill> {
            Vec::new()
        }

        async fn link_workspace_skills(
            &self,
            _workspace: &std::path::Path,
            _rel_dirs: &[&str],
            _skills: &[aionui_conversation::skill_resolver::ResolvedAgentSkill],
        ) -> usize {
            0
        }
    }

    let stub_conv_repo = Arc::new(StubConvRepo::new());
    let stub_conv_repo_trait: Arc<dyn IConversationRepository> = stub_conv_repo.clone();
    let task_manager: Arc<dyn aionui_ai_agent::task_manager::IWorkerTaskManager> = Arc::new(StubTaskManager);
    let conv_service = Arc::new(ConversationService::new(
        std::env::temp_dir(),
        bc.clone() as Arc<dyn EventBroadcaster>,
        Arc::new(StubSkillResolver),
        Arc::clone(&task_manager),
        Arc::clone(&stub_conv_repo_trait),
        Arc::clone(&agent_metadata_repo),
        acp_session_repo,
    ));
    conv_service.with_assistant_definition_repo(assistant_definition_repo.clone());
    conv_service.with_assistant_state_repo(assistant_overlay_repo.clone());
    conv_service.with_assistant_preference_repo(assistant_preference_repo);
    let agent_registry = AgentRegistry::new(agent_metadata_repo.clone());
    agent_registry.hydrate().await.unwrap();
    let executor = Arc::new(JobExecutor::new(
        task_manager,
        stub_conv_repo_trait,
        conv_service.clone(),
        data_dir.clone(),
        data_dir.clone(),
        bc.clone() as Arc<dyn EventBroadcaster>,
        agent_registry,
    ));

    let scheduler = Arc::new(CronScheduler::new(Arc::new(|_| {})));

    let emitter = CronEventEmitter::new(bc.clone() as Arc<dyn EventBroadcaster>);
    let svc = CronService::new(CronServiceDeps {
        repo: cron_repo.clone(),
        agent_metadata_repo: agent_metadata_repo.clone(),
        assistant_definition_repo: assistant_definition_repo.clone(),
        assistant_overlay_repo: assistant_overlay_repo.clone(),
        scheduler,
        executor,
        emitter,
        data_dir,
    });

    seed_assistant_definition(
        &assistant_definition_repo,
        "asstdef_default",
        "assistant-default",
        "claude",
    )
    .await;
    seed_bare_assistant_definitions(&assistant_definition_repo).await;

    std::mem::forget(db);
    (svc, cron_repo, bc, stub_conv_repo, conv_service, agent_metadata_repo)
}

async fn setup_with_assistant_repos() -> (
    CronService,
    Arc<dyn ICronRepository>,
    Arc<MockBroadcaster>,
    Arc<StubConvRepo>,
    Arc<dyn IAssistantDefinitionRepository>,
    Arc<dyn IAssistantOverlayRepository>,
) {
    let db = init_database_memory().await.unwrap();
    let pool = db.pool().clone();
    let cron_repo: Arc<dyn ICronRepository> = Arc::new(SqliteCronRepository::new(pool.clone()));
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(SqliteAgentMetadataRepository::new(pool.clone()));
    let assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository> =
        Arc::new(SqliteAssistantDefinitionRepository::new(pool.clone()));
    let assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository> =
        Arc::new(SqliteAssistantOverlayRepository::new(pool.clone()));
    let assistant_preference_repo: Arc<dyn IAssistantPreferenceRepository> =
        Arc::new(SqliteAssistantPreferenceRepository::new(pool.clone()));
    let acp_session_repo: Arc<dyn IAcpSessionRepository> = Arc::new(SqliteAcpSessionRepository::new(pool));
    let bc = Arc::new(MockBroadcaster::new());
    let data_dir = std::env::temp_dir().join(format!("aionui-cron-test-{}", now_ms()));
    std::fs::create_dir_all(&data_dir).unwrap();

    struct StubSkillResolver;
    #[async_trait::async_trait]
    impl aionui_conversation::skill_resolver::SkillResolver for StubSkillResolver {
        async fn auto_inject_names(&self) -> Vec<String> {
            Vec::new()
        }

        async fn resolve_skills(
            &self,
            _names: &[String],
        ) -> Vec<aionui_conversation::skill_resolver::ResolvedAgentSkill> {
            Vec::new()
        }

        async fn link_workspace_skills(
            &self,
            _workspace: &std::path::Path,
            _rel_dirs: &[&str],
            _skills: &[aionui_conversation::skill_resolver::ResolvedAgentSkill],
        ) -> usize {
            0
        }
    }

    let stub_conv_repo = Arc::new(StubConvRepo::new());
    let stub_conv_repo_trait: Arc<dyn IConversationRepository> = stub_conv_repo.clone();
    let task_manager: Arc<dyn aionui_ai_agent::task_manager::IWorkerTaskManager> = Arc::new(StubTaskManager);
    let conv_service = Arc::new(ConversationService::new(
        std::env::temp_dir(),
        bc.clone() as Arc<dyn EventBroadcaster>,
        Arc::new(StubSkillResolver),
        Arc::clone(&task_manager),
        Arc::clone(&stub_conv_repo_trait),
        Arc::clone(&agent_metadata_repo),
        acp_session_repo,
    ));
    conv_service.with_assistant_definition_repo(assistant_definition_repo.clone());
    conv_service.with_assistant_state_repo(assistant_overlay_repo.clone());
    conv_service.with_assistant_preference_repo(assistant_preference_repo);
    let agent_registry = AgentRegistry::new(agent_metadata_repo.clone());
    agent_registry.hydrate().await.unwrap();
    let executor = Arc::new(JobExecutor::new(
        task_manager,
        stub_conv_repo_trait,
        conv_service,
        data_dir.clone(),
        data_dir.clone(),
        bc.clone() as Arc<dyn EventBroadcaster>,
        agent_registry,
    ));

    let scheduler = Arc::new(CronScheduler::new(Arc::new(|_| {})));
    let emitter = CronEventEmitter::new(bc.clone() as Arc<dyn EventBroadcaster>);
    let svc = CronService::new(CronServiceDeps {
        repo: cron_repo.clone(),
        agent_metadata_repo,
        assistant_definition_repo: assistant_definition_repo.clone(),
        assistant_overlay_repo: assistant_overlay_repo.clone(),
        scheduler,
        executor,
        emitter,
        data_dir,
    });

    seed_assistant_definition(
        &assistant_definition_repo,
        "asstdef_default",
        "assistant-default",
        "claude",
    )
    .await;
    seed_bare_assistant_definitions(&assistant_definition_repo).await;

    std::mem::forget(db);
    (
        svc,
        cron_repo,
        bc,
        stub_conv_repo,
        assistant_definition_repo,
        assistant_overlay_repo,
    )
}

fn make_create_req(name: &str, schedule: CronScheduleDto) -> CreateCronJobRequest {
    CreateCronJobRequest {
        name: name.into(),
        description: Some("test description".into()),
        schedule,
        prompt: None,
        message: Some("test message".into()),
        conversation_id: "conv_1".into(),
        conversation_title: Some("Test Conv".into()),
        created_by: "user".into(),
        execution_mode: None,
        queue_enabled: false,
        agent_config: Some(aionui_api_types::CronAgentConfigWriteDto {
            name: "Default Assistant".into(),
            cli_path: None,
            assistant_id: Some("assistant-default".into()),
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        }),
    }
}

async fn seed_assistant_definition(
    repo: &Arc<dyn IAssistantDefinitionRepository>,
    definition_id: &str,
    assistant_id: &str,
    agent_backend: &str,
) {
    let agent_id = seeded_agent_id(agent_backend);
    repo.upsert(&UpsertAssistantDefinitionParams {
        id: definition_id,
        assistant_id,
        source: "user",
        owner_type: "user",
        source_ref: Some(assistant_id),
        name: assistant_id,
        name_i18n: "{}",
        description: Some("test assistant"),
        description_i18n: "{}",
        avatar_type: "emoji",
        avatar_value: Some("🤖"),
        agent_id,
        rule_resource_type: "user_file",
        rule_resource_ref: None,
        recommended_prompts: "[]",
        recommended_prompts_i18n: "{}",
        default_model_mode: "auto",
        default_model_value: None,
        default_permission_mode: "auto",
        default_permission_value: None,
        default_thought_level_mode: "auto",
        default_thought_level_value: None,
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

async fn seed_bare_assistant_definitions(repo: &Arc<dyn IAssistantDefinitionRepository>) {
    for (definition_id, assistant_id, agent_backend) in [
        ("asstdef_bare_gemini", "bare:cc126dd5", "gemini"),
        ("asstdef_bare_codex", "bare:8e1acf31", "codex"),
        ("asstdef_bare_aionrs", "bare:632f31d2", "aionrs"),
    ] {
        seed_assistant_definition(repo, definition_id, assistant_id, agent_backend).await;
    }
}

async fn seed_assistant_overlay(
    repo: &Arc<dyn IAssistantOverlayRepository>,
    definition_id: &str,
    agent_backend_override: Option<&str>,
) {
    let agent_id_override = agent_backend_override.map(seeded_agent_id);
    repo.upsert(&UpsertAssistantOverlayParams {
        assistant_definition_id: definition_id,
        enabled: true,
        sort_order: 0,
        agent_id_override,
        last_used_at: None,
    })
    .await
    .unwrap();
}

fn seeded_agent_id(value: &str) -> &str {
    match value {
        "claude" => "2d23ff1c",
        "codex" => "8e1acf31",
        "gemini" => "cc126dd5",
        "aionrs" => "632f31d2",
        other => other,
    }
}

fn every_60s() -> CronScheduleDto {
    CronScheduleDto::Every {
        every_ms: 60000,
        description: Some("every minute".into()),
    }
}

fn at_future(offset_ms: i64) -> CronScheduleDto {
    CronScheduleDto::At {
        at_ms: now_ms() + offset_ms,
        description: Some("once".into()),
    }
}

fn cron_every_5min() -> CronScheduleDto {
    CronScheduleDto::Cron {
        expr: "0 */5 * * * *".into(),
        tz: None,
        description: Some("every 5 min".into()),
    }
}

// ── CJ-1: Create cron job ──────────────────────────────────────────

#[tokio::test]
async fn cj1_create_cron_job() {
    let (svc, _, bc) = setup().await;
    let req = make_create_req("Daily Report", every_60s());

    let job = svc.add_job(req).await.unwrap();

    assert!(job.id.starts_with("cron_"));
    assert_eq!(job.name, "Daily Report");
    assert!(job.enabled);
    assert!(job.next_run_at.is_some());
    assert_eq!(job.run_count, 0);

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-created");
}

#[tokio::test]
async fn create_job_allows_missing_task_description() {
    let (svc, cron_repo, _) = setup().await;
    let mut req = make_create_req("No Description", every_60s());
    req.description = None;

    let job = svc.add_job(req).await.unwrap();

    assert_eq!(job.description, None);
    let row = cron_repo.get_by_id(&job.id).await.unwrap().unwrap();
    assert_eq!(row.description, None);
}

#[tokio::test]
async fn create_job_strips_legacy_agent_ids_when_assistant_id_present() {
    let (svc, _, _, _, definition_repo, _) = setup_with_assistant_repos().await;
    seed_assistant_definition(&definition_repo, "asstdef_assistant_1", "assistant-1", "claude").await;
    let mut req = make_create_req("Assistant Only Create", every_60s());
    req.agent_config = Some(aionui_api_types::CronAgentConfigWriteDto {
        name: "Helper".into(),
        cli_path: None,
        assistant_id: Some("assistant-1".into()),
        mode: Some("default".into()),
        model_id: Some("claude-sonnet-4".into()),
        model: None,
        config_options: None,
        workspace: None,
    });

    let job = svc.add_job(req).await.unwrap();
    let config = job.agent_config.expect("agent config");

    assert_eq!(config.assistant_id.as_deref(), Some("assistant-1"));
    assert!(config.custom_agent_id.is_none());
    assert!(config.is_preset.is_none());
}

#[tokio::test]
async fn create_job_uses_assistant_full_auto_mode_instead_of_requested_mode() {
    let (svc, _, _) = setup().await;
    let req = make_create_req("Full Auto Mode", every_60s());

    let job = svc.add_job(req).await.unwrap();
    let config = job.agent_config.expect("agent config");

    assert_eq!(config.assistant_id.as_deref(), Some("assistant-default"));
    assert_eq!(config.mode.as_deref(), Some("bypassPermissions"));
}

#[tokio::test]
async fn create_job_derives_assistant_runtime_without_backend_hint() {
    let (svc, _, _) = setup().await;

    let mut req = make_create_req("Stale Backend Hint", every_60s());
    req.agent_config = Some(aionui_api_types::CronAgentConfigWriteDto {
        name: "Stale Backend Hint".into(),
        cli_path: None,
        assistant_id: Some("assistant-default".into()),
        mode: Some("default".into()),
        model_id: Some("claude-sonnet-4".into()),
        model: None,
        config_options: None,
        workspace: None,
    });

    let job = svc.add_job(req).await.unwrap();

    assert_eq!(job.agent_type, "acp");
}

#[tokio::test]
async fn create_job_requires_assistant_id_for_new_jobs() {
    let (svc, _, _) = setup().await;
    let mut req = make_create_req("Missing Runtime Type", every_60s());
    req.agent_config = None;

    let err = svc.add_job(req).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidAgentConfig(_)));
    assert!(err.to_string().contains("assistant_id is required for new cron jobs"));
}

#[tokio::test]
async fn create_job_derives_runtime_type_from_aionrs_assistant() {
    let (svc, _, _, _, definition_repo, _) = setup_with_assistant_repos().await;
    seed_assistant_definition(&definition_repo, "asstdef_runtime_aionrs", "assistant-aionrs", "aionrs").await;

    let mut req = make_create_req("Assistant Aionrs", every_60s());
    req.agent_config = Some(aionui_api_types::CronAgentConfigWriteDto {
        name: "Aionrs Assistant".into(),
        cli_path: None,
        assistant_id: Some("assistant-aionrs".into()),
        mode: Some("yolo".into()),
        model_id: Some("gemini-3.1-pro-preview".into()),
        model: Some(ProviderWithModel {
            provider_id: "provider-gemini".into(),
            model: "gemini-3.1-pro-preview".into(),
            use_model: None,
        }),
        config_options: None,
        workspace: None,
    });

    let job = svc.add_job(req).await.unwrap();

    assert_eq!(job.agent_type, "aionrs");
}

#[tokio::test]
async fn create_job_derives_runtime_type_from_assistant_overlay_override() {
    let (svc, _, _, _, definition_repo, overlay_repo) = setup_with_assistant_repos().await;
    seed_assistant_definition(
        &definition_repo,
        "asstdef_runtime_override",
        "assistant-override",
        "claude",
    )
    .await;
    seed_assistant_overlay(&overlay_repo, "asstdef_runtime_override", Some("aionrs")).await;

    let mut req = make_create_req("Assistant Override", every_60s());
    req.agent_config = Some(aionui_api_types::CronAgentConfigWriteDto {
        name: "Override Assistant".into(),
        cli_path: None,
        assistant_id: Some("assistant-override".into()),
        mode: Some("acceptEdits".into()),
        model_id: Some("gpt-5.4".into()),
        model: Some(ProviderWithModel {
            provider_id: "provider-openai".into(),
            model: "gpt-5.4".into(),
            use_model: None,
        }),
        config_options: None,
        workspace: None,
    });

    let job = svc.add_job(req).await.unwrap();

    assert_eq!(job.agent_type, "aionrs");
}

#[tokio::test]
async fn create_job_allows_assistant_backed_acp_jobs_without_backend_hint() {
    let (svc, _, _, _, definition_repo, _) = setup_with_assistant_repos().await;
    seed_assistant_definition(&definition_repo, "asstdef_assistant_2", "assistant-2", "claude").await;

    let mut req = make_create_req("Assistant Without Backend", every_60s());
    req.agent_config = Some(aionui_api_types::CronAgentConfigWriteDto {
        name: "Helper".into(),
        cli_path: None,
        assistant_id: Some("assistant-2".into()),
        mode: Some("default".into()),
        model_id: Some("claude-sonnet-4".into()),
        model: None,
        config_options: None,
        workspace: None,
    });

    let job = svc.add_job(req).await.unwrap();
    let config = job.agent_config.expect("agent config");

    assert_eq!(job.agent_type, "acp");
    assert_eq!(config.assistant_id.as_deref(), Some("assistant-2"));
}

#[tokio::test]
async fn create_job_rejects_backend_fallback_when_assistant_id_cannot_resolve() {
    let (svc, _, _) = setup().await;

    let mut req = make_create_req("Assistant Missing", every_60s());
    req.agent_config = Some(aionui_api_types::CronAgentConfigWriteDto {
        name: "Helper".into(),
        cli_path: None,
        assistant_id: Some("missing-assistant".into()),
        mode: Some("default".into()),
        model_id: Some("claude-sonnet-4".into()),
        model: None,
        config_options: None,
        workspace: None,
    });

    let err = svc
        .add_job(req)
        .await
        .expect_err("missing assistant must not fall back to backend");

    assert!(matches!(err, aionui_cron::error::CronError::InvalidAgentConfig(_)));
    assert!(err.to_string().contains("missing-assistant"), "unexpected error: {err}");
}

// ── CJ-2: Create three schedule types ──────────────────────────────

#[tokio::test]
async fn cj2_create_three_schedule_types() {
    let (svc, _, _) = setup().await;
    let now = now_ms();

    let at_job = svc
        .add_job(make_create_req("At Job", at_future(3600000)))
        .await
        .unwrap();
    assert!(at_job.next_run_at.unwrap() > now);

    let every_job = svc.add_job(make_create_req("Every Job", every_60s())).await.unwrap();
    let next = every_job.next_run_at.unwrap();
    assert!((next - now - 60000).abs() < 2000);

    let cron_job = svc
        .add_job(make_create_req("Cron Job", cron_every_5min()))
        .await
        .unwrap();
    assert!(cron_job.next_run_at.unwrap() > now);
}

// ── CJ-4: Get single job ──────────────────────────────────────────

#[tokio::test]
async fn cj4_get_single_job() {
    let (svc, _, _) = setup().await;
    let created = svc.add_job(make_create_req("Get Test", every_60s())).await.unwrap();

    let fetched = svc.get_job(&created.id).await.unwrap();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.name, "Get Test");
}

// ── CJ-5: Get nonexistent job ─────────────────────────────────────

#[tokio::test]
async fn cj5_get_nonexistent_job() {
    let (svc, _, _) = setup().await;
    let err = svc.get_job("cron_nonexistent").await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::JobNotFound(_)));
}

// ── CJ-6: List all jobs ───────────────────────────────────────────

#[tokio::test]
async fn cj6_list_all_jobs() {
    let (svc, _, _) = setup().await;
    for i in 0..3 {
        svc.add_job(make_create_req(&format!("Job {i}"), every_60s()))
            .await
            .unwrap();
    }

    let jobs = svc.list_jobs(&ListCronJobsQuery::default()).await.unwrap();
    assert!(jobs.len() >= 3);
}

#[tokio::test]
async fn list_jobs_allows_legacy_custom_agent_id_without_assistant_id() {
    let (svc, cron_repo, _) = setup().await;
    cron_repo
        .insert(&CronJobRow {
            id: "cron_legacy_custom_agent".into(),
            name: "Legacy custom agent job".into(),
            enabled: true,
            schedule_kind: "every".into(),
            schedule_value: "60000".into(),
            schedule_tz: None,
            schedule_description: Some("every minute".into()),
            payload_message: "ping".into(),
            execution_mode: "new_conversation".into(),
            agent_config: Some(
                serde_json::json!({
                    "name": "Legacy assistant",
                    "custom_agent_id": "assistant-default",
                    "is_preset": true
                })
                .to_string(),
            ),
            conversation_id: "conv_legacy".into(),
            conversation_title: None,
            created_by: "user".into(),
            skill_content: None,
            description: None,
            created_at: now_ms(),
            updated_at: now_ms(),
            next_run_at: None,
            last_run_at: None,
            last_status: None,
            last_error: None,
            run_count: 0,
            retry_count: 0,
            max_retries: 3,
            queue_enabled: false,
        })
        .await
        .unwrap();

    let jobs = svc.list_jobs(&ListCronJobsQuery::default()).await.unwrap();

    let legacy = jobs
        .iter()
        .find(|job| job.id == "cron_legacy_custom_agent")
        .expect("legacy job should be listed");
    assert_eq!(legacy.agent_type, "acp");
}

// ── CJ-7: List by conversation ────────────────────────────────────

#[tokio::test]
async fn cj7_list_by_conversation() {
    let (svc, _, _) = setup().await;

    let mut req1 = make_create_req("Job A", every_60s());
    req1.conversation_id = "conv_target".into();
    svc.add_job(req1).await.unwrap();

    let mut req2 = make_create_req("Job B", every_60s());
    req2.conversation_id = "conv_target".into();
    svc.add_job(req2).await.unwrap();

    let mut req3 = make_create_req("Job C", every_60s());
    req3.conversation_id = "conv_other".into();
    svc.add_job(req3).await.unwrap();

    let query = ListCronJobsQuery {
        conversation_id: Some("conv_target".into()),
    };
    let jobs = svc.list_jobs(&query).await.unwrap();
    assert_eq!(jobs.len(), 2);
}

#[tokio::test]
async fn cj7b_add_job_binds_existing_conversation_to_job() {
    let (svc, _, _, conv_repo) = setup_with_conv_repo().await;

    let mut req = make_create_req("Bound Existing Conversation", every_60s());
    req.conversation_id = "conv_existing_bind".into();

    let job = svc.add_job(req).await.unwrap();

    let bound = conv_repo.get("conv_existing_bind").await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&bound.extra).unwrap();
    assert_eq!(extra["cron_job_id"], job.id);
    assert_eq!(extra["cronJobId"], job.id);

    let linked = conv_repo.list_by_cron_job("user_1", &job.id).await.unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].id, "conv_existing_bind");
}

// ── CJ-8: Update job ──────────────────────────────────────────────

#[tokio::test]
async fn cj8_update_job() {
    let (svc, _, bc) = setup().await;
    let created = svc.add_job(make_create_req("Original", every_60s())).await.unwrap();
    bc.take_events();

    let req = UpdateCronJobRequest {
        name: Some("Updated Name".into()),
        description: Some("Updated description".into()),
        enabled: Some(false),
        schedule: None,
        message: None,
        execution_mode: None,
        agent_config: None,
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let updated = svc.update_job(&created.id, req).await.unwrap();
    assert_eq!(updated.name, "Updated Name");
    assert_eq!(updated.description.as_deref(), Some("Updated description"));
    assert!(!updated.enabled);
    assert!(updated.updated_at >= created.created_at);

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-updated");
}

#[tokio::test]
async fn update_existing_conversation_job_rejects_agent_config_changes() {
    let (svc, _, _) = setup().await;
    let created = svc
        .add_job(make_create_req("Existing Conversation Assistant Lock", every_60s()))
        .await
        .unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: None,
        agent_config: Some(aionui_api_types::CronAgentConfigWriteDto {
            name: "Other Assistant".into(),
            cli_path: None,
            assistant_id: Some("assistant-default".into()),
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        }),
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let err = svc.update_job(&created.id, req).await.unwrap_err();
    assert!(
        matches!(err, aionui_cron::error::CronError::InvalidAgentConfig(message) if message.contains("ongoing conversation"))
    );
}

#[tokio::test]
async fn update_existing_conversation_job_rejects_agent_config_even_when_switching_to_new_conversation() {
    let (svc, _, _) = setup().await;
    let created = svc
        .add_job(make_create_req(
            "Existing Conversation Assistant Lock Mode Switch",
            every_60s(),
        ))
        .await
        .unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: Some("new_conversation".into()),
        agent_config: Some(aionui_api_types::CronAgentConfigWriteDto {
            name: "Other Assistant".into(),
            cli_path: None,
            assistant_id: Some("assistant-default".into()),
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        }),
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let err = svc.update_job(&created.id, req).await.unwrap_err();
    assert!(
        matches!(err, aionui_cron::error::CronError::InvalidAgentConfig(message) if message.contains("ongoing conversation"))
    );
}

#[tokio::test]
async fn update_existing_job_to_new_conversation_removes_previous_conversation_binding() {
    use aionui_common::OnConversationDelete;

    let (svc, cron_repo, _, conv_repo) = setup_with_conv_repo().await;
    let mut create_req = make_create_req("Mode Switch Clears Binding", every_60s());
    create_req.conversation_id = "conv_mode_switch".into();
    create_req.execution_mode = Some("existing".into());
    let created = svc.add_job(create_req).await.unwrap();

    let bound_before = conv_repo.get("conv_mode_switch").await.unwrap().unwrap();
    let extra_before: serde_json::Value = serde_json::from_str(&bound_before.extra).unwrap();
    assert_eq!(extra_before["cron_job_id"], created.id);
    assert_eq!(extra_before["cronJobId"], created.id);

    let updated = svc
        .update_job(
            &created.id,
            UpdateCronJobRequest {
                name: None,
                description: None,
                enabled: None,
                schedule: None,
                message: None,
                execution_mode: Some("new_conversation".into()),
                agent_config: None,
                conversation_title: None,
                max_retries: None,
                queue_enabled: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.execution_mode.as_str(), "new_conversation");

    let row = cron_repo.get_by_id(&created.id).await.unwrap().unwrap();
    assert_eq!(row.execution_mode, "new_conversation");
    assert_eq!(row.conversation_id, "");
    assert!(row.conversation_title.is_none());

    let bound_after = conv_repo.get("conv_mode_switch").await.unwrap().unwrap();
    let extra_after: serde_json::Value = serde_json::from_str(&bound_after.extra).unwrap();
    assert!(extra_after.get("cron_job_id").is_none());
    assert!(extra_after.get("cronJobId").is_none());

    svc.on_conversation_deleted("conv_mode_switch").await;
    assert!(
        svc.get_job(&created.id).await.is_ok(),
        "deleting the previous existing-mode conversation must not delete the switched new-conversation job"
    );
}

#[tokio::test]
async fn update_existing_job_to_new_conversation_clears_previous_auto_workspace() {
    let (svc, cron_repo, _, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let conversation_id = format!("conv_mode_switch_workspace_{}", now_ms());
    let auto_workspace_path = std::env::temp_dir()
        .join("conversations")
        .join(format!("acp-temp-{conversation_id}"));
    std::fs::create_dir_all(&auto_workspace_path).unwrap();
    let auto_workspace = auto_workspace_path.to_string_lossy().to_string();
    conv_repo.set_conversation_extra(
        &conversation_id,
        serde_json::json!({
            "workspace": auto_workspace,
        }),
    );

    let mut create_req = make_create_req("Mode Switch Clears Workspace", every_60s());
    create_req.conversation_id = conversation_id.clone();
    create_req.execution_mode = Some("existing".into());
    create_req.agent_config.as_mut().unwrap().workspace = Some(auto_workspace);
    let created = svc.add_job(create_req).await.unwrap();

    let bound_conversation = conv_repo.get(&conversation_id).await.unwrap().unwrap();
    assert!(
        conv_service
            .auto_workspace_to_delete_for_row(&bound_conversation, &conversation_id)
            .is_some(),
        "test setup should use a workspace ConversationService will delete"
    );

    svc.update_job(
        &created.id,
        UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: None,
            schedule: None,
            message: None,
            execution_mode: Some("new_conversation".into()),
            agent_config: None,
            conversation_title: None,
            max_retries: None,
            queue_enabled: None,
        },
    )
    .await
    .unwrap();

    let row = cron_repo.get_by_id(&created.id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert!(
        config.workspace.is_none(),
        "switching away from an existing conversation should drop that conversation's auto workspace"
    );
}

#[tokio::test]
async fn update_existing_job_to_new_conversation_preserves_custom_workspace() {
    let (svc, cron_repo, _, conv_repo) = setup_with_conv_repo().await;
    let conversation_id = format!("conv_mode_switch_custom_workspace_{}", now_ms());
    let custom_workspace = ensure_named_workspace_path(&format!("aionui-cron-switch-custom-{conversation_id}"));
    conv_repo.set_conversation_extra(
        &conversation_id,
        serde_json::json!({
            "workspace": custom_workspace,
        }),
    );

    let mut create_req = make_create_req("Mode Switch Preserves Workspace", every_60s());
    create_req.conversation_id = conversation_id.clone();
    create_req.execution_mode = Some("existing".into());
    create_req.agent_config.as_mut().unwrap().workspace = Some(custom_workspace.clone());
    let created = svc.add_job(create_req).await.unwrap();

    svc.update_job(
        &created.id,
        UpdateCronJobRequest {
            name: None,
            description: None,
            enabled: None,
            schedule: None,
            message: None,
            execution_mode: Some("new_conversation".into()),
            agent_config: None,
            conversation_title: None,
            max_retries: None,
            queue_enabled: None,
        },
    )
    .await
    .unwrap();

    let row = cron_repo.get_by_id(&created.id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert_eq!(config.workspace.as_deref(), Some(custom_workspace.as_str()));
}

#[tokio::test]
async fn update_team_conversation_job_rejects_execution_mode_change() {
    let (svc, _, _, conv_repo) = setup_with_conv_repo().await;
    let mut create_req = make_create_req("Team Cron Mode Lock", every_60s());
    create_req.conversation_id = "conv_team_cron".into();
    conv_repo.set_conversation_extra(
        "conv_team_cron",
        serde_json::json!({
            "team_id": "team-1",
            "workspace": ensure_named_workspace_path("aionui-cron-service-team-workspace")
        }),
    );
    let created = svc.add_job(create_req).await.unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: Some("new_conversation".into()),
        agent_config: None,
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let err = svc.update_job(&created.id, req).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidExecutionMode(message) if message.contains("Team")));
}

#[tokio::test]
async fn update_job_strips_legacy_agent_ids_when_assistant_id_present() {
    let (svc, _, _, _, definition_repo, _) = setup_with_assistant_repos().await;
    seed_assistant_definition(&definition_repo, "asstdef_update_assistant_1", "assistant-1", "claude").await;
    let mut create_req = make_create_req("Assistant Only Update", every_60s());
    create_req.execution_mode = Some("new_conversation".into());
    let created = svc.add_job(create_req).await.unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: Some("new_conversation".into()),
        agent_config: Some(aionui_api_types::CronAgentConfigWriteDto {
            name: "Helper".into(),
            cli_path: None,
            assistant_id: Some("assistant-1".into()),
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        }),
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let updated = svc.update_job(&created.id, req).await.unwrap();
    let config = updated.agent_config.expect("agent config");

    assert_eq!(config.assistant_id.as_deref(), Some("assistant-1"));
    assert!(config.custom_agent_id.is_none());
    assert!(config.is_preset.is_none());
}

#[tokio::test]
async fn update_job_rejects_when_assistant_id_cannot_resolve() {
    let (svc, _, _) = setup().await;
    let mut create_req = make_create_req("Assistant Missing Update", every_60s());
    create_req.execution_mode = Some("new_conversation".into());
    let created = svc.add_job(create_req).await.unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: Some("new_conversation".into()),
        agent_config: Some(aionui_api_types::CronAgentConfigWriteDto {
            name: "Helper".into(),
            cli_path: None,
            assistant_id: Some("missing-assistant".into()),
            mode: Some("default".into()),
            model_id: Some("claude-sonnet-4".into()),
            model: None,
            config_options: None,
            workspace: None,
        }),
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let err = svc
        .update_job(&created.id, req)
        .await
        .expect_err("missing assistant must not fall back to backend");

    assert!(matches!(err, aionui_cron::error::CronError::InvalidAgentConfig(_)));
    assert!(
        err.to_string().contains("assistant 'missing-assistant' not found"),
        "unexpected error: {err}"
    );
}

// ── CJ-9: Update schedule type ────────────────────────────────────

#[tokio::test]
async fn cj9_update_schedule_type() {
    let (svc, _, _) = setup().await;
    let created = svc
        .add_job(make_create_req("Schedule Change", every_60s()))
        .await
        .unwrap();

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: Some(cron_every_5min()),
        message: None,
        execution_mode: None,
        agent_config: None,
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };

    let updated = svc.update_job(&created.id, req).await.unwrap();
    assert!(matches!(
        updated.schedule,
        aionui_cron::types::CronSchedule::Cron { .. }
    ));
    assert!(updated.next_run_at.is_some());
}

// ── CJ-10: Update nonexistent job ─────────────────────────────────

#[tokio::test]
async fn cj10_update_nonexistent() {
    let (svc, _, _) = setup().await;
    let req = UpdateCronJobRequest {
        name: Some("x".into()),
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: None,
        agent_config: None,
        conversation_title: None,
        max_retries: None,
        queue_enabled: None,
    };
    let err = svc.update_job("cron_nonexistent", req).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::JobNotFound(_)));
}

// ── CJ-11: Delete job ─────────────────────────────────────────────

#[tokio::test]
async fn cj11_delete_job() {
    let (svc, _, bc) = setup().await;
    let created = svc.add_job(make_create_req("To Delete", every_60s())).await.unwrap();
    bc.take_events();

    svc.remove_job(&created.id).await.unwrap();

    let err = svc.get_job(&created.id).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::JobNotFound(_)));

    let events = bc.take_events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].name, "cron.job-removed");
}

// ── CJ-12: Delete nonexistent ─────────────────────────────────────

#[tokio::test]
async fn cj12_delete_nonexistent() {
    let (svc, _, _) = setup().await;
    let err = svc.remove_job("cron_nonexistent").await.unwrap_err();
    assert!(matches!(
        err,
        aionui_cron::error::CronError::Database(aionui_db::DbError::NotFound(_))
    ));
}

// ── SK-1: Save skill ──────────────────────────────────────────────

#[tokio::test]
async fn sk1_save_skill() {
    let (svc, _, _) = setup().await;
    let job = svc.add_job(make_create_req("Skill Job", every_60s())).await.unwrap();

    let req = SaveCronSkillRequest {
        content: "---\nname: test\ndescription: test skill\n---\nDo something".into(),
    };
    svc.save_skill(&job.id, req).await.unwrap();
}

#[tokio::test]
async fn sk1_1_save_skill_marks_related_skill_suggest_artifacts_saved() {
    let (svc, _, bc, conv_repo) = setup_with_conv_repo().await;
    let job = svc
        .add_job(make_create_req("Skill Artifact Job", every_60s()))
        .await
        .unwrap();

    conv_repo.upsert_artifact_row(aionui_db::ConversationArtifactRow {
        id: format!("conv_1:skill_suggest:{}", job.id),
        conversation_id: "conv_1".into(),
        cron_job_id: Some(job.id.clone()),
        kind: "skill_suggest".into(),
        status: "active".into(),
        payload: serde_json::json!({
            "cron_job_id": job.id,
            "name": "daily-report",
            "description": "Daily report",
            "skillContent": "---\nname: daily-report\n---\nUse it."
        })
        .to_string(),
        created_at: 1000,
        updated_at: 1000,
    });

    svc.save_skill(
        &job.id,
        SaveCronSkillRequest {
            content: "---\nname: daily-report\ndescription: Daily report\n---\nUse it.".into(),
        },
    )
    .await
    .unwrap();

    let artifacts = conv_repo.artifacts();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].status, "saved");

    let events = bc.take_events();
    let saved_event = events
        .iter()
        .find(|event| {
            event.name == "conversation.artifact"
                && event.data["id"] == artifacts[0].id
                && event.data["status"] == "saved"
        })
        .expect("save_skill should broadcast saved artifact upsert");
    assert_eq!(saved_event.data["conversation_id"], "conv_1");
}

// ── SK-2: Has skill (true) ────────────────────────────────────────

#[tokio::test]
async fn sk2_has_skill_true() {
    let (svc, _, _) = setup().await;
    let job = svc.add_job(make_create_req("Skill Check", every_60s())).await.unwrap();

    svc.save_skill(
        &job.id,
        SaveCronSkillRequest {
            content: "---\nname: x\n---\nContent".into(),
        },
    )
    .await
    .unwrap();

    let resp = svc.has_skill(&job.id).await.unwrap();
    assert!(resp.has_skill);
}

// ── SK-3: Has skill (false) ───────────────────────────────────────

#[tokio::test]
async fn sk3_has_skill_false() {
    let (svc, _, _) = setup().await;
    let job = svc.add_job(make_create_req("No Skill", every_60s())).await.unwrap();

    let resp = svc.has_skill(&job.id).await.unwrap();
    assert!(!resp.has_skill);
}

// ── SK-4: Save empty skill ────────────────────────────────────────

#[tokio::test]
async fn sk4_save_empty_skill() {
    let (svc, _, _) = setup().await;
    let job = svc.add_job(make_create_req("Empty Skill", every_60s())).await.unwrap();

    let err = svc
        .save_skill(&job.id, SaveCronSkillRequest { content: "".into() })
        .await
        .unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidSkillContent(_)));
}

// ── SK-5: Save placeholder skill ──────────────────────────────────

#[tokio::test]
async fn sk5_save_placeholder_skill() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(make_create_req("Placeholder Skill", every_60s()))
        .await
        .unwrap();

    let err = svc
        .save_skill(
            &job.id,
            SaveCronSkillRequest {
                content: "TODO: fill in later".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidSkillContent(_)));
}

// ── SK-6: Save skill for nonexistent job ──────────────────────────

#[tokio::test]
async fn sk6_save_skill_nonexistent() {
    let (svc, _, _) = setup().await;
    let err = svc
        .save_skill(
            "cron_nonexistent",
            SaveCronSkillRequest {
                content: "---\nname: x\n---\nOk".into(),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::JobNotFound(_)));
}

// ── SK-7: Delete skill on job removal ─────────────────────────────

#[tokio::test]
async fn sk7_delete_cleans_skill() {
    let (svc, _, _) = setup().await;
    let job = svc
        .add_job(make_create_req("Skill Cleanup", every_60s()))
        .await
        .unwrap();
    svc.save_skill(
        &job.id,
        SaveCronSkillRequest {
            content: "---\nname: x\n---\nContent".into(),
        },
    )
    .await
    .unwrap();

    svc.remove_job(&job.id).await.unwrap();

    let err = svc.has_skill(&job.id).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::JobNotFound(_)));
}

// ── SC-3: Every type next_run ─────────────────────────────────────

#[tokio::test]
async fn sc3_every_type_next_run() {
    let (svc, _, _) = setup().await;
    let now = now_ms();
    let job = svc.add_job(make_create_req("Every 60s", every_60s())).await.unwrap();

    let next = job.next_run_at.unwrap();
    let diff = (next - now - 60000).abs();
    assert!(diff < 2000, "expected next_run ≈ now+60000, diff={diff}");
}

// ── SC-5: Invalid cron expression ─────────────────────────────────

#[tokio::test]
async fn sc5_invalid_cron_expression() {
    let (svc, _, _) = setup().await;
    let req = make_create_req(
        "Invalid Cron",
        CronScheduleDto::Cron {
            expr: "invalid cron".into(),
            tz: None,
            description: None,
        },
    );
    let err = svc.add_job(req).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidCronExpression(_)));
}

// ── SC-6: Cron with timezone ──────────────────────────────────────

#[tokio::test]
async fn sc6_cron_with_timezone() {
    let (svc, _, _) = setup().await;
    let now = now_ms();
    let req = make_create_req(
        "Shanghai Job",
        CronScheduleDto::Cron {
            expr: "0 0 9 * * *".into(),
            tz: Some("Asia/Shanghai".into()),
            description: None,
        },
    );
    let job = svc.add_job(req).await.unwrap();
    assert!(job.next_run_at.unwrap() > now);
}

// ── SC-7: Every zero interval ─────────────────────────────────────

#[tokio::test]
async fn sc7_every_zero_interval() {
    let (svc, _, _) = setup().await;
    let req = make_create_req(
        "Zero Interval",
        CronScheduleDto::Every {
            every_ms: 0,
            description: None,
        },
    );
    let err = svc.add_job(req).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidSchedule(_)));
}

// ── SC-8: Every negative interval ─────────────────────────────────

#[tokio::test]
async fn sc8_every_negative_interval() {
    let (svc, _, _) = setup().await;
    let req = make_create_req(
        "Negative Interval",
        CronScheduleDto::Every {
            every_ms: -1000,
            description: None,
        },
    );
    let err = svc.add_job(req).await.unwrap_err();
    assert!(matches!(err, aionui_cron::error::CronError::InvalidSchedule(_)));
}

// ── OC-1: Init preserves lazy-bind "existing" jobs with empty conversation_id ─────

#[tokio::test]
async fn oc1_init_preserves_lazy_existing_jobs() {
    // "existing + empty conversation_id" is a legitimate lazy-binding job:
    // the frontend creates a cron from the standalone cron page before any
    // conversation exists, and the first execution materializes it. Those
    // jobs must survive init, not be cleaned up as orphans.
    let (svc, _repo, _) = setup().await;

    let mut req = make_create_req("Lazy Existing", every_60s());
    req.conversation_id = "".into();
    req.execution_mode = Some("existing".into());
    let lazy = svc.add_job(req).await.unwrap();

    let normal_req = make_create_req("Normal", every_60s());
    let normal = svc.add_job(normal_req).await.unwrap();

    svc.init().await;

    let found_lazy = svc.get_job(&lazy.id).await;
    assert!(found_lazy.is_ok(), "lazy-bind existing job should survive init");

    let found = svc.get_job(&normal.id).await;
    assert!(found.is_ok());
}

// NewConversation jobs don't depend on any existing conversation — they
// create one on every run. They must never be cleaned up as orphans.
#[tokio::test]
async fn oc1b_init_preserves_new_conversation_jobs() {
    let (svc, _repo, _) = setup().await;

    let mut empty_req = make_create_req("New-conv empty", every_60s());
    empty_req.conversation_id = "".into();
    empty_req.execution_mode = Some("new_conversation".into());
    let empty = svc.add_job(empty_req).await.unwrap();

    let mut stale_req = make_create_req("New-conv with stale id", every_60s());
    stale_req.conversation_id = "conv-that-no-longer-exists".into();
    stale_req.execution_mode = Some("new_conversation".into());
    let stale = svc.add_job(stale_req).await.unwrap();

    svc.init().await;

    assert!(
        svc.get_job(&empty.id).await.is_ok(),
        "empty new_conversation job must survive"
    );
    assert!(
        svc.get_job(&stale.id).await.is_ok(),
        "new_conversation job with stale id must survive"
    );
}

#[tokio::test]
async fn oc2_init_preserves_existing_jobs_with_missing_conversation() {
    let (svc, _repo, _) = setup().await;

    let mut missing_req = make_create_req("Missing Conversation", every_60s());
    missing_req.conversation_id = "missing-conv-1".into();
    let missing = svc.add_job(missing_req).await.unwrap();

    let mut normal_req = make_create_req("Existing Conversation", every_60s());
    normal_req.conversation_id = "conv-existing".into();
    let normal = svc.add_job(normal_req).await.unwrap();

    svc.init().await;

    let missing_found = svc.get_job(&missing.id).await;
    assert!(
        missing_found.is_ok(),
        "existing job with deleted conversation should survive init and recover on next execution"
    );

    let found = svc.get_job(&normal.id).await;
    assert!(found.is_ok());
}

#[tokio::test]
async fn existing_job_with_missing_conversation_run_now_creates_replacement_conversation() {
    let (svc, _repo, _bc, conv_repo) = setup_with_conv_repo().await;

    let mut req = make_create_req("Missing Existing RunNow", every_60s());
    req.conversation_id = "missing-conv-run-now".into();
    req.execution_mode = Some("existing".into());
    let job = svc.add_job(req).await.unwrap();

    let response = svc.run_now(&job.id).await.unwrap();

    assert_ne!(response.conversation_id, "missing-conv-run-now");
    assert!(
        conv_repo.get(&response.conversation_id).await.unwrap().is_some(),
        "run-now should create a replacement conversation for an existing job whose previous conversation was deleted"
    );

    let rebound = svc.get_job(&job.id).await.unwrap();
    assert_eq!(
        rebound.conversation_id, response.conversation_id,
        "existing-mode replacement conversations must bind before the async turn finishes"
    );
}

#[tokio::test]
async fn run_now_on_running_existing_conversation_returns_active_conversation_without_new_execution() {
    let (svc, cron_repo, bc, _conv_repo, conv_service) = setup_with_conv_runtime().await;

    let mut req = make_create_req("Running Existing RunNow", every_60s());
    req.conversation_id = "conv-running-run-now".into();
    req.execution_mode = Some("existing".into());
    let job = svc.add_job(req).await.unwrap();
    bc.take_events();

    let claim = conv_service
        .runtime_state()
        .try_claim_turn(&job.conversation_id, "turn-active")
        .expect("runtime claim should succeed");

    let response = svc.run_now(&job.id).await.unwrap();

    assert_eq!(response.conversation_id, job.conversation_id);
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let row = cron_repo.get_by_id(&job.id).await.unwrap().unwrap();
    assert_eq!(row.run_count, 0);
    assert!(row.last_status.is_none());
    assert!(
        bc.take_events().iter().all(|event| event.name != "cron.job-executed"),
        "clicking run-now on an already running conversation should only return it for navigation"
    );

    drop(claim);
}

// ── Delete skill explicitly ───────────────────────────────────────

#[tokio::test]
async fn delete_skill_clears_content() {
    let (svc, _, _) = setup().await;
    let job = svc.add_job(make_create_req("Del Skill", every_60s())).await.unwrap();

    svc.save_skill(
        &job.id,
        SaveCronSkillRequest {
            content: "---\nname: x\n---\nOk".into(),
        },
    )
    .await
    .unwrap();
    assert!(svc.has_skill(&job.id).await.unwrap().has_skill);

    svc.delete_skill(&job.id).await.unwrap();
    assert!(!svc.has_skill(&job.id).await.unwrap().has_skill);
}

fn conversation_cron_request(message: &str) -> CreateConversationCronRequest {
    CreateConversationCronRequest {
        name: "Agent Helper Job".into(),
        schedule: "0 */10 * * * *".into(),
        schedule_description: "every 10 min".into(),
        message: message.into(),
    }
}

#[tokio::test]
async fn create_for_conversation_helper_creates_claimed_conversation_job_with_multiline_message() {
    let (svc, cron_repo, _, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_create")
        .expect("claim conversation");

    let response = svc
        .create_for_conversation_helper("u1", "conv_1", conversation_cron_request("first\nsecond\nthird"))
        .await
        .unwrap();

    assert!(response.job_id.starts_with("cron_"));
    assert!(response.message.contains("Agent Helper Job"));

    let row = cron_repo.get_by_id(&response.job_id).await.unwrap().unwrap();
    assert_eq!(row.payload_message, "first\nsecond\nthird");
    assert_eq!(row.conversation_id, "conv_1");
    assert_eq!(row.created_by, "agent");

    let bound = conv_repo.get("conv_1").await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&bound.extra).unwrap();
    assert_eq!(extra["cron_job_id"], response.job_id);
    assert_eq!(extra["cronJobId"], response.job_id);

    let linked = conv_repo.list_by_cron_job("u1", &response.job_id).await.unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(linked[0].id, "conv_1");
}

#[tokio::test]
async fn create_for_conversation_helper_keeps_conversation_extra_mode_unchanged() {
    let (svc, cron_repo, _, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_mode_default", "turn_helper_create_full_auto")
        .expect("claim conversation");

    let response = svc
        .create_for_conversation_helper(
            "u1",
            "conv_mode_default",
            conversation_cron_request("create files without prompting"),
        )
        .await
        .unwrap();

    let row = cron_repo.get_by_id(&response.job_id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert_eq!(config.mode.as_deref(), Some("yolo"));

    let bound = conv_repo.get("conv_mode_default").await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&bound.extra).unwrap();
    assert_eq!(extra["cron_job_id"], response.job_id);
    assert_eq!(extra["cronJobId"], response.job_id);
    assert_eq!(extra["session_mode"], "default");
    assert!(extra.get("current_mode_id").is_none());
}

#[tokio::test]
async fn create_for_conversation_helper_uses_assistant_metadata_full_auto_mode() {
    let (svc, cron_repo, _, _, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_mode_codex", "turn_helper_create_codex_full_auto")
        .expect("claim conversation");

    let response = svc
        .create_for_conversation_helper(
            "u1",
            "conv_mode_codex",
            conversation_cron_request("run codex without prompting"),
        )
        .await
        .unwrap();

    let row = cron_repo.get_by_id(&response.job_id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert_eq!(config.mode.as_deref(), Some("agent-full-access"));
}

#[tokio::test]
async fn create_for_conversation_helper_uses_codex_canonical_full_auto_mode_from_fallback() {
    let (svc, cron_repo, _, _, conv_service, agent_metadata_repo) = setup_with_conv_runtime_and_agent_metadata().await;
    let codex = agent_metadata_repo
        .find_builtin_by_backend("codex")
        .await
        .unwrap()
        .expect("seeded codex metadata");
    agent_metadata_repo
        .upsert(&UpsertAgentMetadataParams {
            id: &codex.id,
            icon: codex.icon.as_deref(),
            name: &codex.name,
            name_i18n: codex.name_i18n.as_deref(),
            description: codex.description.as_deref(),
            description_i18n: codex.description_i18n.as_deref(),
            backend: codex.backend.as_deref(),
            agent_type: &codex.agent_type,
            agent_source: &codex.agent_source,
            agent_source_info: codex.agent_source_info.as_deref(),
            enabled: codex.enabled,
            command: codex.command.as_deref(),
            args: codex.args.as_deref(),
            env: codex.env.as_deref(),
            native_skills_dirs: codex.native_skills_dirs.as_deref(),
            behavior_policy: codex.behavior_policy.as_deref(),
            yolo_id: None,
            agent_capabilities: codex.agent_capabilities.as_deref(),
            auth_methods: codex.auth_methods.as_deref(),
            config_options: codex.config_options.as_deref(),
            available_modes: codex.available_modes.as_deref(),
            available_models: codex.available_models.as_deref(),
            available_commands: codex.available_commands.as_deref(),
            sort_order: codex.sort_order,
        })
        .await
        .unwrap();

    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_mode_codex", "turn_helper_create_codex_fallback_full_auto")
        .expect("claim conversation");

    let response = svc
        .create_for_conversation_helper(
            "u1",
            "conv_mode_codex",
            conversation_cron_request("run codex fallback without prompting"),
        )
        .await
        .unwrap();

    let row = cron_repo.get_by_id(&response.job_id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert_eq!(config.mode.as_deref(), Some("agent-full-access"));
}

#[tokio::test]
async fn create_for_conversation_helper_fails_when_conversation_binding_fails() {
    let (svc, cron_repo, _, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_create_bind_failure")
        .expect("claim conversation");
    conv_repo.fail_updates_for("conv_1");

    let err = svc
        .create_for_conversation_helper("u1", "conv_1", conversation_cron_request("hello"))
        .await
        .expect_err("helper must not report success when conversation binding fails");

    assert!(matches!(err, aionui_cron::error::CronError::Database(_)));

    let rows = cron_repo.list_by_conversation("conv_1").await.unwrap();
    assert!(rows.is_empty());
}

#[tokio::test]
async fn conversation_cron_routes_create_list_and_update_claimed_job() {
    let (svc, cron_repo, _, _, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_route")
        .expect("claim conversation");

    let app = cron_routes(CronRouterState {
        cron_service: Arc::new(svc),
        conversation_service: (*conv_service).clone(),
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/internal/conversation-cron/create")
                .header("content-type", "application/json")
                .header("x-aionui-user-id", "u1")
                .header("x-aionui-conversation-id", "conv_1")
                .body(Body::from(
                    serde_json::to_vec(&conversation_cron_request("first\nsecond\nthird")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let envelope: ApiResponse<CreateConversationCronResponse> = serde_json::from_slice(&body).unwrap();
    let payload = envelope.data.expect("response should contain created job id");

    let row = cron_repo.get_by_id(&payload.job_id).await.unwrap().unwrap();
    assert_eq!(row.payload_message, "first\nsecond\nthird");
    assert_eq!(row.conversation_id, "conv_1");
    assert_eq!(row.created_by, "agent");

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::GET)
                .uri("/api/internal/conversation-cron/list")
                .header("x-aionui-user-id", "u1")
                .header("x-aionui-conversation-id", "conv_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_response.status(), StatusCode::OK);
    let body = to_bytes(list_response.into_body(), usize::MAX).await.unwrap();
    let envelope: ApiResponse<Vec<CronJobResponse>> = serde_json::from_slice(&body).unwrap();
    let jobs = envelope.data.expect("response should contain helper jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, payload.job_id);

    let update_response = app
        .oneshot(
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/internal/conversation-cron/jobs/{}", payload.job_id))
                .header("content-type", "application/json")
                .header("x-aionui-user-id", "u1")
                .header("x-aionui-conversation-id", "conv_1")
                .body(Body::from(
                    serde_json::to_vec(&UpdateConversationCronRequest {
                        name: "Updated Route Job".into(),
                        schedule: "0 */20 * * * *".into(),
                        schedule_description: "every 20 min".into(),
                        message: "updated\nsecond\nthird".into(),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);

    let row = cron_repo.get_by_id(&payload.job_id).await.unwrap().unwrap();
    assert_eq!(row.name, "Updated Route Job");
    assert_eq!(row.payload_message, "updated\nsecond\nthird");
    assert_eq!(row.schedule_value, "0 */20 * * * *");
}

#[tokio::test]
async fn conversation_cron_routes_reject_missing_headers_unclaimed_and_wrong_user() {
    let (svc, _, _, _, conv_service) = setup_with_conv_runtime().await;
    let app = cron_routes(CronRouterState {
        cron_service: Arc::new(svc),
        conversation_service: (*conv_service).clone(),
    });

    let missing_header = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/internal/conversation-cron/create")
                .header("content-type", "application/json")
                .header("x-aionui-conversation-id", "conv_1")
                .body(Body::from(
                    serde_json::to_vec(&conversation_cron_request("hello")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(missing_header.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(to_bytes(missing_header.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    assert!(body.contains("x-aionui-user-id"));

    let unclaimed = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/internal/conversation-cron/create")
                .header("content-type", "application/json")
                .header("x-aionui-user-id", "u1")
                .header("x-aionui-conversation-id", "conv_1")
                .body(Body::from(
                    serde_json::to_vec(&conversation_cron_request("hello")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unclaimed.status(), StatusCode::BAD_REQUEST);
    let body = String::from_utf8(to_bytes(unclaimed.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    assert!(body.contains("active conversation turn"));

    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_route_wrong_user")
        .expect("claim conversation");

    let wrong_user = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/internal/conversation-cron/create")
                .header("content-type", "application/json")
                .header("x-aionui-user-id", "other_user")
                .header("x-aionui-conversation-id", "conv_1")
                .body(Body::from(
                    serde_json::to_vec(&conversation_cron_request("hello")).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_user.status(), StatusCode::NOT_FOUND);
    let body = String::from_utf8(to_bytes(wrong_user.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
    assert!(body.contains("conv_1"));
}

#[tokio::test]
async fn create_for_conversation_helper_rejects_unclaimed_conversation() {
    let (svc, _, _, _, _) = setup_with_conv_runtime().await;

    let err = svc
        .create_for_conversation_helper("u1", "conv_1", conversation_cron_request("hello"))
        .await
        .expect_err("helper must require active turn claim");

    assert!(matches!(err, aionui_cron::error::CronError::InvalidAgentConfig(_)));
    assert!(err.to_string().contains("active conversation turn"));
}

#[tokio::test]
async fn create_for_conversation_helper_rejects_wrong_user() {
    let (svc, _, _, _, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_wrong_user")
        .expect("claim conversation");

    let err = svc
        .create_for_conversation_helper("other_user", "conv_1", conversation_cron_request("hello"))
        .await
        .expect_err("helper must verify conversation owner");

    assert!(matches!(err, aionui_cron::error::CronError::Conversation(_)));
}

#[tokio::test]
async fn list_for_conversation_helper_returns_claimed_conversation_jobs() {
    let (svc, _, _, _, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_list")
        .expect("claim conversation");

    svc.create_for_conversation_helper("u1", "conv_1", conversation_cron_request("hello"))
        .await
        .unwrap();

    let jobs = svc
        .list_for_conversation_helper("u1", "conv_1")
        .await
        .expect("list helper jobs");

    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "Agent Helper Job");
    assert_eq!(jobs[0].conversation_id, "conv_1");
}

#[tokio::test]
async fn update_for_conversation_helper_updates_claimed_conversation_job() {
    let (svc, cron_repo, _, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_update")
        .expect("claim conversation");

    let created = svc
        .create_for_conversation_helper("u1", "conv_1", conversation_cron_request("old message"))
        .await
        .unwrap();

    let updated = svc
        .update_for_conversation_helper(
            "u1",
            "conv_1",
            &created.job_id,
            UpdateConversationCronRequest {
                name: "Updated Helper Job".into(),
                schedule: "0 */20 * * * *".into(),
                schedule_description: "every 20 min".into(),
                message: "new message\nsecond line".into(),
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.name, "Updated Helper Job");
    let row = cron_repo.get_by_id(&created.job_id).await.unwrap().unwrap();
    assert_eq!(row.payload_message, "new message\nsecond line");
    assert_eq!(row.schedule_value, "0 */20 * * * *");

    conv_repo
        .update(
            "conv_1",
            &ConversationRowUpdate {
                extra: Some("{}".into()),
                updated_at: Some(now_ms()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    svc.update_for_conversation_helper(
        "u1",
        "conv_1",
        &created.job_id,
        UpdateConversationCronRequest {
            name: "Rebound Helper Job".into(),
            schedule: "0 */30 * * * *".into(),
            schedule_description: "every 30 min".into(),
            message: "rebind message".into(),
        },
    )
    .await
    .unwrap();

    let bound = conv_repo.get("conv_1").await.unwrap().unwrap();
    let extra: serde_json::Value = serde_json::from_str(&bound.extra).unwrap();
    assert_eq!(extra["cron_job_id"], created.job_id);
    assert_eq!(extra["cronJobId"], created.job_id);
}

#[tokio::test]
async fn update_for_conversation_helper_fails_when_conversation_binding_fails() {
    let (svc, _, _, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim = runtime_state
        .try_claim_turn("conv_1", "turn_helper_update_bind_failure")
        .expect("claim conversation");

    let created = svc
        .create_for_conversation_helper("u1", "conv_1", conversation_cron_request("old message"))
        .await
        .unwrap();

    conv_repo
        .update(
            "conv_1",
            &ConversationRowUpdate {
                extra: Some("{}".into()),
                updated_at: Some(now_ms()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    conv_repo.fail_updates_for("conv_1");

    let err = svc
        .update_for_conversation_helper(
            "u1",
            "conv_1",
            &created.job_id,
            UpdateConversationCronRequest {
                name: "Failed Rebind Helper Job".into(),
                schedule: "0 */20 * * * *".into(),
                schedule_description: "every 20 min".into(),
                message: "new message".into(),
            },
        )
        .await
        .expect_err("helper must not report success when conversation binding fails");

    assert!(matches!(err, aionui_cron::error::CronError::Database(_)));
}

#[tokio::test]
async fn update_for_conversation_helper_rejects_job_from_other_conversation() {
    let (svc, _, _, _, conv_service) = setup_with_conv_runtime().await;
    let runtime_state = conv_service.runtime_state();
    let _claim_1 = runtime_state
        .try_claim_turn("conv_1", "turn_helper_update_wrong_conv_1")
        .expect("claim first conversation");

    let created = svc
        .create_for_conversation_helper("u1", "conv_1", conversation_cron_request("hello"))
        .await
        .unwrap();
    drop(_claim_1);

    let _claim_2 = runtime_state
        .try_claim_turn("conv_2", "turn_helper_update_wrong_conv_2")
        .expect("claim second conversation");

    let err = svc
        .update_for_conversation_helper(
            "u1",
            "conv_2",
            &created.job_id,
            UpdateConversationCronRequest {
                name: "Wrong Conversation".into(),
                schedule: "0 */20 * * * *".into(),
                schedule_description: "every 20 min".into(),
                message: "nope".into(),
            },
        )
        .await
        .expect_err("helper must reject jobs outside the claimed conversation");

    assert!(matches!(err, aionui_cron::error::CronError::JobNotFound(_)));
}

// ── Update with max_retries ───────────────────────────────────────

#[tokio::test]
async fn update_max_retries() {
    let (svc, _, _) = setup().await;
    let job = svc.add_job(make_create_req("Retries", every_60s())).await.unwrap();
    assert_eq!(job.max_retries, 3);

    let req = UpdateCronJobRequest {
        name: None,
        description: None,
        enabled: None,
        schedule: None,
        message: None,
        execution_mode: None,
        agent_config: None,
        conversation_title: None,
        max_retries: Some(5),
        queue_enabled: None,
    };
    let updated = svc.update_job(&job.id, req).await.unwrap();
    assert_eq!(updated.max_retries, 5);
}

#[tokio::test]
async fn create_and_update_queue_enabled() {
    let (svc, _, _) = setup().await;
    let mut create = make_create_req("Queued", every_60s());
    create.queue_enabled = true;
    let job = svc.add_job(create).await.unwrap();
    assert!(job.queue_enabled);
    assert!(CronService::to_response(&job).state.queue_enabled);

    let updated = svc
        .update_job(
            &job.id,
            UpdateCronJobRequest {
                queue_enabled: Some(false),
                ..UpdateCronJobRequest {
                    name: None,
                    description: None,
                    enabled: None,
                    schedule: None,
                    message: None,
                    execution_mode: None,
                    agent_config: None,
                    conversation_title: None,
                    max_retries: None,
                    queue_enabled: None,
                }
            },
        )
        .await
        .unwrap();
    assert!(!updated.queue_enabled);
}

// ── SC-1: At type — future timestamp, nextRunAtMs == atMs ────────

#[tokio::test]
async fn sc1_at_type_future_timestamp() {
    let (svc, _, _) = setup().await;
    let target_ms = now_ms() + 3_600_000;
    let req = make_create_req(
        "At Future",
        CronScheduleDto::At {
            at_ms: target_ms,
            description: Some("once in 1h".into()),
        },
    );
    let job = svc.add_job(req).await.unwrap();
    assert_eq!(job.next_run_at, Some(target_ms));
}

// ── SC-2: At type — past timestamp, nextRunAtMs == atMs ──────────

#[tokio::test]
async fn sc2_at_type_past_timestamp() {
    let (svc, _, _) = setup().await;
    let target_ms = now_ms() - 3_600_000;
    let req = make_create_req(
        "At Past",
        CronScheduleDto::At {
            at_ms: target_ms,
            description: Some("once in the past".into()),
        },
    );
    let job = svc.add_job(req).await.unwrap();
    assert_eq!(job.next_run_at, Some(target_ms));
}

// ── SR-1: System resume detects missed jobs ──────────────────────

#[tokio::test]
async fn sr1_system_resume_missed_job() {
    let (svc, repo, bc, conv_repo) = setup_with_conv_repo().await;

    let req = make_create_req("Resume Job", every_60s());
    let job = svc.add_job(req).await.unwrap();
    bc.take_events();

    let past_ms = now_ms() - 10_000;
    let params = aionui_db::UpdateCronJobParams {
        next_run_at: Some(Some(past_ms)),
        ..Default::default()
    };
    repo.update(&job.id, &params).await.unwrap();

    svc.handle_system_resume().await;

    let updated = svc.get_job(&job.id).await.unwrap();
    assert!(
        updated.last_run_at.is_none(),
        "missed job should not be auto-executed on resume"
    );
    assert_eq!(updated.last_status, Some(JobStatus::Missed));
    assert!(
        updated.next_run_at.is_some(),
        "job should be rescheduled after being marked missed"
    );
    assert!(
        updated.next_run_at.unwrap() > now_ms() - 2000,
        "next_run_at should be in the future"
    );

    let messages = conv_repo.take_messages();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].r#type, "tips");
    assert!(messages[0].content.contains("Resume Job"));
    assert!(messages[0].content.contains("not run automatically"));

    let events = bc.take_events();
    assert!(
        events
            .iter()
            .any(|event| { event.name == "cron.job-executed" && event.data["status"] == "missed" }),
        "resume should emit a missed execution event"
    );
    assert!(
        events.iter().any(|event| {
            event.name == "message.stream" && event.data["type"] == "tips" && event.data["conversation_id"] == "conv_1"
        }),
        "resume should emit a tips websocket message"
    );
}

// ── CD-1: Preserve cron jobs when conversation is deleted ─────────

#[tokio::test]
async fn cd1_delete_by_conversation_preserves_jobs() {
    let (svc, _repo, bc) = setup().await;

    let mut req_a = make_create_req("Cascade A", every_60s());
    req_a.conversation_id = "conv_cascade".into();
    let job_a = svc.add_job(req_a).await.unwrap();

    let mut req_b = make_create_req("Cascade B", every_60s());
    req_b.conversation_id = "conv_cascade".into();
    let job_b = svc.add_job(req_b).await.unwrap();

    let mut req_c = make_create_req("Unrelated", every_60s());
    req_c.conversation_id = "conv_other".into();
    let _job_c = svc.add_job(req_c).await.unwrap();

    bc.take_events();

    svc.delete_jobs_by_conversation("conv_cascade").await;

    assert!(svc.get_job(&job_a.id).await.is_ok());
    assert!(svc.get_job(&job_b.id).await.is_ok());

    let remaining = svc.list_jobs(&ListCronJobsQuery::default()).await.unwrap();
    assert_eq!(remaining.len(), 3, "all cron jobs should remain");

    let events = bc.take_events();
    let removed_events: Vec<_> = events.iter().filter(|e| e.name == "cron.job-removed").collect();
    assert!(
        removed_events.is_empty(),
        "conversation delete should not emit cron removal events"
    );
}

// ── CD-2: Preserve on empty conversation (no-op) ──────────────────

#[tokio::test]
async fn cd2_delete_by_conversation_no_matching_jobs() {
    let (svc, _repo, bc) = setup().await;

    svc.add_job(make_create_req("Existing", every_60s())).await.unwrap();
    bc.take_events();

    svc.delete_jobs_by_conversation("conv_nonexistent").await;

    let events = bc.take_events();
    assert!(events.is_empty(), "no events should be emitted when no jobs match");

    let all = svc.list_jobs(&ListCronJobsQuery::default()).await.unwrap();
    assert_eq!(all.len(), 1, "existing job should remain untouched");
}

// ── CD-3: OnConversationDelete trait preserves jobs ───────────────

#[tokio::test]
async fn cd3_on_conversation_delete_trait_preserves_jobs() {
    use aionui_common::OnConversationDelete;

    let (svc, _repo, bc) = setup().await;

    let mut req = make_create_req("Trait Cascade", every_60s());
    req.conversation_id = "conv_trait_del".into();
    let job = svc.add_job(req).await.unwrap();
    bc.take_events();

    svc.on_conversation_deleted("conv_trait_del").await;

    assert!(svc.get_job(&job.id).await.is_ok());

    let events = bc.take_events();
    assert!(events.is_empty());
}

#[tokio::test]
async fn cd3b_on_conversation_delete_clears_deleted_workspace_from_jobs() {
    use aionui_common::OnConversationDelete;

    let (svc, cron_repo, bc, conv_repo, conv_service) = setup_with_conv_runtime().await;
    let conversation_id = format!("conv_workspace_deleted_{}", now_ms());
    let deleted_workspace_path = std::env::temp_dir()
        .join("conversations")
        .join(format!("acp-temp-{conversation_id}"));
    std::fs::create_dir_all(&deleted_workspace_path).unwrap();
    let deleted_workspace = deleted_workspace_path.to_string_lossy().to_string();
    conv_repo.set_conversation_extra(
        &conversation_id,
        serde_json::json!({
            "workspace": deleted_workspace,
        }),
    );

    let mut req = make_create_req("Clears Deleted Workspace", every_60s());
    req.conversation_id = conversation_id.clone();
    req.agent_config.as_mut().unwrap().workspace = Some(deleted_workspace);
    let job = svc.add_job(req).await.unwrap();
    bc.take_events();

    let bound_conversation = conv_repo.get(&conversation_id).await.unwrap().unwrap();
    assert!(
        conv_service
            .auto_workspace_to_delete_for_row(&bound_conversation, &conversation_id)
            .is_some(),
        "test setup should use a workspace ConversationService will delete"
    );
    let row_before = cron_repo.get_by_id(&job.id).await.unwrap().unwrap();
    let config_before: CronAgentConfig = serde_json::from_str(row_before.agent_config.as_deref().unwrap()).unwrap();
    assert_eq!(
        config_before.workspace.as_deref(),
        Some(deleted_workspace_path.to_str().unwrap())
    );

    svc.on_conversation_deleted(&conversation_id).await;

    assert!(svc.get_job(&job.id).await.is_ok());
    let row = cron_repo.get_by_id(&job.id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert!(
        config.workspace.is_none(),
        "cron job should drop workspace cached from the deleted conversation"
    );

    let events = bc.take_events();
    assert!(events.is_empty());
}

#[tokio::test]
async fn cd3c_on_conversation_delete_preserves_custom_workspace_on_jobs() {
    use aionui_common::OnConversationDelete;

    let (svc, cron_repo, bc, conv_repo) = setup_with_conv_repo().await;
    let conversation_id = format!("conv_workspace_custom_{}", now_ms());
    let custom_workspace = ensure_named_workspace_path(&format!("aionui-cron-custom-workspace-{conversation_id}"));
    conv_repo.set_conversation_extra(
        &conversation_id,
        serde_json::json!({
            "workspace": custom_workspace,
        }),
    );

    let mut req = make_create_req("Preserves Custom Workspace", every_60s());
    req.conversation_id = conversation_id.clone();
    req.agent_config.as_mut().unwrap().workspace = Some(custom_workspace.clone());
    let job = svc.add_job(req).await.unwrap();
    bc.take_events();

    svc.on_conversation_deleted(&conversation_id).await;

    let row = cron_repo.get_by_id(&job.id).await.unwrap().unwrap();
    let config: CronAgentConfig = serde_json::from_str(row.agent_config.as_deref().unwrap()).unwrap();
    assert_eq!(config.workspace.as_deref(), Some(custom_workspace.as_str()));

    let events = bc.take_events();
    assert!(events.is_empty());
}

#[tokio::test]
async fn cd4_on_conversation_delete_preserves_all_cron_jobs() {
    use aionui_common::OnConversationDelete;

    let (svc, _repo, bc) = setup().await;

    let mut new_conversation_req = make_create_req("Generated Run History", every_60s());
    new_conversation_req.conversation_id = "conv_generated_run".into();
    new_conversation_req.execution_mode = Some("new_conversation".into());
    let new_conversation_job = svc.add_job(new_conversation_req).await.unwrap();

    let mut existing_req = make_create_req("Existing Bound Job", every_60s());
    existing_req.conversation_id = "conv_generated_run".into();
    existing_req.execution_mode = Some("existing".into());
    let existing_job = svc.add_job(existing_req).await.unwrap();

    bc.take_events();

    svc.on_conversation_deleted("conv_generated_run").await;

    assert!(
        svc.get_job(&new_conversation_job.id).await.is_ok(),
        "deleting a generated run conversation must not delete its new-conversation cron job"
    );
    assert!(
        svc.get_job(&existing_job.id).await.is_ok(),
        "existing-mode jobs should also survive conversation deletion"
    );

    let events = bc.take_events();
    let removed_events: Vec<_> = events.iter().filter(|e| e.name == "cron.job-removed").collect();
    assert!(removed_events.is_empty());
}
