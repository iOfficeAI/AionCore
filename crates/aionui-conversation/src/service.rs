use std::path::PathBuf;
use std::sync::Arc;

use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
use aionui_ai_agent::{AgentInstance, IWorkerTaskManager};

use crate::response_middleware::ICronService;
use crate::runtime_state::ConversationRuntimeStateService;
use aionui_api_types::{
    ApprovalCheckResponse, CloneConversationRequest, ConfirmRequest, ConfirmationListResponse,
    ConversationArtifactKind, ConversationArtifactListResponse, ConversationArtifactResponse,
    ConversationArtifactStatus, ConversationListResponse, ConversationMcpStatus, ConversationMcpStatusKind,
    ConversationResponse, ConversationRuntimeSummary, CreateConversationRequest, ListConversationsQuery,
    ListMessagesQuery, MessageListResponse, MessageResponse, MessageSearchResponse, SearchMessagesQuery,
    SendMessageRequest, SessionMcpServer, SessionMcpTransport, UpdateConversationArtifactRequest,
    UpdateConversationRequest, WebSocketMessage,
};
use aionui_common::{
    AgentType, AppError, ConversationSource, ConversationStatus, ErrorChain, MessageType, OnConversationDelete,
    PaginatedResult, generate_short_id, now_ms, workspace_path_has_whitespace_segment,
};
use aionui_db::models::{ConversationRow, MessageRow};
use aionui_db::{
    ConversationFilters, ConversationRowUpdate, CreateAcpSessionParams, IAcpSessionRepository,
    IAgentMetadataRepository, IConversationRepository, IMcpServerRepository, SaveRuntimeStateParams, SortOrder,
};
use aionui_mcp::{AcpMcpCapabilities, parse_acp_mcp_capabilities};
use aionui_realtime::EventBroadcaster;
use aionui_runtime::{RuntimeCommandProbe, probe_node_runtime_supported, probe_runtime_command, resolve_command_path};
use std::collections::{HashMap, HashSet};
use tokio::sync::oneshot;
use tracing::{debug, error, info, warn};

use crate::convert::{
    TOOL_CONTENT_COMPACT_THRESHOLD_BYTES, row_to_artifact_response, row_to_message_response,
    row_to_message_response_compact, row_to_response, row_to_response_with_extra, search_row_to_item, string_to_enum,
};
use crate::error::ConversationError;
use crate::skill_resolver::SkillResolver;
use crate::skill_snapshot::{backfill_skills_if_missing, compute_initial_skills};
use crate::stream_relay::StreamRelay;
use std::sync::RwLock;

const MAX_CRON_CONTINUATIONS_PER_TURN: usize = 4;

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

#[derive(Clone)]
pub struct ConversationService {
    workspace_root: PathBuf,
    broadcaster: Arc<dyn EventBroadcaster>,
    skill_resolver: Arc<dyn SkillResolver>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    /// Hooks invoked during `delete()` before the DB row is removed so other services
    /// (`WorkerTaskManagerImpl`, `CronService`, …) can clean up their
    /// per-conversation state. Wrapped in `Arc<RwLock<…>>` so registration
    /// can happen post-construction without breaking the `Clone` impl —
    /// mirrors the `cron_service` slot pattern below.
    delete_hooks: Arc<RwLock<Vec<Arc<dyn OnConversationDelete>>>>,
    cron_service: Arc<RwLock<Option<Arc<dyn ICronService>>>>,
    mcp_server_repo: Arc<RwLock<Option<Arc<dyn IMcpServerRepository>>>>,
    runtime_state: Arc<ConversationRuntimeStateService>,

    // Repos for conversation, acp_session and agent_metadata access.
    conversation_repo: Arc<dyn IConversationRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    acp_session_repo: Arc<dyn IAcpSessionRepository>,
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
            cron_service: Arc::new(RwLock::new(None)),
            mcp_server_repo: Arc::new(RwLock::new(None)),
            runtime_state: Arc::new(ConversationRuntimeStateService::default()),

