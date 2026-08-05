mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aionui_ai_agent::session_context::{
    AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, ConversationContext, WorkspaceContext,
};
use aionui_ai_agent::task_manager::AgentFactory;
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_ai_agent::{ActiveLeaseRegistry, AgentError, IWorkerTaskManager, WorkerTaskManagerImpl};
use aionui_api_types::{
    AcpBuildExtra, AcpConfigOptionDto, AcpConfigSelectOptionDto, AddAgentRequest, CreateTeamRequest,
    GetConfigOptionsResponse, TeamAgentInput, TeamRunSource, WebSocketMessage,
};
use aionui_common::{AgentKillReason, AgentType, PaginatedResult, ProviderWithModel};
use aionui_db::models::{
    AgentMetadataRow, AssistantDefinitionRow, AssistantOverlayRow, ConversationRow, MessageRow,
    UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpsertAgentMetadataParams,
    UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
};
use aionui_db::{
    ActivityCursor, ConversationFilters, ConversationRowUpdate, DbError, IAgentMetadataRepository,
    IAssistantDefinitionRepository, IAssistantOverlayRepository, IConversationRepository, IProviderRepository,
    ITeamRepository, MessagePageParams, MessagePageResult, MessageRowUpdate, MessageSearchRow, PageDirection,
    resolve_agent_binding_from_rows,
};
use aionui_realtime::EventBroadcaster;

use aionui_team::ports::{
    AgentTurnCancellationPort, AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnOutcome, AgentTurnRequest,
    AgentTurnStarted, AgentTurnStatus, TeamAssistantCatalogEntry, TeamAssistantCatalogPort,
    TeamConversationBindingLookup, TeamConversationLookupPort,
};
use aionui_team::session::SpawnAgentRequest;
use aionui_team::{
    TeamConversationCreateRequest, TeamConversationCreateResult, TeamConversationProvisioningPort,
    TeamProjectionMessageStore,
};
use aionui_team::{TeamError, TeamSessionService};
use common::MockTeamRepo;

// ---------------------------------------------------------------------------
// Mock ConversationRepository — minimal impl for TeamSessionService tests
// ---------------------------------------------------------------------------

struct MockConversationRepo {
    conversations: std::sync::Mutex<Vec<ConversationRow>>,
    messages: std::sync::Mutex<Vec<MessageRow>>,
}

impl MockConversationRepo {
    fn new() -> Self {
        Self {
            conversations: std::sync::Mutex::new(Vec::new()),
            messages: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn get_extra(&self, id: &str) -> Option<serde_json::Value> {
        let convs = self.conversations.lock().unwrap();
        convs
            .iter()
            .find(|c| c.id == id)
            .and_then(|c| serde_json::from_str(&c.extra).ok())
    }

    fn conversation_count(&self) -> usize {
        self.conversations.lock().unwrap().len()
    }

    fn messages_for(&self, conversation_id: &str) -> Vec<MessageRow> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.conversation_id == conversation_id)
            .cloned()
            .collect()
    }

    fn patch_extra(&self, id: &str, patch: serde_json::Value) -> Result<(), DbError> {
        let mut convs = self.conversations.lock().unwrap();
        let conv = convs
            .iter_mut()
            .find(|c| c.id == id)
            .ok_or_else(|| DbError::NotFound(id.to_owned()))?;
        let mut extra: serde_json::Value = serde_json::from_str(&conv.extra).unwrap_or_else(|_| serde_json::json!({}));
        if let (Some(target), Some(source)) = (extra.as_object_mut(), patch.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        conv.extra = serde_json::to_string(&extra).unwrap();
        Ok(())
    }
}

#[async_trait::async_trait]
impl IConversationRepository for MockConversationRepo {
    async fn get(&self, user_id: &str, id: &str) -> Result<Option<ConversationRow>, DbError> {
        let convs = self.conversations.lock().unwrap();
        Ok(convs.iter().find(|c| c.user_id == user_id && c.id == id).cloned())
    }
    async fn owner_user_id(&self, id: &str) -> Result<Option<String>, DbError> {
        let convs = self.conversations.lock().unwrap();
        Ok(convs.iter().find(|c| c.id == id).map(|c| c.user_id.clone()))
    }
    async fn create(&self, row: &ConversationRow) -> Result<(), DbError> {
        self.conversations.lock().unwrap().push(row.clone());
        Ok(())
    }
    async fn update(&self, user_id: &str, id: &str, updates: &ConversationRowUpdate) -> Result<(), DbError> {
        let mut convs = self.conversations.lock().unwrap();
        let conv = convs
            .iter_mut()
            .find(|c| c.user_id == user_id && c.id == id)
            .ok_or_else(|| DbError::NotFound(id.to_owned()))?;
        if let Some(ref extra) = updates.extra {
            conv.extra = extra.clone();
        }
        if let Some(ref name) = updates.name {
            conv.name = name.clone();
        }
        if let Some(pinned) = updates.pinned {
            conv.pinned = pinned;
        }
        if let Some(ref model) = updates.model {
            conv.model = model.clone();
        }
        if let Some(updated_at) = updates.updated_at {
            conv.updated_at = updated_at;
        }
        Ok(())
    }
    async fn delete(&self, user_id: &str, id: &str) -> Result<(), DbError> {
        self.conversations
            .lock()
            .unwrap()
            .retain(|c| c.user_id != user_id || c.id != id);
        Ok(())
    }
    async fn list_paginated(
        &self,
        _user_id: &str,
        _filters: &ConversationFilters,
    ) -> Result<PaginatedResult<ConversationRow>, DbError> {
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
    ) -> Result<Option<ConversationRow>, DbError> {
        Ok(None)
    }
    async fn list_by_cron_job(&self, _user_id: &str, _cron_job_id: &str) -> Result<Vec<ConversationRow>, DbError> {
        Ok(vec![])
    }
    async fn list_associated(&self, _user_id: &str, _conversation_id: &str) -> Result<Vec<ConversationRow>, DbError> {
        Ok(vec![])
    }
    async fn list_messages_page(
        &self,
        _user_id: &str,
        _conv_id: &str,
        _params: &MessagePageParams,
    ) -> Result<MessagePageResult, DbError> {
        Ok(MessagePageResult {
            items: vec![],
            has_more_before: false,
            has_more_after: false,
        })
    }
    async fn insert_message(&self, _user_id: &str, message: &MessageRow) -> Result<(), DbError> {
        self.messages.lock().unwrap().push(message.clone());
        Ok(())
    }
    async fn update_message(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _id: &str,
        _updates: &MessageRowUpdate,
    ) -> Result<(), DbError> {
        Ok(())
    }
    async fn delete_messages_by_conversation(&self, _user_id: &str, _conv_id: &str) -> Result<(), DbError> {
        Ok(())
    }
    async fn get_message_by_msg_id(
        &self,
        _user_id: &str,
        conv_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .find(|row| {
                row.conversation_id == conv_id && row.msg_id.as_deref() == Some(msg_id) && row.r#type == msg_type
            })
            .cloned())
    }
    async fn search_messages(
        &self,
        _user_id: &str,
        _keyword: &str,
        _page: u32,
        _page_size: u32,
    ) -> Result<PaginatedResult<MessageSearchRow>, DbError> {
        Ok(PaginatedResult {
            items: vec![],
            total: 0,
            has_more: false,
        })
    }
}

// ---------------------------------------------------------------------------
// NullBroadcaster — no-op event broadcaster
// ---------------------------------------------------------------------------

struct NullBroadcaster;
impl EventBroadcaster for NullBroadcaster {
    fn broadcast(&self, _msg: WebSocketMessage<serde_json::Value>) {}
}

struct NoopTurnPort;

#[async_trait::async_trait]
impl AgentTurnExecutionPort for NoopTurnPort {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError> {
        if let Some(on_started) = request.on_started.as_ref() {
            on_started(AgentTurnStarted {
                team_run_id: request.team_run_id.clone(),
                slot_id: request.slot_id.clone(),
                role: request.role.clone(),
                conversation_id: request.conversation_id.clone(),
                turn_id: "turn-test".into(),
            })
            .await;
        }
        Ok(AgentTurnOutcome {
            conversation_id: request.conversation_id,
            turn_id: "turn-test".into(),
            status: AgentTurnStatus::Completed,
            runtime: None,
        })
    }
}

#[derive(Default)]
struct RecordingTurnPort {
    requests: Mutex<Vec<AgentTurnRequest>>,
}

#[async_trait::async_trait]
impl AgentTurnExecutionPort for RecordingTurnPort {
    async fn run_agent_turn(&self, request: AgentTurnRequest) -> Result<AgentTurnOutcome, AgentTurnExecutionError> {
        if let Some(on_started) = request.on_started.as_ref() {
            on_started(AgentTurnStarted {
                team_run_id: request.team_run_id.clone(),
                slot_id: request.slot_id.clone(),
                role: request.role.clone(),
                conversation_id: request.conversation_id.clone(),
                turn_id: format!("turn-{}", request.slot_id),
            })
            .await;
        }
        self.requests.lock().unwrap().push(request.clone());
        Ok(AgentTurnOutcome {
            conversation_id: request.conversation_id,
            turn_id: "turn-recorded".into(),
            status: AgentTurnStatus::Completed,
            runtime: None,
        })
    }
}

fn noop_turn_port() -> Arc<dyn AgentTurnExecutionPort> {
    Arc::new(NoopTurnPort)
}

struct NoopCancellationPort;

#[async_trait::async_trait]
impl AgentTurnCancellationPort for NoopCancellationPort {
    async fn cancel_agent_turn(
        &self,
        _user_id: &str,
        _conversation_id: &str,
        _turn_id: &str,
    ) -> Result<(), AgentTurnExecutionError> {
        Ok(())
    }
}

fn noop_cancellation_port() -> Arc<dyn AgentTurnCancellationPort> {
    Arc::new(NoopCancellationPort)
}

