use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use aionui_ai_agent::session_context::{AgentSessionContext, AgentSessionKind};
use aionui_ai_agent::types::BuildTaskOptions;
use aionui_ai_agent::{
    ActiveLeaseRegistry, AgentAvailabilityFeedbackPort, AgentError, AgentInstance, AgentSendError, IWorkerTaskManager,
};

use crate::message_cursor::{decode_message_cursor, encode_message_cursor};
use crate::runtime_completion::RuntimeCompletionPublisher;
use crate::runtime_persistence::{RuntimePersistenceCoordinator, RuntimeWriteKind};
use crate::runtime_state::ConversationRuntimeStateService;
use aionui_api_types::{
    ApprovalCheckResponse, AssistantConversationOverridesRequest, CancelConversationResponse, CloneConversationRequest,
    ConfirmRequest, ConfirmationListResponse, ConversationArtifactKind, ConversationArtifactListResponse,
    ConversationArtifactResponse, ConversationArtifactStatus, ConversationListResponse, ConversationMcpStatus,
    ConversationMcpStatusKind, ConversationResponse, ConversationRuntimeSummary, CreateConversationRequest,
    EnsureConversationRuntimeResponse, ListConversationsQuery, ListMessagesQuery, MessageListResponse, MessageResponse,
    MessageSearchResponse, SearchMessagesQuery, SendMessageRequest, SendMessageResponse, SessionMcpServer,
    SessionMcpTransport, TeamSessionBinding, UpdateConversationArtifactRequest, UpdateConversationRequest,
    WebSocketMessage, assistant_avatar_response_value, assistant_avatar_response_value_with_version,
};
use aionui_common::{
    AgentKillReason, AgentType, ConversationSource, ConversationStatus, ErrorChain, MessageType, OnConversationDelete,
    PaginatedResult, WorkspacePathValidationError, generate_short_id, now_ms, validate_workspace_path_availability,
};
use aionui_db::models::{AssistantDefinitionRow, ConversationAssistantSnapshotRow, ConversationRow, MessageRow};
use aionui_db::{
    AgentBindingResolution, ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, IAcpSessionRepository,
    IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IAssistantPreferenceRepository, IConversationRepository, IMcpServerRepository, MessagePageCursor,
    MessagePageDirection, MessagePageParams, SaveRuntimeStateParams, UpsertConversationAssistantSnapshotParams,
    resolve_agent_binding_from_rows,
};
use aionui_extension::AssistantRuleDispatcher;
use aionui_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{RuntimeCommandProbe, probe_node_runtime_supported, probe_runtime_command, resolve_command_path};
use chrono::Datelike;
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tracing::{debug, error, info, warn};

use crate::convert::{
    TOOL_CONTENT_COMPACT_THRESHOLD_BYTES, row_to_artifact_response, row_to_message_response,
    row_to_message_response_compact, row_to_response, row_to_response_with_extra, search_row_to_item, string_to_enum,
};
use crate::error::ConversationError;
use crate::session_context::SessionContextBuilder;
use crate::skill_resolver::SkillResolver;
use crate::skill_snapshot::{backfill_skills_if_missing, compute_initial_skills};
use crate::turn_orchestrator::{ConversationTurnOrchestrator, ConversationTurnStatus, TurnStartInput};
use std::sync::RwLock;

pub(crate) const MAX_SYSTEM_RESPONSE_CONTINUATIONS_PER_TURN: usize = 4;
const ACP_CANCEL_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
const LEGACY_CONVERSATION_ARCHIVED_MESSAGE: &str =
    "This historical conversation can no longer be continued. Please start a new conversation.";
const DEPRECATED_AGENT_TYPE_MESSAGE: &str = "This agent type is no longer supported for new conversations.";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct AssistantConversationOverrides {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    permission: Option<String>,
    #[serde(default)]
    thought_level: Option<String>,
    #[serde(default)]
    skill_ids: Option<Vec<String>>,
    #[serde(default)]
    disabled_builtin_skill_ids: Option<Vec<String>>,
    #[serde(default)]
    mcp_ids: Option<Vec<String>>,
}