            conversation_repo,
            agent_metadata_repo,
            acp_session_repo,
        }
    }

    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = runtime_state;
        self
    }

    pub fn with_cron_service(&self, cron_service: Option<Arc<dyn ICronService>>) {
        if let Ok(mut guard) = self.cron_service.write() {
            *guard = cron_service;
        }
    }

    pub fn with_mcp_server_repo(&self, repo: Arc<dyn IMcpServerRepository>) {
        if let Ok(mut guard) = self.mcp_server_repo.write() {
            *guard = Some(repo);
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

    pub fn conversation_repo(&self) -> &Arc<dyn IConversationRepository> {
        &self.conversation_repo
    }

    pub(crate) fn acp_session_repo(&self) -> &Arc<dyn IAcpSessionRepository> {
        &self.acp_session_repo
    }

    pub fn runtime_state(&self) -> Arc<ConversationRuntimeStateService> {
        self.runtime_state.clone()
    }

    pub(crate) fn task(&self, conversation_id: &str) -> Result<AgentInstance, ConversationError> {
        self.task_manager
            .get_task(conversation_id)
            .ok_or_else(|| ConversationError::ActiveAgentNotFound {
                conversation_id: conversation_id.to_owned(),
            })
    }

    pub async fn runtime_summary_for(&self, conversation_id: &str) -> ConversationRuntimeSummary {
        let agent = self.task_manager.get_task(conversation_id);
        let has_task = agent.is_some();
        let task_status = agent.as_ref().and_then(|agent| agent.status());
        let pending_confirmations = agent.as_ref().map(|agent| agent.get_confirmations().len()).unwrap_or(0);

        self.runtime_state
            .summary_from_parts(conversation_id, task_status, has_task, pending_confirmations)
    }

    pub async fn complete_turn(&self, conversation_id: &str) {
        if self.runtime_state.is_deleting(conversation_id) {
            debug!(
                conversation_id,
                "Skipping turn completion because conversation is deleting"
            );
            return;
        }

        let runtime = self.runtime_summary_for(conversation_id).await;
        StreamRelay::complete_conversation_with_runtime(
            &self.conversation_repo,
            &self.broadcaster,
            conversation_id,
            Some(runtime),
        )
        .await;
    }

    async fn complete_released_turn(&self, conversation_id: &str, was_deleting: bool) {
        if was_deleting {
            debug!(
                conversation_id,
                "Skipping turn completion because conversation was deleting at claim release"
            );
            return;
        }

        self.complete_turn(conversation_id).await;
    }
}

// ── Conversation CRUD ───────────────────────────────────────────────

impl ConversationService {
    /// Create a new conversation.
    ///
    /// Generates a UUID v7, sets status to `pending`, defaults source
    /// to `aionui`, and broadcasts `conversation.listChanged(created)`.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, agent_type = ?req.r#type))]
    pub async fn create(
        &self,
        user_id: &str,
        req: CreateConversationRequest,
    ) -> Result<ConversationResponse, ConversationError> {
        let id = generate_short_id();
        let now = now_ms();
        let source = req.source.unwrap_or(ConversationSource::Aionui);

        // Type-aware rule: top-level `model` is aionrs-only. Other agent types
        // carry model/mode via `extra` (see spec 2026-05-12). Reject early so
        // clients that still ship the legacy shape get a loud 400 instead of
        // a silent write to a column nobody reads.
        if req.r#type != AgentType::Aionrs && req.model.is_some() {
            return Err(ConversationError::BadRequest {
                reason: format!(
                    "top-level `model` is only accepted for aionrs conversations; pass model via `extra` for {}",
                    req.r#type.serde_name()
                ),
            });
        }

        let mut extra = req.extra;

        // aionrs source-of-truth rule: top-level `model` wins. If an older client
        // still packs `extra.model`, strip it before persist so the stored row
        // has a single canonical model representation.
        if req.r#type == AgentType::Aionrs
            && let Some(obj) = extra.as_object_mut()
            && obj.remove("model").is_some()
        {
            warn!("aionrs create: stripped legacy `extra.model`; top-level `model` is canonical");
        }

        // Determine whether the user chose this workspace ("custom") or we
        // auto-provision one under `{data_dir}/conversations/{label}-temp-{id}/`.
        // `is_custom_workspace` is the authoritative signal consumed later to
        // decide whether we should wire skill symlinks (temp workspaces only
        // — user-chosen paths must not be mutated).
        let user_supplied_workspace = match extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(workspace) => Some(normalize_workspace_path(workspace)?),
            None => None,
        };
        let is_custom_workspace = user_supplied_workspace.is_some();
        if let Some(workspace) = user_supplied_workspace.as_ref() {
            extra["workspace"] = serde_json::Value::String(workspace.clone());
        }

        let auto_provisioned_workspace = if user_supplied_workspace.is_none() {
            // Per-conversation temp workspaces live under
            // `{data_dir}/conversations/{label}-temp-{id}/`. The label lets
            // operators eyeball the agent type; the conversation id keeps
            // the mapping back to the DB row unique.
            let label = conversation_label(&req.r#type, extra.get("backend"));
            let ws_path = self
                .workspace_root
                .join("conversations")
                .join(format!("{label}-temp-{id}"));
            std::fs::create_dir_all(&ws_path)
                .map_err(|e| AppError::Internal(format!("Failed to create workspace: {e}")))?;
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

        let (preset_enabled, exclude_auto_inject) = match extra.as_object_mut() {
            Some(obj) => {
                let preset = take_string_array(obj, &["preset_enabled_skills", "enabled_skills"]);
                let exclude = take_string_array(obj, &["exclude_auto_inject_skills", "exclude_builtin_skills"]);
                // Strip the stale cache field if a clone copied it in.
                obj.remove("loaded_skills");
                (preset, exclude)
            }
            None => (Vec::new(), Vec::new()),
        };

        let auto_inject_names = self.skill_resolver.auto_inject_names().await;
        let initial_skills = compute_initial_skills(&auto_inject_names, &preset_enabled, &exclude_auto_inject);

        // Wire skill symlinks into the auto-provisioned workspace so the
        // agent CLI picks them up via its native skills dir (e.g.
        // `.claude/skills/`). Runs only for temp workspaces — a user-chosen
        // path must not be mutated.
        if let Some(ws_path) = auto_provisioned_workspace.as_ref()
            && !is_custom_workspace
            && !initial_skills.is_empty()
            && let Some(rel_dirs) =
                native_skills_dirs(&self.agent_metadata_repo, &req.r#type, extra.get("backend")).await
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
                if has_selection { Some(ids) } else { None }
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

        let mcp_support = self.resolve_mcp_support_policy(&req.r#type, &extra).await?;
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
                    .map_err(|e| AppError::Internal(format!("Failed to load selected MCP servers: {e}")))?,
                None => repo
                    .list()
                    .await
                    .map_err(|e| AppError::Internal(format!("Failed to list MCP servers: {e}")))?,
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
                serde_json::to_value(&selected_mcp_statuses)
                    .map_err(|e| AppError::Internal(format!("Failed to serialize MCP status snapshot: {e}")))?,
            );
            if let Some(session_servers) = selected_session_mcp_servers.as_ref() {
                obj.insert(
                    "session_mcp_servers".to_owned(),
                    serde_json::to_value(session_servers)
                        .map_err(|e| AppError::Internal(format!("Failed to serialize session MCP snapshot: {e}")))?,
                );
            }
        }

        let row = aionui_db::models::ConversationRow {
            id: id.clone(),
            user_id: user_id.to_owned(),
            name: req.name.unwrap_or_default(),
            r#type: enum_to_db(&req.r#type)?,
            extra: serde_json::to_string(&extra)
                .map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?,
            model: req
                .model
                .as_ref()
                .map(serde_json::to_string)
                .transpose()
                .map_err(|e| AppError::Internal(format!("Failed to serialize model: {e}")))?,
            status: Some(enum_to_db(&ConversationStatus::Pending)?),
            source: Some(enum_to_db(&source)?),
            channel_chat_id: req.channel_chat_id,
            pinned: false,
            pinned_at: None,
            created_at: now,
            updated_at: now,
        };

        self.conversation_repo.create(&row).await?;

        // ACP conversations own one `acp_session` row (1:1 by
        // conversation_id). Other agent types have no session-level
        // state so we only create it for ACP.
        if req.r#type == AgentType::Acp {
            self.create_acp_session_row(&id, &extra).await?;
        }

        let response = row_to_response(row, &self.workspace_root)?;

        self.broadcast_list_changed(&response.id, "created", response.source.as_ref());

        log_conversation_created(&response, &extra);

        Ok(response)
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    async fn create_acp_session_row(&self, conversation_id: &str, extra: &serde_json::Value) -> Result<(), AppError> {
        debug!("Creating acp_session row");

        // Identity comes from the user's agent choice in `extra`.
        // `agent_id` is the catalog row id; `backend` is the vendor
        // label; `agent_source` says builtin/extension/custom. The
        // frontend always posts agent_id for picked rows, but older
        // payloads may only carry `backend`, so we resolve defensively.
        let agent_id_from_extra = extra.get("agent_id").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
        let backend = extra.get("backend").and_then(|v| v.as_str()).unwrap_or_default();
        let agent_source = extra.get("agent_source").and_then(|v| v.as_str()).unwrap_or("builtin");

        // Fallback: older clients (electron main, legacy webhooks) only
        // post `backend` without `agent_id`. Resolve the builtin row for
        // that vendor so the session still has a concrete catalog
        // reference. Non-builtin agents must provide `agent_id`
        // explicitly — custom/extension rows have no unique lookup key
        // from `(backend, agent_source)` alone.
        let resolved_agent_id = match agent_id_from_extra {
            Some(id) => id.to_owned(),
            None if !backend.is_empty() && agent_source == "builtin" => self
                .agent_metadata_repo
                .find_builtin_by_backend(backend)
                .await
                .map_err(|e| AppError::Internal(format!("agent_metadata lookup: {e}")))?
                .map(|row| row.id)
                .unwrap_or_default(),
            None => String::new(),
        };

        let params = CreateAcpSessionParams {
            conversation_id,
            agent_backend: backend,
            agent_source,
            agent_id: &resolved_agent_id,
        };
        self.acp_session_repo
            .create(&params)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to create acp_session row: {e}")))?;

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
                .map_err(|e| AppError::Internal(format!("Failed to seed acp_session runtime state: {e}")))?;
        }
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

        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;
        self.backfill_extra_inplace(&row.id, &mut extra).await;
        let mut response = row_to_response_with_extra(row, extra, &self.workspace_root)?;
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
                Ok(resp) => items.push(resp),
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

        // Type-aware rule: top-level `model` is aionrs-only. For non-aionrs
        // conversations, model/mode must be updated via `extra` (see spec
        // 2026-05-12).
        let existing_type: AgentType = string_to_enum(&existing.r#type)?;
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
                    .map_err(|e| AppError::Internal(format!("Failed to serialize merged extra: {e}")))?,
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
                    .map_err(|e| AppError::Internal(format!("Failed to serialize model: {e}")))
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
            .ok_or_else(|| AppError::Internal("Conversation vanished after update".into()))?;

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
                    .map_err(|e| AppError::Internal(format!("Failed to serialize merged extra: {e}")))?,
            ),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        self.conversation_repo.update(conversation_id, &updates).await?;
        debug!("Conversation extra merged");
        Ok(())
    }

    pub async fn save_acp_runtime_mode(&self, conversation_id: &str, mode: &str) -> Result<(), ConversationError> {
        let params = SaveRuntimeStateParams {
            current_mode_id: Some(Some(mode)),
            ..Default::default()
        };
        self.acp_session_repo
            .save_runtime_state(conversation_id, &params)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to persist runtime mode: {e}")))?;
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
        rows.into_iter()
            .map(|row| row_to_response(row, &self.workspace_root))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ConversationError::from)
    }

    /// List conversations spawned by a specific cron job.
    pub async fn list_by_cron_job(
        &self,
        user_id: &str,
        cron_job_id: &str,
    ) -> Result<Vec<ConversationResponse>, ConversationError> {
        let rows = self.conversation_repo.list_by_cron_job(user_id, cron_job_id).await?;
        rows.into_iter()
            .map(|row| row_to_response(row, &self.workspace_root))
            .collect::<Result<Vec<_>, _>>()
            .map_err(ConversationError::from)
    }
}