struct FakeConversationPorts {
    repo: Arc<MockConversationRepo>,
    workspace_root: std::path::PathBuf,
    preset_snapshots: Mutex<HashMap<String, FakePresetAssistantSnapshot>>,
    fail_team_temp_create: std::sync::atomic::AtomicBool,
    fail_leader_workspace_patch: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
struct FakePresetAssistantSnapshot {
    rules: String,
    skills: Vec<String>,
    mcp_server_ids: Vec<String>,
}

impl FakeConversationPorts {
    fn new(repo: Arc<MockConversationRepo>) -> Self {
        let workspace_root =
            std::env::temp_dir().join(format!("aionui-team-fake-workspaces-{}", aionui_common::generate_id()));
        Self {
            repo,
            workspace_root,
            preset_snapshots: Mutex::new(HashMap::new()),
            fail_team_temp_create: std::sync::atomic::AtomicBool::new(false),
            fail_leader_workspace_patch: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn upsert_preset_snapshot(&self, id: &str, snapshot: FakePresetAssistantSnapshot) {
        self.preset_snapshots.lock().unwrap().insert(id.to_owned(), snapshot);
    }

    fn apply_preset_snapshot(&self, extra: &mut serde_json::Value) {
        let Some(preset_id) = extra
            .get("assistant_id")
            .or_else(|| extra.get("preset_assistant_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let Some(snapshot) = self.preset_snapshots.lock().unwrap().get(&preset_id).cloned() else {
            return;
        };
        extra["preset_context"] = serde_json::Value::String(snapshot.rules.clone());
        extra["preset_rules"] = serde_json::Value::String(snapshot.rules);
        extra["skills"] =
            serde_json::Value::Array(snapshot.skills.into_iter().map(serde_json::Value::String).collect());
        extra["mcp_server_ids"] = serde_json::Value::Array(
            snapshot
                .mcp_server_ids
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        );
    }
}

#[async_trait::async_trait]
impl TeamConversationProvisioningPort for FakeConversationPorts {
    async fn create_team_conversation(
        &self,
        request: TeamConversationCreateRequest,
    ) -> Result<TeamConversationCreateResult, aionui_team::TeamError> {
        if request.assistant_id.is_some() && request.agent_type.is_some() {
            return Err(aionui_team::TeamError::InvalidRequest(
                "assistant-backed team conversations must not provide agent_type".into(),
            ));
        }
        let id = aionui_common::generate_id();
        let now = aionui_common::now_ms();
        let workspace = request
            .extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| {
                let path = self.workspace_root.join("conversations").join(format!("acp-temp-{id}"));
                std::fs::create_dir_all(&path).unwrap();
                path.to_string_lossy().into_owned()
            });
        let mut extra = request.extra;
        extra["workspace"] = serde_json::Value::String(workspace.clone());
        self.apply_preset_snapshot(&mut extra);
        self.repo
            .create(&ConversationRow {
                id: id.clone(),
                user_id: request.user_id,
                name: request.name,
                r#type: request.agent_type.unwrap_or(AgentType::Acp).serde_name().to_owned(),
                pinned: false,
                pinned_at: None,
                source: None,
                channel_chat_id: None,
                extra: serde_json::to_string(&extra).unwrap(),
                model: request
                    .top_level_model
                    .map(|m| serde_json::to_string(&m).expect("serialize provider model")),
                status: Some("pending".into()),
                created_at: now,
                updated_at: now,
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await?;
        Ok(TeamConversationCreateResult {
            conversation_id: id,
            workspace,
        })
    }

    async fn conversation_workspace(&self, conversation_id: &str) -> Result<Option<String>, aionui_team::TeamError> {
        Ok(self.repo.get_extra(conversation_id).and_then(|extra| {
            extra
                .get("workspace")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        }))
    }

    async fn conversation_assistant_id(&self, conversation_id: &str) -> Result<Option<String>, aionui_team::TeamError> {
        Ok(self.repo.get_extra(conversation_id).and_then(|extra| {
            extra
                .get("assistant_id")
                .or_else(|| extra.get("preset_assistant_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        }))
    }

    async fn create_team_temp_workspace(
        &self,
        _user_id: &str,
        team_id: &str,
    ) -> Result<String, aionui_team::TeamError> {
        if self
            .fail_team_temp_create
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(aionui_team::TeamError::InvalidRequest(
                "failed to create Team temporary workspace for test".into(),
            ));
        }
        let path = self
            .workspace_root
            .join("conversations")
            .join(format!("team-temp-{team_id}"));
        std::fs::create_dir_all(&path).unwrap();
        Ok(path.to_string_lossy().into_owned())
    }

    async fn patch_runtime_config(
        &self,
        conversation_id: &str,
        patch: serde_json::Value,
    ) -> Result<(), aionui_team::TeamError> {
        if patch.get("workspace").is_some()
            && self
                .fail_leader_workspace_patch
                .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(aionui_team::TeamError::InvalidRequest(
                "forced leader workspace patch failure".into(),
            ));
        }
        let mut extra = self
            .repo
            .get_extra(conversation_id)
            .unwrap_or_else(|| serde_json::json!({}));
        if let (Some(target), Some(source)) = (extra.as_object_mut(), patch.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        let user_id = self
            .repo
            .owner_user_id(conversation_id)
            .await?
            .ok_or_else(|| aionui_team::TeamError::AgentNotFound(conversation_id.to_owned()))?;
        self.repo
            .update(
                &user_id,
                conversation_id,
                &ConversationRowUpdate {
                    name: None,
                    model: None,
                    pinned: None,
                    pinned_at: None,
                    extra: Some(serde_json::to_string(&extra).unwrap()),
                    status: None,
                    updated_at: Some(aionui_common::now_ms()),
                    project_id: None,
                    folder_id: None,
                    name_source: None,
                },
            )
            .await?;
        Ok(())
    }

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), aionui_team::TeamError> {
        self.patch_runtime_config(conversation_id, serde_json::json!({ "session_mode": mode }))
            .await
    }

    async fn get_config_options(
        &self,
        conversation_id: &str,
    ) -> Result<GetConfigOptionsResponse, aionui_team::TeamError> {
        let extra = self
            .repo
            .get_extra(conversation_id)
            .ok_or_else(|| aionui_team::TeamError::AgentNotFound(conversation_id.to_owned()))?;
        let model = extra
            .get("current_model_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("mock-model")
            .to_owned();
        Ok(GetConfigOptionsResponse {
            config_options: vec![AcpConfigOptionDto {
                id: "model".to_owned(),
                name: None,
                label: Some("Model".to_owned()),
                description: None,
                category: Some("model".to_owned()),
                option_type: "select".to_owned(),
                current_value: Some(model.clone()),
                options: vec![AcpConfigSelectOptionDto {
                    value: model.clone(),
                    name: None,
                    label: Some(model),
                    description: None,
                }],
            }],
        })
    }

    async fn warmup_agent_process(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), aionui_team::TeamError> {
        let row = self.repo.get(user_id, conversation_id).await?.ok_or_else(|| {
            aionui_team::TeamError::InvalidRequest(format!("conversation not found: {conversation_id}"))
        })?;
        let extra: serde_json::Value = serde_json::from_str(&row.extra)?;
        let team = aionui_api_types::TeamSessionBinding::from_extra_value(&extra)?;
        let config: AcpBuildExtra = serde_json::from_value(extra.clone()).unwrap_or_default();
        let workspace = extra
            .get("workspace")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_owned();
        let provider_id = extra
            .get("provider_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("acp")
            .to_owned();
        let model = extra
            .get("current_model_id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("claude")
            .to_owned();
        let context = AgentSessionContext {
            conversation: ConversationContext {
                conversation_id: row.id.clone(),
                user_id: row.user_id,
                agent_type: AgentType::Acp,
                source: row.source,
            },
            workspace: WorkspaceContext {
                path: workspace.clone(),
                stored_path: workspace,
                is_custom: false,
            },
            model: ProviderWithModel {
                provider_id,
                model,
                use_model: None,
            },
            skills: config.skills.clone(),
            runtime_env: Vec::new(),
            team: team.clone(),
            kind: AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
                config,
                team: team.clone(),
                belongs_to_team: team.is_some(),
                session_id: None,
                session_snapshot: None,
            })),
        };
        task_manager
            .get_or_build_task(conversation_id, BuildTaskOptions::new(context))
            .await
            .map_err(|error| {
                aionui_team::TeamError::InvalidRequest(format!("failed to warm up agent process: {error}"))
            })?;
        Ok(())
    }

    async fn delete_team_conversation(
        &self,
        _user_id: &str,
        conversation_id: &str,
    ) -> Result<(), aionui_team::TeamError> {
        self.repo.delete(_user_id, conversation_id).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TeamProjectionMessageStore for FakeConversationPorts {
    fn mint_message_id(&self) -> String {
        aionui_common::generate_id()
    }

    async fn find_projected_message(
        &self,
        conversation_id: &str,
        msg_id: &str,
        msg_type: &str,
    ) -> Result<Option<MessageRow>, aionui_team::TeamError> {
        let Some(user_id) = self.repo.owner_user_id(conversation_id).await? else {
            return Ok(None);
        };
        Ok(self
            .repo
            .get_message_by_msg_id(&user_id, conversation_id, msg_id, msg_type)
            .await?)
    }

    async fn insert_projected_message(&self, row: &MessageRow) -> Result<(), aionui_team::TeamError> {
        let user_id = self
            .repo
            .owner_user_id(&row.conversation_id)
            .await?
            .ok_or_else(|| aionui_team::TeamError::InvalidRequest("conversation not found".into()))?;
        self.repo.insert_message(&user_id, row).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TeamConversationLookupPort for FakeConversationPorts {
    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, aionui_team::TeamError> {
        let Some(user_id) = self.repo.owner_user_id(conversation_id).await? else {
            return Ok(None);
        };
        let Some(row) = self.repo.get(&user_id, conversation_id).await? else {
            return Ok(None);
        };
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
        Ok(Some(TeamConversationBindingLookup {
            conversation_id: row.id,
            user_id: row.user_id,
            team_id: extra
                .get("teamId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            slot_id: extra
                .get("slot_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            role: extra.get("role").and_then(serde_json::Value::as_str).map(str::to_owned),
        }))
    }
}

#[derive(Default)]
struct RecordingBroadcaster {
    events: std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl RecordingBroadcaster {
    fn new() -> Self {
        Self::default()
    }

    fn events_by_name(&self, name: &str) -> Vec<WebSocketMessage<serde_json::Value>> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.name == name)
            .cloned()
            .collect()
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }
}

impl EventBroadcaster for RecordingBroadcaster {
    fn broadcast(&self, msg: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(msg);
    }
}

// ---------------------------------------------------------------------------
// Full MockTeamRepo with actual team CRUD (not stubs)
// ---------------------------------------------------------------------------

struct FullMockTeamRepo {
    inner: MockTeamRepo,
    teams: std::sync::Mutex<Vec<aionui_db::models::TeamRow>>,
    fail_workspace_update: std::sync::Mutex<bool>,
    fail_agent_update: std::sync::Mutex<bool>,
    fail_message_writes: std::sync::Mutex<bool>,
}

impl FullMockTeamRepo {
    fn new() -> Self {
        Self {
            inner: MockTeamRepo::new(),
            teams: std::sync::Mutex::new(Vec::new()),
            fail_workspace_update: std::sync::Mutex::new(false),
            fail_agent_update: std::sync::Mutex::new(false),
            fail_message_writes: std::sync::Mutex::new(false),
        }
    }

    fn fail_workspace_update(&self) {
        *self.fail_workspace_update.lock().unwrap() = true;
    }

    fn fail_agent_updates(&self) {
        *self.fail_agent_update.lock().unwrap() = true;
    }

    fn fail_message_writes(&self) {
        *self.fail_message_writes.lock().unwrap() = true;
    }
}

#[async_trait::async_trait]
impl ITeamRepository for FullMockTeamRepo {
    async fn create_team(&self, row: &aionui_db::models::TeamRow) -> Result<(), DbError> {
        self.teams.lock().unwrap().push(row.clone());
        Ok(())
    }
    async fn list_teams_for_restore(&self) -> Result<Vec<aionui_db::models::TeamRow>, DbError> {
        Ok(self.teams.lock().unwrap().clone())
    }
    async fn list_teams_by_user(&self, user_id: &str) -> Result<Vec<aionui_db::models::TeamRow>, DbError> {
        Ok(self
            .teams
            .lock()
            .unwrap()
            .iter()
            .filter(|team| team.user_id == user_id)
            .cloned()
            .collect())
    }
    async fn get_team(&self, user_id: &str, id: &str) -> Result<Option<aionui_db::models::TeamRow>, DbError> {
        Ok(self
            .teams
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.user_id == user_id && t.id == id)
            .cloned())
    }
    async fn get_team_for_restore(&self, id: &str) -> Result<Option<aionui_db::models::TeamRow>, DbError> {
        Ok(self.teams.lock().unwrap().iter().find(|t| t.id == id).cloned())
    }
    async fn update_team(&self, user_id: &str, id: &str, params: &aionui_db::UpdateTeamParams) -> Result<(), DbError> {
        if params.workspace.is_some() && *self.fail_workspace_update.lock().unwrap() {
            return Err(DbError::Init("forced workspace writeback failure".into()));
        }
        if params.agents.is_some() && *self.fail_agent_update.lock().unwrap() {
            return Err(DbError::Init("forced agent update failure".into()));
        }
        let mut teams = self.teams.lock().unwrap();
        let team = teams
            .iter_mut()
            .find(|t| t.user_id == user_id && t.id == id)
            .ok_or_else(|| DbError::NotFound(id.to_owned()))?;
        if let Some(ref name) = params.name {
            team.name = name.clone();
        }
        if let Some(ref workspace) = params.workspace {
            team.workspace = workspace.clone();
        }
        if let Some(ref agents) = params.agents {
            team.agents = agents.clone();
        }
        if let Some(ref lead_id) = params.lead_agent_id {
            team.lead_agent_id = Some(lead_id.clone());
        }
        team.updated_at = aionui_common::now_ms();
        Ok(())
    }
    async fn delete_team(&self, user_id: &str, id: &str) -> Result<(), DbError> {
        self.teams
            .lock()
            .unwrap()
            .retain(|t| t.user_id != user_id || t.id != id);
        Ok(())
    }

    async fn write_message(&self, user_id: &str, row: &aionui_db::models::MailboxMessageRow) -> Result<(), DbError> {
        if *self.fail_message_writes.lock().unwrap() {
            return Err(DbError::Init("forced mailbox write failure".into()));
        }
        self.inner.write_message(user_id, row).await
    }
    async fn read_unread_and_mark(
        &self,
        user_id: &str,
        team_id: &str,
        to_agent_id: &str,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.read_unread_and_mark(user_id, team_id, to_agent_id).await
    }
    async fn peek_unread(
        &self,
        user_id: &str,
        team_id: &str,
        to_agent_id: &str,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.peek_unread(user_id, team_id, to_agent_id).await
    }
    async fn mark_read_batch(&self, user_id: &str, team_id: &str, ids: &[String]) -> Result<(), DbError> {
        self.inner.mark_read_batch(user_id, team_id, ids).await
    }
    async fn get_history(
        &self,
        user_id: &str,
        team_id: &str,
        to_agent_id: &str,
        limit: Option<i64>,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.get_history(user_id, team_id, to_agent_id, limit).await
    }
    async fn list_messages_by_team(
        &self,
        team_id: &str,
        limit: i64,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.list_messages_by_team(team_id, limit).await
    }
    async fn list_messages_by_team_paged(
        &self,
        team_id: &str,
        cursor: Option<ActivityCursor>,
        direction: PageDirection,
        limit: i64,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner
            .list_messages_by_team_paged(team_id, cursor, direction, limit)
            .await
    }
    async fn list_messages_by_ids(&self, ids: &[String]) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.list_messages_by_ids(ids).await
    }
    async fn delete_mailbox_by_team(&self, user_id: &str, team_id: &str) -> Result<(), DbError> {
        self.inner.delete_mailbox_by_team(user_id, team_id).await
    }

    async fn create_task(&self, user_id: &str, row: &aionui_db::models::TeamTaskRow) -> Result<(), DbError> {
        self.inner.create_task(user_id, row).await
    }
    async fn find_task_by_id(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
    ) -> Result<Option<aionui_db::models::TeamTaskRow>, DbError> {
        self.inner.find_task_by_id(user_id, team_id, task_id).await
    }
    async fn update_task(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
        params: &aionui_db::UpdateTaskParams,
    ) -> Result<(), DbError> {
        self.inner.update_task(user_id, team_id, task_id, params).await
    }
    async fn list_tasks(&self, user_id: &str, team_id: &str) -> Result<Vec<aionui_db::models::TeamTaskRow>, DbError> {
        self.inner.list_tasks(user_id, team_id).await
    }
    async fn list_tasks_by_ids(
        &self,
        user_id: &str,
        team_id: &str,
        ids: &[String],
    ) -> Result<Vec<aionui_db::models::TeamTaskRow>, DbError> {
        self.inner.list_tasks_by_ids(user_id, team_id, ids).await
    }
    async fn list_tasks_paged(
        &self,
        user_id: &str,
        team_id: &str,
        cursor: Option<ActivityCursor>,
        direction: PageDirection,
        limit: i64,
    ) -> Result<Vec<aionui_db::models::TeamTaskRow>, DbError> {
        self.inner
            .list_tasks_paged(user_id, team_id, cursor, direction, limit)
            .await
    }
    async fn append_to_blocks(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
        blocked_task_id: &str,
    ) -> Result<(), DbError> {
        self.inner
            .append_to_blocks(user_id, team_id, task_id, blocked_task_id)
            .await
    }
    async fn remove_from_blocked_by(
        &self,
        user_id: &str,
        team_id: &str,
        task_id: &str,
        unblocked_task_id: &str,
    ) -> Result<(), DbError> {
        self.inner
            .remove_from_blocked_by(user_id, team_id, task_id, unblocked_task_id)
            .await
    }
    async fn delete_tasks_by_team(&self, user_id: &str, team_id: &str) -> Result<(), DbError> {
        self.inner.delete_tasks_by_team(user_id, team_id).await
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct StubAgentMetadataRepo {
    rows_by_id: HashMap<String, AgentMetadataRow>,
    builtin_by_backend: HashMap<String, AgentMetadataRow>,
}

impl StubAgentMetadataRepo {
    fn empty() -> Self {
        Self::default()
    }

    fn with_rows(rows: Vec<AgentMetadataRow>) -> Self {
        let mut repo = Self::default();
        for row in rows {
            if row.agent_source == "builtin"
                && let Some(backend) = row.backend.as_deref()
            {
                repo.builtin_by_backend.insert(backend.to_owned(), row.clone());
            }
            repo.rows_by_id.insert(row.id.clone(), row);
        }
        repo
    }
}

#[async_trait::async_trait]
impl IAgentMetadataRepository for StubAgentMetadataRepo {
    async fn list_all(&self) -> Result<Vec<AgentMetadataRow>, DbError> {
        Ok(self.rows_by_id.values().cloned().collect())
    }
    async fn list_all_for_user(&self, _user_id: &str) -> Result<Vec<AgentMetadataRow>, DbError> {
        self.list_all().await
    }
    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(self.rows_by_id.get(id).cloned())
    }
    async fn get_for_user(&self, _user_id: &str, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        self.get(id).await
    }
    async fn find_by_source_and_name(
        &self,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(self
            .rows_by_id
            .values()
            .find(|row| row.agent_source == agent_source && row.name == name)
            .cloned())
    }
    async fn find_by_source_and_name_for_user(
        &self,
        _user_id: &str,
        agent_source: &str,
        name: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.find_by_source_and_name(agent_source, name).await
    }
    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(self.builtin_by_backend.get(backend).cloned())
    }
    async fn find_builtin_by_backend_for_user(
        &self,
        _user_id: &str,
        backend: &str,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.find_builtin_by_backend(backend).await
    }
    async fn upsert(&self, _params: &UpsertAgentMetadataParams<'_>) -> Result<AgentMetadataRow, DbError> {
        Err(DbError::Init("stub".into()))
    }
    async fn upsert_for_user(
        &self,
        _user_id: &str,
        params: &UpsertAgentMetadataParams<'_>,
    ) -> Result<AgentMetadataRow, DbError> {
        self.upsert(params).await
    }
    async fn apply_handshake(
        &self,
        _id: &str,
        _params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn apply_handshake_for_user(
        &self,
        _user_id: &str,
        id: &str,
        params: &UpdateAgentHandshakeParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.apply_handshake(id, params).await
    }
    async fn update_availability_snapshot(
        &self,
        _id: &str,
        _params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(None)
    }
    async fn update_availability_snapshot_for_user(
        &self,
        _user_id: &str,
        id: &str,
        params: &UpdateAgentAvailabilitySnapshotParams<'_>,
    ) -> Result<Option<AgentMetadataRow>, DbError> {
        self.update_availability_snapshot(id, params).await
    }
    async fn update_agent_overrides(
        &self,
        _id: &str,
        _command_override: Option<&str>,
        _env_override: Option<&str>,
    ) -> Result<(), DbError> {
        Ok(())
    }
    async fn update_agent_overrides_for_user(
        &self,
        _user_id: &str,
        id: &str,
        command_override: Option<&str>,
        env_override: Option<&str>,
    ) -> Result<(), DbError> {
        self.update_agent_overrides(id, command_override, env_override).await
    }
    async fn set_enabled(&self, _id: &str, _enabled: bool) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn set_enabled_for_user(&self, _user_id: &str, id: &str, enabled: bool) -> Result<bool, DbError> {
        self.set_enabled(id, enabled).await
    }
    async fn delete(&self, _id: &str) -> Result<bool, DbError> {
        Ok(false)
    }
    async fn delete_for_user(&self, _user_id: &str, id: &str) -> Result<bool, DbError> {
        self.delete(id).await
    }
}

// ---------------------------------------------------------------------------
// Counting task manager — wraps WorkerTaskManagerImpl so tests can assert
// kill / get_or_build_task call counts by conversation id.
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct TaskManagerCalls {
    kill: Vec<(String, Option<AgentKillReason>)>,
    build: Vec<String>,
}

struct CountingTaskManager {
    inner: WorkerTaskManagerImpl,
    calls: Mutex<TaskManagerCalls>,
}

impl CountingTaskManager {
    fn new(factory: AgentFactory) -> Self {
        Self {
            inner: WorkerTaskManagerImpl::new(factory),
            calls: Mutex::new(TaskManagerCalls::default()),
        }
    }

    async fn reset(&self) {
        self.inner.clear().await;
        *self.calls.lock().unwrap() = TaskManagerCalls::default();
    }

    fn snapshot(&self) -> TaskManagerCalls {
        self.calls.lock().unwrap().clone()
    }

    fn reset_calls(&self) {
        *self.calls.lock().unwrap() = TaskManagerCalls::default();
    }

    async fn remove_task_without_recording(&self, conversation_id: &str) {
        self.inner
            .kill_and_wait(conversation_id, Some(AgentKillReason::TeamMcpRebuild))
            .await;
    }
}

#[async_trait::async_trait]
impl IWorkerTaskManager for CountingTaskManager {
    fn get_task(&self, conversation_id: &str) -> Option<aionui_ai_agent::AgentInstance> {
        self.inner.get_task(conversation_id)
    }
    async fn get_or_build_task(
        &self,
        conversation_id: &str,
        options: BuildTaskOptions,
    ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
        self.calls.lock().unwrap().build.push(conversation_id.to_owned());
        self.inner.get_or_build_task(conversation_id, options).await
    }
    fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.calls
            .lock()
            .unwrap()
            .kill
            .push((conversation_id.to_owned(), reason));
        self.inner.kill(conversation_id, reason)
    }
    fn kill_and_wait(
        &self,
        conversation_id: &str,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        self.calls
            .lock()
            .unwrap()
            .kill
            .push((conversation_id.to_owned(), reason));
        self.inner.kill_and_wait(conversation_id, reason)
    }
    async fn clear(&self) {
        self.inner.clear().await
    }
    fn active_count(&self) -> usize {
        self.inner.active_count()
    }
    fn collect_idle(&self, idle_threshold_ms: aionui_common::TimestampMs) -> Vec<String> {
        self.inner.collect_idle(idle_threshold_ms)
    }
}

// Minimal stub agent returned by the test factory: ensure_session only
// asks the task manager to kill + rebuild; the returned handle never has
// `send_message` called on it.
mod mock_agent {
    use aionui_ai_agent::AgentError;
    use aionui_ai_agent::agent_task::{IAgentTask, IMockAgent};
    use aionui_ai_agent::protocol::events::AgentStreamEvent;
    use aionui_ai_agent::types::SendMessageData;
    use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, TimestampMs};
    use tokio::sync::broadcast;

    pub struct MockAgent {
        pub conversation_id: String,
        pub workspace: String,
        pub event_tx: broadcast::Sender<AgentStreamEvent>,
        pub confirmations: Vec<Confirmation>,
        pub status: Option<std::sync::Arc<std::sync::Mutex<Option<ConversationStatus>>>>,
    }

    impl MockAgent {
        pub fn new(conversation_id: String, workspace: String) -> Self {
            Self::with_confirmations_and_status(conversation_id, workspace, Vec::new(), None)
        }

        pub fn with_confirmations(
            conversation_id: String,
            workspace: String,
            confirmations: Vec<Confirmation>,
        ) -> Self {
            Self::with_confirmations_and_status(conversation_id, workspace, confirmations, None)
        }

        fn with_confirmations_and_status(
            conversation_id: String,
            workspace: String,
            confirmations: Vec<Confirmation>,
            status: Option<std::sync::Arc<std::sync::Mutex<Option<ConversationStatus>>>>,
        ) -> Self {
            let (event_tx, _) = broadcast::channel(16);
            Self {
                conversation_id,
                workspace,
                event_tx,
                confirmations,
                status,
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
            &self.workspace
        }
        fn status(&self) -> Option<ConversationStatus> {
            self.status.as_ref().and_then(|status| *status.lock().unwrap())
        }
        fn last_activity_at(&self) -> TimestampMs {
            0
        }
        fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
            self.event_tx.subscribe()
        }
        async fn send_message(&self, _data: SendMessageData) -> Result<(), aionui_ai_agent::AgentSendError> {
            Ok(())
        }
        async fn cancel(&self) -> Result<(), AgentError> {
            Ok(())
        }
        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            Ok(())
        }
    }

    impl IMockAgent for MockAgent {
        fn get_confirmations(&self) -> Vec<Confirmation> {
            self.confirmations.clone()
        }
    }
}

fn success_factory() -> AgentFactory {
    use futures_util::FutureExt;
    Arc::new(|opts: BuildTaskOptions| {
        async move {
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    })
}

fn blocking_first_build_factory(started: Arc<tokio::sync::Notify>, release: Arc<tokio::sync::Notify>) -> AgentFactory {
    use futures_util::FutureExt;

    let calls = Arc::new(AtomicUsize::new(0));
    Arc::new(move |opts: BuildTaskOptions| {
        let started = Arc::clone(&started);
        let release = Arc::clone(&release);
        let calls = Arc::clone(&calls);
        async move {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                started.notify_one();
                release.notified().await;
            }

            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    })
}

struct GatedProvisioningFactory {
    enabled: AtomicBool,
    starts: Mutex<Vec<String>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
}

impl Default for GatedProvisioningFactory {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(false),
            starts: Mutex::new(Vec::new()),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        }
    }
}

impl GatedProvisioningFactory {
    fn factory(self: &Arc<Self>) -> AgentFactory {
        use futures_util::FutureExt;

        let gate = Arc::clone(self);
        Arc::new(move |opts: BuildTaskOptions| {
            let gate = Arc::clone(&gate);
            async move {
                let conversation_id = opts.context.conversation.conversation_id.clone();
                if gate.enabled.swap(false, Ordering::SeqCst) {
                    gate.starts.lock().unwrap().push(conversation_id.clone());
                    gate.started.notify_waiters();
                    gate.release.acquire().await.expect("provisioning gate closed").forget();
                }
                Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                    mock_agent::MockAgent::new(conversation_id, opts.context.workspace.path),
                )))
            }
            .boxed()
        })
    }

    fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    async fn wait_for_starts(&self, count: usize) {
        loop {
            let notified = self.started.notified();
            if self.starts.lock().unwrap().len() >= count {
                return;
            }
            notified.await;
        }
    }

    fn starts(&self) -> Vec<String> {
        self.starts.lock().unwrap().clone()
    }

    fn release(&self, count: usize) {
        self.release.add_permits(count);
    }
}

fn confirmations_factory(count: usize) -> AgentFactory {
    use aionui_common::Confirmation;
    use futures_util::FutureExt;
    Arc::new(move |opts: BuildTaskOptions| {
        let confirmations = (0..count)
            .map(|idx| Confirmation {
                id: format!("tool-{idx}"),
                call_id: format!("tool-{idx}"),
                title: None,
                action: None,
                description: format!("Confirm tool {idx}"),
                command_type: None,
                options: vec![],
            })
            .collect::<Vec<_>>();
        async move {
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::with_confirmations(
                    opts.context.conversation.conversation_id,
                    opts.context.workspace.path,
                    confirmations,
                ),
            )))
        }
        .boxed()
    })
}

fn test_acp_build_options(conversation_id: String, workspace: String) -> BuildTaskOptions {
    BuildTaskOptions::new(AgentSessionContext {
        conversation: ConversationContext {
            conversation_id,
            user_id: "user1".into(),
            agent_type: aionui_common::AgentType::Acp,
            source: None,
        },
        workspace: WorkspaceContext {
            path: workspace.clone(),
            stored_path: workspace,
            is_custom: true,
        },
        model: ProviderWithModel {
            provider_id: "test".into(),
            model: "claude".into(),
            use_model: None,
        },
        skills: Vec::new(),
        runtime_env: Vec::new(),
        team: None,
        kind: AgentSessionKind::Acp(Box::new(AcpSessionBuildContext {
            config: AcpBuildExtra::default(),
            team: None,
            belongs_to_team: false,
            session_id: None,
            session_snapshot: None,
        })),
    })
}

struct EmptyTeamAssistantCatalog;

#[async_trait::async_trait]
impl TeamAssistantCatalogPort for EmptyTeamAssistantCatalog {
    async fn list_team_selectable_assistants(
        &self,
        _user_id: &str,
    ) -> Result<Vec<TeamAssistantCatalogEntry>, TeamError> {
        Ok(Vec::new())
    }
}

struct TestTeamAssistantCatalog {
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
}

#[async_trait::async_trait]
impl TeamAssistantCatalogPort for TestTeamAssistantCatalog {
    async fn list_team_selectable_assistants(
        &self,
        user_id: &str,
    ) -> Result<Vec<TeamAssistantCatalogEntry>, TeamError> {
        let agent_rows = self.agent_metadata_repo.list_all_for_user(user_id).await?;
        let definitions = self.assistant_definition_repo.list_for_user(user_id).await?;
        let mut result = Vec::new();

        for definition in definitions {
            let overlay = self
                .assistant_overlay_repo
                .get_for_user(user_id, &definition.id)
                .await?;
            if overlay.as_ref().is_some_and(|row| !row.enabled) {
                continue;
            }
            let effective_agent_id = overlay
                .as_ref()
                .and_then(|row| row.agent_id_override.as_deref())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(definition.agent_id.as_str());
            let backend = resolve_agent_binding_from_rows(&agent_rows, effective_agent_id)
                .map(|binding| binding.runtime_backend)
                .unwrap_or_else(|| effective_agent_id.to_owned());
            result.push(TeamAssistantCatalogEntry {
                assistant_id: definition.assistant_id,
                name: definition.name,
                backend,
                description: definition.description.unwrap_or_default(),
                skills: Vec::new(),
            });
        }

        Ok(result)
    }
}

struct EmptyProviderRepo;

#[async_trait::async_trait]
impl IProviderRepository for EmptyProviderRepo {
    async fn list(&self, _user_id: &str) -> Result<Vec<aionui_db::models::Provider>, DbError> {
        Ok(vec![])
    }
    async fn find_by_id(&self, _user_id: &str, _id: &str) -> Result<Option<aionui_db::models::Provider>, DbError> {
        Ok(None)
    }
    async fn create(
        &self,
        _params: aionui_db::CreateProviderParams<'_>,
    ) -> Result<aionui_db::models::Provider, DbError> {
        Err(DbError::NotFound("not implemented".into()))
    }
    async fn update(
        &self,
        _user_id: &str,
        _id: &str,
        _params: aionui_db::UpdateProviderParams<'_>,
    ) -> Result<aionui_db::models::Provider, DbError> {
        Err(DbError::NotFound("not implemented".into()))
    }
    async fn delete(&self, _user_id: &str, _id: &str) -> Result<(), DbError> {
        Err(DbError::NotFound("not implemented".into()))
    }
}

struct EmptyAssistantDefinitionRepo;

#[async_trait::async_trait]
impl IAssistantDefinitionRepository for EmptyAssistantDefinitionRepo {
    async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        Ok(vec![])
    }

    async fn list_for_user(&self, _user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.list().await
    }

    async fn list_including_deleted_for_user(&self, _user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.list().await
    }