impl From<AssistantConversationOverridesRequest> for AssistantConversationOverrides {
    fn from(value: AssistantConversationOverridesRequest) -> Self {
        Self {
            model: value.model,
            permission: value.permission,
            thought_level: value.thought_level,
            skill_ids: value.skill_ids,
            disabled_builtin_skill_ids: value.disabled_builtin_skill_ids,
            mcp_ids: value.mcp_ids,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AssistantSnapshotResolvedDefaults {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    permission: Option<String>,
    #[serde(default)]
    thought_level: Option<String>,
    #[serde(default)]
    skill_ids: Vec<String>,
    #[serde(default)]
    disabled_builtin_skill_ids: Vec<String>,
    #[serde(default)]
    mcp_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct AssistantSnapshotDefaultModes {
    #[serde(default)]
    model: String,
    #[serde(default)]
    permission: String,
    #[serde(default)]
    thought_level: String,
    #[serde(default)]
    skills: String,
    #[serde(default)]
    mcps: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AssistantSnapshotRules {
    content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct AssistantSnapshot {
    assistant_definition_id: String,
    assistant_id: String,
    assistant_source: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    avatar_type: String,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    agent_id: String,
    #[serde(default, deserialize_with = "deserialize_string_or_null")]
    agent_source: String,
    #[serde(default, alias = "agent_backend", deserialize_with = "deserialize_string_or_null")]
    runtime_backend: String,
    #[serde(default = "default_assistant_snapshot_agent_type")]
    agent_type: AgentType,
    rules: AssistantSnapshotRules,
    #[serde(default)]
    default_modes: AssistantSnapshotDefaultModes,
    resolved_defaults: AssistantSnapshotResolvedDefaults,
    created_at: i64,
}

fn deserialize_string_or_null<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(<Option<String> as serde::Deserialize>::deserialize(deserializer)?.unwrap_or_default())
}

fn default_assistant_snapshot_agent_type() -> AgentType {
    AgentType::Acp
}

#[derive(Debug, Clone, Copy)]
struct AssistantEffectiveDefaultModes<'a> {
    model: &'a str,
    permission: &'a str,
    thought_level: &'a str,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct AssistantRuntimePreferenceUpdate<'a> {
    pub(crate) model: Option<&'a str>,
    pub(crate) permission: Option<&'a str>,
    pub(crate) thought_level: Option<&'a str>,
}

fn assistant_snapshot_modes<'a>(
    snapshot: &'a AssistantSnapshot,
    definition: &'a aionui_db::AssistantDefinitionRow,
) -> AssistantEffectiveDefaultModes<'a> {
    AssistantEffectiveDefaultModes {
        model: if snapshot.default_modes.model.is_empty() {
            definition.default_model_mode.as_str()
        } else {
            snapshot.default_modes.model.as_str()
        },
        permission: if snapshot.default_modes.permission.is_empty() {
            definition.default_permission_mode.as_str()
        } else {
            snapshot.default_modes.permission.as_str()
        },
        thought_level: if snapshot.default_modes.thought_level.is_empty() {
            definition.default_thought_level_mode.as_str()
        } else {
            snapshot.default_modes.thought_level.as_str()
        },
    }
}

fn parse_agent_type_from_metadata(value: &str) -> Result<AgentType, ConversationError> {
    let quoted = format!("\"{}\"", value.trim());
    serde_json::from_str::<AgentType>(&quoted).map_err(|_| ConversationError::BadRequest {
        reason: format!("unsupported assistant agent type in agent_metadata: {value}"),
    })
}

fn resolve_create_agent_type(
    explicit_type: Option<AgentType>,
    assistant_snapshot: Option<&AssistantSnapshot>,
) -> Result<AgentType, ConversationError> {
    if let Some(snapshot) = assistant_snapshot {
        let derived = snapshot.agent_type;
        if let Some(explicit) = explicit_type
            && explicit != derived
        {
            warn!(
                explicit_type = explicit.serde_name(),
                derived_type = derived.serde_name(),
                backend = snapshot.runtime_backend,
                assistant_id = snapshot.assistant_id,
                "assistant-backed create request carried a mismatched explicit type; using assistant-derived type"
            );
        }
        return Ok(derived);
    }

    explicit_type.ok_or_else(|| ConversationError::BadRequest {
        reason: "Either `type` or `assistant.id` is required when creating a conversation.".into(),
    })
}

#[derive(Debug, Clone, Copy)]
struct McpSupportPolicy {
    stdio: bool,
    http: bool,
    sse: bool,
    streamable_http: bool,
}

impl McpSupportPolicy {
    const AIONRS: Self = Self {
        stdio: true,
        http: true,
        sse: true,
        streamable_http: true,
    };

    fn from_acp_capabilities(capabilities: AcpMcpCapabilities) -> Self {
        Self {
            stdio: capabilities.stdio,
            http: capabilities.http,
            sse: capabilities.sse,
            streamable_http: capabilities.http,
        }
    }

    fn supports_row_transport(self, transport_type: &str) -> bool {
        match transport_type {
            "stdio" => self.stdio,
            "http" => self.http,
            "sse" => self.sse,
            "streamable_http" => self.streamable_http,
            _ => false,
        }
    }

    fn supports_session_transport(self, transport: &SessionMcpTransport) -> bool {
        match transport {
            SessionMcpTransport::Stdio { .. } => self.stdio,
            SessionMcpTransport::Http { .. } => self.http,
            SessionMcpTransport::Sse { .. } => self.sse,
            SessionMcpTransport::StreamableHttp { .. } => self.streamable_http,
        }
    }
}

fn parse_agent_type_from_row(row: &ConversationRow) -> Option<AgentType> {
    serde_json::from_value::<AgentType>(serde_json::Value::String(row.r#type.clone())).ok()
}

fn reject_deprecated_runtime_row(row: &ConversationRow) -> Result<(), ConversationError> {
    let Some(agent_type) = parse_agent_type_from_row(row) else {
        return Ok(());
    };

    if agent_type.is_deprecated_runtime() {
        debug!(
            conversation_id = %row.id,
            agent_type = agent_type.serde_name(),
            "Rejected deprecated runtime conversation"
        );
        return Err(ConversationError::Archived {
            id: row.id.clone(),
            reason: LEGACY_CONVERSATION_ARCHIVED_MESSAGE.into(),
        });
    }

    Ok(())
}

#[derive(Clone)]
pub struct ConversationService {
    workspace_root: PathBuf,
    broadcaster: Arc<dyn EventBroadcaster>,
    skill_resolver: Arc<dyn SkillResolver>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    /// Hooks invoked during `delete()` before the DB row is removed so other services
    /// (`WorkerTaskManagerImpl`, `CronService`, …) can clean up their
    /// per-conversation state. Wrapped in `Arc<RwLock<…>>` so registration
    /// can happen post-construction without breaking the `Clone` impl.
    delete_hooks: Arc<RwLock<Vec<Arc<dyn OnConversationDelete>>>>,
    mcp_server_repo: Arc<RwLock<Option<Arc<dyn IMcpServerRepository>>>>,
    assistant_definition_repo: Arc<RwLock<Option<Arc<dyn IAssistantDefinitionRepository>>>>,
    assistant_state_repo: Arc<RwLock<Option<Arc<dyn IAssistantOverlayRepository>>>>,
    assistant_preference_repo: Arc<RwLock<Option<Arc<dyn IAssistantPreferenceRepository>>>>,
    assistant_dispatcher: Arc<RwLock<Option<Arc<dyn AssistantRuleDispatcher>>>>,
    agent_availability_feedback: Arc<RwLock<Option<Arc<dyn AgentAvailabilityFeedbackPort>>>>,
    runtime_state: Arc<ConversationRuntimeStateService>,
    runtime_helper_bin: Option<String>,
    runtime_base_url: Option<String>,

    // Repos for conversation, acp_session and agent_metadata access.
    conversation_repo: Arc<dyn IConversationRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    acp_session_repo: Arc<dyn IAcpSessionRepository>,
}

#[derive(Clone)]
pub struct ConversationAgentTurnRequest {
    pub user_id: String,
    pub conversation_id: String,
    pub content: String,
    pub files: Vec<String>,
    pub inject_skills: Vec<String>,
    pub required_runtime_mode: Option<String>,
    pub persist_user_message: bool,
    pub user_message_hidden: bool,
    pub on_started: Option<ConversationAgentTurnStartedCallback>,
}

pub type ConversationAgentTurnStartedCallback =
    Arc<dyn Fn(ConversationAgentTurnStarted) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationAgentTurnStarted {
    pub conversation_id: String,
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationAgentTurnStatus {
    Completed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct ConversationAgentTurnOutcome {
    pub conversation_id: String,
    pub turn_id: String,
    pub status: ConversationAgentTurnStatus,
    pub error_message: Option<String>,
    pub runtime: ConversationRuntimeSummary,
}

// ── Construction & Dependency Injection ──────────────────────────────

impl ConversationService {
    pub fn new(
        workspace_root: PathBuf,
        broadcaster: Arc<dyn EventBroadcaster>,
        skill_resolver: Arc<dyn SkillResolver>,
        task_manager: Arc<dyn IWorkerTaskManager>,

        conversation_repo: Arc<dyn IConversationRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        acp_session_repo: Arc<dyn IAcpSessionRepository>,
    ) -> Self {
        Self {
            workspace_root,
            broadcaster,
            skill_resolver,
            task_manager,
            delete_hooks: Arc::new(RwLock::new(Vec::new())),
            mcp_server_repo: Arc::new(RwLock::new(None)),
            assistant_definition_repo: Arc::new(RwLock::new(None)),
            assistant_state_repo: Arc::new(RwLock::new(None)),
            assistant_preference_repo: Arc::new(RwLock::new(None)),
            assistant_dispatcher: Arc::new(RwLock::new(None)),
            agent_availability_feedback: Arc::new(RwLock::new(None)),
            runtime_state: Arc::new(ConversationRuntimeStateService::default()),
            runtime_helper_bin: None,
            runtime_base_url: None,

            conversation_repo,
            agent_metadata_repo,
            acp_session_repo,
        }
    }

    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = runtime_state;
        self
    }

    pub fn with_runtime_helper_context(mut self, helper_bin: String, base_url: String) -> Self {
        self.runtime_helper_bin = Some(helper_bin);
        self.runtime_base_url = Some(base_url);
        self
    }

    pub fn create_team_temp_workspace(&self, team_id: &str) -> Result<String, ConversationError> {
        let ws_path = auto_workspace_parent(&self.workspace_root).join(format!("team-temp-{team_id}"));
        std::fs::create_dir_all(&ws_path)
            .map_err(|e| ConversationError::internal(format!("Failed to create Team temporary workspace: {e}")))?;
        Ok(ws_path.to_string_lossy().into_owned())
    }

    pub fn with_mcp_server_repo(&self, repo: Arc<dyn IMcpServerRepository>) {
        if let Ok(mut guard) = self.mcp_server_repo.write() {
            *guard = Some(repo);
        }
    }

    pub fn with_assistant_definition_repo(&self, repo: Arc<dyn IAssistantDefinitionRepository>) {
        if let Ok(mut guard) = self.assistant_definition_repo.write() {
            *guard = Some(repo);
        }
    }

    pub fn with_assistant_state_repo(&self, repo: Arc<dyn IAssistantOverlayRepository>) {
        if let Ok(mut guard) = self.assistant_state_repo.write() {
            *guard = Some(repo);
        }
    }

    pub fn with_assistant_preference_repo(&self, repo: Arc<dyn IAssistantPreferenceRepository>) {
        if let Ok(mut guard) = self.assistant_preference_repo.write() {
            *guard = Some(repo);
        }
    }

    pub fn with_assistant_dispatcher(&self, dispatcher: Arc<dyn AssistantRuleDispatcher>) {
        if let Ok(mut guard) = self.assistant_dispatcher.write() {
            *guard = Some(dispatcher);
        }
    }

    pub fn with_agent_availability_feedback(&self, feedback: Arc<dyn AgentAvailabilityFeedbackPort>) {
        if let Ok(mut guard) = self.agent_availability_feedback.write() {
            *guard = Some(feedback);
        }
    }

    /// Register a hook to be notified when a conversation is deleted.
    ///
    /// Hooks are dispatched sequentially in registration order before
    /// `delete()` removes the conversation row. Used by `aionui-app` to wire up `WorkerTaskManagerImpl`
    /// (kill the agent process) and `CronService` (cascade-delete cron jobs).
    pub fn with_delete_hook(&self, hook: Arc<dyn OnConversationDelete>) {
        if let Ok(mut guard) = self.delete_hooks.write() {
            guard.push(hook);
        }
    }

    /// The single source of truth for `msg_id` values across the backend.
    ///
    /// Every `msg_id` — user message id, assistant message id, cron/tips WS
    /// event id, agent correlation id (`SendMessageData.msg_id`), etc. — must
    /// be produced here. This keeps the ID space uniform and prevents
    /// downstream modules from accidentally forking their own format.
    ///
    /// The value is purely functional (no state), exposed as an associated
    /// function so callers that hold only `ConversationService::mint_msg_id`
    /// (or none of the service at all, via re-export) can use it.
    pub fn mint_msg_id() -> String {
        generate_short_id()
    }

    pub fn mint_turn_id() -> String {
        format!("turn_{}", generate_short_id())
    }

    pub fn conversation_repo(&self) -> &Arc<dyn IConversationRepository> {
        &self.conversation_repo
    }

    pub(crate) fn broadcaster(&self) -> &Arc<dyn EventBroadcaster> {
        &self.broadcaster
    }

    pub(crate) fn acp_session_repo(&self) -> &Arc<dyn IAcpSessionRepository> {
        &self.acp_session_repo
    }

    pub fn runtime_state(&self) -> Arc<ConversationRuntimeStateService> {
        self.runtime_state.clone()
    }

    pub fn auto_workspace_to_delete_for_row(
        &self,
        row: &aionui_db::models::ConversationRow,
        conversation_id: &str,
    ) -> Option<PathBuf> {
        auto_provisioned_workspace_to_delete(&self.workspace_root, row, conversation_id)
    }

    fn assistant_definition_repo(&self) -> Option<Arc<dyn IAssistantDefinitionRepository>> {
        self.assistant_definition_repo
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    fn assistant_state_repo(&self) -> Option<Arc<dyn IAssistantOverlayRepository>> {
        self.assistant_state_repo
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    fn assistant_preference_repo(&self) -> Option<Arc<dyn IAssistantPreferenceRepository>> {
        self.assistant_preference_repo
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    fn assistant_dispatcher(&self) -> Option<Arc<dyn AssistantRuleDispatcher>> {
        self.assistant_dispatcher
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    pub(crate) fn agent_availability_feedback(&self) -> Option<Arc<dyn AgentAvailabilityFeedbackPort>> {
        self.agent_availability_feedback
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    pub(crate) fn runtime_persistence(&self) -> RuntimePersistenceCoordinator {
        RuntimePersistenceCoordinator::new(self.runtime_state())
    }

    pub(crate) fn completion_publisher(&self) -> RuntimeCompletionPublisher {
        RuntimeCompletionPublisher::new(
            self.conversation_repo.clone(),
            self.broadcaster.clone(),
            self.runtime_persistence(),
        )
    }

    pub(crate) fn task(&self, conversation_id: &str) -> Result<AgentInstance, ConversationError> {
        self.task_manager
            .get_task(conversation_id)
            .ok_or_else(|| ConversationError::ActiveAgentNotFound {
                conversation_id: conversation_id.to_owned(),
            })
    }

    pub(crate) fn task_manager(&self) -> &Arc<dyn IWorkerTaskManager> {
        &self.task_manager
    }

    pub async fn runtime_summary_for(&self, conversation_id: &str) -> ConversationRuntimeSummary {
        let agent = self.task_manager.get_task(conversation_id);
        let has_task = agent.is_some();
        let task_status = agent.as_ref().and_then(|agent| agent.status());
        let pending_confirmations = agent.as_ref().map(|agent| agent.get_confirmations().len()).unwrap_or(0);

        self.runtime_state
            .summary_from_parts(conversation_id, task_status, has_task, pending_confirmations)
    }

    async fn send_message_response(
        &self,
        conversation_id: &str,
        msg_id: String,
        turn_id: String,
    ) -> SendMessageResponse {
        SendMessageResponse {
            msg_id,
            turn_id,
            runtime: self.runtime_summary_for(conversation_id).await,
        }
    }

    pub async fn complete_turn(&self, conversation_id: &str, turn_id: &str) {
        let runtime = self.runtime_summary_for(conversation_id).await;
        self.completion_publisher()
            .publish(conversation_id, turn_id, Some(runtime))
            .await;
    }

    pub(crate) async fn complete_released_turn(&self, conversation_id: &str, turn_id: &str, was_deleting: bool) {
        if was_deleting {
            debug!(
                conversation_id,
                turn_id, "Skipping turn completion because conversation was deleting at claim release"
            );
            return;
        }

        self.complete_turn(conversation_id, turn_id).await;
    }
}

// ── Conversation CRUD ───────────────────────────────────────────────

impl ConversationService {
    async fn attach_assistant_identity(&self, response: &mut ConversationResponse) -> Result<(), ConversationError> {
        if response.assistant.is_some() {
            return Ok(());
        }

        if let Some(snapshot) = self.conversation_repo.get_assistant_snapshot(&response.id).await? {
            response.assistant = Some(self.assistant_identity_from_snapshot(&snapshot).await?);
        }

        Ok(())
    }

    async fn assistant_identity_from_snapshot(
        &self,
        snapshot: &ConversationAssistantSnapshotRow,
    ) -> Result<aionui_api_types::ConversationAssistantIdentityResponse, ConversationError> {
        let runtime_backend = self
            .resolve_assistant_agent_binding(&snapshot.agent_id)
            .await?
            .map(|binding| binding.runtime_backend)
            .unwrap_or_else(|| snapshot.agent_id.clone());
        let current_definition = self.current_assistant_definition(&snapshot.assistant_id).await?;
        let (source, name, avatar) = match current_definition {
            Some(definition) => (
                definition.source,
                definition.name,
                assistant_avatar_response_value_with_version(
                    definition.avatar_type.as_str(),
                    definition.avatar_value.as_deref(),
                    definition.assistant_id.as_str(),
                    definition.updated_at,
                )
                .unwrap_or_default(),
            ),
            None => (
                snapshot.assistant_source.clone(),
                snapshot.assistant_id.clone(),
                String::new(),
            ),
        };

        Ok(aionui_api_types::ConversationAssistantIdentityResponse {
            id: snapshot.assistant_id.clone(),
            source,
            name,
            avatar,
            backend: runtime_backend,
        })
    }

    async fn current_assistant_definition(
        &self,
        assistant_id: &str,
    ) -> Result<Option<AssistantDefinitionRow>, ConversationError> {
        let Some(definition_repo) = self.assistant_definition_repo() else {
            return Ok(None);
        };
        definition_repo
            .get_by_assistant_id(assistant_id)
            .await
            .map_err(|e| ConversationError::internal(format!("assistant definition lookup failed: {e}")))
    }

    /// Create a new conversation.
    ///
    /// Generates a UUID v7, sets status to `pending`, defaults source
    /// to `aionui`, and broadcasts `conversation.listChanged(created)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, req_type = ?req.r#type))]
    pub async fn create(
        &self,
        user_id: &str,
        req: CreateConversationRequest,
    ) -> Result<ConversationResponse, ConversationError> {
        let id = generate_short_id();
        let now = now_ms();
        let source = req.source.unwrap_or(ConversationSource::Aionui);

        let mut extra = req.extra;

        let assistant_id = req
            .assistant
            .as_ref()
            .map(|assistant| assistant.id.clone())
            .or_else(|| {
                extra
                    .as_object()
                    .and_then(|obj| obj.get("preset_assistant_id"))
                    .and_then(|value| value.as_str().map(ToOwned::to_owned))
            });
        let assistant_locale = req.assistant.as_ref().and_then(|assistant| assistant.locale.clone());
        let assistant_overrides = req
            .assistant
            .clone()
            .and_then(|assistant| assistant.conversation_overrides)
            .map(AssistantConversationOverrides::from)
            .unwrap_or_default();
        let assistant_snapshot = match assistant_id.as_deref() {
            Some(id) => {
                self.resolve_assistant_snapshot(id, assistant_locale.as_deref(), &assistant_overrides, &extra)
                    .await?
            }
            None => None,
        };
        let explicit_type = req.r#type;
        let effective_type = resolve_create_agent_type(explicit_type, assistant_snapshot.as_ref())?;

        if !effective_type.supports_new_conversation() {
            info!(
                agent_type = effective_type.serde_name(),
                source = ?source,
                "Rejected deprecated agent type for new conversation"
            );
            return Err(ConversationError::BadRequest {
                reason: DEPRECATED_AGENT_TYPE_MESSAGE.into(),
            });
        }

        // Type-aware rule: top-level `model` is aionrs-only. Other agent types
        // carry model/mode via `extra` (see spec 2026-05-12). Reject early so
        // clients that still ship the legacy shape get a loud 400 instead of
        // a silent write to a column nobody reads.
        if effective_type != AgentType::Aionrs && req.model.is_some() {
            return Err(ConversationError::BadRequest {
                reason: format!(
                    "top-level `model` is only accepted for aionrs conversations; pass model via `extra` for {}",
                    effective_type.serde_name()
                ),
            });
        }

        // aionrs source-of-truth rule: top-level `model` wins. If an older client
        // still packs `extra.model`, strip it before persist so the stored row
        // has a single canonical model representation.
        if effective_type == AgentType::Aionrs
            && let Some(obj) = extra.as_object_mut()
            && obj.remove("model").is_some()
        {
            warn!("aionrs create: stripped legacy `extra.model`; top-level `model` is canonical");
        }

        // Determine whether the user chose this workspace ("custom") or we
        // auto-provision one under
        // `{data_dir}/conversations/YYYY/MM/DD/{label}-temp-{id}/`.
        // Skill wiring runs for both kinds so native CLI discovery behaves
        // consistently.
        let user_supplied_workspace = match extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(workspace) => Some(normalize_workspace_path(workspace)?),
            None => None,
        };
        if let Some(workspace) = user_supplied_workspace.as_ref() {
            extra["workspace"] = serde_json::Value::String(workspace.clone());
        }

        let assistant_backend = assistant_snapshot
            .as_ref()
            .map(|snapshot| snapshot.runtime_backend.clone())
            .filter(|backend| !backend.is_empty());
        let effective_backend = assistant_backend.or_else(|| {
            extra
                .get("backend")
                .and_then(|v| v.as_str())
                .filter(|backend| !backend.is_empty())
                .map(str::to_owned)
        });

        let auto_provisioned_workspace = if user_supplied_workspace.is_none() {
            // Per-conversation temp workspaces live under
            // `{data_dir}/conversations/YYYY/MM/DD/{label}-temp-{id}/`.
            // The label lets operators eyeball the agent type; the
            // conversation id keeps the mapping back to the DB row unique.
            let label = conversation_label(
                &effective_type,
                effective_backend
                    .as_ref()
                    .map(|backend| serde_json::Value::String(backend.clone()))
                    .as_ref(),
            );
            let ws_path = auto_workspace_parent(&self.workspace_root).join(format!("{label}-temp-{id}"));
            std::fs::create_dir_all(&ws_path)
                .map_err(|e| ConversationError::internal(format!("Failed to create workspace: {e}")))?;
            extra["workspace"] = serde_json::Value::String(ws_path.to_string_lossy().into_owned());
            Some(ws_path)
        } else {
            None
        };

        // Strip the request-only custom_workspace toggle — it was read above
        // and must not be persisted as an extra field.
        if let Some(obj) = extra.as_object_mut() {
            obj.remove("custom_workspace");
        }

        if let Some(snapshot) = assistant_snapshot.as_ref()
            && let Some(obj) = extra.as_object_mut()
        {
            // Phase 2 frontends only send `assistant.id` and rely on the
            // backend to resolve runtime identity from the snapshot. The
            // legacy `extra.{backend, agent_id, agent_source}` triple is
            // still consumed by ACP factory paths (`factory/acp.rs:34`),
            // ACP session creation (`create_acp_session_row`), the
            // session-context fallback chain, and several downstream
            // helpers. Persisting them here keeps one source of truth —
            // the assistant — while preserving the contract those
            // downstreams already depend on.
            if !snapshot.runtime_backend.is_empty() {
                obj.insert(
                    "backend".to_owned(),
                    serde_json::Value::String(snapshot.runtime_backend.clone()),
                );
            }
            if !snapshot.agent_id.is_empty() {
                obj.insert(
                    "agent_id".to_owned(),
                    serde_json::Value::String(snapshot.agent_id.clone()),
                );
            } else {
                obj.remove("agent_id");
            }
            if !snapshot.agent_source.is_empty() {
                obj.insert(
                    "agent_source".to_owned(),
                    serde_json::Value::String(snapshot.agent_source.clone()),
                );
            } else {
                obj.remove("agent_source");
            }
            if let Some(model_id) = snapshot.resolved_defaults.model.as_ref() {
                obj.insert(
                    "current_model_id".to_owned(),
                    serde_json::Value::String(model_id.clone()),
                );
            }
            if let Some(permission) = snapshot.resolved_defaults.permission.as_ref() {
                obj.insert("session_mode".to_owned(), serde_json::Value::String(permission.clone()));
                if matches!(effective_type, AgentType::Acp) {
                    obj.insert(
                        "current_mode_id".to_owned(),
                        serde_json::Value::String(permission.clone()),
                    );
                }
            }
            if let Some(thought_level) = snapshot.resolved_defaults.thought_level.as_ref() {
                obj.insert(
                    "thought_level".to_owned(),
                    serde_json::Value::String(thought_level.clone()),
                );
            }
            if !snapshot.rules.content.trim().is_empty() {
                match effective_type {
                    AgentType::Acp => {
                        obj.insert(
                            "preset_context".to_owned(),
                            serde_json::Value::String(snapshot.rules.content.clone()),
                        );
                        obj.remove("preset_rules");
                    }
                    AgentType::Aionrs => {
                        obj.insert(
                            "preset_rules".to_owned(),
                            serde_json::Value::String(snapshot.rules.content.clone()),
                        );
                        obj.remove("preset_context");
                    }
                    AgentType::Gemini
                    | AgentType::Codex
                    | AgentType::OpenclawGateway
                    | AgentType::Remote
                    | AgentType::Nanobot => {}
                }
            }
        }

        // Consume transient skill-shaping inputs and freeze the initial
        // `skills` snapshot into `extra.skills`. These request-only fields
        // must not land in the stored row. Legacy names (`enabled_skills`,
        // `exclude_builtin_skills`) are accepted as aliases for compatibility
        // with older frontend builds and pre-snapshot presets (§7.1).
        fn take_string_array(obj: &mut serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Vec<String> {
            for key in keys {
                if let Some(v) = obj.remove(*key)
                    && let Ok(arr) = serde_json::from_value::<Vec<String>>(v)
                {
                    return arr;
                }
            }
            Vec::new()
        }

        fn merge_string_lists(primary: &[String], secondary: &[String]) -> Vec<String> {
            let mut merged = primary.to_vec();
            for value in secondary {
                if !merged.iter().any(|existing| existing == value) {
                    merged.push(value.clone());
                }
            }
            merged
        }

        let (preset_enabled, exclude_auto_inject) = match extra.as_object_mut() {
            Some(obj) => {
                let extra_preset = take_string_array(obj, &["preset_enabled_skills", "enabled_skills"]);
                let extra_exclude = take_string_array(obj, &["exclude_auto_inject_skills", "exclude_builtin_skills"]);
                // Strip the stale cache field if a clone copied it in.
                obj.remove("loaded_skills");

                match assistant_snapshot.as_ref() {
                    Some(snapshot) => (
                        merge_string_lists(&snapshot.resolved_defaults.skill_ids, &extra_preset),
                        merge_string_lists(&snapshot.resolved_defaults.disabled_builtin_skill_ids, &extra_exclude),
                    ),
                    None => (extra_preset, extra_exclude),
                }
            }
            None => (Vec::new(), Vec::new()),
        };

        let auto_inject_names = self.skill_resolver.auto_inject_names().await;
        let initial_skills = compute_initial_skills(&auto_inject_names, &preset_enabled, &exclude_auto_inject);

        // Wire skill links into the runtime workspace so the agent CLI picks
        // them up via its native skills dir (e.g. `.claude/skills/`). This
        // applies to both temp and user-selected workspaces.
        let skill_link_workspace = user_supplied_workspace
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| auto_provisioned_workspace.clone());
        if let Some(ws_path) = skill_link_workspace.as_ref()
            && !initial_skills.is_empty()
            && let Some(rel_dirs) = native_skills_dirs(
                &self.agent_metadata_repo,
                &effective_type,
                effective_backend
                    .as_ref()
                    .map(|backend| serde_json::Value::String(backend.clone()))
                    .as_ref(),
            )
            .await
        {
            let resolved = self.skill_resolver.resolve_skills(&initial_skills).await;
            if !resolved.is_empty() {
                let rel_dirs_refs: Vec<&str> = rel_dirs.iter().map(String::as_str).collect();
                let n = self
                    .skill_resolver
                    .link_workspace_skills(ws_path, &rel_dirs_refs, &resolved)
                    .await;
                debug!(
                    conversation_id = %id,
                    workspace = %ws_path.display(),
                    links = n,
                    "wired skill symlinks into workspace"
                );
            }
        }

        if let Some(obj) = extra.as_object_mut() {
            obj.insert(
                "skills".to_owned(),
                serde_json::Value::Array(initial_skills.into_iter().map(serde_json::Value::String).collect()),
            );
        }

        let selected_mcp_server_ids = match extra.as_object_mut() {
            Some(obj) => {
                let has_selection = obj.contains_key("selected_mcp_server_ids");
                let ids = take_string_array(obj, &["selected_mcp_server_ids"]);
                if has_selection {
                    Some(ids)
                } else {
                    assistant_snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.resolved_defaults.mcp_ids.clone())
                        .filter(|ids| !ids.is_empty())
                }
            }
            None => None,
        };
        let selected_session_mcp_servers = match extra.as_object_mut() {
            Some(obj) => match obj.remove("selected_session_mcp_servers") {
                Some(value) => Some(serde_json::from_value::<Vec<SessionMcpServer>>(value).map_err(|e| {
                    ConversationError::BadRequest {
                        reason: format!("Invalid selected_session_mcp_servers: {e}"),
                    }
                })?),
                None => None,
            },
            None => None,
        };

        let mcp_support = self.resolve_mcp_support_policy(&effective_type, &extra).await?;
        let mut selected_row_ids: Vec<String> = Vec::new();
        let mut selected_mcp_names: Vec<String> = Vec::new();
        let mut selected_mcp_statuses: Vec<ConversationMcpStatus> = Vec::new();
        let mut seen_mcp_names = HashSet::new();
        let mut status_index_by_name: HashMap<String, usize> = HashMap::new();
        let repo = self
            .mcp_server_repo
            .read()
            .ok()
            .and_then(|guard| guard.as_ref().cloned());
        if let Some(repo) = repo {
            let rows = match selected_mcp_server_ids.as_ref() {
                Some(ids) => repo
                    .list_by_ids_any(ids)
                    .await
                    .map_err(|e| ConversationError::internal(format!("Failed to load selected MCP servers: {e}")))?,
                None => repo
                    .list()
                    .await
                    .map_err(|e| ConversationError::internal(format!("Failed to list MCP servers: {e}")))?,
            };
            let selected_rows = rows
                .into_iter()
                .filter(|row| !row.builtin)
                .filter(|row| match selected_mcp_server_ids.as_ref() {
                    Some(ids) => ids.iter().any(|id| id == &row.id),
                    None => row.enabled,
                })
                .collect::<Vec<_>>();
            selected_row_ids = selected_rows.iter().map(|row| row.id.clone()).collect();
            for row in &selected_rows {
                if seen_mcp_names.insert(row.name.clone()) {
                    selected_mcp_names.push(row.name.clone());
                }
                upsert_conversation_mcp_status(
                    &mut selected_mcp_statuses,
                    &mut status_index_by_name,
                    classify_repo_mcp_status(row, mcp_support),
                );
            }
        }

        if let Some(session_servers) = selected_session_mcp_servers.as_ref() {
            for server in session_servers {
                if seen_mcp_names.insert(server.name.clone()) {
                    selected_mcp_names.push(server.name.clone());
                }
                upsert_conversation_mcp_status(
                    &mut selected_mcp_statuses,
                    &mut status_index_by_name,
                    classify_session_mcp_status(server, mcp_support),
                );
            }
        }

        if let Some(obj) = extra.as_object_mut() {
            obj.insert(
                "mcp_server_ids".to_owned(),
                serde_json::Value::Array(selected_row_ids.into_iter().map(serde_json::Value::String).collect()),
            );
            obj.insert(
                "mcp_servers".to_owned(),
                serde_json::Value::Array(selected_mcp_names.into_iter().map(serde_json::Value::String).collect()),
            );
            obj.insert(
                "mcp_statuses".to_owned(),
                serde_json::to_value(&selected_mcp_statuses).map_err(|e| {
                    ConversationError::internal(format!("Failed to serialize MCP status snapshot: {e}"))
                })?,
            );
            if let Some(session_servers) = selected_session_mcp_servers.as_ref() {
                obj.insert(
                    "session_mcp_servers".to_owned(),
                    serde_json::to_value(session_servers).map_err(|e| {
                        ConversationError::internal(format!("Failed to serialize session MCP snapshot: {e}"))
                    })?,
                );
            }
        }

        let row = aionui_db::models::ConversationRow {
            id: id.clone(),
            user_id: user_id.to_owned(),
            name: req.name.unwrap_or_default(),
            r#type: enum_to_db(&effective_type)?,
            extra: serde_json::to_string(&extra)
                .map_err(|e| ConversationError::internal(format!("Failed to serialize extra: {e}")))?,
            model: req
                .model
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| ConversationError::internal(format!("Failed to serialize model: {e}")))?,
            status: Some(enum_to_db(&ConversationStatus::Pending)?),
            source: Some(enum_to_db(&source)?),
            channel_chat_id: req.channel_chat_id,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
        };

        self.conversation_repo.create(&row).await?;

        if let Some(snapshot) = assistant_snapshot.as_ref() {
            let resolved_skill_ids = serde_json::to_string(&snapshot.resolved_defaults.skill_ids).map_err(|e| {
                ConversationError::internal(format!("Failed to serialize assistant skill snapshot: {e}"))
            })?;
            let resolved_disabled_builtin_skill_ids =
                serde_json::to_string(&snapshot.resolved_defaults.disabled_builtin_skill_ids).map_err(|e| {
                    ConversationError::internal(format!(
                        "Failed to serialize assistant disabled builtin skill snapshot: {e}"
                    ))
                })?;
            let resolved_mcp_ids = serde_json::to_string(&snapshot.resolved_defaults.mcp_ids)
                .map_err(|e| ConversationError::internal(format!("Failed to serialize assistant MCP snapshot: {e}")))?;

            self.conversation_repo
                .upsert_assistant_snapshot(&UpsertConversationAssistantSnapshotParams {
                    conversation_id: &row.id,
                    assistant_definition_id: &snapshot.assistant_definition_id,
                    assistant_id: &snapshot.assistant_id,
                    assistant_source: &snapshot.assistant_source,
                    agent_id: &snapshot.agent_id,
                    rules_content: &snapshot.rules.content,
                    default_model_mode: &snapshot.default_modes.model,
                    resolved_model_id: snapshot.resolved_defaults.model.as_deref(),
                    default_permission_mode: &snapshot.default_modes.permission,
                    resolved_permission_value: snapshot.resolved_defaults.permission.as_deref(),
                    default_thought_level_mode: &snapshot.default_modes.thought_level,
                    resolved_thought_level_value: snapshot.resolved_defaults.thought_level.as_deref(),
                    default_skills_mode: &snapshot.default_modes.skills,
                    resolved_skill_ids: &resolved_skill_ids,
                    resolved_disabled_builtin_skill_ids: &resolved_disabled_builtin_skill_ids,
                    default_mcps_mode: &snapshot.default_modes.mcps,
                    resolved_mcp_ids: &resolved_mcp_ids,
                })
                .await?
                .ok_or_else(|| ConversationError::internal("assistant snapshot upsert returned no row"))?;
        }

        // ACP conversations own one `acp_session` row (1:1 by
        // conversation_id). Other agent types have no session-level
        // state so we only create it for ACP.
        if effective_type == AgentType::Acp {
            self.create_acp_session_row(&id, &extra, assistant_snapshot.as_ref())
                .await?;
        }

        if let Some(snapshot) = assistant_snapshot.as_ref() {
            self.persist_assistant_preferences_from_snapshot(snapshot).await?;
        }

        let mut response = row_to_response(row, &self.workspace_root)?;
        if let Some(snapshot) = assistant_snapshot.as_ref() {
            response.assistant = Some(aionui_api_types::ConversationAssistantIdentityResponse {
                id: snapshot.assistant_id.clone(),
                source: snapshot.assistant_source.clone(),
                name: snapshot.name.clone(),
                avatar: assistant_avatar_response_value(
                    snapshot.avatar_type.as_str(),
                    snapshot.avatar.as_deref(),
                    snapshot.assistant_id.as_str(),
                )
                .unwrap_or_default(),
                backend: snapshot.runtime_backend.clone(),
            });
        }

        self.broadcast_list_changed(&response.id, "created", response.source.as_ref());

        log_conversation_created(&response, &extra);

        Ok(response)
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    async fn create_acp_session_row(
        &self,
        conversation_id: &str,
        extra: &serde_json::Value,
        assistant_snapshot: Option<&AssistantSnapshot>,
    ) -> Result<(), ConversationError> {
        debug!("Creating acp_session row");

        // Identity comes from the user's agent choice in `extra`.
        // `agent_id` is the catalog row id; `backend` is the vendor
        // label; `agent_source` says builtin/extension/custom. The
        // frontend always posts agent_id for picked rows, but older
        // payloads may only carry `backend`, so we resolve defensively.
        let agent_id_from_extra = extra.get("agent_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let backend = assistant_snapshot
            .map(|snapshot| snapshot.runtime_backend.as_str())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                extra
                    .get("backend")
                    .and_then(|v| v.as_str())
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_default();
        let agent_source = assistant_snapshot
            .map(|snapshot| snapshot.agent_source.as_str())
            .filter(|value| !value.is_empty())
            .or_else(|| extra.get("agent_source").and_then(|v| v.as_str()))
            .unwrap_or("builtin");

        // Fallback: older clients (electron main, legacy webhooks) only
        // post `backend` without `agent_id`. Resolve the builtin row for
        // that vendor so the session still has a concrete catalog
        // reference. Non-builtin agents must provide `agent_id`
        // explicitly — custom/extension rows have no unique lookup key
        // from `(backend, agent_source)` alone.
        let resolved_agent_id = match assistant_snapshot
            .map(|snapshot| snapshot.agent_id.as_str())
            .filter(|id| !id.is_empty())
            .or(agent_id_from_extra)
        {
            Some(id) => id.to_owned(),
            None if !backend.is_empty() && agent_source == "builtin" => self
                .agent_metadata_repo
                .find_builtin_by_backend(backend)
                .await
                .map_err(|e| ConversationError::internal(format!("agent_metadata lookup: {e}")))?
                .map(|row| row.id)
                .unwrap_or_default(),
            None => String::new(),
        };

        let params = CreateAcpSessionParams {
            conversation_id,
            agent_source,
            agent_id: &resolved_agent_id,
        };
        self.acp_session_repo
            .create(&params)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to create acp_session row: {e}")))?;

        // Seed optional runtime state from create payload. Empty strings are
        // treated as absent, matching the "send key only when value present"
        // contract on the wire. Mode/model take effect on the first
        // reconcile right after session/new.
        let mode = extra
            .get("current_mode_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        let model = extra
            .get("current_model_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());
        if mode.is_some() || model.is_some() {
            let params = SaveRuntimeStateParams {
                current_mode_id: mode.map(Some),
                current_model_id: model.map(Some),
                config_selections_json: None,
                context_usage_json: None,
            };
            self.acp_session_repo
                .save_runtime_state(conversation_id, &params)
                .await
                .map_err(|e| ConversationError::internal(format!("Failed to seed acp_session runtime state: {e}")))?;
        }
        Ok(())
    }

    async fn resolve_assistant_agent_binding(
        &self,
        value: &str,
    ) -> Result<Option<AgentBindingResolution>, ConversationError> {
        let rows = self
            .agent_metadata_repo
            .list_all()
            .await
            .map_err(|e| ConversationError::internal(format!("agent_metadata lookup failed: {e}")))?;
        Ok(resolve_agent_binding_from_rows(&rows, value))
    }

    async fn resolve_assistant_snapshot(
        &self,
        assistant_id: &str,
        locale: Option<&str>,
        overrides: &AssistantConversationOverrides,
        extra: &serde_json::Value,
    ) -> Result<Option<AssistantSnapshot>, ConversationError> {
        let (Some(definition_repo), Some(state_repo), Some(preference_repo)) = (
            self.assistant_definition_repo(),
            self.assistant_state_repo(),
            self.assistant_preference_repo(),
        ) else {
            return Ok(None);
        };

        let Some(definition) = definition_repo
            .get_by_assistant_id(assistant_id)
            .await
            .map_err(|e| ConversationError::internal(format!("assistant definition lookup failed: {e}")))?
        else {
            return Ok(None);
        };

        let state = state_repo
            .get(&definition.id)
            .await
            .map_err(|e| ConversationError::internal(format!("assistant state lookup failed: {e}")))?;
        let preference = preference_repo
            .get(&definition.id)
            .await
            .map_err(|e| ConversationError::internal(format!("assistant preference lookup failed: {e}")))?;

        let skill_ids = match overrides.skill_ids.as_ref() {
            Some(value) => value.clone(),
            None if definition.default_skills_mode == "fixed" => {
                parse_json_string_list(Some(definition.default_skill_ids.as_str()), "default_skill_ids")?
            }
            None => preference
                .as_ref()
                .map(|row| parse_json_string_list(Some(row.last_skill_ids.as_str()), "last_skill_ids"))
                .transpose()?
                .unwrap_or_default(),
        };
        let disabled_builtin_skill_ids = match overrides.disabled_builtin_skill_ids.as_ref() {
            Some(value) => value.clone(),
            None if definition.default_skills_mode == "fixed" => parse_json_string_list(
                Some(definition.default_disabled_builtin_skill_ids.as_str()),
                "default_disabled_builtin_skill_ids",
            )?,
            None => preference
                .as_ref()
                .map(|row| {
                    parse_json_string_list(
                        Some(row.last_disabled_builtin_skill_ids.as_str()),
                        "last_disabled_builtin_skill_ids",
                    )
                })
                .transpose()?
                .unwrap_or_default(),
        };
        let mcp_ids = match overrides.mcp_ids.as_ref() {
            Some(value) => value.clone(),
            None if definition.default_mcps_mode == "fixed" => {
                parse_json_string_list(Some(definition.default_mcp_ids.as_str()), "default_mcp_ids")?
            }
            None => preference
                .as_ref()
                .map(|row| parse_json_string_list(Some(row.last_mcp_ids.as_str()), "last_mcp_ids"))
                .transpose()?
                .unwrap_or_default(),
        };

        let model = overrides
            .model
            .clone()
            .or_else(|| match definition.default_model_mode.as_str() {
                "fixed" => definition.default_model_value.clone(),
                "auto" => preference.as_ref().and_then(|row| row.last_model_id.clone()),
                _ => None,
            });
        let permission = overrides
            .permission
            .clone()
            .or_else(|| match definition.default_permission_mode.as_str() {
                "fixed" => definition.default_permission_value.clone(),
                "auto" => preference.as_ref().and_then(|row| row.last_permission_value.clone()),
                _ => None,
            });
        let thought_level =
            overrides
                .thought_level
                .clone()
                .or_else(|| match definition.default_thought_level_mode.as_str() {
                    "fixed" => definition.default_thought_level_value.clone(),
                    "auto" => preference.as_ref().and_then(|row| row.last_thought_level_value.clone()),
                    _ => None,
                });

        let rules_content = if let Some(dispatcher) = self.assistant_dispatcher() {
            dispatcher
                .read_rule(assistant_id, locale)
                .await
                .map_err(|e| ConversationError::internal(format!("assistant rule lookup failed: {e}")))?
        } else {
            String::new()
        };
        let fallback_rules = extra
            .get("preset_context")
            .and_then(serde_json::Value::as_str)
            .or_else(|| extra.get("preset_rules").and_then(serde_json::Value::as_str))
            .unwrap_or_default();
        let effective_agent_id = state
            .as_ref()
            .and_then(|row| row.agent_id_override.clone())
            .unwrap_or_else(|| definition.agent_id.clone());
        let agent_binding = self
            .resolve_assistant_agent_binding(&effective_agent_id)
            .await?
            .ok_or_else(|| ConversationError::BadRequest {
                reason: format!("assistant agent `{effective_agent_id}` is not registered in agent_metadata"),
            })?;
        let agent_type = parse_agent_type_from_metadata(&agent_binding.agent_type)?;

        Ok(Some(AssistantSnapshot {
            assistant_definition_id: definition.id,
            assistant_id: assistant_id.to_owned(),
            assistant_source: definition.source,
            name: definition.name,
            avatar_type: definition.avatar_type,
            avatar: definition.avatar_value,
            agent_id: agent_binding.agent_id,
            agent_source: agent_binding.agent_source,
            runtime_backend: agent_binding.runtime_backend,
            agent_type,
            rules: AssistantSnapshotRules {
                content: if rules_content.is_empty() {
                    fallback_rules.to_owned()
                } else {
                    rules_content
                },
            },
            default_modes: AssistantSnapshotDefaultModes {
                model: definition.default_model_mode.clone(),
                permission: definition.default_permission_mode.clone(),
                thought_level: definition.default_thought_level_mode.clone(),
                skills: definition.default_skills_mode.clone(),
                mcps: definition.default_mcps_mode.clone(),
            },
            resolved_defaults: AssistantSnapshotResolvedDefaults {
                model,
                permission,
                thought_level,
                skill_ids,
                disabled_builtin_skill_ids,
                mcp_ids,
            },
            created_at: now_ms(),
        }))
    }

    async fn persist_assistant_preferences_from_snapshot(
        &self,
        snapshot: &AssistantSnapshot,
    ) -> Result<(), ConversationError> {
        let Some(preference_repo) = self.assistant_preference_repo() else {
            return Ok(());
        };

        let existing_preference = preference_repo
            .get(&snapshot.assistant_definition_id)
            .await
            .map_err(|e| ConversationError::internal(format!("assistant preference lookup failed: {e}")))?;
        let last_model_id = if snapshot.default_modes.model == "auto" {
            snapshot.resolved_defaults.model.clone()
        } else {
            existing_preference.as_ref().and_then(|row| row.last_model_id.clone())
        };
        let last_permission_value = if snapshot.default_modes.permission == "auto" {
            snapshot.resolved_defaults.permission.clone()
        } else {
            existing_preference
                .as_ref()
                .and_then(|row| row.last_permission_value.clone())
        };
        let last_thought_level_value = if snapshot.default_modes.thought_level == "auto" {
            snapshot.resolved_defaults.thought_level.clone()
        } else {
            existing_preference
                .as_ref()
                .and_then(|row| row.last_thought_level_value.clone())
        };
        let last_skill_ids = if snapshot.default_modes.skills == "auto" {
            serde_json::to_string(&snapshot.resolved_defaults.skill_ids)
                .map_err(|e| ConversationError::internal(format!("encode assistant skills: {e}")))?
        } else {
            existing_preference
                .as_ref()
                .map(|row| row.last_skill_ids.clone())
                .unwrap_or_else(|| "[]".to_string())
        };
        let last_disabled_builtin_skill_ids = if snapshot.default_modes.skills == "auto" {
            serde_json::to_string(&snapshot.resolved_defaults.disabled_builtin_skill_ids)
                .map_err(|e| ConversationError::internal(format!("encode assistant disabled builtin skills: {e}")))?
        } else {
            existing_preference
                .as_ref()
                .map(|row| row.last_disabled_builtin_skill_ids.clone())
                .unwrap_or_else(|| "[]".to_string())
        };
        let last_mcp_ids = if snapshot.default_modes.mcps == "auto" {
            serde_json::to_string(&snapshot.resolved_defaults.mcp_ids)
                .map_err(|e| ConversationError::internal(format!("encode assistant mcps: {e}")))?
        } else {
            existing_preference
                .as_ref()
                .map(|row| row.last_mcp_ids.clone())
                .unwrap_or_else(|| "[]".to_string())
        };

        preference_repo
            .upsert(&aionui_db::UpsertAssistantPreferenceParams {
                assistant_definition_id: &snapshot.assistant_definition_id,
                last_model_id: last_model_id.as_deref(),
                last_permission_value: last_permission_value.as_deref(),
                last_thought_level_value: last_thought_level_value.as_deref(),
                last_skill_ids: &last_skill_ids,
                last_disabled_builtin_skill_ids: &last_disabled_builtin_skill_ids,
                last_mcp_ids: &last_mcp_ids,
            })
            .await
            .map_err(|e| ConversationError::internal(format!("assistant preference upsert failed: {e}")))?;

        Ok(())
    }

    pub(crate) async fn persist_runtime_assistant_snapshot(
        &self,
        conversation_id: &str,
        updates: AssistantRuntimePreferenceUpdate<'_>,
    ) -> Result<(), ConversationError> {
        let Some(snapshot) = self
            .conversation_repo
            .get_assistant_snapshot(conversation_id)
            .await
            .map_err(|e| {
                ConversationError::internal(format!(
                    "Failed to load persisted assistant snapshot for runtime sync: {e}"
                ))
            })?
        else {
            return Ok(());
        };

        self.conversation_repo
            .upsert_assistant_snapshot(&UpsertConversationAssistantSnapshotParams {
                conversation_id: &snapshot.conversation_id,
                assistant_definition_id: &snapshot.assistant_definition_id,
                assistant_id: &snapshot.assistant_id,
                assistant_source: &snapshot.assistant_source,
                agent_id: &snapshot.agent_id,
                rules_content: &snapshot.rules_content,
                default_model_mode: &snapshot.default_model_mode,
                resolved_model_id: updates.model.or(snapshot.resolved_model_id.as_deref()),
                default_permission_mode: &snapshot.default_permission_mode,
                resolved_permission_value: updates.permission.or(snapshot.resolved_permission_value.as_deref()),
                default_thought_level_mode: &snapshot.default_thought_level_mode,
                resolved_thought_level_value: updates
                    .thought_level
                    .or(snapshot.resolved_thought_level_value.as_deref()),
                default_skills_mode: &snapshot.default_skills_mode,
                resolved_skill_ids: &snapshot.resolved_skill_ids,
                resolved_disabled_builtin_skill_ids: &snapshot.resolved_disabled_builtin_skill_ids,
                default_mcps_mode: &snapshot.default_mcps_mode,
                resolved_mcp_ids: &snapshot.resolved_mcp_ids,
            })
            .await
            .map_err(|e| ConversationError::internal(format!("assistant snapshot upsert failed: {e}")))?;

        Ok(())
    }

    pub(crate) async fn persist_runtime_assistant_preferences(
        &self,
        conversation_id: &str,
        updates: AssistantRuntimePreferenceUpdate<'_>,
    ) -> Result<(), ConversationError> {
        let (Some(definition_repo), Some(preference_repo)) =
            (self.assistant_definition_repo(), self.assistant_preference_repo())
        else {
            return Ok(());
        };

        let persisted_snapshot = self
            .conversation_repo
            .get_assistant_snapshot(conversation_id)
            .await
            .map_err(|e| {
                ConversationError::internal(format!(
                    "Failed to load persisted assistant snapshot for preference sync: {e}"
                ))
            })?;

        let fallback = if persisted_snapshot.is_none() {
            let Some(conversation) = self.conversation_repo.get(conversation_id).await.map_err(|e| {
                ConversationError::internal(format!(
                    "Failed to load conversation for assistant preference sync: {e}"
                ))
            })?
            else {
                return Ok(());
            };
            let extra: serde_json::Value = serde_json::from_str(&conversation.extra).map_err(|e| {
                ConversationError::internal(format!("Invalid extra JSON for assistant preference sync: {e}"))
            })?;
            let legacy_snapshot = extra
                .get("assistant_snapshot")
                .cloned()
                .map(serde_json::from_value::<AssistantSnapshot>)
                .transpose()
                .map_err(|e| {
                    ConversationError::internal(format!("Invalid assistant snapshot for preference sync: {e}"))
                })?;
            let assistant_id = legacy_snapshot
                .as_ref()
                .map(|value| value.assistant_id.clone())
                .or_else(|| {
                    extra
                        .get("assistant_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .or_else(|| {
                    extra
                        .get("preset_assistant_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToOwned::to_owned)
                });
            let Some(assistant_id) = assistant_id else {
                return Ok(());
            };
            let Some(definition) = definition_repo
                .get_by_assistant_id(&assistant_id)
                .await
                .map_err(|e| ConversationError::internal(format!("assistant definition lookup failed: {e}")))?
            else {
                return Ok(());
            };
            Some((definition, legacy_snapshot))
        } else {
            None
        };

        let (definition_id, default_modes) = if let Some(snapshot) = persisted_snapshot.as_ref() {
            (
                snapshot.assistant_definition_id.clone(),
                AssistantEffectiveDefaultModes {
                    model: snapshot.default_model_mode.as_str(),
                    permission: snapshot.default_permission_mode.as_str(),
                    thought_level: snapshot.default_thought_level_mode.as_str(),
                },
            )
        } else {
            let (definition, legacy_snapshot) = fallback
                .as_ref()
                .ok_or_else(|| ConversationError::internal("assistant preference sync fallback missing"))?;
            (
                definition.id.clone(),
                legacy_snapshot
                    .as_ref()
                    .map(|value| assistant_snapshot_modes(value, definition))
                    .unwrap_or_else(|| AssistantEffectiveDefaultModes {
                        model: definition.default_model_mode.as_str(),
                        permission: definition.default_permission_mode.as_str(),
                        thought_level: definition.default_thought_level_mode.as_str(),
                    }),
            )
        };

        let existing_preference = preference_repo
            .get(&definition_id)
            .await
            .map_err(|e| ConversationError::internal(format!("assistant preference lookup failed: {e}")))?;

        let last_model_id = if default_modes.model == "auto" {
            updates
                .model
                .map(ToOwned::to_owned)
                .or_else(|| existing_preference.as_ref().and_then(|row| row.last_model_id.clone()))
        } else {
            existing_preference.as_ref().and_then(|row| row.last_model_id.clone())
        };
        let last_permission_value = if default_modes.permission == "auto" {
            updates.permission.map(ToOwned::to_owned).or_else(|| {
                existing_preference
                    .as_ref()
                    .and_then(|row| row.last_permission_value.clone())
            })
        } else {
            existing_preference
                .as_ref()
                .and_then(|row| row.last_permission_value.clone())
        };
        let last_thought_level_value = if default_modes.thought_level == "auto" {
            updates.thought_level.map(ToOwned::to_owned).or_else(|| {
                existing_preference
                    .as_ref()
                    .and_then(|row| row.last_thought_level_value.clone())
            })
        } else {
            existing_preference
                .as_ref()
                .and_then(|row| row.last_thought_level_value.clone())
        };

        preference_repo
            .upsert(&aionui_db::UpsertAssistantPreferenceParams {
                assistant_definition_id: &definition_id,
                last_model_id: last_model_id.as_deref(),
                last_permission_value: last_permission_value.as_deref(),
                last_thought_level_value: last_thought_level_value.as_deref(),
                last_skill_ids: existing_preference
                    .as_ref()
                    .map(|row| row.last_skill_ids.as_str())
                    .unwrap_or("[]"),
                last_disabled_builtin_skill_ids: existing_preference
                    .as_ref()
                    .map(|row| row.last_disabled_builtin_skill_ids.as_str())
                    .unwrap_or("[]"),
                last_mcp_ids: existing_preference
                    .as_ref()
                    .map(|row| row.last_mcp_ids.as_str())
                    .unwrap_or("[]"),
            })
            .await
            .map_err(|e| ConversationError::internal(format!("assistant runtime preference upsert failed: {e}")))?;

        Ok(())
    }

    /// Get a single conversation by ID.
    ///
    /// Returns `NotFound` if the conversation does not exist or does not
    /// belong to the given user (avoids leaking existence to other users).
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn get(&self, user_id: &str, id: &str) -> Result<ConversationResponse, ConversationError> {
        let row = self
            .conversation_repo
            .get(id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra)
            .map_err(|e| ConversationError::internal(format!("Invalid extra JSON: {e}")))?;
        self.backfill_extra_inplace(&row.id, &mut extra).await;
        let mut response = row_to_response_with_extra(row, extra, &self.workspace_root)?;
        self.attach_assistant_identity(&mut response).await?;
        response.runtime = Some(self.runtime_summary_for(id).await);
        Ok(response)
    }

    /// List conversations with cursor-based pagination and optional filters.
    #[tracing::instrument(skip_all, fields(user_id = %user_id))]
    pub async fn list(
        &self,
        user_id: &str,
        query: ListConversationsQuery,
    ) -> Result<ConversationListResponse, ConversationError> {
        let filters = ConversationFilters {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(0),
            source: query.source,
            cron_job_id: query.cron_job_id,
            pinned: query.pinned,
        };

        let result = self.conversation_repo.list_paginated(user_id, &filters).await?;

        // Tolerate per-row deserialization failures — a single legacy row
        // (e.g. an abandoned agent_type='gemini' conversation post-migration)
        // must not take down the whole listing. Skip-and-log is the
        // explicit resilience contract from the Gemini→ACP migration spec.
        let mut items = Vec::with_capacity(result.items.len());
        for row in result.items {
            let row_id = row.id.clone();
            let mut extra: serde_json::Value = match serde_json::from_str(&row.extra) {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        conversation_id = %row_id,
                        error = %ErrorChain(&err),
                        "Skipping unreadable conversation row in list"
                    );
                    continue;
                }
            };
            self.backfill_extra_inplace(&row_id, &mut extra).await;
            match row_to_response_with_extra(row, extra, &self.workspace_root) {
                Ok(mut resp) => {
                    self.attach_assistant_identity(&mut resp).await?;
                    items.push(resp);
                }
                Err(err) => warn!(
                    conversation_id = %row_id,
                    error = %ErrorChain(&err),
                    "Skipping unreadable conversation row in list"
                ),
            }
        }

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }

    /// Update a conversation (partial update with extra-merge semantics).
    ///
    /// If `extra` is provided, it is merged into the existing extra JSON
    /// (top-level keys are overwritten, unlisted keys are preserved).
    /// Broadcasts `conversation.listChanged(updated)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn update(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateConversationRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConversationResponse, ConversationError> {
        let existing = self
            .conversation_repo
            .get(id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;

        let existing_type: AgentType = string_to_enum(&existing.r#type)?;

        // Snapshot invariant: once written at create time, `extra.skills`
        // must not be re-shaped by PATCH. The frontend must clone the
        // conversation to produce a new snapshot.
        if let Some(incoming) = &req.extra
            && (incoming.get("skills").is_some()
                || incoming.get("mcp_server_ids").is_some()
                || incoming.get("mcp_servers").is_some()
                || incoming.get("mcp_statuses").is_some()
                || incoming.get("session_mcp_servers").is_some())
        {
            return Err(ConversationError::BadRequest {
                reason: "extra.skills and MCP snapshots are immutable post-creation".into(),
            });
        }

        if existing_type == AgentType::Acp
            && let Some(incoming) = &req.extra
            && (incoming.get("current_model_id").is_some() || incoming.get("current_mode_id").is_some())
        {
            warn!(
                conversation_id = %id,
                "Rejected ACP runtime current-state write through conversation.extra"
            );
            return Err(ConversationError::BadRequest {
                reason: "ACP runtime current mode/model must be changed via /config-options, not conversation.extra"
                    .into(),
            });
        }

        // Type-aware rule: top-level `model` is aionrs-only. For non-aionrs
        // conversations, model/mode must be updated via `extra` (see spec
        // 2026-05-12).
        if existing_type != AgentType::Aionrs && req.model.is_some() {
            return Err(ConversationError::BadRequest {
                reason: format!(
                    "top-level `model` is only accepted for aionrs conversations; pass model via `extra` for {}",
                    existing.r#type
                ),
            });
        }

        let now = now_ms();

        // Merge extra if provided. For aionrs, strip `extra.model` post-merge
        // so the row keeps a single canonical model source (top-level column).
        let merged_extra = if let Some(new_extra) = &req.extra {
            let mut existing_extra: serde_json::Value =
                serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
            merge_json(&mut existing_extra, new_extra);
            if existing_type == AgentType::Aionrs
                && let Some(obj) = existing_extra.as_object_mut()
                && obj.remove("model").is_some()
            {
                warn!("aionrs update: stripped legacy `extra.model` from merged extra");
            }
            if new_extra.get("workspace").is_some() {
                normalize_workspace_extra(&mut existing_extra)?;
            }
            Some(
                serde_json::to_string(&existing_extra)
                    .map_err(|e| ConversationError::internal(format!("Failed to serialize merged extra: {e}")))?,
            )
        } else {
            None
        };

        // Handle pinned_at: set timestamp on pin, clear on unpin
        let pinned_at = req.pinned.map(|p| if p { Some(now) } else { None });

        let model_changed = req.model.as_ref().is_some_and(|new_model| {
            let new_json = serde_json::to_string(new_model).unwrap_or_default();
            existing.model.as_deref() != Some(new_json.as_str())
        });

        let model_json = req
            .model
            .as_ref()
            .map(|m| {
                serde_json::to_string(m)
                    .map(Some)
                    .map_err(|e| ConversationError::internal(format!("Failed to serialize model: {e}")))
            })
            .transpose()?;

        let updates = ConversationRowUpdate {
            name: req.name,
            pinned: req.pinned,
            pinned_at,
            model: model_json,
            extra: merged_extra,
            status: None,
            updated_at: Some(now),
        };

        self.conversation_repo.update(id, &updates).await?;

        if let Some(model) = req.model.as_ref() {
            let selected_model = model.use_model.as_deref().unwrap_or(model.model.as_str());
            self.persist_runtime_assistant_snapshot(
                id,
                AssistantRuntimePreferenceUpdate {
                    model: Some(selected_model),
                    ..Default::default()
                },
            )
            .await?;
            self.persist_runtime_assistant_preferences(
                id,
                AssistantRuntimePreferenceUpdate {
                    model: Some(selected_model),
                    ..Default::default()
                },
            )
            .await?;
        }

        if model_changed {
            info!(
                model_changed = true,
                "Conversation updated, killing agent task due to model change"
            );
            if let Err(e) = task_manager.kill(id, None) {
                warn!(error = %ErrorChain(&e), "Failed to kill agent after model change");
            }
        }

        // Re-fetch to return the updated version
        let updated = self
            .conversation_repo
            .get(id)
            .await?
            .ok_or_else(|| ConversationError::internal("Conversation vanished after update"))?;

        let response = row_to_response(updated, &self.workspace_root)?;

        info!("Conversation updated");
        self.broadcast_list_changed(id, "updated", response.source.as_ref());

        Ok(response)
    }

    /// Merge a JSON patch into `conversation.extra` without touching model,
    /// name, pinned flag, or task lifecycle. Intended for internal callers
    /// (e.g. `TeamSessionService::ensure_session` writing
    /// `team_mcp_stdio_config`) where a full `update()` would kill the agent
    /// on a spurious model comparison.
    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn update_extra(&self, conversation_id: &str, patch: serde_json::Value) -> Result<(), ConversationError> {
        let existing =
            self.conversation_repo
                .get(conversation_id)
                .await?
                .ok_or_else(|| ConversationError::NotFound {
                    id: conversation_id.to_owned(),
                })?;

        let mut merged: serde_json::Value =
            serde_json::from_str(&existing.extra).unwrap_or_else(|_| serde_json::json!({}));
        merge_json(&mut merged, &patch);
        if patch.get("workspace").is_some() {
            normalize_workspace_extra(&mut merged)?;
        }

        let updates = ConversationRowUpdate {
            extra: Some(
                serde_json::to_string(&merged)
                    .map_err(|e| ConversationError::internal(format!("Failed to serialize merged extra: {e}")))?,
            ),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(conversation_id, &updates).await?;
        debug!("Conversation extra merged");
        Ok(())
    }

    pub async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), ConversationError> {
        let runtime_state = self
            .acp_session_repo
            .load_runtime_state(conversation_id)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to load runtime mode state: {e}")))?;
        let mut config_selections = runtime_state
            .and_then(|state| state.config_selections_json)
            .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(&raw).ok())
            .unwrap_or_default();
        config_selections.insert("mode".to_owned(), mode.to_owned());
        let config_selections_json = serde_json::to_string(&config_selections)
            .map_err(|e| ConversationError::internal(format!("Failed to serialize runtime mode selection: {e}")))?;
        let params = SaveRuntimeStateParams {
            current_mode_id: Some(Some(mode)),
            config_selections_json: Some(Some(config_selections_json.as_str())),
            ..Default::default()
        };
        self.acp_session_repo
            .save_runtime_state(conversation_id, &params)
            .await
            .map_err(|e| ConversationError::internal(format!("Failed to persist runtime mode: {e}")))?;
        Ok(())
    }

    /// Delete a conversation (messages cascade via FK).
    ///
    /// Broadcasts `conversation.listChanged(deleted)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn delete(&self, user_id: &str, id: &str) -> Result<(), ConversationError> {
        // Get existing to retrieve source for broadcast and verify ownership
        let existing = self
            .conversation_repo
            .get(id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;

        let source: Option<ConversationSource> = existing
            .source
            .as_deref()
            .and_then(|s| string_to_enum::<ConversationSource>(s).ok());
        let auto_workspace_to_delete = auto_provisioned_workspace_to_delete(&self.workspace_root, &existing, id);

        let had_active_turn = self.runtime_state.mark_deleting(id);

        // Snapshot the hook list under the read lock, then drop the guard
        // before awaiting — `RwLockReadGuard` is not `Send`, so holding it
        // across `.await` would make this future non-`Send`.
        let hooks: Vec<Arc<dyn OnConversationDelete>> =
            self.delete_hooks.read().map(|guard| guard.clone()).unwrap_or_default();
        for hook in hooks {
            hook.on_conversation_deleted(id).await;
        }

        if let Err(err) = self.conversation_repo.delete(id).await {
            self.runtime_state.clear_deleting(id);
            return Err(err.into());
        }
        if !had_active_turn {
            self.runtime_state.clear_deleting(id);
        }
        // No FK / CASCADE on `acp_session`: clean it up here so non-ACP
        // conversations that used to be ACP (shouldn't happen but is
        // cheap to cover) still drop their orphaned session row.
        if let Err(err) = self.acp_session_repo.delete(id).await {
            warn!(
                error = %ErrorChain(&err),
                "Failed to delete acp_session row on conversation delete"
            );
        }
        if let Some(workspace) = auto_workspace_to_delete {
            let workspace_removed = match tokio::fs::remove_dir_all(&workspace).await {
                Ok(()) => {
                    info!(
                        conversation_id = %id,
                        workspace = %workspace.display(),
                        "Deleted auto-provisioned conversation workspace"
                    );
                    true
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
                Err(err) => {
                    warn!(
                        conversation_id = %id,
                        workspace = %workspace.display(),
                        error = %err,
                        "Failed to delete auto-provisioned conversation workspace"
                    );
                    false
                }
            };
            if workspace_removed {
                cleanup_empty_date_workspace_parents(&self.workspace_root, &workspace).await;
            }
        }

        info!("Conversation deleted");
        self.broadcast_list_changed(id, "deleted", source.as_ref());

        Ok(())
    }

    /// Create a conversation from a `CloneConversationRequest`.
    ///
    /// Historically this method supported cloning from a source conversation
    /// (inheriting name / extra / cron binding). That use case has been
    /// removed — the method is retained only because `POST
    /// /api/conversations/clone` has three active callers
    /// (`_AddNewConversation`, worker task manager, legacy repo shim) that
    /// send a pre-built payload shape. New code should prefer `create`.
    pub async fn clone_create(
        &self,
        user_id: &str,
        req: CloneConversationRequest,
    ) -> Result<ConversationResponse, ConversationError> {
        self.create(user_id, req.conversation).await
    }

    /// Reset a conversation: clear messages and set status back to pending.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %id))]
    pub async fn reset(&self, user_id: &str, id: &str) -> Result<(), ConversationError> {
        // Verify existence and ownership
        self.conversation_repo
            .get(id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;

        // Delete all messages
        self.conversation_repo.delete_messages_by_conversation(id).await?;
        self.conversation_repo.delete_artifacts_by_conversation(id).await?;

        // Reset status to pending
        let now = now_ms();
        let updates = ConversationRowUpdate {
            status: Some(enum_to_db(&ConversationStatus::Pending)?),
            updated_at: Some(now),
            ..Default::default()
        };
        self.conversation_repo.update(id, &updates).await?;

        info!("Conversation reset");
        Ok(())
    }

    /// List conversations associated by the same workspace.
    pub async fn list_associated(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Vec<ConversationResponse>, ConversationError> {
        self.conversation_repo
            .get(id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound { id: id.to_owned() })?;

        let rows = self.conversation_repo.list_associated(user_id, id).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let mut response = row_to_response(row, &self.workspace_root)?;
            self.attach_assistant_identity(&mut response).await?;
            items.push(response);
        }
        Ok(items)
    }

    /// List conversations spawned by a specific cron job.
    pub async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationResponse>, ConversationError> {
        let rows = self.conversation_repo.list_by_cron_job(user_id, cron_job_id).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let mut response = row_to_response(row, &self.workspace_root)?;
            self.attach_assistant_identity(&mut response).await?;
            items.push(response);
        }
        Ok(items)
    }
}

// ── Messages & Artifacts ────────────────────────────────────────────

const DEFAULT_MESSAGE_PAGE_LIMIT: u32 = 50;
const MAX_MESSAGE_PAGE_LIMIT: u32 = 200;

fn effective_message_limit(limit: Option<u32>) -> u32 {
    match limit.unwrap_or(DEFAULT_MESSAGE_PAGE_LIMIT) {
        0 => DEFAULT_MESSAGE_PAGE_LIMIT,
        n => n.min(MAX_MESSAGE_PAGE_LIMIT),
    }
}

fn message_locator_count(query: &ListMessagesQuery) -> usize {
    usize::from(query.before.is_some())
        + usize::from(query.after.is_some())
        + usize::from(query.anchor_message_id.is_some())
}

impl ConversationService {
    /// List messages for a conversation with cursor-based pagination.
    pub async fn list_messages(
        &self,
        user_id: &str,
        conversation_id: &str,
        query: ListMessagesQuery,
    ) -> Result<MessageListResponse, ConversationError> {
        // Verify conversation exists and belongs to user
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let limit = effective_message_limit(query.limit);
        if message_locator_count(&query) > 1 {
            return Err(ConversationError::bad_request(
                "before, after, and anchor_message_id are mutually exclusive",
            ));
        }

        let direction = if let Some(cursor) = query.before.as_deref() {
            MessagePageDirection::Before {
                cursor: decode_message_cursor(cursor)?,
            }
        } else if let Some(cursor) = query.after.as_deref() {
            MessagePageDirection::After {
                cursor: decode_message_cursor(cursor)?,
            }
        } else if let Some(message_id) = query.anchor_message_id.clone() {
            MessagePageDirection::Anchor { message_id }
        } else {
            MessagePageDirection::InitialLatest
        };
        let compact_content = matches!(query.content_mode.as_deref(), Some("compact"));

        let page = self
            .conversation_repo
            .list_messages_page(conversation_id, &MessagePageParams { limit, direction })
            .await?;

        let mut compacted_count = 0usize;
        let mut total_original_content_bytes = 0usize;
        let mut total_response_content_bytes = 0usize;
        let oldest_cursor = page
            .items
            .first()
            .map(|row| encode_message_cursor(&MessagePageCursor::from(row)))
            .transpose()?;
        let newest_cursor = page
            .items
            .last()
            .map(|row| encode_message_cursor(&MessagePageCursor::from(row)))
            .transpose()?;
        let mut items = Vec::with_capacity(page.items.len());
        for row in page.items {
            let original_content_bytes = row.content.len();
            total_original_content_bytes += original_content_bytes;
            let response = if compact_content {
                row_to_message_response_compact(row)?
            } else {
                row_to_message_response(row)?
            };

            if compact_content {
                if response
                    .content
                    .get("_compact")
                    .and_then(|compact| compact.get("truncated"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false)
                {
                    compacted_count += 1;
                }
                total_response_content_bytes += response.content.to_string().len();
            }
            items.push(response);
        }

        if compact_content && compacted_count > 0 {
            info!(
                conversation_id,
                limit,
                items = items.len(),
                compacted = compacted_count,
                total_original_content_bytes,
                total_response_content_bytes,
                "Compacted tool message list response"
            );
        }

        Ok(MessageListResponse {
            items,
            oldest_cursor,
            newest_cursor,
            has_more_before: page.has_more_before,
            has_more_after: page.has_more_after,
        })
    }

    /// Return one full message for a conversation after verifying ownership.
    pub async fn get_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        message_id: &str,
    ) -> Result<MessageResponse, ConversationError> {
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let row = self
            .conversation_repo
            .get_message(conversation_id, message_id)
            .await?
            .ok_or_else(|| ConversationError::MessageNotFound {
                id: message_id.to_owned(),
            })?;

        let content_bytes = row.content.len();
        let response = row_to_message_response(row)?;
        if is_tool_message_type(response.r#type) || content_bytes > TOOL_CONTENT_COMPACT_THRESHOLD_BYTES {
            info!(
                conversation_id,
                message_id,
                message_type = ?response.r#type,
                content_bytes,
                "Loaded full message content"
            );
        }

        Ok(response)
    }

    /// List artifacts for a conversation with durable status state.
    pub async fn list_artifacts(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<ConversationArtifactListResponse, ConversationError> {
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let mut items = self
            .conversation_repo
            .list_artifacts(conversation_id)
            .await?
            .into_iter()
            .map(row_to_artifact_response)
            .collect::<Result<Vec<_>, _>>()?;

        let mut legacy_items = self
            .conversation_repo
            .list_legacy_cron_trigger_messages(conversation_id)
            .await?
            .into_iter()
            .filter_map(|row| legacy_cron_trigger_to_artifact(row).ok())
            .collect::<Vec<_>>();

        items.append(&mut legacy_items);
        items.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });

        Ok(items)
    }

    /// Update the durable status of a conversation artifact and broadcast the upsert.
    pub async fn update_artifact(
        &self,
        user_id: &str,
        conversation_id: &str,
        artifact_id: &str,
        req: UpdateConversationArtifactRequest,
    ) -> Result<ConversationArtifactResponse, ConversationError> {
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let status = serde_json::to_value(req.status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .ok_or_else(|| ConversationError::internal("Failed to serialize artifact status"))?;

        let row = self
            .conversation_repo
            .update_artifact_status(conversation_id, artifact_id, &status, now_ms())
            .await?
            .ok_or_else(|| ConversationError::ArtifactNotFound {
                id: artifact_id.to_owned(),
            })?;

        let response = row_to_artifact_response(row)?;
        self.broadcaster.broadcast(WebSocketMessage::new(
            "conversation.artifact",
            serde_json::to_value(&response)
                .map_err(|e| ConversationError::internal(format!("Failed to serialize artifact event: {e}")))?,
        ));

        Ok(response)
    }

    /// Search messages across all conversations for the user.
    pub async fn search_messages(
        &self,
        user_id: &str,
        query: SearchMessagesQuery,
    ) -> Result<MessageSearchResponse, ConversationError> {
        if query.keyword.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "keyword must not be empty".into(),
            });
        }

        let page = query.page.unwrap_or(1);
        let page_size = query.page_size.unwrap_or(20);

        let result = self
            .conversation_repo
            .search_messages(user_id, &query.keyword, page, page_size)
            .await?;

        let items = result
            .items
            .into_iter()
            .map(|row| search_row_to_item(row, &self.workspace_root))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
        })
    }
}

// ── Confirmation System ─────────────────────────────────────────────

impl ConversationService {
    /// Get the list of pending confirmations for a conversation.
    pub async fn list_confirmations(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ConfirmationListResponse, ConversationError> {
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let agent = match task_manager.get_task(conversation_id) {
            Some(a) => a,
            None => return Ok(Vec::new()),
        };

        Ok(agent.get_confirmations())
    }

    /// Confirm a pending tool call.
    ///
    /// Sends the confirmation result to the agent and broadcasts a
    /// `confirmation.remove` WebSocket event.
    pub async fn confirm(
        &self,
        user_id: &str,
        conversation_id: &str,
        call_id: &str,
        req: ConfirmRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), ConversationError> {
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let agent = task_manager
            .get_task(conversation_id)
            .ok_or_else(|| ConversationError::ActiveAgentNotFound {
                conversation_id: conversation_id.to_owned(),
            })?;

        let confirmations = agent.get_confirmations();
        let conf_id = confirmations
            .iter()
            .find(|c| c.call_id == call_id)
            .map(|c| c.id.clone());

        agent.confirm(&req.msg_id, call_id, req.data, req.always_allow)?;

        if let Some(conf_id) = conf_id {
            let payload = serde_json::json!({
                "conversation_id": conversation_id,
                "id": conf_id,
            });
            let msg = WebSocketMessage::new("confirmation.remove", payload);
            self.broadcaster.broadcast(msg);
        }

        Ok(())
    }

    /// Check whether an action has been auto-approved in the current session.
    pub async fn check_approval(
        &self,
        user_id: &str,
        conversation_id: &str,
        action: &str,
        command_type: Option<&str>,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<ApprovalCheckResponse, ConversationError> {
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let approved = task_manager
            .get_task(conversation_id)
            .is_some_and(|agent| agent.check_approval(action, command_type));

        Ok(ApprovalCheckResponse { approved })
    }
}

// ── Message Flow (send / stop / warmup) ─────────────────────────────

impl ConversationService {
    /// Send a user message to the conversation.
    ///
    /// 1. Validates the conversation belongs to the user
    /// 2. Stores the user message (position: "right", status: "finish")
    /// 3. Claims the conversation in runtime state
    /// 4. Spawns background agent build/send and stream relay work
    /// 5. Returns immediately (202 Accepted semantics)
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn send_message(
        &self,
        user_id: &str,
        conversation_id: &str,
        req: SendMessageRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<SendMessageResponse, ConversationError> {
        if req.content.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "Message content must not be empty".into(),
            });
        }
        let send_started_at = now_ms();

        // Verify conversation exists and belongs to user
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        if let Some(team_id) = team_id_from_extra(&row.extra) {
            info!(
                conversation_id = %conversation_id,
                team_id = %team_id,
                outcome = "rejected",
                error_code = "FORBIDDEN",
                "Ordinary send rejected for team-owned conversation"
            );
            return Err(ConversationError::Forbidden {
                reason: "Team-owned conversations must be sent through Team API".into(),
            });
        }

        reject_deprecated_runtime_row(&row)?;

        let turn_id = Self::mint_turn_id();
        let turn_claim = self.runtime_state.try_claim_turn(conversation_id, &turn_id)?;

        // Store user message. `msg_id` is server-generated so the WebSocket
        // stream, DB row, and client-side message index all agree on the same
        // key. We reuse the same value for `id` (primary key) and `msg_id`
        // to preserve legacy callers that still rely on `id == msg_id`.
        let user_msg_id = Self::mint_msg_id();
        let user_msg = aionui_db::models::MessageRow {
            id: user_msg_id.clone(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(user_msg_id.clone()),
            r#type: "text".into(),
            content: serde_json::json!({ "content": req.content }).to_string(),
            position: Some("right".into()),
            status: Some("finish".into()),
            hidden: req.hidden,
            created_at: now_ms(),
        };
        if !self
            .runtime_persistence()
            .allows(conversation_id, RuntimeWriteKind::UserMessage)
        {
            let mut turn_claim = turn_claim;
            let was_deleting = turn_claim.release();
            self.complete_released_turn(conversation_id, &turn_id, was_deleting)
                .await;
            return Ok(self.send_message_response(conversation_id, user_msg_id, turn_id).await);
        }
        if let Err(e) = self.conversation_repo.insert_message(&user_msg).await {
            warn!(msg_id = %user_msg_id, error = %ErrorChain(&e), "Failed to insert user message");
            return Err(e.into());
        }

        info!(msg_id = %user_msg_id, "User message persisted");

        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.userCreated",
            serde_json::json!({
                "conversation_id": conversation_id,
                "msg_id": &user_msg_id,
                "content": &req.content,
                "position": "right",
                "status": "finish",
                "hidden": req.hidden,
                "created_at": user_msg.created_at,
            }),
        ));

        // Build task options from conversation row
        let mut build_opts = match self.build_task_options(&row).await {
            Ok(opts) => opts,
            Err(err) => {
                error!(
                    error_code = err.error_code(),
                    error = %ErrorChain(&err),
                    "Failed to build task options for message send"
                );
                let top_level_code = err.error_code();
                let send_error = AgentSendError::from_agent_error(err.to_agent_error());
                self.persist_and_broadcast_send_failure_tip(
                    conversation_id,
                    &turn_id,
                    &send_error,
                    Some(top_level_code),
                )
                .await;
                let mut turn_claim = turn_claim;
                let was_deleting = turn_claim.release();
                self.complete_released_turn(conversation_id, &turn_id, was_deleting)
                    .await;
                return Ok(self.send_message_response(conversation_id, user_msg_id, turn_id).await);
            }
        };
        self.apply_conversation_runtime_context(&mut build_opts, user_id, conversation_id);
        self.ensure_workspace_skill_links(&row, &build_opts).await;
        let stored_workspace = build_opts.context.workspace.stored_path.clone();

        let user_msg_id_ret = user_msg_id.clone();
        ConversationTurnOrchestrator::new(self.clone(), Arc::clone(task_manager)).spawn_user_turn(TurnStartInput {
            user_id: user_id.to_owned(),
            conversation: row,
            request: req,
            required_runtime_mode: None,
            build_options: build_opts,
            stored_workspace,
            turn_id: turn_id.clone(),
            turn_claim,
        });

        info!(
            conversation_id = %conversation_id,
            msg_id = %user_msg_id_ret,
            turn_id = %turn_id,
            elapsed_ms = now_ms().saturating_sub(send_started_at),
            "Message accepted, agent work scheduled"
        );
        Ok(self
            .send_message_response(conversation_id, user_msg_id_ret, turn_id)
            .await)
    }

    /// Run a conversation-backed agent turn without expressing it as the
    /// ordinary user-message API. This is used by upper-level domains that own
    /// their own message projection and scheduling semantics.
    #[tracing::instrument(skip_all, fields(user_id = %request.user_id, conversation_id = %request.conversation_id))]
    pub async fn run_agent_turn(
        &self,
        request: ConversationAgentTurnRequest,
    ) -> Result<ConversationAgentTurnOutcome, ConversationError> {
        if request.content.trim().is_empty() {
            return Err(ConversationError::BadRequest {
                reason: "Agent turn content must not be empty".into(),
            });
        }

        let row = self
            .conversation_repo
            .get(&request.conversation_id)
            .await?
            .filter(|r| r.user_id == request.user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: request.conversation_id.clone(),
            })?;

        reject_deprecated_runtime_row(&row)?;

        let turn_id = Self::mint_turn_id();
        let turn_claim = self.runtime_state.try_claim_turn(&request.conversation_id, &turn_id)?;
        if request.persist_user_message {
            let user_msg_id = Self::mint_msg_id();
            let user_msg = aionui_db::models::MessageRow {
                id: user_msg_id.clone(),
                conversation_id: request.conversation_id.clone(),
                msg_id: Some(user_msg_id),
                r#type: "text".into(),
                content: serde_json::json!({ "content": request.content }).to_string(),
                position: Some("right".into()),
                status: Some("finish".into()),
                hidden: request.user_message_hidden,
                created_at: now_ms(),
            };
            if self
                .runtime_persistence()
                .allows(&request.conversation_id, RuntimeWriteKind::UserMessage)
                && let Err(e) = self.conversation_repo.insert_message(&user_msg).await
            {
                warn!(
                    msg_id = %user_msg.id,
                    error = %ErrorChain(&e),
                    "Failed to insert agent turn user message"
                );
                let mut turn_claim = turn_claim;
                let was_deleting = turn_claim.release();
                self.complete_released_turn(&request.conversation_id, &turn_id, was_deleting)
                    .await;
                return Err(e.into());
            }
        }
        if let Some(on_started) = request.on_started.as_ref() {
            on_started(ConversationAgentTurnStarted {
                conversation_id: request.conversation_id.clone(),
                turn_id: turn_id.clone(),
            })
            .await;
        }

        let mut build_opts = match self.build_task_options(&row).await {
            Ok(opts) => opts,
            Err(err) => {
                let top_level_code = err.error_code();
                let send_error = AgentSendError::from_agent_error(err.to_agent_error());
                self.persist_and_broadcast_send_failure_tip(
                    &request.conversation_id,
                    &turn_id,
                    &send_error,
                    Some(top_level_code),
                )
                .await;
                let mut turn_claim = turn_claim;
                let was_deleting = turn_claim.release();
                self.complete_released_turn(&request.conversation_id, &turn_id, was_deleting)
                    .await;
                return Ok(ConversationAgentTurnOutcome {
                    conversation_id: request.conversation_id.clone(),
                    turn_id,
                    status: ConversationAgentTurnStatus::Failed,
                    error_message: Some(send_error_display_message(&send_error)),
                    runtime: self.runtime_summary_for(&request.conversation_id).await,
                });
            }
        };

        self.apply_conversation_runtime_context(&mut build_opts, &request.user_id, &request.conversation_id);
        self.ensure_workspace_skill_links(&row, &build_opts).await;
        let stored_workspace = build_opts.context.workspace.stored_path.clone();
        let conversation_id = request.conversation_id.clone();
        let result = ConversationTurnOrchestrator::new(self.clone(), self.task_manager.clone())
            .run_user_turn(TurnStartInput {
                user_id: request.user_id,
                conversation: row,
                request: SendMessageRequest {
                    content: request.content,
                    files: request.files,
                    inject_skills: request.inject_skills,
                    hidden: request.user_message_hidden,
                },
                required_runtime_mode: request.required_runtime_mode,
                build_options: build_opts,
                stored_workspace,
                turn_id: turn_id.clone(),
                turn_claim,
            })
            .await;

        Ok(ConversationAgentTurnOutcome {
            runtime: self.runtime_summary_for(&conversation_id).await,
            conversation_id,
            turn_id,
            status: match result.status {
                ConversationTurnStatus::Completed => ConversationAgentTurnStatus::Completed,
                ConversationTurnStatus::Failed => ConversationAgentTurnStatus::Failed,
            },
            error_message: result.error_message,
        })
    }

    pub async fn latest_conversation_error_message(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>, ConversationError> {
        let page = self
            .conversation_repo
            .list_messages_page(
                conversation_id,
                &MessagePageParams {
                    limit: 30,
                    direction: MessagePageDirection::InitialLatest,
                },
            )
            .await?;

        Ok(page.items.iter().rev().find_map(error_message_from_message_row))
    }

    pub(crate) async fn persist_and_broadcast_send_failure_tip(
        &self,
        conversation_id: &str,
        turn_id: &str,
        err: &AgentSendError,
        top_level_code: Option<&'static str>,
    ) {
        let Some(row) = self
            .persist_send_failure_tip(conversation_id, err, top_level_code)
            .await
        else {
            return;
        };

        let msg_id = row.msg_id.clone().unwrap_or_else(|| row.id.clone());
        let content_value: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::Value::String(row.content.clone()));
        self.broadcaster.broadcast(WebSocketMessage::new(
            "message.stream",
            serde_json::json!({
                "conversation_id": row.conversation_id,
                "msg_id": msg_id,
                "turn_id": turn_id,
                "type": row.r#type,
                "data": content_value,
                "position": row.position,
                "status": row.status,
                "hidden": row.hidden,
                "replace": true,
            }),
        ));
    }

    /// Insert a pre-built `MessageRow` into the conversation's message history
    /// and broadcast a `message.stream` event so live subscribers render it
    /// immediately.
    ///
    /// Used by paths outside the normal user→agent turn (e.g. the team
    /// scheduler writing an incoming teammate message as a left bubble in the
    /// target agent's conversation so the UI shows who spoke).
    pub async fn insert_raw_message(&self, row: &MessageRow) -> Result<(), ConversationError> {
        self.conversation_repo.insert_message(row).await?;

        let msg_id = row.msg_id.clone().unwrap_or_else(|| row.id.clone());
        let content_value: serde_json::Value =
            serde_json::from_str(&row.content).unwrap_or_else(|_| serde_json::Value::String(row.content.clone()));
        let payload = serde_json::json!({
            "conversation_id": row.conversation_id,
            "msg_id": msg_id,
            "type": row.r#type,
            "data": content_value,
            "position": row.position,
            "status": row.status,
            "hidden": row.hidden,
            "replace": true,
        });
        self.broadcaster
            .broadcast(WebSocketMessage::new("message.stream", payload));
        Ok(())
    }

    /// Stop the current streaming response for a conversation.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn cancel(
        &self,
        user_id: &str,
        conversation_id: &str,
        turn_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<CancelConversationResponse, ConversationError> {
        // Verify conversation exists and belongs to user
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let active_turn_id = self.runtime_state.active_turn_id_for(conversation_id);
        if active_turn_id.as_deref() != Some(turn_id) {
            info!(
                conversation_id,
                requested_turn_id = %turn_id,
                active_turn_id = active_turn_id.as_deref(),
                "cancel ignored because turn id mismatched"
            );
            return Ok(CancelConversationResponse {
                runtime: self.runtime_summary_for(conversation_id).await,
            });
        }

        let Some(agent) = task_manager.get_task(conversation_id) else {
            info!(
                conversation_id,
                turn_id, "No active agent to cancel; returning runtime summary"
            );
            return Ok(CancelConversationResponse {
                runtime: self.runtime_summary_for(conversation_id).await,
            });
        };

        self.runtime_state.mark_cancelling(conversation_id);
        if let Err(e) = agent.cancel().await {
            self.runtime_state.clear_cancelling(conversation_id);
            warn!(conversation_id, turn_id, error = %ErrorChain(&e), "Failed to cancel agent");
            return Err(e.into());
        }

        if agent.agent_type() == AgentType::Acp {
            let runtime_state = self.runtime_state();
            let task_manager = Arc::clone(task_manager);
            let conv_id = conversation_id.to_owned();
            let active_turn = turn_id.to_owned();

            tokio::spawn(async move {
                tokio::time::sleep(ACP_CANCEL_DRAIN_TIMEOUT).await;
                if runtime_state.active_turn_id_for(&conv_id).as_deref() == Some(active_turn.as_str())
                    && runtime_state.is_cancelling(&conv_id)
                {
                    warn!(
                        conversation_id = %conv_id,
                        turn_id = %active_turn,
                        timeout_ms = ACP_CANCEL_DRAIN_TIMEOUT.as_millis() as u64,
                        "ACP cancel did not drain before timeout; killing task"
                    );
                    task_manager
                        .kill_and_wait(&conv_id, Some(AgentKillReason::UserCancelTimeout))
                        .await;
                }
            });
        }

        info!(conversation_id, turn_id, "Stream cancel acknowledged");
        Ok(CancelConversationResponse {
            runtime: self.runtime_summary_for(conversation_id).await,
        })
    }

    pub async fn renew_active_lease(
        &self,
        user_id: &str,
        conversation_id: &str,
        active_leases: &ActiveLeaseRegistry,
    ) -> Result<(), ConversationError> {
        let row = match self.conversation_repo.get(conversation_id).await {
            Ok(row) => row,
            Err(error) => {
                warn!(
                    kind = "conversation",
                    conversation_id,
                    user_id,
                    error = %ErrorChain(&error),
                    "Conversation active lease renew failed"
                );
                return Err(error.into());
            }
        };

        let Some(row) = row.filter(|row| row.user_id == user_id) else {
            debug!(
                kind = "conversation",
                conversation_id, user_id, "Conversation active lease renew rejected"
            );
            return Err(ConversationError::NotFound {
                id: conversation_id.to_owned(),
            });
        };

        let expires_at = active_leases.renew(&row.id);
        debug!(
            kind = "conversation",
            conversation_id = %row.id,
            expires_at,
            "Conversation active lease renewed"
        );
        Ok(())
    }

    /// Pre-initialize an agent task for a conversation (warmup).
    ///
    /// This builds the agent task without sending a message, so the
    /// first real message can be processed faster.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn warmup(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), ConversationError> {
        let _ = self
            .ensure_runtime_agent(user_id, conversation_id, task_manager, "warmup")
            .await?;
        debug!("Agent warmed up");
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, conversation_id = %conversation_id))]
    pub async fn ensure_runtime(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<EnsureConversationRuntimeResponse, ConversationError> {
        let (agent, recovered) = self
            .ensure_runtime_agent(user_id, conversation_id, task_manager, "runtime_ensure")
            .await?;
        let config_options = agent
            .get_config_options()
            .await
            .map_err(ConversationError::from)?
            .config_options;

        Ok(EnsureConversationRuntimeResponse {
            recovered,
            config_options,
            runtime: self.runtime_summary_for(conversation_id).await,
        })
    }

    async fn ensure_runtime_agent(
        &self,
        user_id: &str,
        conversation_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
        phase: &'static str,
    ) -> Result<(AgentInstance, bool), ConversationError> {
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        reject_deprecated_runtime_row(&row)?;

        if let Some(agent) = task_manager.get_task(conversation_id) {
            debug!(conversation_id, phase, "Conversation runtime already active");
            return Ok((agent, false));
        }

        let mut build_opts = self.build_task_options(&row).await?;
        self.apply_conversation_runtime_context(&mut build_opts, user_id, conversation_id);
        self.ensure_workspace_skill_links(&row, &build_opts).await;
        let stored_workspace = build_opts.context.workspace.stored_path.clone();
        let backend = build_options_backend(&build_opts).map(str::to_owned);
        let agent = match task_manager.get_or_build_task(conversation_id, build_opts).await {
            Ok(agent) => agent,
            Err(err) => {
                let send_error = AgentSendError::from_agent_error_ref_for_backend(&err, backend.as_deref());
                if send_error.is_openclaw_gateway_unreachable() {
                    warn!(
                        conversation_id = %conversation_id,
                        backend = "openclaw",
                        error_kind = "openclaw_gateway_unreachable",
                        port = 18789_u16,
                        phase,
                        "OpenClaw Gateway unreachable during ACP startup"
                    );
                    let detail = send_error
                        .stream_error()
                        .detail
                        .clone()
                        .unwrap_or_else(|| send_error.stream_error().message.clone());
                    return Err(ConversationError::OpenClawGatewayUnreachable { detail });
                }
                return Err(err.into());
            }
        };

        // Persist auto-resolved workspace if factory picked a different path.
        self.maybe_persist_workspace(conversation_id, &stored_workspace, agent.workspace())
            .await?;

        info!(conversation_id, phase, "Conversation runtime recovered");
        Ok((agent, true))
    }
}

fn send_error_display_message(error: &AgentSendError) -> String {
    error
        .stream_error()
        .detail
        .as_deref()
        .filter(|detail| !detail.trim().is_empty())
        .unwrap_or(error.stream_error().message.as_str())
        .to_owned()
}

fn error_message_from_message_row(row: &MessageRow) -> Option<String> {
    if row.r#type != "tips" && row.status.as_deref() != Some("error") {
        return None;
    }

    let content: serde_json::Value = serde_json::from_str(&row.content).ok()?;
    let is_error_tip = content.get("type").and_then(serde_json::Value::as_str) == Some("error")
        || row.status.as_deref() == Some("error");
    if !is_error_tip {
        return None;
    }

    [
        content.pointer("/error/detail"),
        content.pointer("/details/detail"),
        content.get("details"),
        content.get("content"),
        content.pointer("/error/message"),
    ]
    .into_iter()
    .flatten()
    .filter_map(non_empty_json_string)
    .find(|message| message != "cron conversation turn failed")
}

fn non_empty_json_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

// ── Internal Helpers ────────────────────────────────────────────────

pub(crate) fn agent_error_top_level_code(error: &AgentError) -> &'static str {
    match error {
        AgentError::BadRequest(_) => "BAD_REQUEST",
        AgentError::Unauthorized(_) => "UNAUTHORIZED",
        AgentError::Forbidden(_) => "FORBIDDEN",
        AgentError::NotFound(_) => "NOT_FOUND",
        AgentError::Conflict(_) => "CONFLICT",
        AgentError::BadGateway(_) | AgentError::Acp(_) => "BAD_GATEWAY",
        AgentError::Timeout(_) => "TIMEOUT",
        AgentError::RateLimited => "RATE_LIMITED",
        AgentError::ConversationArchived(_) => "CONVERSATION_ARCHIVED",
        AgentError::WorkspacePathRuntimeUnavailable(_) => "WORKSPACE_PATH_RUNTIME_UNAVAILABLE",
        AgentError::Internal(_) => "INTERNAL_ERROR",
        _ => "INTERNAL_ERROR",
    }
}

impl ConversationService {
    /// Build typed agent runtime context from a conversation database row.
    ///
    /// Raw `conversation.extra` parsing lives in [`SessionContextBuilder`]
    /// so the task manager and concrete agent factories consume typed
    /// session context instead of the DB envelope.
    pub(crate) async fn build_task_options(
        &self,
        row: &aionui_db::models::ConversationRow,
    ) -> Result<BuildTaskOptions, ConversationError> {
        reject_deprecated_runtime_row(row)?;
        SessionContextBuilder::new(&self.workspace_root, &self.agent_metadata_repo, &self.acp_session_repo)
            .build_options(row)
            .await
    }

    pub async fn build_task_options_for_runtime(
        &self,
        row: &aionui_db::models::ConversationRow,
        workspace_override: Option<&str>,
    ) -> Result<BuildTaskOptions, ConversationError> {
        reject_deprecated_runtime_row(row)?;
        SessionContextBuilder::new(&self.workspace_root, &self.agent_metadata_repo, &self.acp_session_repo)
            .build_options_with_workspace_override(row, workspace_override)
            .await
    }

    fn apply_conversation_runtime_context(
        &self,
        build_opts: &mut BuildTaskOptions,
        user_id: &str,
        conversation_id: &str,
    ) {
        build_opts.apply_conversation_runtime_context(
            user_id,
            conversation_id,
            self.runtime_helper_bin.as_deref(),
            self.runtime_base_url.as_deref(),
        );
    }

    /// Ensure native skill links exist in the runtime workspace. Auto
    /// workspaces are constrained to AionUi's generated path; custom
    /// workspaces were validated when the session context was built.
    pub(crate) async fn ensure_workspace_skill_links(&self, row: &ConversationRow, build_opts: &BuildTaskOptions) {
        let context = &build_opts.context;
        let backend = context_backend_value(context);

        let workspace = PathBuf::from(context.workspace.path.trim());
        if !context.workspace.is_custom {
            let expected_workspace = expected_auto_workspace_path(
                &self.workspace_root,
                &row.id,
                &context.conversation.agent_type,
                backend.as_ref(),
            );

            if workspace != expected_workspace {
                return;
            }
        }

        let skill_names = context_skill_names(context);
        if skill_names.is_empty() {
            return;
        }

        let Some(rel_dirs) = native_skills_dirs(
            &self.agent_metadata_repo,
            &context.conversation.agent_type,
            backend.as_ref(),
        )
        .await
        else {
            return;
        };
        if rel_dirs.is_empty() {
            return;
        }

        let resolved = self.skill_resolver.resolve_skills(&skill_names).await;
        if resolved.is_empty() {
            return;
        }

        let rel_dirs_refs: Vec<&str> = rel_dirs.iter().map(String::as_str).collect();
        let n = self
            .skill_resolver
            .link_workspace_skills(&workspace, &rel_dirs_refs, &resolved)
            .await;
        debug!(
            conversation_id = %row.id,
            workspace = %workspace.display(),
            links = n,
            "ensured skill symlinks in auto workspace"
        );
    }

    /// Write the resolved workspace back to `conversation.extra.workspace` when
    /// the factory picked a different (auto-generated) path than what was stored.
    ///
    /// This handles legacy conversations whose `extra.workspace` was empty:
    /// the factory creates a temp dir at task-build time, and we persist that
    /// path here so the frontend can display the workspace panel correctly.
    pub(crate) async fn maybe_persist_workspace(
        &self,
        conversation_id: &str,
        stored_workspace: &str,
        resolved_workspace: &str,
    ) -> Result<(), ConversationError> {
        if resolved_workspace.is_empty() || resolved_workspace == stored_workspace {
            return Ok(());
        }
        if !self
            .runtime_persistence()
            .allows(conversation_id, RuntimeWriteKind::ResolvedWorkspace)
        {
            return Ok(());
        }

        // Fetch latest extra, merge the resolved workspace path in, and persist.
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .ok_or_else(|| ConversationError::internal("Conversation vanished during workspace sync"))?;

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        extra["workspace"] = serde_json::Value::String(resolved_workspace.to_owned());

        let extra_json = serde_json::to_string(&extra)
            .map_err(|e| ConversationError::internal(format!("Failed to serialize extra: {e}")))?;

        let update = ConversationRowUpdate {
            extra: Some(extra_json),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(conversation_id, &update).await?;

        debug!(
            conversation_id,
            workspace = resolved_workspace,
            "Persisted auto-resolved workspace to conversation.extra"
        );
        Ok(())
    }

    /// Broadcast a `conversation.listChanged` WebSocket event.
    pub(crate) fn broadcast_list_changed(
        &self,
        conversation_id: &str,
        action: &str,
        source: Option<&ConversationSource>,
    ) {
        let payload = serde_json::json!({
            "conversation_id": conversation_id,
            "action": action,
            "source": source,
        });
        let event = WebSocketMessage::new("conversation.listChanged", payload);
        self.broadcaster.broadcast(event);
    }

    pub(crate) fn skill_resolver(&self) -> Arc<dyn SkillResolver> {
        Arc::clone(&self.skill_resolver)
    }

    /// Backfill `extra.skills` if the row predates the snapshot model.
    /// Persists the mutation asynchronously; failures are logged and
    /// swallowed so a read path never 500s because of a backfill write
    /// failure.
    async fn backfill_extra_inplace(&self, conversation_id: &str, extra: &mut serde_json::Value) {
        let auto_inject = self.skill_resolver.auto_inject_names().await;
        let mut mutated = backfill_skills_if_missing(extra, &auto_inject);
        mutated |= backfill_cron_job_id_alias(extra);
        if !mutated {
            return;
        }
        let serialized = match serde_json::to_string(extra) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&e),
                    "backfill serialize failed; returning in-memory value"
                );
                return;
            }
        };
        let update = ConversationRowUpdate {
            extra: Some(serialized),
            ..Default::default()
        };
        if let Err(e) = self.conversation_repo.update(conversation_id, &update).await {
            warn!(
                conversation_id,
                error = %ErrorChain(&e),
                "backfill persist failed; returning in-memory value"
            );
        }
    }
}