// ── Messages & Artifacts ────────────────────────────────────────────

impl ConversationService {
    /// List messages for a conversation with page-based pagination.
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

        let page = query.page.unwrap_or(1);
        let page_size = query.page_size.unwrap_or(50);
        let order = match query.order.as_deref() {
            Some("DESC" | "desc") => SortOrder::Desc,
            _ => SortOrder::Asc,
        };
        let compact_content = matches!(query.content_mode.as_deref(), Some("compact"));

        let result = self
            .conversation_repo
            .get_messages(conversation_id, page, page_size, order)
            .await?;

        let mut compacted_count = 0usize;
        let mut total_original_content_bytes = 0usize;
        let mut total_response_content_bytes = 0usize;
        let mut items = Vec::with_capacity(result.items.len());
        for row in result.items {
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
                page,
                page_size,
                order = ?order,
                items = items.len(),
                total = result.total,
                compacted = compacted_count,
                total_original_content_bytes,
                total_response_content_bytes,
                "Compacted tool message list response"
            );
        }

        Ok(PaginatedResult {
            items,
            total: result.total,
            has_more: result.has_more,
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
            .ok_or_else(|| AppError::Internal("Failed to serialize artifact status".into()))?;

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
                .map_err(|e| AppError::Internal(format!("Failed to serialize artifact event: {e}")))?,
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
    ) -> Result<String, ConversationError> {
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

        // Short-circuit for legacy Gemini conversations: the dedicated Gemini
        // runtime has been removed, so we cannot build an agent for this row.
        // Emit CONVERSATION_ARCHIVED (HTTP 410 Gone) without touching the
        // legacy `model` column, which may hold shapes the new parser can't
        // deserialize. The client identifies this case by `code` and renders
        // a dedicated archived-conversation UI rather than a generic banner.
        if row.r#type == "gemini" {
            return Err(ConversationError::Archived {
                id: conversation_id.to_owned(),
                reason: "This conversation was created with the legacy Gemini runtime, which has been \
                         removed. Please start a new conversation with the Gemini ACP backend to continue."
                    .into(),
            });
        }

        let turn_claim = self.runtime_state.try_claim_turn(conversation_id)?;

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
        let build_opts = match self.build_task_options(&row) {
            Ok(opts) => opts,
            Err(err) => {
                error!(
                    error_code = err.error_code(),
                    error = %ErrorChain(&err),
                    "Failed to build task options for message send"
                );
                let _ = self.persist_send_failure_tip(conversation_id, &err).await;
                return Err(err.into());
            }
        };
        self.ensure_auto_workspace_skill_links(&row, &build_opts).await;
        let stored_workspace = build_opts.workspace.clone();

        let conv_id = conversation_id.to_owned();
        let repo = Arc::clone(&self.conversation_repo);
        let broadcaster = Arc::clone(&self.broadcaster);
        let cron_service = self.current_cron_service();
        let runtime_state = self.runtime_state();
        let user_id_owned = user_id.to_owned();
        let service = self.clone();
        let task_manager = Arc::clone(task_manager);

        // Send message to the agent in a background task.
        // prompt() blocks until the PromptResponse arrives (turn completed),
        // but the HTTP handler should return 202 immediately.
        //
        // Every turn mints a fresh msg_id and passes it as the agent
        // correlation id so DB row, WebSocket stream events, and
        // agent-internal tracing all share one identifier per turn.
        let user_msg_id_ret = user_msg_id.clone();
        tokio::spawn(async move {
            let mut turn_claim = turn_claim;
            let build_started_at = now_ms();
            info!(conversation_id = %conv_id, "Agent task build started");
            let agent = match task_manager.get_or_build_task(&conv_id, build_opts).await {
                Ok(agent) => agent,
                Err(err) => {
                    error!(
                        conversation_id = %conv_id,
                        error_code = err.error_code(),
                        error = %ErrorChain(&err),
                        "Agent task build failed"
                    );
                    if service.runtime_state.is_deleting(&conv_id) {
                        debug!(
                            conversation_id = %conv_id,
                            "Skipping send failure persistence because conversation is deleting"
                        );
                    } else {
                        service.persist_and_broadcast_send_failure_tip(&conv_id, &err).await;
                    }
                    let was_deleting = turn_claim.release();
                    service.complete_released_turn(&conv_id, was_deleting).await;
                    return;
                }
            };

            // If the factory resolved a different workspace (e.g. auto-created temp
            // dir for a legacy conversation with empty workspace), persist it back.
            if let Err(err) = service
                .maybe_persist_workspace(&conv_id, &stored_workspace, agent.workspace())
                .await
            {
                error!(
                    conversation_id = %conv_id,
                    error_code = err.error_code(),
                    error = %ErrorChain(&err),
                    "Failed to persist resolved workspace"
                );
                if service.runtime_state.is_deleting(&conv_id) {
                    debug!(
                        conversation_id = %conv_id,
                        "Skipping workspace failure persistence because conversation is deleting"
                    );
                } else {
                    service.persist_and_broadcast_send_failure_tip(&conv_id, &err).await;
                }
                let was_deleting = turn_claim.release();
                service.complete_released_turn(&conv_id, was_deleting).await;
                return;
            }

            info!(
                conversation_id = %conv_id,
                agent_type = ?agent.agent_type(),
                elapsed_ms = now_ms().saturating_sub(build_started_at),
                "Agent task ready"
            );

            let first_turn_msg_id = Self::mint_msg_id();
            let mut pending_send = Some((
                SendMessageData {
                    content: req.content,
                    msg_id: first_turn_msg_id.clone(),
                    files: req.files,
                    inject_skills: req.inject_skills,
                },
                first_turn_msg_id,
            ));
            let mut continuation_count = 0usize;

            while let Some((current_send, msg_id)) = pending_send.take() {
                if continuation_count >= MAX_CRON_CONTINUATIONS_PER_TURN {
                    warn!(
                        conversation_id = %conv_id,
                        max = MAX_CRON_CONTINUATIONS_PER_TURN,
                        "Reached cron continuation limit; ending turn early"
                    );
                    break;
                }

                let relay = StreamRelay::new(
                    conv_id.clone(),
                    msg_id,
                    user_id_owned.clone(),
                    Arc::clone(&repo),
                    Arc::clone(&broadcaster),
                    cron_service.clone(),
                )
                .with_runtime_state(Arc::clone(&runtime_state))
                .with_turn_completion(false);

                let rx = agent.subscribe();
                let send_agent = agent.clone();
                let conv_id_send = conv_id.clone();
                let (send_error_tx, send_error_rx) = oneshot::channel();
                // 1. Send the message to the agent and concurrently run the relay to stream events.
                tokio::spawn(async move {
                    if let Err(e) = send_agent.send_message(current_send).await {
                        let task_status = send_agent.status();
                        let agent_type = send_agent.agent_type();
                        error!(
                            conversation_id = %conv_id_send,
                            ?agent_type,
                            ?task_status,
                            error = %ErrorChain(&e),
                            "Agent send_message failed"
                        );
                        if task_status == Some(ConversationStatus::Finished) {
                            debug!(
                                conversation_id = %conv_id_send,
                                ?agent_type,
                                "Agent send_message failure already published runtime terminal; skipping fallback stream error"
                            );
                        } else {
                            warn!(
                                conversation_id = %conv_id_send,
                                ?agent_type,
                                code = ?e.code(),
                                ownership = ?e.ownership(),
                                "Agent send_message returned error without runtime terminal; injecting fallback stream error"
                            );
                            let _ = send_error_tx.send(e);
                        }
                    }
                });
                // 2. Wait for the agent to process the message and complete the turn, while the relay streams events in real time.
                let outcome = relay.consume_with_send_error(rx, send_error_rx).await;

                if runtime_state.is_deleting(&conv_id) {
                    debug!(
                        conversation_id = %conv_id,
                        "Skipping post-terminal persistence because conversation is deleting"
                    );
                    break;
                }

                if let Some(session_key) = agent.get_session_key() {
                    persist_session_key(&repo, &conv_id, &session_key).await;
                }

                if service
                    .evict_acp_task_after_terminal_error(&conv_id, agent.agent_type(), &outcome, &task_manager)
                    .await
                {
                    break;
                }

                if outcome.system_responses.is_empty() {
                    break;
                }
                continuation_count += 1;
                let next_turn_msg_id = Self::mint_msg_id();
                pending_send = Some((
                    SendMessageData {
                        content: outcome.system_responses.join("\n"),
                        msg_id: next_turn_msg_id.clone(),
                        files: vec![],
                        inject_skills: vec![],
                    },
                    next_turn_msg_id,
                ));
            }

            let was_deleting = turn_claim.release();
            service.complete_released_turn(&conv_id, was_deleting).await;
        });

        info!(
            msg_id = %user_msg_id_ret,
            elapsed_ms = now_ms().saturating_sub(send_started_at),
            "Message accepted, agent work scheduled"
        );
        Ok(user_msg_id_ret)
    }

    async fn persist_and_broadcast_send_failure_tip(&self, conversation_id: &str, err: &AppError) {
        let Some(row) = self.persist_send_failure_tip(conversation_id, err).await else {
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
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), ConversationError> {
        // Verify conversation exists and belongs to user
        self.conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let Some(agent) = task_manager.get_task(conversation_id) else {
            info!("No active agent to cancel; treating as idempotent success");
            return Ok(());
        };

        if let Err(e) = agent.cancel().await {
            warn!(error = %ErrorChain(&e), "Failed to cancel agent");
            return Err(e.into());
        }

        info!("Stream canceled");
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
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: conversation_id.to_owned(),
            })?;

        let build_opts = self.build_task_options(&row)?;
        self.ensure_auto_workspace_skill_links(&row, &build_opts).await;
        let stored_workspace = build_opts.workspace.clone();
        let agent = task_manager.get_or_build_task(conversation_id, build_opts).await?;

        // Persist auto-resolved workspace if factory picked a different path.
        self.maybe_persist_workspace(conversation_id, &stored_workspace, agent.workspace())
            .await?;

        debug!("Agent warmed up");
        Ok(())
    }
}