    async fn get_by_assistant_id(&self, _assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn get_by_assistant_id_for_user(
        &self,
        _user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id(assistant_id).await
    }

    async fn get_by_assistant_id_including_deleted_for_user(
        &self,
        _user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id(assistant_id).await
    }

    async fn get_by_id(&self, _definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn get_by_id_for_user(
        &self,
        _user_id: &str,
        definition_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_id(definition_id).await
    }

    async fn get_by_source_ref(
        &self,
        _source: &str,
        _source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn get_by_source_ref_for_user(
        &self,
        _user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref(source, source_ref).await
    }

    async fn get_by_source_ref_including_deleted_for_user(
        &self,
        _user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref(source, source_ref).await
    }

    async fn upsert(&self, _params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn upsert_for_user(
        &self,
        _user_id: &str,
        params: &UpsertAssistantDefinitionParams<'_>,
    ) -> Result<AssistantDefinitionRow, DbError> {
        self.upsert(params).await
    }

    async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, DbError> {
        Ok(false)
    }

    async fn soft_delete_for_user(
        &self,
        _user_id: &str,
        definition_id: &str,
        deleted_at: i64,
    ) -> Result<bool, DbError> {
        self.soft_delete(definition_id, deleted_at).await
    }
}

struct EmptyAssistantOverlayRepo;

#[async_trait::async_trait]
impl IAssistantOverlayRepository for EmptyAssistantOverlayRepo {
    async fn get(&self, _definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        Ok(None)
    }

    async fn get_for_user(&self, _user_id: &str, definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        self.get(definition_id).await
    }

    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
        Ok(vec![])
    }

    async fn list_for_user(&self, _user_id: &str) -> Result<Vec<AssistantOverlayRow>, DbError> {
        self.list().await
    }

    async fn upsert(&self, _params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn upsert_for_user(
        &self,
        _user_id: &str,
        params: &UpsertAssistantOverlayParams<'_>,
    ) -> Result<AssistantOverlayRow, DbError> {
        self.upsert(params).await
    }

    async fn delete(&self, _definition_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }

    async fn delete_for_user(&self, _user_id: &str, definition_id: &str) -> Result<bool, DbError> {
        self.delete(definition_id).await
    }
}

struct SingleAssistantDefinitionRepo {
    row: AssistantDefinitionRow,
}

#[async_trait::async_trait]
impl IAssistantDefinitionRepository for SingleAssistantDefinitionRepo {
    async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        Ok(vec![self.row.clone()])
    }

    async fn list_for_user(&self, _user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.list().await
    }

    async fn list_including_deleted_for_user(&self, _user_id: &str) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        self.list().await
    }

    async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok((self.row.assistant_id == assistant_id).then_some(self.row.clone()))
    }

    async fn get_by_assistant_id_for_user(
        &self,
        _user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id(assistant_id).await
    }

    async fn get_by_assistant_id_including_deleted_for_user(
        &self,
        _user_id: &str,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_assistant_id(assistant_id).await
    }

    async fn get_by_id(&self, definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok((self.row.id == definition_id).then_some(self.row.clone()))
    }

    async fn get_by_id_for_user(
        &self,
        _user_id: &str,
        definition_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_id(definition_id).await
    }

    async fn get_by_source_ref(
        &self,
        _source: &str,
        _source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn get_by_source_ref_for_user(
        &self,
        _user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref(source, source_ref).await
    }

    async fn get_by_source_ref_including_deleted_for_user(
        &self,
        _user_id: &str,
        source: &str,
        source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        self.get_by_source_ref(source, source_ref).await
    }

    async fn upsert(&self, _params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn upsert_for_user(
        &self,
        _user_id: &str,
        params: &UpsertAssistantDefinitionParams<'_>,
    ) -> Result<AssistantDefinitionRow, DbError> {
        self.upsert(params).await
    }

    async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, DbError> {
        Ok(false)
    }

    async fn soft_delete_for_user(
        &self,
        _user_id: &str,
        definition_id: &str,
        deleted_at: i64,
    ) -> Result<bool, DbError> {
        self.soft_delete(definition_id, deleted_at).await
    }
}

struct SingleAssistantOverlayRepo {
    row: AssistantOverlayRow,
}

#[async_trait::async_trait]
impl IAssistantOverlayRepository for SingleAssistantOverlayRepo {
    async fn get(&self, definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        Ok((self.row.assistant_definition_id == definition_id).then_some(self.row.clone()))
    }

    async fn get_for_user(&self, _user_id: &str, definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        self.get(definition_id).await
    }

    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
        Ok(vec![self.row.clone()])
    }

    async fn list_for_user(&self, _user_id: &str) -> Result<Vec<AssistantOverlayRow>, DbError> {
        self.list().await
    }

    async fn upsert(&self, _params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn upsert_for_user(
        &self,
        _user_id: &str,
        params: &UpsertAssistantOverlayParams<'_>,
    ) -> Result<AssistantOverlayRow, DbError> {
        self.upsert(params).await
    }

    async fn delete(&self, _definition_id: &str) -> Result<bool, DbError> {
        Ok(false)
    }

    async fn delete_for_user(&self, _user_id: &str, definition_id: &str) -> Result<bool, DbError> {
        self.delete(definition_id).await
    }
}

fn setup_with_factory(factory: AgentFactory) -> (Arc<TeamSessionService>, Arc<CountingTaskManager>) {
    setup_with_factory_and_metadata(factory, Arc::new(StubAgentMetadataRepo::empty()))
}

fn setup_with_factory_and_metadata(
    factory: AgentFactory,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
) -> (Arc<TeamSessionService>, Arc<CountingTaskManager>) {
    let (svc, task_manager, _) = setup_with_factory_and_metadata_and_conversation_repo(factory, agent_metadata_repo);
    (svc, task_manager)
}

fn setup_with_factory_and_metadata_and_conversation_repo(
    factory: AgentFactory,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
) -> (
    Arc<TeamSessionService>,
    Arc<CountingTaskManager>,
    Arc<MockConversationRepo>,
) {
    let (svc, _, task_manager, conv_repo) =
        setup_with_factory_metadata_team_repo_and_conversation_repo(factory, agent_metadata_repo);
    (svc, task_manager, conv_repo)
}

fn setup_with_factory_metadata_team_repo_and_conversation_repo(
    factory: AgentFactory,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
) -> (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<CountingTaskManager>,
    Arc<MockConversationRepo>,
) {
    let team_repo = Arc::new(FullMockTeamRepo::new());
    let team_repo_dyn: Arc<dyn ITeamRepository> = team_repo.clone();
    let conv_repo = Arc::new(MockConversationRepo::new());
    let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
    let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo.clone()));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
    let task_manager = Arc::new(CountingTaskManager::new(factory));
    let task_manager_dyn: Arc<dyn IWorkerTaskManager> = task_manager.clone();
    let backend_binary_path = Arc::new(std::path::PathBuf::from("/tmp/aioncore-test"));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(EmptyProviderRepo);
    let svc = TeamSessionService::new(
        team_repo_dyn,
        agent_metadata_repo,
        Arc::new(EmptyTeamAssistantCatalog),
        Arc::new(EmptyAssistantDefinitionRepo),
        Arc::new(EmptyAssistantOverlayRepo),
        provider_repo,
        conversation_port,
        projection_store,
        broadcaster,
        task_manager_dyn,
        noop_turn_port(),
        noop_cancellation_port(),
        backend_binary_path,
    );
    (svc, team_repo, task_manager, conv_repo)
}

fn setup_with_factory_metadata_assistants_and_conversation_repo(
    factory: AgentFactory,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
) -> (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<CountingTaskManager>,
    Arc<MockConversationRepo>,
) {
    let team_repo = Arc::new(FullMockTeamRepo::new());
    let team_repo_dyn: Arc<dyn ITeamRepository> = team_repo.clone();
    let conv_repo = Arc::new(MockConversationRepo::new());
    let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
    let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo.clone()));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
    let task_manager = Arc::new(CountingTaskManager::new(factory));
    let task_manager_dyn: Arc<dyn IWorkerTaskManager> = task_manager.clone();
    let backend_binary_path = Arc::new(std::path::PathBuf::from("/tmp/aioncore-test"));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(EmptyProviderRepo);
    let assistant_catalog: Arc<dyn TeamAssistantCatalogPort> = Arc::new(TestTeamAssistantCatalog {
        agent_metadata_repo: agent_metadata_repo.clone(),
        assistant_definition_repo: assistant_definition_repo.clone(),
        assistant_overlay_repo: assistant_overlay_repo.clone(),
    });
    let svc = TeamSessionService::new(
        team_repo_dyn,
        agent_metadata_repo,
        assistant_catalog,
        assistant_definition_repo,
        assistant_overlay_repo,
        provider_repo,
        conversation_port,
        projection_store,
        broadcaster,
        task_manager_dyn,
        noop_turn_port(),
        noop_cancellation_port(),
        backend_binary_path,
    );
    (svc, team_repo, task_manager, conv_repo)
}

fn setup_with_ports_team_repo_and_conversation_repo(
    factory: AgentFactory,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
) -> (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<FakeConversationPorts>,
    Arc<MockConversationRepo>,
) {
    setup_with_ports_metadata_assistants_and_conversation_repo(
        factory,
        agent_metadata_repo,
        Arc::new(EmptyAssistantDefinitionRepo),
        Arc::new(EmptyAssistantOverlayRepo),
    )
}

fn setup_with_ports_metadata_assistants_and_conversation_repo(
    factory: AgentFactory,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
) -> (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<FakeConversationPorts>,
    Arc<MockConversationRepo>,
) {
    let team_repo = Arc::new(FullMockTeamRepo::new());
    let team_repo_dyn: Arc<dyn ITeamRepository> = team_repo.clone();
    let conv_repo = Arc::new(MockConversationRepo::new());
    let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
    let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo.clone()));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(CountingTaskManager::new(factory));
    let backend_binary_path = Arc::new(std::path::PathBuf::from("/tmp/aioncore-test"));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(EmptyProviderRepo);
    let assistant_catalog: Arc<dyn TeamAssistantCatalogPort> = Arc::new(TestTeamAssistantCatalog {
        agent_metadata_repo: agent_metadata_repo.clone(),
        assistant_definition_repo: assistant_definition_repo.clone(),
        assistant_overlay_repo: assistant_overlay_repo.clone(),
    });
    let svc = TeamSessionService::new(
        team_repo_dyn,
        agent_metadata_repo,
        assistant_catalog,
        assistant_definition_repo,
        assistant_overlay_repo,
        provider_repo,
        conversation_port,
        projection_store,
        broadcaster,
        task_manager,
        noop_turn_port(),
        noop_cancellation_port(),
        backend_binary_path,
    );
    (svc, team_repo, conversation_ports, conv_repo)
}

fn setup_with_recording_turn_port() -> (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<RecordingTurnPort>,
    Arc<MockConversationRepo>,
) {
    let team_repo = Arc::new(FullMockTeamRepo::new());
    let team_repo_dyn: Arc<dyn ITeamRepository> = team_repo.clone();
    let conv_repo = Arc::new(MockConversationRepo::new());
    let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
    let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo.clone()));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(CountingTaskManager::new(success_factory()));
    let turn_port = Arc::new(RecordingTurnPort::default());
    let backend_binary_path = Arc::new(std::path::PathBuf::from("/tmp/aioncore-test"));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(EmptyProviderRepo);
    let svc = TeamSessionService::new(
        team_repo_dyn,
        Arc::new(StubAgentMetadataRepo::empty()),
        Arc::new(EmptyTeamAssistantCatalog),
        Arc::new(EmptyAssistantDefinitionRepo),
        Arc::new(EmptyAssistantOverlayRepo),
        provider_repo,
        conversation_port,
        projection_store,
        broadcaster,
        task_manager,
        turn_port.clone(),
        noop_cancellation_port(),
        backend_binary_path,
    );
    (svc, team_repo, turn_port, conv_repo)
}

fn setup() -> Arc<TeamSessionService> {
    setup_with_factory(success_factory()).0
}

#[tokio::test]
async fn recovery_creates_system_run_intents_without_restoring_old_memory_run() {
    let (svc, team_repo, turn_port, _conv_repo) = setup_with_recording_turn_port();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Recover".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let lead_slot_id = created.leader_assistant_id.clone().expect("lead");
    svc.stop_session("user1", &created.id)
        .await
        .expect("clear existing session");

    team_repo
        .write_message(
            "user1",
            &aionui_db::models::MailboxMessageRow {
                id: "mailbox-orphan-1".into(),
                team_id: created.id.clone(),
                to_agent_id: lead_slot_id.clone(),
                from_agent_id: "worker-or-user".into(),
                msg_type: "message".into(),
                content: "orphan backlog".into(),
                summary: None,
                files: None,
                read: false,
                created_at: aionui_common::now_ms(),
            },
        )
        .await
        .expect("seed orphan mailbox");

    svc.ensure_session("user1", &created.id).await.expect("ensure");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if !turn_port.requests.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovery turn should run");

    let requests = turn_port.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].slot_id, lead_slot_id);
    // Recovery drain now runs the recovered turn inside a freshly opened
    // SystemLifecycle run (not run-less background, and not a restored
    // pre-restart run), so the recovered turn carries a run id.
    assert!(
        requests[0].team_run_id.is_some(),
        "recovery work must run inside a system run, not run-less background"
    );
}

#[tokio::test]
async fn teammate_first_wake_uses_canonical_prompt_at_service_boundary() {
    let (svc, _team_repo, turn_port, _conv_repo) = setup_with_recording_turn_port();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Recover Teammate".into(),
                agents: aionrs_two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let worker_slot_id = created.assistants[1].slot_id.clone();
    svc.stop_session("user1", &created.id)
        .await
        .expect("clear existing session");

    svc.ensure_session("user1", &created.id).await.expect("ensure");

    // Leader-only warmup: the teammate is dormant at first start. Delivering a
    // message lazily wakes it, and its first turn is a cold wake built with the
    // canonical role prompt plus the delivered content (spec 5.1).
    svc.send_message_to_agent("user1", &created.id, &worker_slot_id, "do X", None)
        .await
        .expect("deliver to teammate triggers lazy wakeup");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if turn_port
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|request| request.slot_id == worker_slot_id)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("teammate recovery turn should run");

    let requests = turn_port.requests.lock().unwrap();
    let worker_request = requests
        .iter()
        .find(|request| request.slot_id == worker_slot_id)
        .expect("worker turn request");
    let first_message = &worker_request.content;
    assert!(first_message.contains("## Team Governance"));
    assert!(first_message.contains("You MUST use the `team_*` MCP tools for ALL team coordination."));
    assert!(first_message.contains("Use team_send_message to report results to the leader"));
    assert!(first_message.contains("STOP GENERATING"));
    assert!(!first_message.contains(
        "You execute tasks assigned by the Lead Agent. Focus on completing your assigned work thoroughly and reporting back."
    ));
    assert!(first_message.contains("do X"));
}

#[tokio::test]
async fn ensure_session_does_not_run_self_message_only_recovery_turn() {
    let (svc, team_repo, turn_port, _conv_repo) = setup_with_recording_turn_port();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Self Only".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let lead_slot_id = created.leader_assistant_id.clone().expect("lead");
    svc.stop_session("user1", &created.id)
        .await
        .expect("clear existing session");

    team_repo
        .write_message(
            "user1",
            &aionui_db::models::MailboxMessageRow {
                id: "mailbox-self-1".into(),
                team_id: created.id.clone(),
                to_agent_id: lead_slot_id.clone(),
                from_agent_id: lead_slot_id,
                msg_type: "message".into(),
                content: "self backlog".into(),
                summary: None,
                files: None,
                read: false,
                created_at: aionui_common::now_ms(),
            },
        )
        .await
        .expect("seed self mailbox");

    svc.ensure_session("user1", &created.id).await.expect("ensure");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    assert!(
        turn_port.requests.lock().unwrap().is_empty(),
        "self-only unread must not start a recovery turn"
    );
}

fn setup_with_recording_broadcaster() -> (Arc<TeamSessionService>, Arc<RecordingBroadcaster>) {
    let team_repo: Arc<dyn ITeamRepository> = Arc::new(FullMockTeamRepo::new());
    let conv_repo = Arc::new(MockConversationRepo::new());
    let recorder = Arc::new(RecordingBroadcaster::new());
    let broadcaster: Arc<dyn EventBroadcaster> = recorder.clone();
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
    let task_manager: Arc<dyn IWorkerTaskManager> = Arc::new(CountingTaskManager::new(success_factory()));
    let backend_binary_path = Arc::new(std::path::PathBuf::from("/tmp/aioncore-test"));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(EmptyProviderRepo);
    let svc = TeamSessionService::new(
        team_repo,
        agent_metadata_repo,
        Arc::new(EmptyTeamAssistantCatalog),
        Arc::new(EmptyAssistantDefinitionRepo),
        Arc::new(EmptyAssistantOverlayRepo),
        provider_repo,
        conversation_port,
        projection_store,
        broadcaster,
        task_manager,
        noop_turn_port(),
        noop_cancellation_port(),
        backend_binary_path,
    );
    (svc, recorder)
}

fn setup_with_factory_and_recording_broadcaster(
    factory: AgentFactory,
) -> (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<CountingTaskManager>,
    Arc<RecordingBroadcaster>,
) {
    let (service, team_repo, task_manager, recorder, _conversation_repo) =
        setup_with_factory_recording_broadcaster_and_conversation_repo(factory);
    (service, team_repo, task_manager, recorder)
}

type RecordingServiceHarness = (
    Arc<TeamSessionService>,
    Arc<FullMockTeamRepo>,
    Arc<CountingTaskManager>,
    Arc<RecordingBroadcaster>,
    Arc<MockConversationRepo>,
);

fn setup_with_factory_recording_broadcaster_and_conversation_repo(factory: AgentFactory) -> RecordingServiceHarness {
    let team_repo = Arc::new(FullMockTeamRepo::new());
    let team_repo_dyn: Arc<dyn ITeamRepository> = team_repo.clone();
    let conv_repo = Arc::new(MockConversationRepo::new());
    let recorder = Arc::new(RecordingBroadcaster::new());
    let broadcaster: Arc<dyn EventBroadcaster> = recorder.clone();
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let conversation_ports = Arc::new(FakeConversationPorts::new(conv_repo.clone()));
    let conversation_port: Arc<dyn TeamConversationProvisioningPort> = conversation_ports.clone();
    let projection_store: Arc<dyn TeamProjectionMessageStore> = conversation_ports.clone();
    let task_manager = Arc::new(CountingTaskManager::new(factory));
    let task_manager_dyn: Arc<dyn IWorkerTaskManager> = task_manager.clone();
    let backend_binary_path = Arc::new(std::path::PathBuf::from("/tmp/aioncore-test"));
    let provider_repo: Arc<dyn IProviderRepository> = Arc::new(EmptyProviderRepo);
    let svc = TeamSessionService::new(
        team_repo_dyn,
        agent_metadata_repo,
        Arc::new(EmptyTeamAssistantCatalog),
        Arc::new(EmptyAssistantDefinitionRepo),
        Arc::new(EmptyAssistantOverlayRepo),
        provider_repo,
        conversation_port,
        projection_store,
        broadcaster,
        task_manager_dyn,
        noop_turn_port(),
        noop_cancellation_port(),
        backend_binary_path,
    );
    (svc, team_repo, task_manager, recorder, conv_repo)
}

fn make_agent_metadata_row(id: &str, backend: &str, icon: &str) -> AgentMetadataRow {
    AgentMetadataRow {
        id: id.to_owned(),
        user_id: None,
        icon: Some(icon.to_owned()),
        name: backend.to_owned(),
        name_i18n: None,
        description: None,
        description_i18n: None,
        backend: Some(backend.to_owned()),
        agent_type: "acp".to_owned(),
        agent_source: "builtin".to_owned(),
        agent_source_info: None,
        enabled: true,
        command: None,
        args: None,
        env: None,
        native_skills_dirs: None,
        behavior_policy: None,
        yolo_id: None,
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
        created_at: 0,
        updated_at: 0,
    }
}

fn acp_agent_metadata_row(id: &str, backend: &str, yolo_id: Option<&str>) -> AgentMetadataRow {
    let mut row = make_agent_metadata_row(id, backend, "");
    row.yolo_id = yolo_id.map(str::to_owned);
    row
}

fn seeded_agent_metadata_repo() -> Arc<dyn IAgentMetadataRepository> {
    Arc::new(StubAgentMetadataRepo::with_rows(vec![
        acp_agent_metadata_row("claude-id", "claude", Some("bypassPermissions")),
        acp_agent_metadata_row("codex-id", "codex", Some("full-access")),
        acp_agent_metadata_row("gemini-id", "gemini", Some("yolo")),
    ]))
}

fn word_creator_definition() -> AssistantDefinitionRow {
    AssistantDefinitionRow {
        id: "def-word-creator".into(),
        assistant_id: "word-creator".into(),
        source: "builtin".into(),
        owner_type: "system".into(),
        source_ref: Some("word-creator".into()),
        name: "Word Creator".into(),
        name_i18n: "{}".into(),
        description: Some("Drafts Word documents".into()),
        description_i18n: "{}".into(),
        avatar_type: "builtin_asset".into(),
        avatar_value: None,
        agent_id: "claude".into(),
        rule_resource_type: "none".into(),
        rule_resource_ref: None,
        recommended_prompts: "[]".into(),
        recommended_prompts_i18n: "{}".into(),
        default_model_mode: "auto".into(),
        default_model_value: None,
        default_permission_mode: "auto".into(),
        default_permission_value: None,
        default_thought_level_mode: "auto".into(),
        default_thought_level_value: None,
        default_skills_mode: "auto".into(),
        default_skill_ids: "[]".into(),
        custom_skill_names: "[]".into(),
        default_disabled_builtin_skill_ids: "[]".into(),
        default_mcps_mode: "auto".into(),
        default_mcp_ids: "[]".into(),
        created_at: 0,
        updated_at: 0,
        deleted_at: None,
    }
}