fn backfill_cron_job_id_alias(extra: &mut serde_json::Value) -> bool {
    let Some(obj) = extra.as_object_mut() else {
        return false;
    };

    let cron_job_id = obj
        .get("cron_job_id")
        .and_then(|value| value.as_str())
        .or_else(|| obj.get("cronJobId").and_then(|value| value.as_str()))
        .map(ToOwned::to_owned);

    let Some(cron_job_id) = cron_job_id else {
        return false;
    };

    let mut mutated = false;
    if obj.get("cron_job_id").and_then(|value| value.as_str()) != Some(cron_job_id.as_str()) {
        obj.insert("cron_job_id".into(), serde_json::Value::String(cron_job_id.clone()));
        mutated = true;
    }
    if obj.get("cronJobId").and_then(|value| value.as_str()) != Some(cron_job_id.as_str()) {
        obj.insert("cronJobId".into(), serde_json::Value::String(cron_job_id));
        mutated = true;
    }

    mutated
}

fn normalize_workspace_extra(extra: &mut serde_json::Value) -> Result<(), ConversationError> {
    let Some(obj) = extra.as_object_mut() else {
        return Ok(());
    };
    let Some(workspace) = obj
        .get("workspace")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
    else {
        return Ok(());
    };
    if workspace.is_empty() {
        return Ok(());
    }

    let normalized = normalize_workspace_path(&workspace)?;
    if normalized != workspace.as_str() {
        obj.insert("workspace".to_owned(), serde_json::Value::String(normalized));
    }
    Ok(())
}

