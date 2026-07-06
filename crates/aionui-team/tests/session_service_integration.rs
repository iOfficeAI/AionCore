mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use aionui_ai_agent::session_context::{
    AcpSessionBuildContext, AgentSessionContext, AgentSessionKind, ConversationContext, WorkspaceContext,
};
use aionui_ai_agent::task_manager::AgentFactory;
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_ai_agent::{ActiveLeaseRegistry, AgentError, IWorkerTaskManager, WorkerTaskManagerImpl};
use aionui_api_types::{AcpBuildExtra, AddAgentRequest, CreateTeamRequest, TeamAgentInput, WebSocketMessage};
use aionui_common::{AgentKillReason, AgentType, PaginatedResult, ProviderWithModel};
use aionui_db::models::{
    AgentMetadataRow, AssistantDefinitionRow, AssistantOverlayRow, ConversationRow, MessageRow,
    UpdateAgentAvailabilitySnapshotParams, UpdateAgentHandshakeParams, UpsertAgentMetadataParams,
    UpsertAssistantDefinitionParams, UpsertAssistantOverlayParams,
};
use aionui_db::{
    ConversationFilters, ConversationRowUpdate, DbError, IAgentMetadataRepository, IAssistantDefinitionRepository,
    IAssistantOverlayRepository, IConversationRepository, IProviderRepository, ITeamRepository, MessagePageParams,
    MessagePageResult, MessageRowUpdate, MessageSearchRow,
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
}