fn setup_with_metadata_rows(rows: Vec<AgentMetadataRow>) -> Arc<TeamSessionService> {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::with_rows(rows));
    setup_with_factory_and_metadata(success_factory(), agent_metadata_repo).0
}

fn two_agent_input() -> Vec<TeamAgentInput> {
    vec![
        TeamAgentInput {
            name: "Lead".into(),
            role: "lead".into(),
            backend: Some("acp".into()),
            model: "claude".into(),
            assistant_id: None,
            conversation_id: None,
        },
        TeamAgentInput {
            name: "Worker".into(),
            role: "teammate".into(),
            backend: Some("acp".into()),
            model: "claude".into(),
            assistant_id: None,
            conversation_id: None,
        },
    ]
}

fn aionrs_two_agent_input() -> Vec<TeamAgentInput> {
    two_agent_input()
        .into_iter()
        .map(|mut agent| {
            agent.backend = Some("aionrs".into());
            agent
        })
        .collect()
}

fn team_agent_input(name: &str, role: &str, model: &str) -> TeamAgentInput {
    TeamAgentInput {
        name: name.into(),
        role: role.into(),
        backend: Some("acp".into()),
        model: model.into(),
        assistant_id: None,
        conversation_id: None,
    }
}

fn four_agent_input_leader_not_first() -> Vec<TeamAgentInput> {
    vec![
        team_agent_input("Worker 1", "teammate", "worker-1"),
        team_agent_input("Lead", "lead", "lead-model"),
        team_agent_input("Worker 2", "teammate", "worker-2"),
        team_agent_input("Worker 3", "teammate", "worker-3"),
    ]
}

fn five_agent_input_leader_not_first() -> Vec<TeamAgentInput> {
    vec![
        team_agent_input("Worker 1", "teammate", "worker-1"),
        team_agent_input("Worker 2", "teammate", "worker-2"),
        team_agent_input("Lead", "lead", "lead-model"),
        team_agent_input("Worker 3", "teammate", "worker-3"),
        team_agent_input("Worker 4", "teammate", "worker-4"),
    ]
}

struct WarmupConcurrencyProbe {
    active: AtomicUsize,
    max_active: AtomicUsize,
    starts: Mutex<Vec<String>>,
}

impl Default for WarmupConcurrencyProbe {
    fn default() -> Self {
        Self {
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            starts: Mutex::new(Vec::new()),
        }
    }
}

impl WarmupConcurrencyProbe {
    fn factory(self: &Arc<Self>, delay: std::time::Duration) -> AgentFactory {
        use futures_util::FutureExt;

        let probe = Arc::clone(self);
        Arc::new(move |opts: BuildTaskOptions| {
            let probe = Arc::clone(&probe);
            async move {
                let conversation_id = opts.context.conversation.conversation_id.clone();
                probe.starts.lock().unwrap().push(conversation_id.clone());
                let current = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
                probe.max_active.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(delay).await;
                probe.active.fetch_sub(1, Ordering::SeqCst);

                Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                    mock_agent::MockAgent::new(conversation_id, opts.context.workspace.path),
                )))
            }
            .boxed()
        })
    }

    fn max_active(&self) -> usize {
        self.max_active.load(Ordering::SeqCst)
    }

    fn starts(&self) -> Vec<String> {
        self.starts.lock().unwrap().clone()
    }
}

async fn reset_runtime_state(svc: &Arc<TeamSessionService>, tm: &Arc<CountingTaskManager>, team_id: &str) {
    svc.stop_session("user1", team_id).await.unwrap();
    tm.reset().await;
}

#[tokio::test]
async fn renew_active_lease_records_all_team_agent_conversations() {
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Lease Team".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let active_leases = ActiveLeaseRegistry::new();

    svc.renew_active_lease("user1", &created.id, &active_leases)
        .await
        .expect("renew team lease");

    for agent in &created.assistants {
        assert!(active_leases.is_active(&agent.conversation_id));
    }
}

#[tokio::test]
async fn renew_active_lease_allows_empty_team_without_unrelated_lease() {
    let (svc, team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo
        .create_team(&aionui_db::models::TeamRow {
            id: "team-empty".into(),
            user_id: "user1".into(),
            name: "Empty".into(),
            workspace: String::new(),
            workspace_mode: "shared".into(),
            agents: "[]".into(),
            lead_agent_id: None,
            session_mode: None,
            agents_version: "1.0.1".into(),
            created_at: aionui_common::now_ms(),
            updated_at: aionui_common::now_ms(),
            project_id: None,
            folder_id: None,
        })
        .await
        .expect("insert empty team");
    let active_leases = ActiveLeaseRegistry::new();

    svc.renew_active_lease("user1", "team-empty", &active_leases)
        .await
        .expect("empty team renew is accepted");

    assert!(!active_leases.is_active("team-empty"));
    assert!(!active_leases.is_active("unrelated-conversation"));
}

#[tokio::test]
async fn renew_active_lease_rejects_team_owned_by_other_user() {
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Lease Team".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let active_leases = ActiveLeaseRegistry::new();

    let err = svc
        .renew_active_lease("other-user", &created.id, &active_leases)
        .await
        .unwrap_err();

    assert!(matches!(err, TeamError::TeamNotFound(_)));
    for agent in &created.assistants {
        assert!(!active_leases.is_active(&agent.conversation_id));
    }
}

async fn force_team_workspace(repo: &Arc<FullMockTeamRepo>, team_id: &str, workspace: &str) {
    repo.update_team(
        "user1",
        team_id,
        &aionui_db::UpdateTeamParams {
            workspace: Some(workspace.to_owned()),
            ..Default::default()
        },
    )
    .await
    .expect("force workspace");
}

// ===========================================================================
// Test: Team CRUD (TC-*, TL-*, TG-*, TD-*, TR-*)
// ===========================================================================

#[tokio::test]
async fn tc1_create_team_with_multiple_agents() {
    let svc = setup();
    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(resp.name, "Alpha");
    assert_eq!(resp.assistants.len(), 2);
    assert_eq!(resp.assistants[0].role, "lead");
    assert_eq!(resp.assistants[1].role, "teammate");
    assert!(resp.leader_assistant_id.is_some());
    assert_eq!(resp.leader_assistant_id, Some(resp.assistants[0].slot_id.clone()));
}

#[tokio::test]
async fn create_team_rejects_existing_conversation_id_request_side_adoption() {
    let svc = setup();

    let err = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "No Adoption".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("claude".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: Some("solo-conv-1".into()),
                }],
                workspace: None,
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, TeamError::InvalidRequest(ref msg) if msg.contains("existing conversations are no longer supported")),
        "unexpected error: {err:?}"
    );
}

#[tokio::test]
async fn create_team_with_workspace_writes_same_workspace_to_team_and_initial_agents() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, _, conv_repo) =
        setup_with_factory_and_metadata_and_conversation_repo(success_factory(), agent_metadata_repo);
    let workspace_dir =
        std::env::temp_dir().join(format!("aionui-team-user-workspace-{}", aionui_common::generate_id()));
    std::fs::create_dir_all(&workspace_dir).unwrap();
    let workspace = workspace_dir.to_string_lossy().into_owned();

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Shared".into(),
                agents: two_agent_input(),
                workspace: Some(workspace.clone()),
            },
        )
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(got.workspace, workspace);
    for agent in &got.assistants {
        let extra = conv_repo.get_extra(&agent.conversation_id).unwrap();
        assert_eq!(
            extra.get("workspace").and_then(serde_json::Value::as_str),
            Some(workspace.as_str())
        );
    }
}

#[tokio::test]
async fn create_team_side_branch_backfills_project_binding_when_injected() {
    // Project-bind side branch: with a ProjectService injected, create_team
    // resolves the team workspace and backfills teams.project_id/folder_id.
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, team_repo, _tm, _conv_repo) =
        setup_with_factory_metadata_team_repo_and_conversation_repo(success_factory(), agent_metadata_repo);

    // Real store on an in-memory DB; leaked so the shared pool outlives the test.
    let db = aionui_db::init_database_memory().await.unwrap();
    // The project tables carry a users(id) FK; seed the acting owner as in
    // production, where the owner row always exists.
    sqlx::query(
        "INSERT INTO users (id, user_type, username, password_hash, status, session_generation, created_at, updated_at) \
         VALUES ('user1', 'local', 'user1', 'hash', 'active', 0, 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let store: Arc<dyn aionui_db::IProjectStore> = Arc::new(aionui_db::SqliteProjectStore::new(db.pool().clone()));
    std::mem::forget(db);
    svc.with_project_service(Arc::new(aionui_project::ProjectService::new(
        store,
        std::env::temp_dir().join("aionui-team-bind-test-conversations"),
    )));

    let workspace_dir = std::env::temp_dir().join(format!("aionui-team-bind-{}", aionui_common::generate_id()));
    std::fs::create_dir_all(&workspace_dir).unwrap();

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Bound".into(),
                agents: two_agent_input(),
                workspace: Some(workspace_dir.to_string_lossy().into_owned()),
            },
        )
        .await
        .unwrap();

    let row = team_repo.get_team("user1", &created.id).await.unwrap().unwrap();
    assert!(
        row.project_id.is_some(),
        "team create side branch should backfill project_id"
    );
    assert!(
        row.folder_id.is_some(),
        "team create side branch should backfill folder_id"
    );
    // Transitional workspace column must be untouched by the side branch.
    assert_eq!(row.workspace, workspace_dir.to_string_lossy());
}

#[tokio::test]
async fn create_team_without_workspace_uses_leader_auto_workspace_for_all_initial_agents() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, _, conv_repo) =
        setup_with_factory_and_metadata_and_conversation_repo(success_factory(), agent_metadata_repo);

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Auto Shared".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert!(!got.workspace.trim().is_empty(), "teams.workspace must be set");
    assert!(
        got.workspace.contains("/conversations/acp-temp-"),
        "unexpected auto workspace: {}",
        got.workspace
    );

    for agent in &got.assistants {
        let extra = conv_repo.get_extra(&agent.conversation_id).unwrap();
        assert_eq!(
            extra.get("workspace").and_then(serde_json::Value::as_str),
            Some(got.workspace.as_str())
        );
    }
}

#[tokio::test]
async fn tc_create_team_prefers_assistant_avatar_over_backend_logo() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(StubAgentMetadataRepo::with_rows(vec![make_agent_metadata_row(
            "builtin-claude",
            "claude",
            "/api/assets/logos/ai-major/claude.svg",
        )]));
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-team-lead".into(),
            assistant_id: "assistant-lead".into(),
            source: "builtin".into(),
            owner_type: "system".into(),
            source_ref: Some("assistant-lead".into()),
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "builtin_asset".into(),
            avatar_value: Some("avatars/assistant-lead.png".into()),
            agent_id: "claude".into(),
            rule_resource_type: "none".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        Arc::new(EmptyAssistantOverlayRepo),
    );

    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("claude".into()),
                    model: "claude".into(),
                    assistant_id: Some("assistant-lead".into()),
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(
        resp.assistants[0].icon.as_deref(),
        Some("/api/assistants/assistant-lead/avatar")
    );
}

#[tokio::test]
async fn tc_create_team_carries_assistant_identity_into_lead_conversation_extra() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = seeded_agent_metadata_repo();
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-team-lead".into(),
            assistant_id: "assistant-lead".into(),
            source: "user".into(),
            owner_type: "user".into(),
            source_ref: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "user_file".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let (svc, _team_repo, _task_manager, conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        Arc::new(EmptyAssistantOverlayRepo),
    );

    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("claude".into()),
                    model: "claude".into(),
                    assistant_id: Some("assistant-lead".into()),
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let row = conv_repo
        .get("user1", &resp.assistants[0].conversation_id)
        .await
        .unwrap()
        .expect("lead conversation row");
    let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();

    assert_eq!(extra["assistant_id"], serde_json::json!("assistant-lead"));
    assert!(extra.get("preset_assistant_id").is_none());
}

#[tokio::test]
async fn tc_create_team_derives_backend_from_assistant_when_backend_missing() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = seeded_agent_metadata_repo();
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-team-lead".into(),
            assistant_id: "assistant-lead".into(),
            source: "user".into(),
            owner_type: "user".into(),
            source_ref: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "user_file".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
        row: AssistantOverlayRow {
            assistant_definition_id: "def-team-lead".into(),
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex".into()),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        },
    });
    let (svc, _team_repo, _task_manager, conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        overlay_repo,
    );

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Assistant Lead".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some(String::new()),
                    model: "gpt-5".into(),
                    assistant_id: Some("assistant-lead".into()),
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(created.assistants[0].backend, "codex");
    let extra = conv_repo
        .get_extra(&created.assistants[0].conversation_id)
        .expect("lead conversation extra");
    assert_eq!(extra.get("backend").and_then(serde_json::Value::as_str), Some("codex"));
    assert_eq!(
        extra.get("assistant_id").and_then(serde_json::Value::as_str),
        Some("assistant-lead")
    );
}

#[tokio::test]
async fn tc_create_team_ignores_requested_backend_when_assistant_id_present() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = seeded_agent_metadata_repo();
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-team-lead".into(),
            assistant_id: "assistant-lead".into(),
            source: "user".into(),
            owner_type: "user".into(),
            source_ref: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "user_file".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
        row: AssistantOverlayRow {
            assistant_definition_id: "def-team-lead".into(),
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex".into()),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        },
    });
    let (svc, _team_repo, _task_manager, conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        overlay_repo,
    );

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Assistant Lead".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("gemini".into()),
                    model: "gpt-5".into(),
                    assistant_id: Some("assistant-lead".into()),
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(created.assistants[0].backend, "codex");
    let extra = conv_repo
        .get_extra(&created.assistants[0].conversation_id)
        .expect("lead conversation extra");
    assert_eq!(extra.get("backend").and_then(serde_json::Value::as_str), Some("codex"));
}

fn fake_preset_snapshot(rules: &str, skills: &[&str], mcp_server_ids: &[&str]) -> FakePresetAssistantSnapshot {
    FakePresetAssistantSnapshot {
        rules: rules.to_owned(),
        skills: skills.iter().map(|value| (*value).to_owned()).collect(),
        mcp_server_ids: mcp_server_ids.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn assert_frozen_preset_extra(extra: &serde_json::Value) {
    assert_eq!(extra["assistant_id"], serde_json::json!("word-creator"));
    assert_eq!(extra["preset_context"], serde_json::json!("assistant rule body"));
    assert_eq!(extra["preset_rules"], serde_json::json!("assistant rule body"));
    assert_eq!(extra["skills"], serde_json::json!(["pdf", "cron"]));
    assert_eq!(extra["mcp_server_ids"], serde_json::json!(["mcp-docs"]));
}

#[tokio::test]
async fn team_preset_assistant_snapshot_is_frozen() {
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: word_creator_definition(),
    });
    let (svc, _team_repo, conversation_ports, conv_repo) = setup_with_ports_metadata_assistants_and_conversation_repo(
        success_factory(),
        seeded_agent_metadata_repo(),
        definition_repo,
        Arc::new(EmptyAssistantOverlayRepo),
    );
    conversation_ports.upsert_preset_snapshot(
        "word-creator",
        fake_preset_snapshot("assistant rule body", &["pdf", "cron"], &["mcp-docs"]),
    );

    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Preset Team".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("claude".into()),
                    model: "claude-sonnet-4".into(),
                    assistant_id: Some("word-creator".into()),
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .expect("create team");

    let extra = conv_repo.get_extra(&resp.assistants[0].conversation_id).unwrap();
    assert_frozen_preset_extra(&extra);

    conversation_ports.upsert_preset_snapshot(
        "word-creator",
        fake_preset_snapshot("changed rule body", &["changed"], &["changed-mcp"]),
    );

    let after_live_change = conv_repo.get_extra(&resp.assistants[0].conversation_id).unwrap();
    assert_frozen_preset_extra(&after_live_change);
}

#[tokio::test]
async fn spawned_preset_assistant_snapshot_is_frozen() {
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: word_creator_definition(),
    });
    let (svc, _team_repo, conversation_ports, conv_repo) = setup_with_ports_metadata_assistants_and_conversation_repo(
        success_factory(),
        seeded_agent_metadata_repo(),
        definition_repo,
        Arc::new(EmptyAssistantOverlayRepo),
    );
    conversation_ports.upsert_preset_snapshot(
        "word-creator",
        fake_preset_snapshot("assistant rule body", &["pdf", "cron"], &["mcp-docs"]),
    );

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Spawn Preset".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let lead_slot_id = created.leader_assistant_id.clone().expect("lead slot");
    svc.ensure_session("user1", &created.id).await.expect("ensure session");
    svc.send_message("user1", &created.id, "start active run", None)
        .await
        .expect("active run");

    let spawned = svc
        .spawn_agent_in_session(
            &created.id,
            &lead_slot_id,
            SpawnAgentRequest {
                name: "Writer".into(),
                assistant_id: Some("word-creator".into()),
            },
        )
        .await
        .expect("spawn preset teammate");

    let extra = conv_repo.get_extra(&spawned.conversation_id).unwrap();
    assert_frozen_preset_extra(&extra);

    conversation_ports.upsert_preset_snapshot(
        "word-creator",
        fake_preset_snapshot("changed rule body", &["changed"], &["changed-mcp"]),
    );

    let after_live_change = conv_repo.get_extra(&spawned.conversation_id).unwrap();
    assert_frozen_preset_extra(&after_live_change);
}

#[tokio::test]
async fn ta_add_agent_uses_model_fallback_for_acp_backend() {
    let svc = setup_with_metadata_rows(vec![make_agent_metadata_row(
        "8e1acf31",
        "codex",
        "/api/assets/logos/tools/coding/codex.svg",
    )]);

    let team = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let added = svc
        .add_agent(
            "user1",
            &team.id,
            AddAgentRequest {
                name: "Coder".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "codex".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(added.icon.as_deref(), Some("/api/assets/logos/tools/coding/codex.svg"));
}

#[tokio::test]
async fn ta_add_agent_derives_backend_from_assistant_when_backend_missing() {
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-team-worker".into(),
            assistant_id: "assistant-worker".into(),
            source: "user".into(),
            owner_type: "user".into(),
            source_ref: None,
            name: "Worker Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "user_file".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
        row: AssistantOverlayRow {
            assistant_definition_id: "def-team-worker".into(),
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex".into()),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        },
    });
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = seeded_agent_metadata_repo();
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        overlay_repo,
    );

    let team = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("claude".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let added = svc
        .add_agent(
            "user1",
            &team.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some(String::new()),
                model: "gpt-5".into(),
                assistant_id: Some("assistant-worker".into()),
            },
        )
        .await
        .unwrap();

    assert_eq!(added.backend, "codex");
    assert_eq!(added.assistant_id.as_deref(), Some("assistant-worker"));
}

#[tokio::test]
async fn ta_add_agent_ignores_requested_backend_when_assistant_id_present() {
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-team-worker".into(),
            assistant_id: "assistant-worker".into(),
            source: "user".into(),
            owner_type: "user".into(),
            source_ref: None,
            name: "Worker Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "user_file".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
        row: AssistantOverlayRow {
            assistant_definition_id: "def-team-worker".into(),
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex".into()),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        },
    });
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = seeded_agent_metadata_repo();
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        overlay_repo,
    );

    let team = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("claude".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let added = svc
        .add_agent(
            "user1",
            &team.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("gemini".into()),
                model: "gpt-5".into(),
                assistant_id: Some("assistant-worker".into()),
            },
        )
        .await
        .unwrap();

    assert_eq!(added.backend, "codex");
}

#[tokio::test]
async fn tc2_create_single_agent_team() {
    let svc = setup();
    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Solo".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(resp.assistants.len(), 1);
    assert_eq!(resp.assistants[0].role, "lead");
}

#[tokio::test]
async fn create_team_uses_explicit_leader_role_when_leader_is_not_first() {
    let svc = setup();
    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![
                    TeamAgentInput {
                        name: "Worker".into(),
                        role: "teammate".into(),
                        backend: Some("acp".into()),
                        model: "claude".into(),
                        assistant_id: None,
                        conversation_id: None,
                    },
                    TeamAgentInput {
                        name: "Lead".into(),
                        role: "lead".into(),
                        backend: Some("acp".into()),
                        model: "claude".into(),
                        assistant_id: None,
                        conversation_id: None,
                    },
                ],
                workspace: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(resp.assistants[0].name, "Lead");
    assert_eq!(resp.assistants[0].role, "lead");
    assert_eq!(resp.assistants[1].name, "Worker");
    assert_eq!(resp.assistants[1].role, "teammate");
    assert_eq!(resp.leader_assistant_id, Some(resp.assistants[0].slot_id.clone()));
}