fn team_id_from_extra(extra: &str) -> Option<String> {
    TeamSessionBinding::team_id_marker_from_extra_str(extra)
}

fn normalize_workspace_path(workspace: &str) -> Result<String, ConversationError> {
    validate_workspace_path_availability(workspace).map_err(map_create_workspace_validation_error)
}

fn map_create_workspace_validation_error(error: WorkspacePathValidationError) -> ConversationError {
    match error {
        WorkspacePathValidationError::Empty => ConversationError::BadRequest {
            reason: "Workspace directory is empty".into(),
        },
        WorkspacePathValidationError::DoesNotExist(path)
        | WorkspacePathValidationError::NotDirectory(path)
        | WorkspacePathValidationError::NotAccessible { path, .. } => {
            ConversationError::WorkspacePathUnavailable { path }
        }
    }
}

// ── Helpers ────────────────────────────────────────────────────────

/// Compute the label used in auto-provisioned workspace directory names.
///
/// For ACP conversations the label is the vendor string from
/// `extra.backend` (e.g. `"claude"`); otherwise the `AgentType` serde
/// name (e.g. `"aionrs"`). Falls back to the agent type's serde name
/// when the backend field is missing or not a string.
fn conversation_label(agent_type: &AgentType, backend: Option<&serde_json::Value>) -> String {
    if *agent_type == AgentType::Acp
        && let Some(serde_json::Value::String(s)) = backend
        && !s.is_empty()
    {
        return s.clone();
    }
    agent_type.serde_name().to_owned()
}