impl MockConversationRepo {
    fn new() -> Self {
        Self {
            conversations: std::sync::Mutex::new(Vec::new()),
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
    async fn get(&self, id: &str) -> Result<Option<ConversationRow>, DbError> {
        let convs = self.conversations.lock().unwrap();
        Ok(convs.iter().find(|c| c.id == id).cloned())
    }
    async fn create(&self, row: &ConversationRow) -> Result<(), DbError> {
        self.conversations.lock().unwrap().push(row.clone());
        Ok(())
    }
    async fn update(&self, id: &str, updates: &ConversationRowUpdate) -> Result<(), DbError> {
        let mut convs = self.conversations.lock().unwrap();
        let conv = convs
            .iter_mut()
            .find(|c| c.id == id)
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
    async fn delete(&self, id: &str) -> Result<(), DbError> {
        self.conversations.lock().unwrap().retain(|c| c.id != id);
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
        _conv_id: &str,
        _params: &MessagePageParams,
    ) -> Result<MessagePageResult, DbError> {
        Ok(MessagePageResult {
            items: vec![],
            has_more_before: false,
            has_more_after: false,
        })
    }
    async fn insert_message(&self, _message: &MessageRow) -> Result<(), DbError> {
        Ok(())
    }
    async fn update_message(&self, _id: &str, _updates: &MessageRowUpdate) -> Result<(), DbError> {
        Ok(())
    }
    async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), DbError> {
        Ok(())
    }
    async fn get_message_by_msg_id(
        &self,
        _conv_id: &str,
        _msg_id: &str,
        _msg_type: &str,
    ) -> Result<Option<MessageRow>, DbError> {
        Ok(None)
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
                team_run_id: request.team_run_id.clone().expect("team run id"),
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
                team_run_id: request.team_run_id.clone().expect("team run id"),
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

    async fn create_team_temp_workspace(&self, team_id: &str) -> Result<String, aionui_team::TeamError> {
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
        self.repo
            .update(
                conversation_id,
                &ConversationRowUpdate {
                    name: None,
                    model: None,
                    pinned: None,
                    pinned_at: None,
                    extra: Some(serde_json::to_string(&extra).unwrap()),
                    status: None,
                    updated_at: Some(aionui_common::now_ms()),
                },
            )
            .await?;
        Ok(())
    }

    async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), aionui_team::TeamError> {
        self.patch_runtime_config(conversation_id, serde_json::json!({ "session_mode": mode }))
            .await
    }

    async fn warmup_agent_process(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), aionui_team::TeamError> {
        let row = self
            .repo
            .get(conversation_id)
            .await?
            .filter(|row| row.user_id == user_id)
            .ok_or_else(|| {
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
        self.repo.delete(conversation_id).await?;
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
        Ok(self
            .repo
            .get_message_by_msg_id(conversation_id, msg_id, msg_type)
            .await?)
    }

    async fn insert_projected_message(&self, row: &MessageRow) -> Result<(), aionui_team::TeamError> {
        self.repo.insert_message(row).await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl TeamConversationLookupPort for FakeConversationPorts {
    async fn lookup_team_binding_by_conversation(
        &self,
        conversation_id: &str,
    ) -> Result<Option<TeamConversationBindingLookup>, aionui_team::TeamError> {
        let Some(row) = self.repo.get(conversation_id).await? else {
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
    async fn list_teams(&self) -> Result<Vec<aionui_db::models::TeamRow>, DbError> {
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
    async fn get_team(&self, id: &str) -> Result<Option<aionui_db::models::TeamRow>, DbError> {
        Ok(self.teams.lock().unwrap().iter().find(|t| t.id == id).cloned())
    }
    async fn update_team(&self, id: &str, params: &aionui_db::UpdateTeamParams) -> Result<(), DbError> {
        if params.workspace.is_some() && *self.fail_workspace_update.lock().unwrap() {
            return Err(DbError::Init("forced workspace writeback failure".into()));
        }
        if params.agents.is_some() && *self.fail_agent_update.lock().unwrap() {
            return Err(DbError::Init("forced agent update failure".into()));
        }
        let mut teams = self.teams.lock().unwrap();
        let team = teams
            .iter_mut()
            .find(|t| t.id == id)
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
    async fn delete_team(&self, id: &str) -> Result<(), DbError> {
        self.teams.lock().unwrap().retain(|t| t.id != id);
        Ok(())
    }

    async fn write_message(&self, row: &aionui_db::models::MailboxMessageRow) -> Result<(), DbError> {
        if *self.fail_message_writes.lock().unwrap() {
            return Err(DbError::Init("forced mailbox write failure".into()));
        }
        self.inner.write_message(row).await
    }
    async fn read_unread_and_mark(
        &self,
        team_id: &str,
        to_agent_id: &str,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.read_unread_and_mark(team_id, to_agent_id).await
    }
    async fn peek_unread(
        &self,
        team_id: &str,
        to_agent_id: &str,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.peek_unread(team_id, to_agent_id).await
    }
    async fn mark_read_batch(&self, ids: &[String]) -> Result<(), DbError> {
        self.inner.mark_read_batch(ids).await
    }
    async fn get_history(
        &self,
        team_id: &str,
        to_agent_id: &str,
        limit: Option<i64>,
    ) -> Result<Vec<aionui_db::models::MailboxMessageRow>, DbError> {
        self.inner.get_history(team_id, to_agent_id, limit).await
    }
    async fn delete_mailbox_by_team(&self, team_id: &str) -> Result<(), DbError> {
        self.inner.delete_mailbox_by_team(team_id).await
    }

    async fn create_task(&self, row: &aionui_db::models::TeamTaskRow) -> Result<(), DbError> {
        self.inner.create_task(row).await
    }
    async fn find_task_by_id(
        &self,
        team_id: &str,
        task_id: &str,
    ) -> Result<Option<aionui_db::models::TeamTaskRow>, DbError> {
        self.inner.find_task_by_id(team_id, task_id).await
    }
    async fn update_task(&self, task_id: &str, params: &aionui_db::UpdateTaskParams) -> Result<(), DbError> {
        self.inner.update_task(task_id, params).await
    }
    async fn list_tasks(&self, team_id: &str) -> Result<Vec<aionui_db::models::TeamTaskRow>, DbError> {
        self.inner.list_tasks(team_id).await
    }
    async fn append_to_blocks(&self, task_id: &str, blocked_task_id: &str) -> Result<(), DbError> {
        self.inner.append_to_blocks(task_id, blocked_task_id).await
    }
    async fn remove_from_blocked_by(&self, task_id: &str, unblocked_task_id: &str) -> Result<(), DbError> {
        self.inner.remove_from_blocked_by(task_id, unblocked_task_id).await
    }
    async fn delete_tasks_by_team(&self, team_id: &str) -> Result<(), DbError> {
        self.inner.delete_tasks_by_team(team_id).await
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
    async fn get(&self, id: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(self.rows_by_id.get(id).cloned())
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
    async fn find_builtin_by_backend(&self, backend: &str) -> Result<Option<AgentMetadataRow>, DbError> {
        Ok(self.builtin_by_backend.get(backend).cloned())
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
        let _ = self.kill(conversation_id, reason);
        Box::pin(std::future::ready(()))
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
    async fn list_team_selectable_assistants(&self) -> Result<Vec<TeamAssistantCatalogEntry>, TeamError> {
        Ok(Vec::new())
    }
}

struct EmptyProviderRepo;

#[async_trait::async_trait]
impl IProviderRepository for EmptyProviderRepo {
    async fn list(&self) -> Result<Vec<aionui_db::models::Provider>, DbError> {
        Ok(vec![])
    }
    async fn find_by_id(&self, _id: &str) -> Result<Option<aionui_db::models::Provider>, DbError> {
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
        _id: &str,
        _params: aionui_db::UpdateProviderParams<'_>,
    ) -> Result<aionui_db::models::Provider, DbError> {
        Err(DbError::NotFound("not implemented".into()))
    }
    async fn delete(&self, _id: &str) -> Result<(), DbError> {
        Err(DbError::NotFound("not implemented".into()))
    }
}

struct EmptyAssistantDefinitionRepo;

#[async_trait::async_trait]
impl IAssistantDefinitionRepository for EmptyAssistantDefinitionRepo {
    async fn list(&self) -> Result<Vec<AssistantDefinitionRow>, DbError> {
        Ok(vec![])
    }

    async fn get_by_assistant_id(&self, _assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn get_by_id(&self, _definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn get_by_source_ref(
        &self,
        _source: &str,
        _source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn upsert(&self, _params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, DbError> {
        Ok(false)
    }
}

struct EmptyAssistantOverlayRepo;

#[async_trait::async_trait]
impl IAssistantOverlayRepository for EmptyAssistantOverlayRepo {
    async fn get(&self, _definition_id: &str) -> Result<Option<AssistantOverlayRow>, DbError> {
        Ok(None)
    }

    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
        Ok(vec![])
    }

    async fn upsert(&self, _params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn delete(&self, _definition_id: &str) -> Result<bool, DbError> {
        Ok(false)
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

    async fn get_by_assistant_id(&self, assistant_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok((self.row.assistant_id == assistant_id).then_some(self.row.clone()))
    }

    async fn get_by_id(&self, definition_id: &str) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok((self.row.id == definition_id).then_some(self.row.clone()))
    }

    async fn get_by_source_ref(
        &self,
        _source: &str,
        _source_ref: &str,
    ) -> Result<Option<AssistantDefinitionRow>, DbError> {
        Ok(None)
    }

    async fn upsert(&self, _params: &UpsertAssistantDefinitionParams<'_>) -> Result<AssistantDefinitionRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn soft_delete(&self, _definition_id: &str, _deleted_at: i64) -> Result<bool, DbError> {
        Ok(false)
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

    async fn list(&self) -> Result<Vec<AssistantOverlayRow>, DbError> {
        Ok(vec![self.row.clone()])
    }

    async fn upsert(&self, _params: &UpsertAssistantOverlayParams<'_>) -> Result<AssistantOverlayRow, DbError> {
        Err(DbError::Init("not implemented".into()))
    }

    async fn delete(&self, _definition_id: &str) -> Result<bool, DbError> {
        Ok(false)
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
    let svc = TeamSessionService::new(
        team_repo_dyn,
        agent_metadata_repo,
        Arc::new(EmptyTeamAssistantCatalog),
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
    let svc = TeamSessionService::new(
        team_repo_dyn,
        agent_metadata_repo,
        Arc::new(EmptyTeamAssistantCatalog),
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
async fn ensure_session_recovery_drain_runs_agent_turn_with_team_run_id() {
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
        .expect("stop auto-started session");

    team_repo
        .write_message(&aionui_db::models::MailboxMessageRow {
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
        })
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
    assert!(requests[0].team_run_id.is_some(), "recovery turn must be TeamRun-owned");
}

#[tokio::test]
async fn teammate_first_wake_uses_canonical_prompt_at_service_boundary() {
    let (svc, team_repo, turn_port, _conv_repo) = setup_with_recording_turn_port();
    let created = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "Recover Teammate".into(),
                agents: two_agent_input(),
                workspace: None,
            },
        )
        .await
        .expect("create team");
    let worker_slot_id = created.assistants[1].slot_id.clone();
    svc.stop_session("user1", &created.id)
        .await
        .expect("stop auto-started session");

    team_repo
        .write_message(&aionui_db::models::MailboxMessageRow {
            id: "mailbox-worker-1".into(),
            team_id: created.id.clone(),
            to_agent_id: worker_slot_id.clone(),
            from_agent_id: "user".into(),
            msg_type: "message".into(),
            content: "do X".into(),
            summary: None,
            files: None,
            read: false,
            created_at: aionui_common::now_ms(),
        })
        .await
        .expect("seed teammate mailbox");

    svc.ensure_session("user1", &created.id).await.expect("ensure");

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
        .expect("stop auto-started session");

    team_repo
        .write_message(&aionui_db::models::MailboxMessageRow {
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
        })
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

fn make_agent_metadata_row(id: &str, backend: &str, icon: &str) -> AgentMetadataRow {
    AgentMetadataRow {
        id: id.to_owned(),
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
        source_version: None,
        source_hash: None,
        name: "Word Creator".into(),
        name_i18n: "{}".into(),
        description: Some("Drafts Word documents".into()),
        description_i18n: "{}".into(),
        avatar_type: "builtin_asset".into(),
        avatar_value: None,
        agent_id: "claude".into(),
        rule_resource_type: "inline".into(),
        rule_resource_ref: None,
        rule_inline_content: None,
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

async fn reset_auto_started_session(svc: &Arc<TeamSessionService>, tm: &Arc<CountingTaskManager>, team_id: &str) {
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

    assert!(matches!(err, TeamError::Forbidden(_)));
    for agent in &created.assistants {
        assert!(!active_leases.is_active(&agent.conversation_id));
    }
}

async fn force_team_workspace(repo: &Arc<FullMockTeamRepo>, team_id: &str, workspace: &str) {
    repo.update_team(
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
            source_version: None,
            source_hash: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "builtin_asset".into(),
            avatar_value: Some("avatars/assistant-lead.png".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
            source_version: None,
            source_hash: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
        .get(&resp.assistants[0].conversation_id)
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
            source_version: None,
            source_hash: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
            source_version: None,
            source_hash: None,
            name: "Lead Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
                model: Some("claude-sonnet-4".into()),
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
            source_version: None,
            source_hash: None,
            name: "Worker Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
            source_version: None,
            source_hash: None,
            name: "Worker Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
async fn tc4_first_agent_is_lead() {
    let svc = setup();
    let resp = svc
        .create_team(
            "user1",
            CreateTeamRequest {
                name: "T".into(),
                agents: vec![
                    TeamAgentInput {
                        name: "A".into(),
                        role: "teammate".into(),
                        backend: Some("acp".into()),
                        model: "claude".into(),
                        assistant_id: None,
                        conversation_id: None,
                    },
                    TeamAgentInput {
                        name: "B".into(),
                        role: "teammate".into(),
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

    assert_eq!(resp.assistants[0].role, "lead");
    assert_eq!(resp.leader_assistant_id, Some(resp.assistants[0].slot_id.clone()));
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

    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
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

    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
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
        .get(&created.assistants[0].conversation_id)
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
            source_version: None,
            source_hash: None,
            name: "Worker Assistant".into(),
            name_i18n: "{}".into(),
            description: None,
            description_i18n: "{}".into(),
            avatar_type: "emoji".into(),
            avatar_value: Some("🤖".into()),
            agent_id: "claude".into(),
            rule_resource_type: "inline".into(),
            rule_resource_ref: None,
            rule_inline_content: None,
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
        model: Some("claude-sonnet-4".into()),
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
async fn lead_send_agent_message_in_session_requires_active_team_run() {
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
        .expect("session should be loaded without active Team Run");

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

    let err = svc
        .send_agent_message_from_agent(&created.id, &lead_slot_id, &worker_slot_id, "Do this")
        .await
        .expect_err("leader direct message should require active Team Run");
    assert!(err.to_string().contains("no active team run"));
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
        model: Some("claude-sonnet-4".into()),
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
        model: Some("claude-sonnet-4".into()),
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

    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
}

// -- W5-D31b-2: team.mcpStatus service-layer broadcasts ---------------------
//
// The happy-path phase transitions are covered by focused service/session
// assertions. This test keeps the load-failed broadcast covered end-to-end.

#[tokio::test]
async fn d31b2_ensure_session_broadcasts_load_failed_for_missing_team() {
    let (svc, recorder) = setup_with_recording_broadcaster();
    let err = svc.ensure_session("user1", "nonexistent-team-xyz").await.unwrap_err();
    assert!(matches!(err, aionui_team::TeamError::TeamNotFound(_)));

    let load_failed = recorder
        .events_by_name("team.mcpStatus")
        .into_iter()
        .find(|e| {
            e.data
                .get("phase")
                .and_then(|v| v.as_str())
                .map(|s| s == "load_failed")
                .unwrap_or(false)
        })
        .expect("load_failed broadcast expected");
    assert_eq!(
        load_failed.data.get("team_id").and_then(|v| v.as_str()),
        Some("nonexistent-team-xyz")
    );
    assert!(load_failed.data.get("error").is_some());
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

    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
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

    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
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

    assert!(matches!(result, Err(aionui_team::TeamError::Forbidden(_))));
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
async fn d9_create_team_auto_start_rebuilds_every_initial_agent() {
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
        calls.kill.len(),
        2,
        "auto-start should kill every initial agent before rebuild"
    );
    assert_eq!(calls.build.len(), 2, "auto-start should warm every initial agent");
    for (i, agent) in created.assistants.iter().enumerate() {
        assert_eq!(calls.kill[i].0, agent.conversation_id);
        assert_eq!(calls.kill[i].1, Some(AgentKillReason::TeamMcpRebuild));
        assert_eq!(calls.build[i], agent.conversation_id);
    }
    assert_eq!(tm.active_count(), 2, "auto-start must register every initial agent");
}

#[tokio::test]
async fn d9_ensure_session_kills_and_rebuilds_every_agent() {
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

    reset_auto_started_session(&svc, &tm, &created.id).await;
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Two agents → kill called 2x and get_or_build_task called 2x, each with
    // the corresponding conversation_id. Order is agents-iteration order.
    let calls = tm.snapshot();
    assert_eq!(calls.kill.len(), 2, "expected 2 kill calls");
    assert_eq!(calls.build.len(), 2, "expected 2 build calls");
    for (i, agent) in created.assistants.iter().enumerate() {
        assert_eq!(calls.kill[i].0, agent.conversation_id);
        assert_eq!(calls.kill[i].1, Some(AgentKillReason::TeamMcpRebuild));
        assert_eq!(calls.build[i], agent.conversation_id);
    }
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
                agents: two_agent_input(),
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

    reset_auto_started_session(&svc, &tm, &created.id).await;
    svc.ensure_session("user1", &created.id).await.unwrap();
    svc.ensure_session("user1", &created.id).await.unwrap();

    // Second call short-circuits — no additional kill/build calls.
    let calls = tm.snapshot();
    assert_eq!(calls.kill.len(), 2, "second ensure_session must not re-kill");
    assert_eq!(calls.build.len(), 2, "second ensure_session must not re-build");
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

    reset_auto_started_session(&svc, &tm, &created.id).await;
    let result = svc.ensure_session("user1", &created.id).await;
    assert!(result.is_err(), "ensure_session should propagate build error");

    // Rebuild aborts on the first warmup failure, so only the first agent
    // is killed/built. No session is inserted, so send_message still errors.
    let calls = tm.snapshot();
    assert_eq!(calls.kill.len(), 1);
    assert_eq!(calls.build.len(), 1);

    let send_result = svc.send_message("user1", &created.id, "Hello", None).await;
    assert!(
        send_result.is_err(),
        "session must not be registered after build failure"
    );
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

    reset_auto_started_session(&svc, &tm, &created.id).await;
    // Bring two agents online — after ensure_session, active_count == 2.
    svc.ensure_session("user1", &created.id).await.unwrap();
    assert_eq!(tm.active_count(), 2, "ensure_session must register 2 live agents");

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