#[tokio::test]
async fn create_team_rejects_zero_leaders() {
    let svc = setup();
    let result = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Worker".into(),
                    role: "teammate".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await;

    assert!(matches!(result, Err(TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn create_team_rejects_multiple_leaders() {
    let svc = setup();
    let result = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![
                    TeamAgentInput {
                        name: "Lead A".into(),
                        role: "lead".into(),
                        backend: Some("acp".into()),
                        model: "claude".into(),
                        assistant_id: None,
                        conversation_id: None,
                    },
                    TeamAgentInput {
                        name: "Lead B".into(),
                        role: "leader".into(),
                        backend: Some("acp".into()),
                        model: "claude".into(),
                        assistant_id: None,
                        conversation_id: None,
                    },
                ],
                workspace: None,
            },
        )
        .await;

    assert!(matches!(result, Err(TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn create_team_rejects_unknown_role() {
    let svc = setup();
    let result = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "captain".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await;

    assert!(matches!(result, Err(TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn tc5_empty_agents_returns_error() {
    let svc = setup();
    let result = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Empty".into(),
                agents: vec![],
                workspace: None,
            },
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn tc3_each_agent_has_conversation_id() {
    let svc = setup();
    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    for agent in &resp.assistants {
        assert!(!agent.conversation_id.is_empty());
    }
    assert_ne!(resp.assistants[0].conversation_id, resp.assistants[1].conversation_id);
}

// -- List teams ---------------------------------------------------------------

#[tokio::test]
async fn tl1_empty_list() {
    let svc = setup();
    let list = svc.list_teams("user1").await.unwrap();
    assert!(list.is_empty());
}

#[tokio::test]
async fn tl2_list_multiple_teams() {
    let svc = setup();
    svc.create_team(
        "user1",
        CreateTeamRequest {
            name: "A".into(),
            agents: two_agent_input(),
            workspace: None,
        },
    )
    .await
    .unwrap();
    svc.create_team(
        "user1",
        CreateTeamRequest {
            name: "B".into(),
            agents: two_agent_input(),
            workspace: None,
        },
    )
    .await
    .unwrap();

    let list = svc.list_teams("user1").await.unwrap();
    assert_eq!(list.len(), 2);
}

#[tokio::test]
async fn tl3_list_teams_filters_by_owner() {
    let svc = setup();
    svc.create_team(
        "user1",
        CreateTeamRequest {
            name: "Owned".into(),
            agents: two_agent_input(),
            workspace: None,
        },
    )
    .await
    .unwrap();
    svc.create_team(
        "user2",
        CreateTeamRequest {
            name: "Other".into(),
            agents: two_agent_input(),
            workspace: None,
        },
    )
    .await
    .unwrap();

    let list = svc.list_teams("user1").await.unwrap();

    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "Owned");
}

#[tokio::test]
async fn tl_list_teams_includes_pending_confirmation_counts_without_rebuilding_tasks() {
    let (svc, task_manager) = setup_with_factory(confirmations_factory(2));
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "With Confirmations".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    let conversation_id = created.assistants[0].conversation_id.clone();
    task_manager
        .get_or_build_task(
            &conversation_id,
            test_acp_build_options(conversation_id.clone(), "/tmp/ws".into()),
        )
        .await
        .unwrap();
    let before = task_manager.snapshot();

    let list = svc.list_teams("user1").await.unwrap();
    let after = task_manager.snapshot();

    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, created.id);
    assert_eq!(list[0].assistants[0].pending_confirmations, 2);
    assert_eq!(after.build, before.build);
}

// -- Get team -----------------------------------------------------------------

#[tokio::test]
async fn tg1_get_existing_team() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(got.id, created.id);
    assert_eq!(got.name, "Alpha");
    assert_eq!(got.assistants.len(), 2);
}

#[tokio::test]
async fn tg2_get_nonexistent_returns_error() {
    let svc = setup();
    let result = svc.get_team("user1", "nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn tg3_get_team_rejects_cross_user_access() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Private".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.get_team("user2", &created.id).await;

    assert!(matches!(result, Err(aionui_team::TeamError::TeamNotFound(_))));
}

// -- Delete team --------------------------------------------------------------

#[tokio::test]
async fn td1_delete_existing_team() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.remove_team("user1", &created.id).await.unwrap();
    let list = svc.list_teams("user1").await.unwrap();
    assert!(list.is_empty());
}

#[tokio::test]
async fn td6_delete_nonexistent_returns_error() {
    let svc = setup();
    let result = svc.remove_team("user1", "nonexistent").await;
    assert!(result.is_err());
}

// -- Rename team --------------------------------------------------------------

#[tokio::test]
async fn tr1_rename_existing_team() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Old".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.rename_team("user1", &created.id, "New Name").await.unwrap();
    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(got.name, "New Name");
}

#[tokio::test]
async fn tr4_rename_nonexistent_returns_error() {
    let svc = setup();
    let result = svc.rename_team("user1", "nonexistent", "X").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn tr5_rename_team_rejects_cross_user_access() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Private".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.rename_team("user2", &created.id, "Nope").await;

    assert!(matches!(result, Err(aionui_team::TeamError::TeamNotFound(_))));
}

// ===========================================================================
// Test: Agent Management (AA-*, AR-*, AN-*)
// ===========================================================================

#[tokio::test]
async fn aa1_add_agent_to_team() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let agent = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(agent.name, "Worker");
    assert_eq!(agent.role, "teammate");
    assert!(!agent.conversation_id.is_empty());

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(got.assistants.len(), 2);
}

#[tokio::test]
async fn manual_add_without_active_run_opens_system_lifecycle_run() {
    let (svc, _, conv_repo) = setup_with_factory_and_metadata_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    let leader_conversation_id = &created.assistants[0].conversation_id;
    let leader_messages = conv_repo.messages_for(leader_conversation_id);
    assert_eq!(leader_messages.len(), 1);
    assert_eq!(leader_messages[0].position.as_deref(), Some("left"));
    let leader_content: serde_json::Value = serde_json::from_str(&leader_messages[0].content).unwrap();
    assert_eq!(leader_content["sender_name"], "team_system");
    assert_eq!(leader_content["teammate_message"], true);
    assert!(
        leader_content["content"]
            .as_str()
            .unwrap()
            .contains("manually added teammate")
    );
    assert!(leader_content["content"].as_str().unwrap().contains(&added.slot_id));

    let added_messages = conv_repo.messages_for(&added.conversation_id);
    assert_eq!(added_messages.len(), 1);
    assert_eq!(added_messages[0].position.as_deref(), Some("left"));
    let added_content: serde_json::Value = serde_json::from_str(&added_messages[0].content).unwrap();
    assert_eq!(added_content["sender_name"], "team_system");
    assert_eq!(added_content["teammate_message"], true);
    assert!(added_content["content"].as_str().unwrap().contains("manually added"));

    let run_state = svc.get_run_state("user1", &created.id).await.unwrap();
    // Manual add during an idle team now opens a SystemLifecycle run so the
    // welcome and membership-change turns run inside a run (invariant: no
    // run-less turn), rather than falling to run-less background as before.
    let active_run = run_state.active_run.expect("manual add must open a system run");
    assert_eq!(active_run.source, TeamRunSource::SystemLifecycle);
    assert!(!active_run.has_user_intervention);
    assert!(run_state.slot_work.iter().any(|slot| slot.slot_id == added.slot_id));
}

#[tokio::test]
async fn add_agent_rejects_leader_role() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Second Leader".into(),
                role: "lead".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await;

    assert!(matches!(result, Err(TeamError::InvalidRequest(_))));
}

#[tokio::test]
async fn add_agent_allows_same_assistant_id_multiple_times() {
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: word_creator_definition(),
    });
    let (svc, _team_repo, _conversation_ports, _conv_repo) = setup_with_ports_metadata_assistants_and_conversation_repo(
        success_factory(),
        seeded_agent_metadata_repo(),
        definition_repo,
        Arc::new(EmptyAssistantOverlayRepo),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    for name in ["Worker Copy A", "Worker Copy B"] {
        svc.add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: name.into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: Some("word-creator".into()),
            },
        )
        .await
        .unwrap();
    }

    let got = svc.get_team("user1", &created.id).await.unwrap();
    let matching = got
        .assistants
        .iter()
        .filter(|agent| agent.assistant_id.as_deref() == Some("word-creator"))
        .count();
    assert_eq!(matching, 2);
}

#[tokio::test]
async fn manual_add_agent_active_session_attaches_runtime_in_background_without_blocking_http() {
    use futures_util::FutureExt;

    let build_count = Arc::new(AtomicUsize::new(0));
    let factory_count = build_count.clone();
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let build_index = factory_count.fetch_add(1, Ordering::SeqCst);
        async move {
            if build_index >= 1 {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, _team_repo, task_manager, _recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task_manager.snapshot().build.len(), 0);
    svc.ensure_session("user1", &created.id).await.unwrap();
    assert_eq!(task_manager.snapshot().build.len(), 1);

    let started_at = std::time::Instant::now();
    let agent = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    assert!(
        started_at.elapsed() < std::time::Duration::from_millis(150),
        "manual add HTTP path must not wait for delayed runtime attach"
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if task_manager
                .snapshot()
                .build
                .iter()
                .any(|conversation_id| conversation_id == &agent.conversation_id)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("manual add background attach should build the new runtime");
}

#[tokio::test]
async fn manual_add_agent_attach_failure_marks_slot_error_without_leader_notice() {
    use futures_util::FutureExt;

    let fail_next = Arc::new(AtomicBool::new(false));
    let factory_fail_next = Arc::clone(&fail_next);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let should_fail = factory_fail_next.swap(false, Ordering::SeqCst);
        async move {
            if should_fail {
                return Err(AgentError::internal("simulated manual add attach failure"));
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, team_repo, task_manager, recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let original_scheduler = svc.get_session_scheduler(&created.id).unwrap();
    let lead_conversation_id = created.assistants[0].conversation_id.clone();
    task_manager.reset_calls();
    fail_next.store(true, Ordering::SeqCst);

    let agent = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let has_error_status = recorder.events_by_name("team.agentStatusChanged").iter().any(|event| {
                event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(agent.slot_id.as_str())
                    && event.data.get("status").and_then(serde_json::Value::as_str) == Some("error")
            });
            if has_error_status {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("manual add attach failure should mark the slot error");

    // Teammate attach failure is inline (spec 5.4②): the per-member runtime
    // status goes `failed` (drives the column's failure UI), but the session
    // lifecycle must NOT fail — the leader is still ready, so the team stays
    // usable and the full-screen warmup overlay never appears.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let runtime_failed = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(agent.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
                });
            if runtime_failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("teammate attach failure must surface inline as a failed runtime status");

    assert!(Arc::ptr_eq(
        &original_scheduler,
        &svc.get_session_scheduler(&created.id)
            .expect("published session remains")
    ));
    assert!(
        task_manager.get_task(&lead_conversation_id).is_some(),
        "healthy leader runtime must remain alive"
    );
    assert!(
        task_manager
            .snapshot()
            .build
            .iter()
            .all(|conversation_id| conversation_id == &agent.conversation_id),
        "dynamic failure must not rebuild healthy members"
    );

    let lead_slot_id = created.leader_assistant_id.as_deref().expect("leader slot");
    let leader_messages = team_repo
        .get_history("user1", &created.id, lead_slot_id, None)
        .await
        .unwrap();
    assert!(
        !leader_messages
            .iter()
            .any(|message| message.content.contains("failed to start its runtime")),
        "user-initiated add failure must NOT wake the leader; it surfaces inline to the user (spec 5.4)"
    );

    svc.ensure_session("user1", &created.id)
        .await
        .expect("later ensure should retry only the failed member");
    assert!(Arc::ptr_eq(
        &original_scheduler,
        &svc.get_session_scheduler(&created.id)
            .expect("same session after retry")
    ));
    assert_eq!(
        task_manager
            .snapshot()
            .build
            .iter()
            .filter(|conversation_id| *conversation_id == &agent.conversation_id)
            .count(),
        2,
        "the failed member should have one failed attach and one later retry"
    );
    assert!(
        recorder
            .events_by_name("team.sessionStatusChanged")
            .iter()
            .any(|event| {
                event.data.get("team_id").and_then(serde_json::Value::as_str) == Some(created.id.as_str())
                    && event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready")
                    && event.data.get("server_count").and_then(serde_json::Value::as_u64) == Some(2)
            }),
        "single-member retry must restore team Ready"
    );
    assert!(
        !recorder
            .events_by_name("team.sessionStatusChanged")
            .iter()
            .any(|event| {
                event.data.get("team_id").and_then(serde_json::Value::as_str) == Some(created.id.as_str())
                    && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
            }),
        "a teammate add-then-retry flow must never fail the session lifecycle; the failure was inline (spec 5.4/5.5)"
    );
}

// The full-screen overlay (warming + failure card) is leader-scoped (spec
// 5.4/5.5). A whole-team `ensure_session` — invoked on page mount, model
// switches, and before sends via `warmupSession` — reconciles non-dormant
// members. If it retries an already-failed TEAMMATE and the retry fails again,
// the team must stay usable (leader ready): `ensure_session` returns Ok, no
// session `failed` is broadcast, and the teammate failure stays inline. Only a
// LEADER reconciliation failure may fail the whole team.
#[tokio::test]
async fn reensure_with_failed_teammate_keeps_team_usable_and_inline() {
    use futures_util::FutureExt;

    // Lead builds once (cold start); every later build fails, so the teammate's
    // attach fails on add AND on the explicit re-ensure retry.
    let build_count = Arc::new(AtomicUsize::new(0));
    let factory_count = Arc::clone(&build_count);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let build_index = factory_count.fetch_add(1, Ordering::SeqCst);
        async move {
            if build_index >= 1 {
                return Err(AgentError::internal("teammate build keeps failing"));
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, _team_repo, _task_manager, recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Re-ensure with broken teammate".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();

    let failed = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Broken".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let runtime_failed = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(failed.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
                });
            if runtime_failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the added teammate must fail inline first");

    recorder.clear();

    // Explicit full re-ensure (e.g. switching the leader's model, page remount)
    // retries the still-broken teammate. `join_all` inside reconciliation means
    // the teammate's failed attach has completed by the time this returns.
    svc.ensure_session("user1", &created.id)
        .await
        .expect("a teammate reconciliation failure must not fail the whole team");

    let session_failed = recorder
        .events_by_name("team.sessionStatusChanged")
        .into_iter()
        .find(|event| {
            event.data.get("team_id").and_then(serde_json::Value::as_str) == Some(created.id.as_str())
                && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
        });
    assert!(
        session_failed.is_none(),
        "a teammate reconciliation failure must not raise the full-screen failure card (session `failed`), got {session_failed:?}"
    );
    assert!(
        recorder
            .events_by_name("team.sessionStatusChanged")
            .iter()
            .any(|event| {
                event.data.get("team_id").and_then(serde_json::Value::as_str) == Some(created.id.as_str())
                    && event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready")
            }),
        "the team stays ready after a teammate reconciliation failure (leader ready = usable)"
    );
    assert!(
        recorder
            .events_by_name("team.agentRuntimeStatusChanged")
            .iter()
            .any(|event| {
                event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(failed.slot_id.as_str())
                    && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
            }),
        "the teammate failure stays inline via agentRuntimeStatusChanged"
    );
}

#[tokio::test]
async fn failed_member_stays_inline_and_removal_restores_ready() {
    use futures_util::FutureExt;

    let build_count = Arc::new(AtomicUsize::new(0));
    let factory_count = Arc::clone(&build_count);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let build_index = factory_count.fetch_add(1, Ordering::SeqCst);
        async move {
            if build_index >= 1 {
                return Err(AgentError::internal("provider-secret: dynamic attach failed"));
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, _team_repo, task_manager, recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Failed member removal".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let original_scheduler = svc.get_session_scheduler(&created.id).unwrap();
    let lead_conversation_id = created.assistants[0].conversation_id.clone();
    task_manager.reset_calls();

    let failed = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Broken".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();
    // The dynamic teammate's attach failure surfaces inline (spec 5.4②): its
    // per-member runtime status goes `failed`. The session lifecycle stays Ready
    // (leader still ready) — a teammate failure is never a whole-team failure.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let runtime_failed = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(failed.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
                });
            if runtime_failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("dynamic teammate failure must surface inline as a failed runtime status");

    // A whole-team re-ensure retries the still-broken teammate but must keep the
    // team usable (leader ready): it returns Ok and the failure stays inline,
    // rather than reporting a session-level conflict (spec 5.4/5.5).
    svc.ensure_session("user1", &created.id)
        .await
        .expect("a teammate reconciliation failure must not fail the whole team");

    task_manager.reset_calls();
    recorder.clear();
    svc.remove_agent("user1", &created.id, &failed.slot_id).await.unwrap();

    assert!(Arc::ptr_eq(
        &original_scheduler,
        &svc.get_session_scheduler(&created.id).expect("healthy session remains")
    ));
    assert!(task_manager.get_task(&lead_conversation_id).is_some());
    assert!(
        task_manager
            .snapshot()
            .kill
            .iter()
            .all(|(conversation_id, _)| conversation_id != &lead_conversation_id)
    );
    assert!(
        recorder
            .events_by_name("team.sessionStatusChanged")
            .iter()
            .any(|event| {
                event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready")
                    && event.data.get("server_count").and_then(serde_json::Value::as_u64) == Some(1)
            })
    );
}

#[tokio::test]
async fn remove_during_attach_cancels_work_and_rejects_late_ready() {
    let gate = Arc::new(GatedProvisioningFactory::default());
    let (svc, _team_repo, task_manager, recorder, conv_repo) =
        setup_with_factory_recording_broadcaster_and_conversation_repo(gate.factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Remove attaching member".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    gate.enable();
    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Attaching".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();
    gate.wait_for_starts(1).await;

    let remove_service = Arc::clone(&svc);
    let team_id = created.id.clone();
    let slot_id = added.slot_id.clone();
    let removal = tokio::spawn(async move { remove_service.remove_agent("user1", &team_id, &slot_id).await });
    tokio::task::yield_now().await;
    assert!(!removal.is_finished(), "removal must join in-flight runtime cleanup");
    gate.release(1);
    removal.await.unwrap().unwrap();

    assert!(task_manager.get_task(&added.conversation_id).is_none());
    assert!(
        conv_repo.get("user1", &added.conversation_id).await.unwrap().is_none(),
        "removed attaching member conversation must be deleted"
    );
    assert!(
        svc.get_team("user1", &created.id)
            .await
            .unwrap()
            .assistants
            .iter()
            .all(|agent| agent.slot_id != added.slot_id)
    );
    assert_eq!(
        recorder
            .events_by_name("team.agentRuntimeStatusChanged")
            .into_iter()
            .filter(|event| {
                event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(added.slot_id.as_str())
                    && event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready")
            })
            .count(),
        0,
        "cancelled attach must not publish late Ready"
    );
}

#[tokio::test]
async fn aa_add_agent_inherits_team_workspace() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, _, conv_repo) =
        setup_with_factory_and_metadata_and_conversation_repo(success_factory(), agent_metadata_repo);
    let workspace = std::env::temp_dir().join(format!("aionui-team-workspace-{}", aionui_common::generate_id()));
    std::fs::create_dir_all(&workspace).unwrap();
    let workspace = workspace.to_string_lossy().into_owned();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: Some(workspace.clone()),
            },
        )
        .await
        .unwrap();

    let agent = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    let extra = conv_repo.get_extra(&agent.conversation_id).unwrap();
    assert_eq!(
        extra.get("workspace").and_then(|v| v.as_str()),
        Some(workspace.as_str())
    );
}

#[tokio::test]
async fn add_agent_backfills_empty_team_workspace_from_leader_workspace() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, team_repo, _, conv_repo) =
        setup_with_factory_metadata_team_repo_and_conversation_repo(success_factory(), agent_metadata_repo);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Legacy".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();
    let leader_workspace = conv_repo.get_extra(&created.assistants[0].conversation_id).unwrap()["workspace"]
        .as_str()
        .unwrap()
        .to_owned();

    force_team_workspace(&team_repo, &created.id, "").await;

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(got.workspace, leader_workspace);
    let added_extra = conv_repo.get_extra(&added.conversation_id).unwrap();
    assert_eq!(
        added_extra.get("workspace").and_then(serde_json::Value::as_str),
        Some(leader_workspace.as_str())
    );
}

#[tokio::test]
async fn add_agent_uses_team_temp_workspace_when_team_and_leader_workspaces_are_unusable() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, team_repo, _, conv_repo) =
        setup_with_factory_metadata_team_repo_and_conversation_repo(success_factory(), agent_metadata_repo);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Legacy Empty".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    force_team_workspace(&team_repo, &created.id, "").await;
    conv_repo
        .patch_extra(
            &created.assistants[0].conversation_id,
            serde_json::json!({ "workspace": "/tmp/aionui-team-missing-leader-workspace" }),
        )
        .unwrap();

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert!(
        got.workspace
            .contains(&format!("/conversations/team-temp-{}", created.id)),
        "unexpected team temp workspace: {}",
        got.workspace
    );
    let added_extra = conv_repo.get_extra(&added.conversation_id).unwrap();
    assert_eq!(
        added_extra.get("workspace").and_then(serde_json::Value::as_str),
        Some(got.workspace.as_str())
    );
}

#[tokio::test]
async fn add_agent_does_not_create_teammate_when_workspace_writeback_fails() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, team_repo, _, conv_repo) =
        setup_with_factory_metadata_team_repo_and_conversation_repo(success_factory(), agent_metadata_repo);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Writeback Failure".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    force_team_workspace(&team_repo, &created.id, "").await;
    team_repo.fail_workspace_update();
    let before_count = conv_repo.conversation_count();

    let err = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .expect_err("workspace writeback failure must block teammate creation");

    assert!(
        err.to_string().contains("forced workspace writeback failure"),
        "unexpected error: {err}"
    );
    assert_eq!(conv_repo.conversation_count(), before_count);
}