fn expected_auto_workspace_path(
    workspace_root: &std::path::Path,
    conversation_id: &str,
    agent_type: &AgentType,
    backend: Option<&serde_json::Value>,
) -> PathBuf {
    auto_workspace_parent(workspace_root).join(format!(
        "{}-temp-{conversation_id}",
        conversation_label(agent_type, backend)
    ))
}

fn auto_workspace_parent(workspace_root: &Path) -> PathBuf {
    let now = chrono::Local::now();
    workspace_root
        .join("conversations")
        .join(format!("{:04}", now.year()))
        .join(format!("{:02}", now.month()))
        .join(format!("{:02}", now.day()))
}

fn auto_provisioned_workspace_to_delete(
    workspace_root: &Path,
    row: &ConversationRow,
    conversation_id: &str,
) -> Option<PathBuf> {
    let extra = serde_json::from_str::<serde_json::Value>(&row.extra).ok()?;
    let workspace = extra.get("workspace")?.as_str()?.trim();
    if workspace.is_empty() {
        return None;
    }

    let workspace_path = Path::new(workspace);
    if !workspace_path.is_absolute() {
        return None;
    }

    let conversations_root = workspace_root.join("conversations");
    let conversations_root = std::fs::canonicalize(conversations_root).ok()?;
    let workspace_path = std::fs::canonicalize(workspace_path).ok()?;
    let file_name = workspace_path.file_name()?.to_str()?;
    let expected_suffix = format!("-temp-{conversation_id}");
    if !file_name.ends_with(&expected_suffix) {
        return None;
    }

    let relative = workspace_path.strip_prefix(&conversations_root).ok()?;
    if !is_auto_workspace_relative_path(relative) {
        return None;
    }

    Some(workspace_path)
}