// ── Internal Helpers ────────────────────────────────────────────────

impl ConversationService {
    /// Build [`BuildTaskOptions`] from a conversation database row.
    ///
    /// Provider/model resolution lives in [`crate::task_options::provider_model_from_conversation_row`]
    /// so the cron executor can derive identical values for the same row.
    /// Diverging the lookup here historically produced
    /// `Provider '<vendor>' not found` failures under cron when the
    /// interactive path worked fine (Sentry ELECTRON-1HM).
    fn build_task_options(&self, row: &aionui_db::models::ConversationRow) -> Result<BuildTaskOptions, AppError> {
        let agent_type = string_to_enum(&row.r#type)?;

        let model = crate::task_options::provider_model_from_conversation_row(row);

        let mut extra: serde_json::Value =
            serde_json::from_str(&row.extra).map_err(|e| AppError::Internal(format!("Invalid extra JSON: {e}")))?;

        // Inject user_id into extra so the Guide MCP bridge can pass it to
        // aion_create_team without a separate lookup. Harmless for non-ACP types.
        if let Some(obj) = extra.as_object_mut() {
            obj.entry("user_id")
                .or_insert_with(|| serde_json::Value::String(row.user_id.clone()));
        }

        // Extract workspace from extra (common across agent types)
        let workspace = match extra.get("workspace").and_then(|v| v.as_str()) {
            Some(workspace) if !workspace.is_empty() => {
                let normalized = validate_runtime_workspace_path(workspace)?;
                if normalized != workspace {
                    extra["workspace"] = serde_json::Value::String(normalized.clone());
                }
                normalized
            }
            _ => String::new(),
        };

        Ok(BuildTaskOptions {
            agent_type,
            workspace,
            model,
            conversation_id: row.id.clone(),
            extra,
        })
    }

    async fn ensure_auto_workspace_skill_links(&self, row: &ConversationRow, build_opts: &BuildTaskOptions) {
        let expected_workspace = self.workspace_root.join("conversations").join(format!(
            "{}-temp-{}",
            conversation_label(&build_opts.agent_type, build_opts.extra.get("backend")),
            row.id
        ));

        let stored_workspace = build_opts.workspace.trim();
        let workspace = if stored_workspace.is_empty() {
            expected_workspace
        } else {
            let workspace = PathBuf::from(stored_workspace);
            if workspace != expected_workspace {
                return;
            }
            workspace
        };

        let skill_names = build_opts
            .extra
            .get("skills")
            .cloned()
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default();
        if skill_names.is_empty() {
            return;
        }

        let Some(rel_dirs) = native_skills_dirs(
            &self.agent_metadata_repo,
            &build_opts.agent_type,
            build_opts.extra.get("backend"),
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
    async fn maybe_persist_workspace(
        &self,
        conversation_id: &str,
        stored_workspace: &str,
        resolved_workspace: &str,
    ) -> Result<(), AppError> {
        if resolved_workspace.is_empty() || resolved_workspace == stored_workspace {
            return Ok(());
        }

        // Fetch latest extra, merge the resolved workspace path in, and persist.
        let row = self
            .conversation_repo
            .get(conversation_id)
            .await?
            .ok_or_else(|| AppError::Internal("Conversation vanished during workspace sync".into()))?;

        let mut extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap_or_else(|_| serde_json::json!({}));
        extra["workspace"] = serde_json::Value::String(resolved_workspace.to_owned());

        let extra_json =
            serde_json::to_string(&extra).map_err(|e| AppError::Internal(format!("Failed to serialize extra: {e}")))?;

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

    fn current_cron_service(&self) -> Option<Arc<dyn ICronService>> {
        match self.cron_service.read() {
            Ok(guard) => guard.as_ref().map(Arc::clone),
            Err(_) => None,
        }
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

fn normalize_workspace_extra(extra: &mut serde_json::Value) -> Result<(), AppError> {
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

fn normalize_workspace_path(workspace: &str) -> Result<String, AppError> {
    if workspace.trim().is_empty() {
        return Err(AppError::BadRequest("Workspace directory is empty".into()));
    }

    let workspace_path = PathBuf::from(workspace);
    if workspace_path_has_whitespace_segment(&workspace_path) {
        return Err(AppError::WorkspacePathContainsWhitespace(
            workspace_path.display().to_string(),
        ));
    }

    Ok(workspace.to_owned())
}

fn validate_runtime_workspace_path(workspace: &str) -> Result<String, AppError> {
    if workspace.trim().is_empty() {
        return Err(AppError::BadRequest("Workspace directory is empty".into()));
    }

    let workspace_path = PathBuf::from(workspace);
    if workspace_path_has_whitespace_segment(&workspace_path) {
        return Err(AppError::WorkspacePathContainsWhitespaceRuntimeUnsupported(
            workspace_path.display().to_string(),
        ));
    }

    Ok(workspace.to_owned())
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
    ) -> Result<McpSupportPolicy, AppError> {
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
) -> Result<McpSupportPolicy, AppError> {
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
            .map_err(|e| AppError::Internal(format!("agent_metadata lookup: {e}")))?,
        None if agent_source == "builtin" => match backend {
            Some(vendor) => repo
                .find_builtin_by_backend(vendor)
                .await
                .map_err(|e| AppError::Internal(format!("agent_metadata lookup: {e}")))?,
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
fn enum_to_db<T: serde::Serialize>(val: &T) -> Result<String, AppError> {
    let json_val =
        serde_json::to_value(val).map_err(|e| AppError::Internal(format!("Enum serialization failed: {e}")))?;
    json_val
        .as_str()
        .map(|s| s.to_owned())
        .ok_or_else(|| AppError::Internal("Expected string enum value".into()))
}

/// Persist the agent's session key into `conversation.extra.sessionKey`.
///
/// Called after send_message completes so the session can be resumed
/// when the user re-enters this conversation later.
async fn persist_session_key(repo: &Arc<dyn IConversationRepository>, conversation_id: &str, session_key: &str) {
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

fn legacy_cron_trigger_to_artifact(row: MessageRow) -> Result<ConversationArtifactResponse, AppError> {
    let payload: serde_json::Value = serde_json::from_str(&row.content)
        .map_err(|e| AppError::Internal(format!("Invalid legacy cron trigger payload JSON: {e}")))?;
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