#[tokio::test]
async fn add_agent_continues_when_team_temp_leader_patch_fails() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, team_repo, conversation_ports, conv_repo) =
        setup_with_ports_team_repo_and_conversation_repo(success_factory(), agent_metadata_repo);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Patch Failure".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    force_team_workspace(&team_repo, &created.id, "").await;
    conv_repo
        .patch_extra(
            &created.assistants[0].conversation_id,
            serde_json::json!({ "workspace": "/tmp/aionui-team-missing-leader-workspace" }),
        )
        .unwrap();
    conversation_ports
        .fail_leader_workspace_patch
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert!(
        got.workspace
            .contains(&format!("/conversations/team-temp-{}", created.id))
    );
    let added_extra = conv_repo.get_extra(&added.conversation_id).unwrap();
    assert_eq!(
        added_extra.get("workspace").and_then(serde_json::Value::as_str),
        Some(got.workspace.as_str())
    );
}

#[tokio::test]
async fn provisioning_writes_typed_team_binding_for_create_and_add_agent() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = Arc::new(StubAgentMetadataRepo::empty());
    let (svc, _, conv_repo) =
        setup_with_factory_and_metadata_and_conversation_repo(success_factory(), agent_metadata_repo);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Typed".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    for agent in &created.assistants {
        let extra = conv_repo.get_extra(&agent.conversation_id).unwrap();
        assert_eq!(extra.get("teamId").and_then(|v| v.as_str()), Some(created.id.as_str()));
        assert_eq!(
            extra.get("slot_id").and_then(|v| v.as_str()),
            Some(agent.slot_id.as_str())
        );
        assert_eq!(extra.get("role").and_then(|v| v.as_str()), Some(agent.role.as_str()));
        assert_eq!(
            extra.get("backend").and_then(|v| v.as_str()),
            Some(agent.backend.as_str())
        );
        assert_eq!(
            extra.get("session_mode").and_then(|v| v.as_str()),
            Some("yolo"),
            "Team provisioning should write the runtime seed for initial agents"
        );
    }

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Extra".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();
    let extra = conv_repo.get_extra(&added.conversation_id).unwrap();
    assert_eq!(extra.get("teamId").and_then(|v| v.as_str()), Some(created.id.as_str()));
    assert_eq!(
        extra.get("slot_id").and_then(|v| v.as_str()),
        Some(added.slot_id.as_str())
    );
    assert_eq!(extra.get("role").and_then(|v| v.as_str()), Some(added.role.as_str()));
    assert_eq!(
        extra.get("backend").and_then(|v| v.as_str()),
        Some(added.backend.as_str())
    );
    assert_eq!(
        extra.get("session_mode").and_then(|v| v.as_str()),
        Some("yolo"),
        "Team provisioning should write the runtime seed for added agents"
    );
}

#[tokio::test]
async fn provisioning_resolves_acp_backend_from_agent_metadata() {
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> =
        Arc::new(StubAgentMetadataRepo::with_rows(vec![acp_agent_metadata_row(
            "future-acp-id",
            "future-acp",
            Some("turbo"),
        )]));
    let (svc, _, conv_repo) =
        setup_with_factory_and_metadata_and_conversation_repo(success_factory(), agent_metadata_repo);

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Metadata ACP".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("future-acp".into()),
                    model: "model-x".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let row = conv_repo
        .get("user1", &created.assistants[0].conversation_id)
        .await
        .unwrap()
        .expect("conversation row");
    assert_eq!(row.r#type, "acp");
    let extra = conv_repo
        .get_extra(&created.assistants[0].conversation_id)
        .expect("conversation extra");
    assert_eq!(
        extra.get("backend").and_then(serde_json::Value::as_str),
        Some("future-acp")
    );
    assert_eq!(
        extra.get("session_mode").and_then(serde_json::Value::as_str),
        Some("turbo")
    );
    assert_eq!(
        extra.get("provider_id").and_then(serde_json::Value::as_str),
        Some("future-acp")
    );
}

#[tokio::test]
async fn aa4_add_agent_to_nonexistent_team() {
    let svc = setup();
    let result = svc
        .add_agent(
            "user1",
            "nonexistent",
            AddAgentRequest {
                name: "X".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn ar1_remove_agent_from_team() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let worker_slot = created.assistants[1].slot_id.clone();
    svc.remove_agent("user1", &created.id, &worker_slot).await.unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(got.assistants.len(), 1);
    assert!(got.assistants.iter().all(|a| a.slot_id != worker_slot));
}

#[tokio::test]
async fn membership_persist_failure_does_not_delete_the_conversation() {
    let (svc, team_repo, task_manager, conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Removal persistence failure".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let worker = created.assistants[1].clone();
    team_repo.fail_agent_updates();

    let error = svc
        .remove_agent("user1", &created.id, &worker.slot_id)
        .await
        .expect_err("forced membership persistence failure must be returned");

    assert!(error.to_string().contains("forced agent update failure"));
    assert!(
        conv_repo.get("user1", &worker.conversation_id).await.unwrap().is_some(),
        "conversation deletion must happen only after membership persistence"
    );
    assert!(
        svc.get_team("user1", &created.id)
            .await
            .unwrap()
            .assistants
            .iter()
            .any(|agent| agent.slot_id == worker.slot_id),
        "failed persistence must leave declared membership intact"
    );
    assert!(
        task_manager.get_task(&worker.conversation_id).is_none(),
        "cancelled runtime must be cleaned before reporting persistence failure"
    );
}

#[tokio::test]
async fn remove_tolerates_current_session_already_missing_the_slot() {
    let (svc, _team_repo, _task_manager, conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Already absent runtime slot".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let worker = created.assistants[1].clone();
    svc.get_session_scheduler(&created.id)
        .unwrap()
        .remove_agent(&worker.slot_id)
        .await
        .unwrap();

    svc.remove_agent("user1", &created.id, &worker.slot_id)
        .await
        .expect("post-persistence runtime cleanup must be idempotent");

    assert!(conv_repo.get("user1", &worker.conversation_id).await.unwrap().is_none());
    assert!(
        svc.get_team("user1", &created.id)
            .await
            .unwrap()
            .assistants
            .iter()
            .all(|agent| agent.slot_id != worker.slot_id)
    );
}

#[tokio::test]
async fn manual_remove_agent_projects_team_system_message_without_active_team_run() {
    let (svc, _, conv_repo) = setup_with_factory_and_metadata_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let leader_conversation_id = created.assistants[0].conversation_id.clone();
    let worker_slot = created.assistants[1].slot_id.clone();

    svc.remove_agent("user1", &created.id, &worker_slot).await.unwrap();

    let leader_messages = conv_repo.messages_for(&leader_conversation_id);
    assert_eq!(leader_messages.len(), 1);
    assert_eq!(leader_messages[0].position.as_deref(), Some("left"));
    let content: serde_json::Value = serde_json::from_str(&leader_messages[0].content).unwrap();
    assert_eq!(content["sender_name"], "team_system");
    assert_eq!(content["teammate_message"], true);
    assert!(
        content["content"]
            .as_str()
            .unwrap()
            .contains("was removed from the team")
    );
    assert!(content["content"].as_str().unwrap().contains(&worker_slot));
}

#[tokio::test]
async fn remove_agent_rejects_leader() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let lead_slot = created.assistants[0].slot_id.clone();
    let result = svc.remove_agent("user1", &created.id, &lead_slot).await;

    assert!(matches!(result, Err(TeamError::InvalidRequest(_))));
    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert!(got.assistants.iter().any(|agent| agent.slot_id == lead_slot));
}

#[tokio::test]
async fn ar4_remove_nonexistent_agent() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.remove_agent("user1", &created.id, "nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn an1_rename_agent() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let slot_id = created.assistants[1].slot_id.clone();
    svc.rename_agent("user1", &created.id, &slot_id, "Senior Worker")
        .await
        .unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    let agent = got.assistants.iter().find(|a| a.slot_id == slot_id).unwrap();
    assert_eq!(agent.name, "Senior Worker");
}

#[tokio::test]
async fn an3_rename_nonexistent_agent() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.rename_agent("user1", &created.id, "nonexistent", "X").await;
    assert!(result.is_err());
}

// ===========================================================================
// Test: Session Management (ES-*, SS-*)
// ===========================================================================

#[tokio::test]
async fn es1_ensure_session_creates_session() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
}

#[tokio::test]
async fn spawn_agent_in_session_succeeds_without_active_team_run() {
    let definition_repo: Arc<dyn IAssistantDefinitionRepository> = Arc::new(SingleAssistantDefinitionRepo {
        row: AssistantDefinitionRow {
            id: "def-spawn-worker".into(),
            assistant_id: "assistant-worker".into(),
            source: "user".into(),
            owner_type: "user".into(),
            source_ref: None,
            name: "Worker Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "user_file".into(),
            rule_resource_ref: None,
            recommended_prompts: "[]".into(),
            recommended_prompts_i18n: "{}".into(),
            default_model_mode: "auto".into(),
            default_model_value: None,
            default_permission_mode: "auto".into(),
            default_permission_value: None,
            default_thought_level_mode: "auto".into(),
            default_thought_level_value: None,
            default_skills_mode: "auto".into(),
            default_skill_ids: "[]".into(),
            custom_skill_names: "[]".into(),
            default_disabled_builtin_skill_ids: "[]".into(),
            default_mcps_mode: "auto".into(),
            default_mcp_ids: "[]".into(),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
        },
    });
    let overlay_repo: Arc<dyn IAssistantOverlayRepository> = Arc::new(SingleAssistantOverlayRepo {
        row: AssistantOverlayRow {
            assistant_definition_id: "def-spawn-worker".into(),
            enabled: true,
            sort_order: 0,
            agent_id_override: Some("codex".into()),
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        },
    });
    let agent_metadata_repo: Arc<dyn IAgentMetadataRepository> = seeded_agent_metadata_repo();
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        agent_metadata_repo,
        definition_repo,
        overlay_repo,
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");

    svc.ensure_session("user1", &created.id)
        .await
        .expect("session should be loaded without active Team Run");
    let lead_slot_id = created
        .leader_assistant_id
        .clone()
        .expect("created team should have a lead slot");

    let req = SpawnAgentRequest {
        name: "Helper".into(),
        assistant_id: Some("assistant-worker".into()),
    };

    let spawned = svc
        .spawn_agent_in_session(&created.id, &lead_slot_id, req)
        .await
        .expect("spawn without active Team Run should still succeed");
    assert_eq!(spawned.name, "Helper");
    assert_eq!(spawned.assistant_id.as_deref(), Some("assistant-worker"));

    let after = svc
        .get_team("user1", &created.id)
        .await
        .expect("team should still be readable");
    assert_eq!(
        after.assistants.len(),
        created.assistants.len() + 1,
        "successful spawn should persist the teammate"
    );
    assert!(
        after.assistants.iter().any(|agent| agent.slot_id == spawned.slot_id),
        "spawned teammate must be visible in persisted team state"
    );
}

#[tokio::test]
async fn leader_spawn_then_immediate_ensure_joins_the_same_attach_operation() {
    let gate = Arc::new(GatedProvisioningFactory::default());
    let (svc, _team_repo, task_manager, _conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        gate.factory(),
        seeded_agent_metadata_repo(),
        Arc::new(SingleAssistantDefinitionRepo {
            row: word_creator_definition(),
        }),
        Arc::new(EmptyAssistantOverlayRepo),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Leader spawn reconciliation".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let original_scheduler = svc.get_session_scheduler(&created.id).unwrap();
    task_manager.reset_calls();
    gate.enable();

    let spawned = svc
        .spawn_agent_in_session(
            &created.id,
            created.leader_assistant_id.as_deref().unwrap(),
            SpawnAgentRequest {
                name: "Writer".into(),
                assistant_id: Some("word-creator".into()),
            },
        )
        .await
        .unwrap();
    gate.wait_for_starts(1).await;

    let ensure_service = Arc::clone(&svc);
    let team_id = created.id.clone();
    let ensure = tokio::spawn(async move { ensure_service.ensure_session("user1", &team_id).await });
    tokio::task::yield_now().await;
    assert!(
        !ensure.is_finished(),
        "ensure must join the in-flight Leader spawn attach"
    );
    assert_eq!(gate.starts(), vec![spawned.conversation_id.clone()]);
    assert_eq!(task_manager.snapshot().build, vec![spawned.conversation_id.clone()]);
    assert!(
        task_manager
            .snapshot()
            .kill
            .iter()
            .all(|(conversation_id, _)| conversation_id == &spawned.conversation_id),
        "Leader spawn reconciliation must not restart healthy members"
    );
    assert!(Arc::ptr_eq(
        &original_scheduler,
        &svc.get_session_scheduler(&created.id).unwrap()
    ));

    gate.release(1);
    ensure.await.unwrap().unwrap();
    assert_eq!(gate.starts(), vec![spawned.conversation_id]);
}

#[tokio::test]
async fn lead_send_agent_message_without_active_run_opens_system_lifecycle_run() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");

    // Session loaded with NO active team run: this is the run-less window in
    // which the leader autonomously wakes a teammate.
    svc.ensure_session("user1", &created.id)
        .await
        .expect("session should load without an active team run");

    let lead_slot_id = created
        .leader_assistant_id
        .clone()
        .expect("created team should have a lead slot");
    let worker_slot_id = created
        .assistants
        .iter()
        .find(|agent| agent.role == "teammate")
        .map(|agent| agent.slot_id.clone())
        .expect("seeded teammate slot");

    // Pre-fix: this returned Err("no active team run for run-scoped wake").
    // Post-fix: the wake is a legitimate work initiation and succeeds.
    let result = svc
        .send_agent_message_from_agent(&created.id, &lead_slot_id, &worker_slot_id, "Do this", None)
        .await
        .expect("run-less run-scoped wake must now succeed");
    assert!(result.team_run_id.is_some(), "the wake must land inside a team run");

    let run_state = svc.get_run_state("user1", &created.id).await.unwrap();
    let active_run = run_state
        .active_run
        .expect("run-scoped wake must open/attach an active team run");
    assert_eq!(active_run.source, TeamRunSource::SystemLifecycle);
    assert!(!active_run.has_user_intervention);
}

#[tokio::test]
async fn lead_shutdown_agent_without_active_run_opens_system_lifecycle_run() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    svc.ensure_session("user1", &created.id)
        .await
        .expect("session should load without an active team run");

    let lead_slot_id = created
        .leader_assistant_id
        .clone()
        .expect("created team should have a lead slot");
    let worker_slot_id = created
        .assistants
        .iter()
        .find(|agent| agent.role == "teammate")
        .map(|agent| agent.slot_id.clone())
        .expect("seeded teammate slot");

    // team_shutdown_agent shares the same InheritRunningBatch binding as send
    // and is NOT gated by the (now-deleted) guard. Pre-fix it enqueued run-less
    // ("background") and opened NO run; post-fix it lands in an active run.
    svc.shutdown_agent_in_session(&created.id, &lead_slot_id, &worker_slot_id, Some("done".into()))
        .await
        .expect("leader shutdown of a teammate must succeed");

    let run_state = svc.get_run_state("user1", &created.id).await.unwrap();
    let active_run = run_state
        .active_run
        .expect("shutdown wake must open/attach an active team run");
    assert_eq!(active_run.source, TeamRunSource::SystemLifecycle);
    assert!(!active_run.has_user_intervention);
}

#[tokio::test]
async fn spawn_agent_in_session_aborts_lease_when_persistence_fails() {
    let (svc, team_repo, _, _) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        seeded_agent_metadata_repo(),
        Arc::new(SingleAssistantDefinitionRepo {
            row: word_creator_definition(),
        }),
        Arc::new(EmptyAssistantOverlayRepo),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.send_message("user1", &created.id, "start active run", None)
        .await
        .expect("active run");
    team_repo.fail_agent_updates();

    let lead_slot_id = created.leader_assistant_id.clone().unwrap();
    let req = SpawnAgentRequest {
        name: "Helper".into(),
        assistant_id: Some("word-creator".into()),
    };

    let err = svc
        .spawn_agent_in_session(&created.id, &lead_slot_id, req)
        .await
        .expect_err("forced agent persistence failure should fail spawn");
    assert!(err.to_string().contains("forced agent update failure"));

    let after = svc.get_team("user1", &created.id).await.unwrap();
    assert!(
        after.assistants.iter().all(|agent| agent.name != "Helper"),
        "failed spawn must not persist helper after aborted spawn lease"
    );
}

#[tokio::test]
async fn spawn_agent_in_session_compensates_when_welcome_mailbox_write_fails() {
    let (svc, team_repo, _, _) = setup_with_factory_metadata_assistants_and_conversation_repo(
        success_factory(),
        seeded_agent_metadata_repo(),
        Arc::new(SingleAssistantDefinitionRepo {
            row: word_creator_definition(),
        }),
        Arc::new(EmptyAssistantOverlayRepo),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Alpha".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.send_message("user1", &created.id, "start active run", None)
        .await
        .expect("active run");
    team_repo.fail_message_writes();

    let lead_slot_id = created.leader_assistant_id.clone().unwrap();
    let req = SpawnAgentRequest {
        name: "Helper".into(),
        assistant_id: Some("word-creator".into()),
    };

    let err = svc
        .spawn_agent_in_session(&created.id, &lead_slot_id, req)
        .await
        .expect_err("welcome mailbox write failure should fail spawn");
    assert!(err.to_string().contains("forced mailbox write failure"));

    let after = svc.get_team("user1", &created.id).await.unwrap();
    assert!(
        after.assistants.iter().all(|agent| agent.name != "Helper"),
        "compensation must remove persisted helper after welcome write failure"
    );
}

#[tokio::test]
async fn es2_ensure_session_is_idempotent() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
}

#[tokio::test]
async fn es3_ensure_session_nonexistent_team() {
    let svc = setup();
    let result = svc.ensure_session("user1", "nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn es4_ensure_session_rejects_cross_user_access() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Private".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.ensure_session("user2", &created.id).await;

    assert!(matches!(result, Err(aionui_team::TeamError::TeamNotFound(_))));
}

// -- W5-D31b-2: team.sessionStatusChanged service-layer broadcasts -----------
//
// The happy-path phase transitions are covered by focused service/session
// assertions. This test keeps the load-failed broadcast covered end-to-end.

#[tokio::test]
async fn d31b2_ensure_session_broadcasts_failed_loading_team_for_missing_team() {
    let (svc, recorder) = setup_with_recording_broadcaster();
    let err = svc.ensure_session("user1", "nonexistent-team-xyz").await.unwrap_err();
    assert!(matches!(err, aionui_team::TeamError::TeamNotFound(_)));

    assert!(
        recorder.events_by_name("team.sessionStatusChanged").is_empty(),
        "missing teams should not emit session status events"
    );
}

#[tokio::test]
async fn ensure_session_broadcasts_starting_and_ready_session_status() {
    let (svc, recorder) = setup_with_recording_broadcaster();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();

    let events = recorder.events_by_name("team.sessionStatusChanged");
    assert!(
        events.iter().any(|event| {
            event.data.get("team_id").and_then(|v| v.as_str()) == Some(created.id.as_str())
                && event.data.get("status").and_then(|v| v.as_str()) == Some("starting")
                && event.data.get("phase").and_then(|v| v.as_str()) == Some("loading_team")
        }),
        "ensure_session must emit starting/loading_team"
    );
    assert!(
        events.iter().any(|event| {
            event.data.get("team_id").and_then(|v| v.as_str()) == Some(created.id.as_str())
                && event.data.get("status").and_then(|v| v.as_str()) == Some("ready")
                && event.data.get("server_count").and_then(|v| v.as_u64()) == Some(2)
        }),
        "ensure_session must emit ready with server_count"
    );
}

#[tokio::test]
async fn ensure_session_existing_ready_session_broadcasts_ready_terminal_status() {
    let (svc, recorder) = setup_with_recording_broadcaster();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    recorder.clear();

    svc.ensure_session("user1", &created.id).await.unwrap();

    let events = recorder.events_by_name("team.sessionStatusChanged");
    assert!(
        events.iter().all(|event| {
            event.data.get("team_id").and_then(|v| v.as_str()) != Some(created.id.as_str())
                || event.data.get("status").and_then(|v| v.as_str()) != Some("starting")
        }),
        "existing ready session fast path must not emit starting because no lifecycle transition is happening"
    );
    assert!(
        events.iter().any(|event| {
            event.data.get("team_id").and_then(|v| v.as_str()) == Some(created.id.as_str())
                && event.data.get("status").and_then(|v| v.as_str()) == Some("ready")
                && event.data.get("server_count").and_then(|v| v.as_u64()) == Some(2)
        }),
        "existing ready session fast path must emit a ready terminal status"
    );
}

#[tokio::test]
async fn ss1_stop_session() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.stop_session("user1", &created.id).await.unwrap();
}

#[tokio::test]
async fn ss3_stop_session_without_active_is_noop() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.stop_session("user1", &created.id).await.unwrap();
}

#[tokio::test]
async fn ss4_stop_session_rejects_cross_user_access() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Private".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.stop_session("user2", &created.id).await;

    assert!(matches!(result, Err(aionui_team::TeamError::TeamNotFound(_))));
}

// ===========================================================================
// Test: Message sending requires active session (SM-*)
// ===========================================================================

#[tokio::test]
async fn sm4_send_message_no_session_returns_error() {
    let svc = setup();
    let result = svc.send_message("user1", "nonexistent", "Hello", None).await;
    assert!(result.is_err());
}