fn is_auto_workspace_relative_path(relative: &Path) -> bool {
    let parts = relative.iter().map(|part| part.to_str()).collect::<Option<Vec<_>>>();
    let Some(parts) = parts else {
        return false;
    };

    match parts.as_slice() {
        [_file_name] => true,
        [year, month, day, _file_name] => {
            year.len() == 4
                && month.len() == 2
                && day.len() == 2
                && year.chars().all(|ch| ch.is_ascii_digit())
                && month.chars().all(|ch| ch.is_ascii_digit())
                && day.chars().all(|ch| ch.is_ascii_digit())
        }
        _ => false,
    }
}

async fn cleanup_empty_date_workspace_parents(workspace_root: &Path, workspace_path: &Path) {
    let Some(date_dirs) = date_workspace_parent_dirs(workspace_root, workspace_path) else {
        return;
    };

    for dir in date_dirs {
        match tokio::fs::remove_dir(&dir).await {
            Ok(()) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                ) =>
            {
                break;
            }
            Err(err) => {
                warn!(
                    workspace_parent = %dir.display(),
                    error = %err,
                    "Failed to remove empty auto-provisioned workspace date directory"
                );
                break;
            }
        }
    }
}

fn date_workspace_parent_dirs(workspace_root: &Path, workspace_path: &Path) -> Option<[PathBuf; 3]> {
    let conversations_root = std::fs::canonicalize(workspace_root.join("conversations")).ok()?;
    let relative = workspace_path.strip_prefix(&conversations_root).ok()?;
    if !is_dated_auto_workspace_relative_path(relative) {
        return None;
    }

    let day_dir = workspace_path.parent()?.to_path_buf();
    let month_dir = day_dir.parent()?.to_path_buf();
    let year_dir = month_dir.parent()?.to_path_buf();
    Some([day_dir, month_dir, year_dir])
}

fn is_dated_auto_workspace_relative_path(relative: &Path) -> bool {
    let parts = relative.iter().map(|part| part.to_str()).collect::<Option<Vec<_>>>();
    let Some(parts) = parts else {
        return false;
    };

    matches!(
        parts.as_slice(),
        [year, month, day, _file_name]
            if year.len() == 4
                && month.len() == 2
                && day.len() == 2
                && year.chars().all(|ch| ch.is_ascii_digit())
                && month.chars().all(|ch| ch.is_ascii_digit())
                && day.chars().all(|ch| ch.is_ascii_digit())
    )
}

fn context_backend_value(context: &AgentSessionContext) -> Option<serde_json::Value> {
    match &context.kind {
        AgentSessionKind::Acp(acp) => acp
            .config
            .backend
            .as_ref()
            .filter(|value| !value.is_empty())
            .map(|value| serde_json::Value::String(value.clone())),
        _ => None,
    }
}

fn build_options_backend(options: &BuildTaskOptions) -> Option<&str> {
    match &options.context.kind {
        AgentSessionKind::Acp(ctx) => ctx.config.backend.as_deref(),
        AgentSessionKind::Aionrs(_) => None,
    }
}

fn context_skill_names(context: &AgentSessionContext) -> Vec<String> {
    context.skills.clone()
}

/// Resolve the native skills directory list for an agent by looking it
/// up in the `agent_metadata` catalog (ACP vendors) or the bundled
/// `AgentType` table (non-ACP built-ins).
///
/// Returns `None` when the agent does not support native skill
/// discovery — callers should then skip the workspace-symlink step and
/// rely on prompt injection instead.
async fn native_skills_dirs(
    repo: &Arc<dyn IAgentMetadataRepository>,
    agent_type: &AgentType,
    backend: Option<&serde_json::Value>,
) -> Option<Vec<String>> {
    if *agent_type == AgentType::Acp
        && let Some(serde_json::Value::String(vendor)) = backend
        && !vendor.is_empty()
    {
        let row = repo.find_builtin_by_backend(vendor).await.ok().flatten()?;
        let raw = row.native_skills_dirs?;
        return serde_json::from_str::<Vec<String>>(&raw).ok();
    }
    agent_type
        .native_skills_dirs()
        .map(|dirs| dirs.iter().map(|s| (*s).to_owned()).collect())
}