#[tokio::test]
async fn sm1_send_message_with_active_session() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.send_message("user1", &created.id, "Hello team", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn sm2_send_message_rejects_cross_user_access() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Private".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let result = svc.send_message("user2", &created.id, "Hello", None).await;

    assert!(matches!(result, Err(aionui_team::TeamError::TeamNotFound(_))));
}

#[tokio::test]
async fn sa_send_message_to_agent_with_active_session() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    let worker_slot = created.assistants[1].slot_id.clone();
    svc.send_message_to_agent("user1", &created.id, &worker_slot, "Do this", None)
        .await
        .unwrap();
}

#[tokio::test]
async fn sa2_send_message_to_agent_rejects_cross_user_access() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Private".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker_slot = created.assistants[1].slot_id.clone();

    let result = svc
        .send_message_to_agent("user2", &created.id, &worker_slot, "Do this", None)
        .await;

    assert!(matches!(result, Err(aionui_team::TeamError::TeamNotFound(_))));
}

#[tokio::test]
async fn sa3_send_message_to_nonexistent_agent() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    let result = svc
        .send_message_to_agent("user1", &created.id, "nonexistent", "Hello", None)
        .await;
    assert!(result.is_err());
}

// ===========================================================================
// Test: dispose_all
// ===========================================================================

#[tokio::test]
async fn dispose_all_cleans_up_sessions() {
    let svc = setup();
    let t1 = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "A".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let t2 = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "B".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &t1.id).await.unwrap();
    svc.ensure_session("user1", &t2.id).await.unwrap();

    svc.dispose_all();

    // After dispose, sessions are cleaned up.
    assert!(svc.get_session_scheduler(&t1.id).is_none());
    assert!(svc.get_session_scheduler(&t2.id).is_none());
}

// ===========================================================================
// Test: Delete team stops active session (TD-2 + integration)
// ===========================================================================

#[tokio::test]
async fn td_delete_team_stops_session() {
    let svc = setup();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.remove_team("user1", &created.id).await.unwrap();

    let result = svc.send_message("user1", &created.id, "Hello", None).await;
    assert!(result.is_err());
}

// ===========================================================================
// Test: D9 ensure_session kill + rebuild closed loop
// ===========================================================================

#[tokio::test]
async fn d9_create_team_persists_without_warming_initial_agents() {
    let (svc, tm) = setup_with_factory(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    let calls = tm.snapshot();
    assert_eq!(
        created.assistants.len(),
        2,
        "create_team should still persist initial agents"
    );
    assert!(calls.kill.is_empty(), "create_team must not rebuild initial agents");
    assert!(calls.build.is_empty(), "create_team must not warm initial agents");
    assert_eq!(tm.active_count(), 0, "create_team must not register live agent tasks");
    assert!(
        svc.get_session_scheduler(&created.id).is_none(),
        "create_team must not create a runtime session"
    );
}

#[tokio::test]
async fn d9_ensure_session_warms_up_only_the_lead() {
    let (svc, tm) = setup_with_factory(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    reset_runtime_state(&svc, &tm, &created.id).await;
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Leader-only warmup (spec 5.1): only the lead is killed+rebuilt at first
    // start; the teammate stays dormant and is never built.
    let lead = created.assistants.iter().find(|a| a.role == "lead").unwrap();
    let worker = created.assistants.iter().find(|a| a.role == "teammate").unwrap();
    let calls = tm.snapshot();
    assert_eq!(
        calls.build,
        vec![lead.conversation_id.clone()],
        "only the lead should be built"
    );
    assert_eq!(calls.kill.len(), 1, "only the lead should be killed+rebuilt");
    assert_eq!(calls.kill[0].0, lead.conversation_id);
    assert_eq!(calls.kill[0].1, Some(AgentKillReason::TeamMcpRebuild));
    assert!(
        !calls.build.contains(&worker.conversation_id),
        "dormant teammate must not be built at first start"
    );
}

#[tokio::test]
async fn d9_ensure_session_warms_up_only_the_lead_without_teammate_stagger() {
    // The batch rebuild machine (bounded concurrency + staggered starts) was
    // removed in favor of leader-only warmup on a single attach path (spec 5.1).
    // First start must warm exactly the lead — no teammate warmup, no stagger.
    let probe = Arc::new(WarmupConcurrencyProbe::default());
    let (svc, _tm) = setup_with_factory(probe.factory(std::time::Duration::from_millis(10)));
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: five_agent_input_leader_not_first(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let lead = created
        .assistants
        .iter()
        .find(|assistant| assistant.role == "lead")
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();

    let starts = probe.starts();
    assert_eq!(
        starts,
        vec![lead.conversation_id.clone()],
        "leader-only warmup must start exactly the lead"
    );
    assert_eq!(
        probe.max_active(),
        1,
        "leader-only warmup runs a single attach, never overlapping teammates"
    );
}

#[tokio::test]
async fn d9_ensure_session_persists_team_mcp_stdio_config() {
    // Each agent's conversation.extra must carry a `team_mcp_stdio_config`
    // object by the time the factory is called — that is what the rebuilt
    // typed Team context will expose to reach the MCP server.
    use futures_util::FutureExt;
    let (svc, _tm) = setup_with_factory(Arc::new(|opts: BuildTaskOptions| {
        async move {
            let context = opts.context;
            let typed_has_cfg = context
                .team
                .as_ref()
                .and_then(|team| team.mcp.as_ref())
                .is_some_and(|mcp| mcp.stdio.port > 0 && !mcp.stdio.slot_id.is_empty());
            let compat_has_cfg = match &context.kind {
                AgentSessionKind::Acp(acp) => {
                    assert!(acp.belongs_to_team);
                    assert!(acp.team.is_some(), "ACP build context must carry typed team binding");
                    acp.config
                        .team_mcp_stdio_config
                        .as_ref()
                        .is_some_and(|cfg| cfg.port > 0 && !cfg.slot_id.is_empty())
                }
                _ => false,
            };
            assert!(
                typed_has_cfg && compat_has_cfg,
                "factory called without typed team_mcp_stdio_config in context: {:?}",
                context.team
            );
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(context.conversation.conversation_id, context.workspace.path),
            )))
        }
        .boxed()
    }));

    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: aionrs_two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    svc.ensure_session("user1", &created.id).await.unwrap();
}

#[tokio::test]
async fn d9_ensure_session_is_idempotent() {
    let (svc, tm) = setup_with_factory(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    reset_runtime_state(&svc, &tm, &created.id).await;
    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Leader-only warmup: first ensure kills+builds only the lead; the second
    // ensure reconciles (lead already Ready, teammate dormant/skipped) and adds
    // no kill/build calls.
    let calls = tm.snapshot();
    assert_eq!(
        calls.kill.len(),
        1,
        "leader-only: only the lead is (re)built, and only once"
    );
    assert_eq!(calls.build.len(), 1, "second ensure_session must not re-build");
}

#[tokio::test]
async fn manual_add_then_immediate_ensure_joins_attach_without_rebuilding_session() {
    let gate = Arc::new(GatedProvisioningFactory::default());
    let (svc, _repo, task_manager, _recorder) = setup_with_factory_and_recording_broadcaster(gate.factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Join dynamic attach".into(),
                agents: vec![team_agent_input("Lead", "lead", "claude")],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let original_scheduler = svc.get_session_scheduler(&created.id).expect("published session");
    let lead_conversation_id = created.assistants[0].conversation_id.clone();
    task_manager.reset_calls();
    gate.enable();

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();
    gate.wait_for_starts(1).await;

    let svc_for_ensure = Arc::clone(&svc);
    let team_id = created.id.clone();
    let ensure = tokio::spawn(async move { svc_for_ensure.ensure_session("user1", &team_id).await });
    tokio::task::yield_now().await;
    assert!(!ensure.is_finished(), "ensure must join the pending dynamic attach");
    assert_eq!(gate.starts(), vec![added.conversation_id.clone()]);
    assert!(Arc::ptr_eq(
        &original_scheduler,
        &svc.get_session_scheduler(&created.id).expect("same published session")
    ));
    assert!(
        task_manager
            .snapshot()
            .kill
            .iter()
            .all(|(conversation_id, _)| conversation_id != &lead_conversation_id),
        "joining a dynamic attach must not kill a healthy member"
    );

    gate.release(1);
    ensure.await.unwrap().unwrap();
    assert_eq!(
        task_manager
            .snapshot()
            .build
            .iter()
            .filter(|conversation_id| *conversation_id == &added.conversation_id)
            .count(),
        1,
        "the dynamic conversation must be built once"
    );
}

#[tokio::test]
async fn concurrent_ensures_launch_one_dynamic_attach() {
    let gate = Arc::new(GatedProvisioningFactory::default());
    let (svc, task_manager) = setup_with_factory(gate.factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Concurrent repair".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    let original_scheduler = svc.get_session_scheduler(&created.id).expect("published session");
    let lead = created.assistants.iter().find(|agent| agent.role == "lead").unwrap();
    let worker = created
        .assistants
        .iter()
        .find(|agent| agent.role == "teammate")
        .unwrap();
    // Leader-only warmup leaves the worker dormant, so reconciliation would skip
    // it (spec 5.1). Repair is exercised against the always-warm lead: drop its
    // task so concurrent ensures reconcile it, and assert lease dedup launches
    // exactly one attach.
    task_manager.remove_task_without_recording(&lead.conversation_id).await;
    task_manager.reset_calls();
    gate.enable();

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let mut handles = Vec::new();
    for _ in 0..2 {
        let svc = Arc::clone(&svc);
        let team_id = created.id.clone();
        let barrier = Arc::clone(&barrier);
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            svc.ensure_session("user1", &team_id).await
        }));
    }
    barrier.wait().await;
    gate.wait_for_starts(1).await;
    tokio::task::yield_now().await;
    assert!(handles.iter().all(|handle| !handle.is_finished()));
    assert_eq!(gate.starts(), vec![lead.conversation_id.clone()]);
    assert_eq!(task_manager.snapshot().build, vec![lead.conversation_id.clone()]);
    assert!(
        task_manager
            .snapshot()
            .kill
            .iter()
            .all(|(conversation_id, _)| conversation_id != &worker.conversation_id),
        "dormant members must not be woken or killed during a one-slot repair"
    );
    assert!(Arc::ptr_eq(
        &original_scheduler,
        &svc.get_session_scheduler(&created.id).expect("same published session")
    ));

    gate.release(1);
    for handle in handles {
        handle.await.unwrap().unwrap();
    }
    assert_eq!(gate.starts(), vec![lead.conversation_id.clone()]);
}