impl ConversationService {
    async fn resolve_mcp_support_policy(
        &self,
        agent_type: &AgentType,
        extra: &serde_json::Value,
    ) -> Result<McpSupportPolicy, ConversationError> {
        match agent_type {
            AgentType::Acp => resolve_acp_mcp_support_policy(&self.agent_metadata_repo, extra).await,
            AgentType::Aionrs => Ok(McpSupportPolicy::AIONRS),
            _ => Ok(McpSupportPolicy::AIONRS),
        }
    }
}

async fn resolve_acp_mcp_support_policy(
    repo: &Arc<dyn IAgentMetadataRepository>,
    extra: &serde_json::Value,
) -> Result<McpSupportPolicy, ConversationError> {
    let agent_id = extra
        .get("agent_id")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let backend = extra
        .get("backend")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty());
    let agent_source = extra
        .get("agent_source")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("builtin");

    let row = match agent_id {
        Some(id) => repo
            .get(id)
            .await
            .map_err(|e| ConversationError::internal(format!("agent_metadata lookup: {e}")))?,
        None if agent_source == "builtin" => match backend {
            Some(vendor) => repo
                .find_builtin_by_backend(vendor)
                .await
                .map_err(|e| ConversationError::internal(format!("agent_metadata lookup: {e}")))?,
            None => None,
        },
        None => None,
    };

    let capabilities = row
        .as_ref()
        .and_then(|row| row.agent_capabilities.as_deref())
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .map(|value| parse_acp_mcp_capabilities(&value))
        .unwrap_or_default();

    Ok(McpSupportPolicy::from_acp_capabilities(capabilities))
}

fn upsert_conversation_mcp_status(
    statuses: &mut Vec<ConversationMcpStatus>,
    status_index_by_name: &mut HashMap<String, usize>,
    status: ConversationMcpStatus,
) {
    if let Some(index) = status_index_by_name.get(&status.name).copied() {
        statuses[index] = status;
        return;
    }
    status_index_by_name.insert(status.name.clone(), statuses.len());
    statuses.push(status);
}

fn classify_repo_mcp_status(row: &aionui_db::models::McpServerRow, support: McpSupportPolicy) -> ConversationMcpStatus {
    if !support.supports_row_transport(&row.transport_type) {
        return ConversationMcpStatus {
            id: row.id.clone(),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Unsupported,
            reason: Some(format!(
                "transport '{}' is not supported by this agent",
                row.transport_type
            )),
        };
    }

    match validate_repo_transport(row.transport_type.as_str(), &row.transport_config) {
        Ok(()) => ConversationMcpStatus {
            id: row.id.clone(),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Loaded,
            reason: None,
        },
        Err(reason) => ConversationMcpStatus {
            id: row.id.clone(),
            name: row.name.clone(),
            status: ConversationMcpStatusKind::Failed,
            reason: Some(reason),
        },
    }
}

fn classify_session_mcp_status(server: &SessionMcpServer, support: McpSupportPolicy) -> ConversationMcpStatus {
    if !support.supports_session_transport(&server.transport) {
        let transport = match &server.transport {
            SessionMcpTransport::Stdio { .. } => "stdio",
            SessionMcpTransport::Http { .. } => "http",
            SessionMcpTransport::Sse { .. } => "sse",
            SessionMcpTransport::StreamableHttp { .. } => "streamable_http",
        };
        return ConversationMcpStatus {
            id: server.id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Unsupported,
            reason: Some(format!("transport '{transport}' is not supported by this agent")),
        };
    }

    match validate_session_transport(&server.transport) {
        Ok(()) => ConversationMcpStatus {
            id: server.id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Loaded,
            reason: None,
        },
        Err(reason) => ConversationMcpStatus {
            id: server.id.clone(),
            name: server.name.clone(),
            status: ConversationMcpStatusKind::Failed,
            reason: Some(reason),
        },
    }
}

fn validate_repo_transport(transport_type: &str, transport_config: &str) -> Result<(), String> {
    let value: serde_json::Value =
        serde_json::from_str(transport_config).map_err(|e| format!("invalid transport config: {e}"))?;

    match transport_type {
        "stdio" => {
            let command = value
                .get("command")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| "stdio transport is missing command".to_owned())?;
            validate_stdio_command(command)
        }
        "http" | "streamable_http" => validate_url_field("http", value.get("url").and_then(serde_json::Value::as_str)),
        "sse" => validate_url_field("sse", value.get("url").and_then(serde_json::Value::as_str)),
        other => Err(format!("unknown transport type: {other}")),
    }
}

fn validate_session_transport(transport: &SessionMcpTransport) -> Result<(), String> {
    match transport {
        SessionMcpTransport::Stdio { command, .. } => validate_stdio_command(command),
        SessionMcpTransport::Http { url, .. } => validate_url_field("http", Some(url)),
        SessionMcpTransport::Sse { url, .. } => validate_url_field("sse", Some(url)),
        SessionMcpTransport::StreamableHttp { url, .. } => validate_url_field("streamable_http", Some(url)),
    }
}

fn validate_stdio_command(command: &str) -> Result<(), String> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err("stdio transport is missing command".to_owned());
    }

    match probe_runtime_command(trimmed) {
        RuntimeCommandProbe::ExplicitPath { path } => {
            if path.exists() {
                return Ok(());
            }
            Err(format!("command '{trimmed}' does not exist"))
        }
        RuntimeCommandProbe::NodeTool { .. } => {
            let support = probe_node_runtime_supported();
            if support.is_supported() {
                Ok(())
            } else {
                Err(format!("command '{trimmed}' is unavailable: {}", support.detail))
            }
        }
        RuntimeCommandProbe::PathLookup { command } => {
            if resolve_command_path(&command).is_some() {
                Ok(())
            } else {
                Err(format!("command '{command}' was not found in PATH"))
            }
        }
    }
}

fn validate_url_field(transport: &str, url: Option<&str>) -> Result<(), String> {
    match url.map(str::trim).filter(|value| !value.is_empty()) {
        Some(_) => Ok(()),
        None => Err(format!("{transport} transport is missing url")),
    }
}

/// Serialize a serde-compatible enum to its JSON string form for DB storage.
///
/// e.g. `AgentType::Acp` → `"acp"`
fn enum_to_db<T: serde::Serialize>(val: &T) -> Result<String, ConversationError> {
    let json_val = serde_json::to_value(val)
        .map_err(|e| ConversationError::internal(format!("Enum serialization failed: {e}")))?;
    json_val
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| ConversationError::internal("Expected string enum value"))
}

/// Persist the agent's session key into `conversation.extra.sessionKey`.
///
/// Called after send_message completes so the session can be resumed
/// when the user re-enters this conversation later.
pub(crate) async fn persist_session_key(
    repo: &Arc<dyn IConversationRepository>,
    persistence: &RuntimePersistenceCoordinator,
    conversation_id: &str,
    session_key: &str,
) {
    if !persistence.allows(conversation_id, RuntimeWriteKind::SessionKey) {
        return;
    }

    let row = match repo.get(conversation_id).await {
        Ok(Some(r)) => r,
        _ => return,
    };

    let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));

    if extra.get("sessionKey").and_then(|v| v.as_str()) == Some(session_key) {
        return;
    }

    extra["sessionKey"] = serde_json::Value::String(session_key.to_owned());

    let extra_json = match serde_json::to_string(&extra) {
        Ok(j) => j,
        Err(e) => {
            warn!(conversation_id, error = %ErrorChain(&e), "Failed to serialize extra for session key persist");
            return;
        }
    };

    let update = ConversationRowUpdate {
        extra: Some(extra_json),
        updated_at: Some(now_ms()),
        ..Default::default()
    };
    if let Err(e) = repo.update(conversation_id, &update).await {
        warn!(conversation_id, error = %ErrorChain(&e), "Failed to persist session key");
    } else {
        debug!(conversation_id, "Persisted session key to conversation.extra");
    }
}

fn legacy_cron_trigger_to_artifact(row: MessageRow) -> Result<ConversationArtifactResponse, ConversationError> {
    let payload: serde_json::Value = serde_json::from_str(&row.content)
        .map_err(|e| ConversationError::internal(format!("Invalid legacy cron trigger payload JSON: {e}")))?;
    let cron_job_id = payload
        .get("cron_job_id")
        .or_else(|| payload.get("cronJobId"))
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);

    Ok(ConversationArtifactResponse {
        id: format!("legacy-cron-trigger:{}", row.id),
        conversation_id: row.conversation_id,
        cron_job_id,
        kind: ConversationArtifactKind::CronTrigger,
        status: ConversationArtifactStatus::Active,
        payload,
        created_at: row.created_at,
        updated_at: row.created_at,
    })
}

/// Merge `patch` into `base` (top-level key overwrite).
fn merge_json(base: &mut serde_json::Value, patch: &serde_json::Value) {
    if let (Some(base_obj), Some(patch_obj)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_obj {
            base_obj.insert(key.clone(), value.clone());
        }
    }
}

fn parse_json_string_list(raw: Option<&str>, field: &str) -> Result<Vec<String>, ConversationError> {
    match raw {
        Some(value) if !value.trim().is_empty() => serde_json::from_str(value)
            .map_err(|e| ConversationError::internal(format!("failed to parse assistant field {field}: {e}"))),
        _ => Ok(Vec::new()),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct AssistantLineage<'a> {
    agent_type: &'a str,
    preset_assistant_id: &'a str,
    custom_agent_id: &'a str,
    agent_id: &'a str,
    agent_name: &'a str,
    backend: &'a str,
    current_model_id: &'a str,
    session_mode: &'a str,
}

impl<'a> AssistantLineage<'a> {
    fn from_response_and_extra(response: &'a ConversationResponse, extra: &'a serde_json::Value) -> Self {
        fn s<'a>(extra: &'a serde_json::Value, key: &str) -> &'a str {
            extra.get(key).and_then(serde_json::Value::as_str).unwrap_or("")
        }
        Self {
            agent_type: response.r#type.serde_name(),
            preset_assistant_id: s(extra, "preset_assistant_id"),
            custom_agent_id: s(extra, "custom_agent_id"),
            agent_id: s(extra, "agent_id"),
            agent_name: s(extra, "agent_name"),
            backend: s(extra, "backend"),
            current_model_id: s(extra, "current_model_id"),
            session_mode: s(extra, "session_mode"),
        }
    }

    fn has_any_identity(&self) -> bool {
        !self.preset_assistant_id.is_empty()
            || !self.custom_agent_id.is_empty()
            || !self.agent_id.is_empty()
            || !self.agent_name.is_empty()
    }
}

fn log_conversation_created(response: &ConversationResponse, extra: &serde_json::Value) {
    let lineage = AssistantLineage::from_response_and_extra(response, extra);
    if lineage.has_any_identity() {
        info!(
            conversation_id = %response.id,
            agent_type = lineage.agent_type,
            preset_assistant_id = lineage.preset_assistant_id,
            custom_agent_id = lineage.custom_agent_id,
            agent_id = lineage.agent_id,
            agent_name = lineage.agent_name,
            backend = lineage.backend,
            current_model_id = lineage.current_model_id,
            session_mode = lineage.session_mode,
            "Conversation created from assistant"
        );
    } else {
        info!(
            conversation_id = %response.id,
            agent_type = lineage.agent_type,
            "Conversation created (no assistant)"
        );
    }
}

fn is_tool_message_type(message_type: MessageType) -> bool {
    matches!(
        message_type,
        MessageType::ToolCall | MessageType::ToolGroup | MessageType::AcpToolCall
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn enum_to_db_agent_type() {
        use aionui_common::AgentType;
        assert_eq!(enum_to_db(&AgentType::Acp).unwrap(), "acp");
        assert_eq!(enum_to_db(&AgentType::Nanobot).unwrap(), "nanobot");
        assert_eq!(enum_to_db(&AgentType::OpenclawGateway).unwrap(), "openclaw-gateway");
    }

    #[test]
    fn enum_to_db_status() {
        assert_eq!(enum_to_db(&ConversationStatus::Pending).unwrap(), "pending");
        assert_eq!(enum_to_db(&ConversationStatus::Running).unwrap(), "running");
        assert_eq!(enum_to_db(&ConversationStatus::Finished).unwrap(), "finished");
    }

    #[test]
    fn enum_to_db_source() {
        assert_eq!(enum_to_db(&ConversationSource::Aionui).unwrap(), "aionui");
        assert_eq!(enum_to_db(&ConversationSource::Telegram).unwrap(), "telegram");
    }

    #[test]
    fn merge_json_top_level_overwrite() {
        let mut base = json!({"a": 1, "b": 2});
        let patch = json!({"b": 3, "c": 4});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"a": 1, "b": 3, "c": 4}));
    }

    #[test]
    fn merge_json_into_empty() {
        let mut base = json!({});
        let patch = json!({"x": "hello"});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"x": "hello"}));
    }

    #[test]
    fn merge_json_non_object_noop() {
        let mut base = json!("string");
        let patch = json!({"a": 1});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!("string"));
    }

    #[test]
    fn merge_json_empty_patch() {
        let mut base = json!({"a": 1});
        let patch = json!({});
        merge_json(&mut base, &patch);
        assert_eq!(base, json!({"a": 1}));
    }

    fn response_with_type(agent_type: aionui_common::AgentType) -> ConversationResponse {
        ConversationResponse {
            id: "conv-1".into(),
            name: "test".into(),
            r#type: agent_type,
            model: None,
            status: ConversationStatus::Pending,
            runtime: None,
            source: None,
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            assistant: None,
            created_at: 0,
            modified_at: 0,
            extra: json!({}),
        }
    }

    #[test]
    fn assistant_lineage_extracts_acp_builtin_fields() {
        use aionui_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "agent_id": "abc-123",
            "agent_name": "Claude Code",
            "backend": "claude",
            "current_model_id": "opus",
            "session_mode": "default",
        });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(lineage.agent_id, "abc-123");
        assert_eq!(lineage.agent_name, "Claude Code");
        assert_eq!(lineage.backend, "claude");
        assert_eq!(lineage.current_model_id, "opus");
        assert_eq!(lineage.session_mode, "default");
        assert_eq!(lineage.preset_assistant_id, "");
        assert_eq!(lineage.custom_agent_id, "");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_extracts_aionrs_preset_id() {
        use aionui_common::AgentType;
        let response = response_with_type(AgentType::Aionrs);
        let extra = json!({ "preset_assistant_id": "preset-xyz" });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "aionrs");
        assert_eq!(lineage.preset_assistant_id, "preset-xyz");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_extracts_acp_custom_agent_id() {
        use aionui_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "custom_agent_id": "custom-1",
            "backend": "openrouter",
        });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert_eq!(lineage.custom_agent_id, "custom-1");
        assert_eq!(lineage.backend, "openrouter");
        assert!(lineage.has_any_identity());
    }

    #[test]
    fn assistant_lineage_no_identity_when_extra_lacks_assistant_fields() {
        use aionui_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({ "workspace": "/project" });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_type, "acp");
        assert!(!lineage.has_any_identity());
    }

    #[test]
    fn validate_stdio_command_accepts_bare_npx_when_runtime_supports_it() {
        let result = validate_stdio_command("npx");
        assert!(
            result.is_ok(),
            "bare npx should be accepted when managed runtime is supported"
        );
    }

    #[test]
    fn assistant_lineage_treats_non_string_fields_as_missing() {
        use aionui_common::AgentType;
        let response = response_with_type(AgentType::Acp);
        let extra = json!({
            "agent_id": 42,
            "agent_name": null,
        });
        let lineage = AssistantLineage::from_response_and_extra(&response, &extra);
        assert_eq!(lineage.agent_id, "");
        assert_eq!(lineage.agent_name, "");
        assert!(!lineage.has_any_identity());
    }

    #[test]
    fn classify_session_mcp_status_marks_unsupported_transport() {
        let status = classify_session_mcp_status(
            &SessionMcpServer {
                id: "mcp-http".into(),
                name: "remote-http".into(),
                transport: SessionMcpTransport::Http {
                    url: "https://example.com/mcp".into(),
                    headers: HashMap::new(),
                },
            },
            McpSupportPolicy {
                stdio: true,
                http: false,
                sse: false,
                streamable_http: false,
            },
        );

        assert_eq!(status.status, ConversationMcpStatusKind::Unsupported);
    }

    #[test]
    fn classify_session_mcp_status_marks_missing_stdio_command_failed() {
        let status = classify_session_mcp_status(
            &SessionMcpServer {
                id: "mcp-stdio".into(),
                name: "broken-stdio".into(),
                transport: SessionMcpTransport::Stdio {
                    command: "__definitely_missing_aionui_mcp_command__".into(),
                    args: Vec::new(),
                    env: HashMap::new(),
                },
            },
            McpSupportPolicy::AIONRS,
        );

        assert_eq!(status.status, ConversationMcpStatusKind::Failed);
    }
}