#[tokio::test]
async fn stopped_session_rejects_late_attach_completion() {
    let gate = Arc::new(GatedProvisioningFactory::default());
    let (svc, _repo, task_manager, recorder) = setup_with_factory_and_recording_broadcaster(gate.factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Stopped late attach".into(),
                agents: vec![team_agent_input("Lead", "lead", "claude")],
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();
    task_manager.reset_calls();
    gate.enable();

    let added = svc
        .add_agent(
            "user1",
            &created.id,
            AddAgentRequest {
                name: "Worker".into(),
                role: "teammate".into(),
                backend: Some("acp".into()),
                model: "claude".into(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();
    gate.wait_for_starts(1).await;
    let lead_conversation_id = created.assistants[0].conversation_id.clone();
    svc.stop_session("user1", &created.id).await.unwrap();

    // Leader-only warmup: the replacement session cold-starts the lead only
    // (its build is not gated), so it completes without touching the
    // dynamically-added worker, which stays dormant in the new session.
    svc.ensure_session("user1", &created.id)
        .await
        .expect("replacement leader-only ensure completes");

    // The worker attach in the old session issued exactly one kill (before its
    // gated build). Record it so we can assert the fenced late completion adds
    // no further kill.
    let worker_kills_before_release = task_manager
        .snapshot()
        .kill
        .iter()
        .filter(|(conversation_id, _)| conversation_id == &added.conversation_id)
        .count();

    // Release the old (stopped) session's still-in-flight worker attach and let
    // it run to completion. The generation fence must reject it: it must never
    // publish Ready, and its stale cleanup must be skipped because a different
    // session is now published (so it cannot kill the new session's runtime).
    gate.release(1);
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }

    let added_events = recorder
        .events_by_name("team.agentRuntimeStatusChanged")
        .into_iter()
        .filter(|event| event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(added.slot_id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        added_events
            .iter()
            .filter(|event| event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready"))
            .count(),
        0,
        "a fenced late attach must not publish Ready; leader-only keeps the worker dormant in the new session"
    );
    let worker_kills_after_release = task_manager
        .snapshot()
        .kill
        .iter()
        .filter(|(conversation_id, _)| conversation_id == &added.conversation_id)
        .count();
    assert_eq!(
        worker_kills_after_release, worker_kills_before_release,
        "the fenced late completion must not kill the worker again once a different session is published"
    );
    assert!(
        task_manager.get_task(&lead_conversation_id).is_some(),
        "stale worker cleanup must not kill the replacement session's live lead runtime"
    );
    assert!(svc.get_session_scheduler(&created.id).is_some());
}

#[tokio::test]
async fn d9_ensure_session_rollbacks_when_build_fails() {
    // Factory always fails → ensure_session must propagate error and not
    // insert into sessions, so send_message afterwards still errors.
    use futures_util::FutureExt;
    let failing_factory: AgentFactory =
        Arc::new(|_opts: BuildTaskOptions| async move { Err(AgentError::internal("simulated build failure")) }.boxed());
    let (svc, tm) = setup_with_factory(failing_factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    reset_runtime_state(&svc, &tm, &created.id).await;
    let result = svc.ensure_session("user1", &created.id).await;
    assert!(
        result.is_err(),
        "ensure_session should propagate the leader build error"
    );

    // Leader-only warmup: only the lead attach is attempted; its failure fails
    // the whole session and no teammate is ever built or killed.
    let lead = created.assistants.iter().find(|a| a.role == "lead").unwrap();
    let worker = created.assistants.iter().find(|a| a.role == "teammate").unwrap();
    let calls = tm.snapshot();
    assert_eq!(
        calls.build,
        vec![lead.conversation_id.clone()],
        "only the lead build is attempted"
    );
    assert!(
        !calls.build.contains(&worker.conversation_id)
            && calls
                .kill
                .iter()
                .all(|(conversation_id, _)| conversation_id != &worker.conversation_id),
        "the dormant teammate is never built or killed on a leader bootstrap failure"
    );

    let send_result = svc.send_message("user1", &created.id, "Hello", None).await;
    assert!(
        send_result.is_err(),
        "session must not be registered after leader build failure"
    );
}

#[tokio::test]
async fn cold_bootstrap_failure_stops_session_when_leader_attach_fails() {
    use futures_util::FutureExt;

    let fail_conversation_id: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let fail_target = Arc::clone(&fail_conversation_id);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let fail_target = Arc::clone(&fail_target);
        async move {
            let conversation_id = opts.context.conversation.conversation_id.clone();
            if fail_target.lock().unwrap().as_deref() == Some(conversation_id.as_str()) {
                return Err(AgentError::internal("simulated build failure"));
            }

            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, _team_repo, tm, recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: four_agent_input_leader_not_first(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    // Leader-only warmup: only the lead is attached at cold start, so only a
    // lead failure can fail the session (teammate failures are isolated and
    // deferred to lazy wakeup, spec 5.1). Fail the lead's build.
    let lead = created
        .assistants
        .iter()
        .find(|assistant| assistant.role == "lead")
        .expect("lead");
    *fail_conversation_id.lock().unwrap() = Some(lead.conversation_id.clone());

    let result = svc.ensure_session("user1", &created.id).await;
    assert!(
        result.is_err(),
        "ensure_session should propagate the leader build error"
    );
    let error = result.unwrap_err().to_string();
    assert!(
        error.contains(&lead.slot_id),
        "leader attach failure should surface as a member-runtime failure for the lead slot: {error}"
    );

    let calls = tm.snapshot();
    assert_eq!(
        calls.build,
        vec![lead.conversation_id.clone()],
        "only the lead is built at cold start"
    );
    for teammate in created.assistants.iter().filter(|a| a.role != "lead") {
        assert!(
            !calls.build.contains(&teammate.conversation_id)
                && calls
                    .kill
                    .iter()
                    .all(|(conversation_id, _)| conversation_id != &teammate.conversation_id),
            "dormant teammate {} must never be built or killed on leader bootstrap failure",
            teammate.conversation_id
        );
    }
    assert_eq!(tm.active_count(), 0);
    assert!(
        svc.get_session_scheduler(&created.id).is_none(),
        "session must not be registered after leader bootstrap failure"
    );

    let team_session_failed = recorder
        .events_by_name("team.sessionStatusChanged")
        .into_iter()
        .find(|event| {
            event.data.get("team_id").and_then(|v| v.as_str()) == Some(created.id.as_str())
                && event.data.get("status").and_then(|v| v.as_str()) == Some("failed")
                && event.data.get("phase").and_then(|v| v.as_str()) == Some("attaching_agents")
        });
    assert!(
        team_session_failed.is_some(),
        "leader bootstrap failure must emit a team-level failed/attaching_agents terminal event"
    );
}

#[tokio::test]
async fn ensure_session_serializes_manual_add_until_rebuild_completes() {
    let build_started = Arc::new(tokio::sync::Notify::new());
    let release_build = Arc::new(tokio::sync::Notify::new());
    let (svc, _tm) = setup_with_factory(blocking_first_build_factory(
        Arc::clone(&build_started),
        Arc::clone(&release_build),
    ));
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let svc_for_ensure = Arc::clone(&svc);
    let ensure_team_id = created.id.clone();
    let ensure_handle = tokio::spawn(async move { svc_for_ensure.ensure_session("user1", &ensure_team_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), build_started.notified())
        .await
        .expect("ensure_session should start rebuilding before mutation attempt");

    let svc_for_add = Arc::clone(&svc);
    let add_team_id = created.id.clone();
    let add_handle = tokio::spawn(async move {
        svc_for_add
            .add_agent(
                "user1",
                &add_team_id,
                AddAgentRequest {
                    name: "Worker".into(),
                    role: "teammate".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                },
            )
            .await
    });

    // Leader-only warmup publishes the session before the (blocking) lead attach
    // and releases the membership guard, so a manual add now runs concurrently
    // with leader warmup instead of being serialized behind it. The invariant
    // that must still hold is a consistent final roster.
    release_build.notify_waiters();
    ensure_handle.await.unwrap().unwrap();
    add_handle.await.unwrap().unwrap();

    let scheduler = svc
        .get_session_scheduler(&created.id)
        .expect("session should be registered after ensure_session");
    assert_eq!(scheduler.list_agents().await.len(), 2);
}

#[tokio::test]
async fn ensure_session_serializes_manual_remove_until_rebuild_completes() {
    let build_started = Arc::new(tokio::sync::Notify::new());
    let release_build = Arc::new(tokio::sync::Notify::new());
    let (svc, _tm) = setup_with_factory(blocking_first_build_factory(
        Arc::clone(&build_started),
        Arc::clone(&release_build),
    ));
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker_slot = created.assistants[1].slot_id.clone();

    let svc_for_ensure = Arc::clone(&svc);
    let ensure_team_id = created.id.clone();
    let ensure_handle = tokio::spawn(async move { svc_for_ensure.ensure_session("user1", &ensure_team_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), build_started.notified())
        .await
        .expect("ensure_session should start rebuilding before mutation attempt");

    let svc_for_remove = Arc::clone(&svc);
    let remove_team_id = created.id.clone();
    let remove_slot = worker_slot.clone();
    let remove_handle = tokio::spawn(async move {
        svc_for_remove
            .remove_agent("user1", &remove_team_id, &remove_slot)
            .await
    });

    // Leader-only warmup runs the manual remove concurrently with leader warmup
    // (see the add variant); assert the final roster is consistent.
    release_build.notify_waiters();
    ensure_handle.await.unwrap().unwrap();
    remove_handle.await.unwrap().unwrap();

    let scheduler = svc
        .get_session_scheduler(&created.id)
        .expect("session should be registered after ensure_session");
    let agents = scheduler.list_agents().await;
    assert_eq!(agents.len(), 1);
    assert!(agents.iter().all(|agent| agent.slot_id != worker_slot));
}

#[tokio::test]
async fn ensure_session_serializes_manual_rename_until_rebuild_completes() {
    let build_started = Arc::new(tokio::sync::Notify::new());
    let release_build = Arc::new(tokio::sync::Notify::new());
    let (svc, _tm) = setup_with_factory(blocking_first_build_factory(
        Arc::clone(&build_started),
        Arc::clone(&release_build),
    ));
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker_slot = created.assistants[1].slot_id.clone();

    let svc_for_ensure = Arc::clone(&svc);
    let ensure_team_id = created.id.clone();
    let ensure_handle = tokio::spawn(async move { svc_for_ensure.ensure_session("user1", &ensure_team_id).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), build_started.notified())
        .await
        .expect("ensure_session should start rebuilding before mutation attempt");

    let svc_for_rename = Arc::clone(&svc);
    let rename_team_id = created.id.clone();
    let rename_slot = worker_slot.clone();
    let rename_handle = tokio::spawn(async move {
        svc_for_rename
            .rename_agent("user1", &rename_team_id, &rename_slot, "Senior Worker")
            .await
    });

    // Leader-only warmup runs the manual rename concurrently with leader warmup
    // (see the add variant); assert the rename is reflected in the final roster.
    release_build.notify_waiters();
    ensure_handle.await.unwrap().unwrap();
    rename_handle.await.unwrap().unwrap();

    let scheduler = svc
        .get_session_scheduler(&created.id)
        .expect("session should be registered after ensure_session");
    let agents = scheduler.list_agents().await;
    let renamed = agents.iter().find(|agent| agent.slot_id == worker_slot).unwrap();
    assert_eq!(renamed.name, "Senior Worker");
}

// ===========================================================================
// Test: D11.5 remove_team cascades kill to every agent process
// ===========================================================================

// ===========================================================================
// Test: W4-D23 add_agent_locks — per-team serialization prevents last-writer-
// wins when two tasks race on add_agent.
// ===========================================================================

#[tokio::test]
async fn w4_d23_concurrent_add_agent_preserves_every_insertion() {
    // Two concurrent add_agent calls on the same team must both be persisted
    // (no silent drop from unsynchronized read-modify-write on the agents
    // JSON blob).
    let svc = Arc::new(setup());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                }],
                workspace: None,
            },
        )
        .await
        .unwrap();

    let svc_a = svc.clone();
    let team_id_a = created.id.clone();
    let task_a = tokio::spawn(async move {
        svc_a
            .add_agent(
                "user1",
                &team_id_a,
                AddAgentRequest {
                    name: "WorkerA".into(),
                    role: "teammate".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                },
            )
            .await
    });

    let svc_b = svc.clone();
    let team_id_b = created.id.clone();
    let task_b = tokio::spawn(async move {
        svc_b
            .add_agent(
                "user1",
                &team_id_b,
                AddAgentRequest {
                    name: "WorkerB".into(),
                    role: "teammate".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                },
            )
            .await
    });

    let (a, b) = tokio::join!(task_a, task_b);
    a.unwrap().unwrap();
    b.unwrap().unwrap();

    let got = svc.get_team("user1", &created.id).await.unwrap();
    assert_eq!(
        got.assistants.len(),
        3,
        "both concurrent add_agent calls must be persisted (1 lead + 2 workers)"
    );
    let names: std::collections::HashSet<_> = got.assistants.iter().map(|a| a.name.clone()).collect();
    assert!(names.contains("Lead"));
    assert!(names.contains("WorkerA"));
    assert!(names.contains("WorkerB"));
}

#[tokio::test]
async fn d115_remove_team_kills_every_agent_process() {
    let (svc, tm) = setup_with_factory(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();

    reset_runtime_state(&svc, &tm, &created.id).await;
    // Leader-only warmup: only the lead is live after ensure_session; the
    // teammate stays dormant (spec 5.1).
    svc.ensure_session("user1", &created.id).await.unwrap();
    assert_eq!(
        tm.active_count(),
        1,
        "leader-only warmup registers only the lead runtime"
    );

    let before_kill = tm.snapshot().kill.len();

    svc.remove_team("user1", &created.id).await.unwrap();

    // remove_team must have issued one kill per agent with reason TeamDeleted,
    // and the task manager's active_count must drop back to 0.
    let calls = tm.snapshot();
    let new_kills = &calls.kill[before_kill..];
    assert_eq!(
        new_kills.len(),
        created.assistants.len(),
        "remove_team must kill every agent once"
    );
    for (i, agent) in created.assistants.iter().enumerate() {
        assert_eq!(new_kills[i].0, agent.conversation_id);
        assert_eq!(new_kills[i].1, Some(AgentKillReason::TeamDeleted));
    }
    assert_eq!(
        tm.active_count(),
        0,
        "every agent worker must be torn down after remove_team"
    );
}

// ===========================================================================
// Task A7: per-member attach route/service — directed retry/wakeup of a single
// member runtime (dormant or failed). auth + CSRF are enforced by the shared
// team router middleware layer (same as add_agent/remove_agent/send_message);
// the service-level behavior is asserted here.
// ===========================================================================

#[tokio::test]
async fn attach_agent_runtime_wakes_dormant_teammate() {
    let (svc, _team_repo, _task_manager, recorder) = setup_with_factory_and_recording_broadcaster(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Directed attach".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker = created
        .assistants
        .iter()
        .find(|assistant| assistant.role == "teammate")
        .expect("teammate")
        .clone();
    svc.ensure_session("user1", &created.id).await.unwrap();

    // The teammate is dormant after leader-only warmup; a directed attach wakes it.
    svc.attach_agent_runtime("user1", &created.id, &worker.slot_id)
        .await
        .expect("directed attach should succeed");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let ready = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(worker.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready")
                });
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("directed attach must bring the dormant teammate to ready");
}

#[tokio::test]
async fn attach_agent_runtime_rejects_cross_user() {
    let (svc, _tm) = setup_with_factory(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Directed attach isolation".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker = created
        .assistants
        .iter()
        .find(|assistant| assistant.role == "teammate")
        .expect("teammate")
        .clone();
    svc.ensure_session("user1", &created.id).await.unwrap();

    let error = svc
        .attach_agent_runtime("intruder", &created.id, &worker.slot_id)
        .await
        .expect_err("cross-user directed attach must be rejected");
    // User-scope convention: cross-user access returns TeamNotFound (404) to
    // avoid leaking team existence, consistent with every other cross-user
    // team operation on this branch.
    assert!(
        matches!(error, TeamError::TeamNotFound(_)),
        "expected TeamNotFound, got {error:?}"
    );
}

#[tokio::test]
async fn attach_agent_runtime_rejects_unknown_slot() {
    let (svc, _tm) = setup_with_factory(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Directed attach unknown slot".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();

    let error = svc
        .attach_agent_runtime("user1", &created.id, "slot-does-not-exist")
        .await
        .expect_err("attaching an unknown slot must be rejected");
    assert!(
        matches!(error, TeamError::AgentNotFound(_)),
        "expected AgentNotFound, got {error:?}"
    );
}

// The full-screen warmup overlay is driven by session-level status and must
// reflect the leader only (spec 5.4/5.5): once the team is Ready (leader ready),
// waking a dormant teammate is an inline, per-column event and must NOT flip the
// session back to `starting` — otherwise the overlay resurfaces on every lazy
// wakeup / add-member.
#[tokio::test]
async fn waking_dormant_teammate_does_not_resurface_session_starting() {
    let (svc, _team_repo, _task_manager, recorder) = setup_with_factory_and_recording_broadcaster(success_factory());
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Lazy wakeup overlay".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker = created
        .assistants
        .iter()
        .find(|assistant| assistant.role == "teammate")
        .expect("teammate")
        .clone();
    svc.ensure_session("user1", &created.id).await.unwrap();

    // The team is Ready with the teammate dormant. Drop the bootstrap events so
    // only the wakeup's session-status broadcasts remain.
    recorder.clear();

    svc.attach_agent_runtime("user1", &created.id, &worker.slot_id)
        .await
        .expect("directed attach should succeed");

    // Wait until the teammate attach has fully completed (ready broadcast).
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let ready = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(worker.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready")
                });
            if ready {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("teammate attach must complete");

    let starting = recorder
        .events_by_name("team.sessionStatusChanged")
        .into_iter()
        .find(|event| {
            event.data.get("team_id").and_then(serde_json::Value::as_str) == Some(created.id.as_str())
                && event.data.get("status").and_then(serde_json::Value::as_str) == Some("starting")
        });
    assert!(
        starting.is_none(),
        "waking a dormant teammate must not resurface the warmup overlay (session `starting`); \
         teammate lifecycle is inline via agentRuntimeStatusChanged, got {starting:?}"
    );
}

// A teammate's lazy-attach FAILURE is inline (spec 5.4②): the per-member
// runtime status goes `failed` and the send box gates that column, but the
// session-level status must stay Ready (leader still ready = team usable). A
// teammate failure must never raise the full-screen failure card.
#[tokio::test]
async fn failed_teammate_wakeup_does_not_flip_session_to_failed() {
    use futures_util::FutureExt;

    let fail_next = Arc::new(AtomicBool::new(false));
    let factory_fail_next = Arc::clone(&fail_next);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let should_fail = factory_fail_next.swap(false, Ordering::SeqCst);
        async move {
            if should_fail {
                return Err(AgentError::internal("simulated teammate lazy attach failure"));
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, _team_repo, _task_manager, recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Teammate failure stays inline".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let worker = created
        .assistants
        .iter()
        .find(|assistant| assistant.role == "teammate")
        .expect("teammate")
        .clone();
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Leader is Ready; drop bootstrap events. Now arm a failure and wake the
    // dormant teammate — the failure must stay inline.
    recorder.clear();
    fail_next.store(true, Ordering::SeqCst);
    svc.send_message_to_agent("user1", &created.id, &worker.slot_id, "please do X", None)
        .await
        .expect("human delivery acks immediately even though the lazy attach will fail");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let failed = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(worker.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
                });
            if failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed lazy attach must broadcast a failed runtime status for the teammate");

    let session_failed = recorder
        .events_by_name("team.sessionStatusChanged")
        .into_iter()
        .find(|event| {
            event.data.get("team_id").and_then(serde_json::Value::as_str) == Some(created.id.as_str())
                && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
        });
    assert!(
        session_failed.is_none(),
        "a teammate lazy-attach failure must stay inline and never flip session status to `failed` \
         (leader still ready = team usable, spec 5.4②), got {session_failed:?}"
    );
}

#[tokio::test]
async fn lazy_attach_failure_preserves_unread_and_skips_leader_on_human_delivery() {
    use futures_util::FutureExt;

    // Fail exactly the next build after it is armed; the lead attaches cleanly
    // during cold start, then the teammate's lazy attach fails.
    let fail_next = Arc::new(AtomicBool::new(false));
    let factory_fail_next = Arc::clone(&fail_next);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let should_fail = factory_fail_next.swap(false, Ordering::SeqCst);
        async move {
            if should_fail {
                return Err(AgentError::internal("simulated teammate lazy attach failure"));
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, team_repo, _task_manager, recorder) = setup_with_factory_and_recording_broadcaster(factory);
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Lazy failure preserves unread".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let lead = created.assistants.iter().find(|a| a.role == "lead").unwrap().clone();
    let worker = created
        .assistants
        .iter()
        .find(|a| a.role == "teammate")
        .unwrap()
        .clone();
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Arm the failure and deliver to the dormant teammate (human-direct).
    fail_next.store(true, Ordering::SeqCst);
    svc.send_message_to_agent("user1", &created.id, &worker.slot_id, "please do X", None)
        .await
        .expect("human delivery acks immediately even though the lazy attach will fail");

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let failed = recorder
                .events_by_name("team.agentRuntimeStatusChanged")
                .iter()
                .any(|event| {
                    event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(worker.slot_id.as_str())
                        && event.data.get("status").and_then(serde_json::Value::as_str) == Some("failed")
                });
            if failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed lazy attach must broadcast a failed runtime status for the teammate");

    // Preserve-unread (spec 5.4b): the delivered message must remain unread so a
    // retry re-drains it via reconcile_mailbox.
    let worker_unread = team_repo
        .peek_unread("user1", &created.id, &worker.slot_id)
        .await
        .unwrap();
    assert!(
        worker_unread.iter().any(|message| message.content == "please do X"),
        "a failed lazy attach must not mark the pending delivery read"
    );

    // Human-direct failure must NOT wake the leader (spec 5.4c): the failure is
    // surfaced inline to the user instead.
    let lead_unread = team_repo
        .peek_unread("user1", &created.id, &lead.slot_id)
        .await
        .unwrap();
    assert!(
        !lead_unread
            .iter()
            .any(|message| message.from_agent_id == worker.slot_id),
        "a human-direct lazy attach failure must not notify the leader"
    );
}

#[tokio::test]
async fn agent_triggered_attach_failure_notifies_leader() {
    use futures_util::FutureExt;

    // Positive counterpart to the human-direct/manual-add "must NOT notify"
    // tests: an agent-triggered attach failure (here a leader-initiated spawn,
    // which flows through the single attach path with
    // notify_leader_on_failure=true) MUST wake the leader so it can re-delegate
    // the work it just handed out (spec 5.4c).
    //
    // The lead attaches cleanly during cold start; only the spawned teammate's
    // attach is armed to fail.
    let fail_next = Arc::new(AtomicBool::new(false));
    let factory_fail_next = Arc::clone(&fail_next);
    let factory: AgentFactory = Arc::new(move |opts: BuildTaskOptions| {
        let should_fail = factory_fail_next.swap(false, Ordering::SeqCst);
        async move {
            if should_fail {
                return Err(AgentError::internal("simulated spawned teammate attach failure"));
            }
            Ok(aionui_ai_agent::AgentInstance::Mock(Arc::new(
                mock_agent::MockAgent::new(opts.context.conversation.conversation_id, opts.context.workspace.path),
            )))
        }
        .boxed()
    });
    let (svc, team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_assistants_and_conversation_repo(
        factory,
        seeded_agent_metadata_repo(),
        Arc::new(SingleAssistantDefinitionRepo {
            row: word_creator_definition(),
        }),
        Arc::new(EmptyAssistantOverlayRepo),
    );
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Agent-triggered failure notifies leader".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .unwrap();
    let lead_slot_id = created.leader_assistant_id.clone().expect("leader slot");
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Arm the failure, then have the leader spawn a teammate whose attach fails.
    fail_next.store(true, Ordering::SeqCst);
    let spawned = svc
        .spawn_agent_in_session(
            &created.id,
            &lead_slot_id,
            SpawnAgentRequest {
                name: "Writer".into(),
                assistant_id: Some("word-creator".into()),
            },
        )
        .await
        .expect("spawn returns before the background attach completes");

    // Agent/leader-triggered failure MUST notify the leader: its mailbox gets a
    // "failed to start its runtime" message from the failed slot so it can
    // re-delegate.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let leader_messages = team_repo
                .get_history("user1", &created.id, &lead_slot_id, None)
                .await
                .unwrap();
            if leader_messages.iter().any(|message| {
                message.from_agent_id == spawned.slot_id && message.content.contains("failed to start its runtime")
            }) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an agent-triggered attach failure must notify the leader (notify_leader_on_failure=true)");
}

// ===========================================================================
// Test: Team activity read endpoints (mailbox & tasks)
// ===========================================================================

fn activity_team_row(id: &str, user_id: &str) -> aionui_db::models::TeamRow {
    aionui_db::models::TeamRow {
        id: id.into(),
        user_id: user_id.into(),
        name: "Activity Team".into(),
        workspace: String::new(),
        workspace_mode: "shared".into(),
        agents: "[]".into(),
        lead_agent_id: None,
        session_mode: None,
        agents_version: "1.0.1".into(),
        created_at: aionui_common::now_ms(),
        updated_at: aionui_common::now_ms(),
        project_id: None,
        folder_id: None,
    }
}

fn activity_message_row(id: &str, team_id: &str, created_at: i64) -> aionui_db::models::MailboxMessageRow {
    aionui_db::models::MailboxMessageRow {
        id: id.into(),
        team_id: team_id.into(),
        to_agent_id: "a1".into(),
        from_agent_id: "lead".into(),
        msg_type: "message".into(),
        content: format!("content-{id}"),
        summary: None,
        files: None,
        read: false,
        created_at,
    }
}

fn activity_task_row(id: &str, team_id: &str, created_at: i64) -> aionui_db::models::TeamTaskRow {
    aionui_db::models::TeamTaskRow {
        id: id.into(),
        team_id: team_id.into(),
        subject: format!("subject-{id}"),
        description: None,
        status: "pending".into(),
        owner: Some("a1".into()),
        blocked_by: "[]".into(),
        blocks: "[]".into(),
        metadata: Some(r#"{"secret":"xxx"}"#.into()),
        created_at,
        updated_at: created_at,
    }
}

#[tokio::test]
async fn list_team_mailbox_happy_path_orders_desc() {
    let (svc, team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    for i in 1..=3 {
        team_repo
            .write_message("user1", &activity_message_row(&format!("m{i}"), "t1", i as i64 * 1000))
            .await
            .unwrap();
    }

    let messages = svc.list_team_mailbox("user1", "t1", 500).await.unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].id, "m3");
    assert_eq!(messages[2].id, "m1");
    // Response envelope-friendly DTO fields present, files normalized to a list.
    assert!(messages[0].files.is_empty());
}

#[tokio::test]
async fn list_team_mailbox_clamps_over_limit() {
    let (svc, team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    for i in 1..=5 {
        team_repo
            .write_message("user1", &activity_message_row(&format!("m{i}"), "t1", i as i64 * 1000))
            .await
            .unwrap();
    }

    // Over-limit is clamped to MAX (1000); still returns all 5.
    let messages = svc.list_team_mailbox("user1", "t1", 99_999).await.unwrap();
    assert_eq!(messages.len(), 5);
}

#[tokio::test]
async fn list_team_tasks_happy_path_desc_and_truncates_without_metadata() {
    let (svc, team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    for i in 1..=4 {
        team_repo
            .create_task("user1", &activity_task_row(&format!("tk{i}"), "t1", i as i64 * 1000))
            .await
            .unwrap();
    }

    let tasks = svc.list_team_tasks("user1", "t1", 2).await.unwrap();
    // DESC by created_at, truncated to 2.
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].id, "tk4");
    assert_eq!(tasks[1].id, "tk3");
    // Metadata is never exposed on the response DTO (compile-time guarantee:
    // the field does not exist), and serialized JSON must not contain it.
    let json = serde_json::to_value(&tasks[0]).unwrap();
    assert!(json.get("metadata").is_none());
    assert!(!serde_json::to_string(&tasks).unwrap().contains("secret"));
}

#[tokio::test]
async fn list_team_mailbox_missing_team_returns_not_found() {
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let err = svc.list_team_mailbox("user1", "missing", 500).await.unwrap_err();
    assert!(matches!(err, TeamError::TeamNotFound(_)));
}

#[tokio::test]
async fn list_team_tasks_missing_team_returns_not_found() {
    let (svc, _team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    let err = svc.list_team_tasks("user1", "missing", 500).await.unwrap_err();
    assert!(matches!(err, TeamError::TeamNotFound(_)));
}

#[tokio::test]
async fn list_team_activity_rejects_other_user() {
    let (svc, team_repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    team_repo
        .write_message("user1", &activity_message_row("m1", "t1", 1000))
        .await
        .unwrap();

    // Cross-user access is scoped away at the query layer: another user's team
    // is indistinguishable from a non-existent one (TeamNotFound), so existence
    // is never leaked. This matches the team-wide isolation model.
    let mailbox_err = svc.list_team_mailbox("intruder", "t1", 500).await.unwrap_err();
    assert!(matches!(mailbox_err, TeamError::TeamNotFound(_)));
    let tasks_err = svc.list_team_tasks("intruder", "t1", 500).await.unwrap_err();
    assert!(matches!(tasks_err, TeamError::TeamNotFound(_)));
}

// ── Unified paginated activity feed (list_team_activity) ──────────────

#[tokio::test]
async fn activity_all_merges_and_orders_desc_with_cursor() {
    let (svc, team_repo, _tm, _cr) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    // Interleaved timestamps: msg@1000,3000  task@2000,4000.
    for (id, ts) in [("m1", 1000), ("m2", 3000)] {
        team_repo
            .write_message("user1", &activity_message_row(id, "t1", ts))
            .await
            .unwrap();
    }
    for (id, ts) in [("k1", 2000), ("k2", 4000)] {
        team_repo
            .create_task("user1", &activity_task_row(id, "t1", ts))
            .await
            .unwrap();
    }

    let page = svc
        .list_team_activity(
            "user1",
            "t1",
            None,
            aionui_db::PageDirection::Desc,
            aionui_team::ActivityKind::All,
            3,
        )
        .await
        .unwrap();

    // desc top3: k2(4000), m2(3000), k1(2000)
    let ids: Vec<_> = page.items.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["k2", "m2", "k1"]);
    assert!(page.has_more); // m1 not yet fetched
    let c = page.next_cursor.unwrap();
    assert_eq!((c.ts, c.id.as_str()), (2000, "k1"));

    // Next page.
    let page2 = svc
        .list_team_activity(
            "user1",
            "t1",
            Some(aionui_db::ActivityCursor {
                created_at: c.ts,
                id: c.id,
            }),
            aionui_db::PageDirection::Desc,
            aionui_team::ActivityKind::All,
            3,
        )
        .await
        .unwrap();
    assert_eq!(page2.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["m1"]);
    assert!(!page2.has_more);
}

#[tokio::test]
async fn activity_kind_task_only_paginates_tasks() {
    let (svc, team_repo, _tm, _cr) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    team_repo
        .write_message("user1", &activity_message_row("m1", "t1", 5000))
        .await
        .unwrap();
    team_repo
        .create_task("user1", &activity_task_row("k1", "t1", 1000))
        .await
        .unwrap();

    let page = svc
        .list_team_activity(
            "user1",
            "t1",
            None,
            aionui_db::PageDirection::Desc,
            aionui_team::ActivityKind::Task,
            10,
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(matches!(page.items[0].kind, aionui_api_types::TeamActivityKind::Task));
}

#[tokio::test]
async fn activity_rejects_other_user() {
    let (svc, team_repo, _tm, _cr) = setup_with_factory_metadata_team_repo_and_conversation_repo(
        success_factory(),
        Arc::new(StubAgentMetadataRepo::empty()),
    );
    team_repo.create_team(&activity_team_row("t1", "user1")).await.unwrap();
    let err = svc
        .list_team_activity(
            "intruder",
            "t1",
            None,
            aionui_db::PageDirection::Desc,
            aionui_team::ActivityKind::All,
            10,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, TeamError::TeamNotFound(_)));
}
