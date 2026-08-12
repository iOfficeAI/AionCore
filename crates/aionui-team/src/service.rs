mod describe_support;
mod response_builder;
pub(crate) mod spawn_support;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, Weak};

use aionui_ai_agent::{ActiveLeaseRegistry, AgentError, AgentInstance, IWorkerTaskManager, IdleCleanupCoordinator};
use aionui_api_types::ChatFileRef;
use aionui_api_types::{
    AddAgentRequest, CreateTeamRequest, GetConfigOptionsResponse, TeamActivityCursor, TeamActivityPageResponse,
    TeamAgentResponse, TeamAgentRuntimeStatus, TeamMailboxMessageResponse, TeamResponse, TeamRunAckResponse,
    TeamRunStateResponse, TeamSessionBinding, TeamSessionPhase, TeamSessionStatus, TeamSessionStatusPayload,
    TeamTaskResponse, TeamToolCall, TeamToolContextResponse, TeamToolErrorCode, TeamToolErrorPayload,
    TeamToolTransport, WebSocketMessage,
};
use aionui_common::{AgentKillReason, ConversationStatus, TimestampMs, generate_id, now_ms};
use aionui_db::models::TeamRow;
use aionui_db::{
    ActivityCursor, IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository,
    IProviderRepository, ITeamRepository, IUserOrderStore, OrderItemRef, OrderItemType, PageDirection,
    UpdateTeamParams,
};
use aionui_project::{ProjectService, canonical};
use aionui_realtime::EventBroadcaster;
use dashmap::DashMap;
use tracing::{debug, info, warn};

use crate::activity_mapping::{
    mailbox_row_to_response, message_row_to_activity_item, sort_activity_items, task_row_to_activity_item,
    task_to_response,
};
use crate::error::TeamError;
use crate::event_loop::{AgentLoopContext, EventLoopRegistrationError};
use crate::events::{
    TEAM_CREATED_EVENT, TEAM_REMOVED_EVENT, TEAM_RENAMED_EVENT, TEAM_SESSION_STATUS_CHANGED_EVENT, TeamEventEmitter,
};
use crate::member_runtime::{
    AttachLease, AttachOutcome, AttachWaiter, BeginRemove, MemberRuntimeFailure, MemberRuntimeSnapshot, ReserveAttach,
};
use crate::message_projection::TeamProjectionMessageStore;
use crate::ports::{
    AgentTurnCancellationPort, AgentTurnExecutionPort, NativeSlashCommandPort, NoopNativeSlashCommandPort,
    TeamAssistantCatalogPort,
};
use crate::prompt_dump::TeamPromptDumpConfig;
use crate::provisioning::{TeamAgentProvisioner, TeamConversationProvisioningPort};
use crate::runtime_tools::{
    ResolvedTeamToolContext, agent_for_conversation, error_payload, execute_with_scheduler, role_to_tool_role,
};
use crate::session::{AgentMessageQueueResult, TeamSession, attach_member_runtime, spawn_attach_agent_process_bg};
use crate::team_run::TeamRunManager;
use crate::types::{Team, TeamAgent, TeamTask, TeammateRole};
use crate::work_source::WorkSource;
use crate::workspace::validate_create_workspace_path;

/// Default number of activity items returned when the client omits `limit`.
pub const DEFAULT_ACTIVITY_LIMIT: i64 = 500;
/// Hard upper bound for the activity `limit` query parameter.
pub const MAX_ACTIVITY_LIMIT: i64 = 1000;
/// Upper bound on how many task ids one dependency-resolution request may
/// look up, to bound query size regardless of client input.
pub const MAX_TASK_ID_LOOKUP: usize = 200;

/// Which item kinds the unified activity feed returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    /// Merged messages and tasks.
    All,
    /// Messages only.
    Message,
    /// Tasks only.
    Task,
}

pub(crate) fn inherit_team_workspace(extra: &mut serde_json::Value, workspace: &str) {
    if !workspace.trim().is_empty() {
        extra["workspace"] = serde_json::Value::String(workspace.to_owned());
    }
}

struct SessionEntry {
    session: Arc<TeamSession>,
    slow_monitor_handle: tokio::task::JoinHandle<()>,
}

pub struct TeamIdleCleanupCoordinator {
    service: Arc<TeamSessionService>,
    active_leases: Arc<ActiveLeaseRegistry>,
}

impl TeamIdleCleanupCoordinator {
    pub fn new(service: Arc<TeamSessionService>, active_leases: Arc<ActiveLeaseRegistry>) -> Self {
        Self { service, active_leases }
    }
}

#[async_trait::async_trait]
impl IdleCleanupCoordinator for TeamIdleCleanupCoordinator {
    async fn cleanup_idle_conversations(
        &self,
        idle_conversation_ids: Vec<String>,
        idle_threshold_ms: TimestampMs,
    ) -> Vec<String> {
        self.service
            .cleanup_idle_team_runtime_tasks(idle_conversation_ids, &self.active_leases, idle_threshold_ms)
            .await
    }
}

struct MemberRuntimeReconcileWork {
    agent: TeamAgent,
    waiter: AttachWaiter,
    owner: Option<AttachLease>,
}

pub struct TeamSessionService {
    repo: Arc<dyn ITeamRepository>,
    agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
    assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
    assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
    provider_repo: Arc<dyn IProviderRepository>,
    conversation_port: Arc<dyn TeamConversationProvisioningPort>,
    projection_store: Arc<dyn TeamProjectionMessageStore>,
    broadcaster: Arc<dyn EventBroadcaster>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    turn_port: Arc<dyn AgentTurnExecutionPort>,
    cancellation_port: Arc<dyn AgentTurnCancellationPort>,
    /// Native slash-command recognizer injected into each `TeamSession`
    /// (ELECTRON-3RN). No-op by default (see `NoopNativeSlashCommandPort`).
    slash_command_port: Arc<dyn NativeSlashCommandPort>,
    backend_binary_path: Arc<PathBuf>,
    prompt_dump: TeamPromptDumpConfig,
    sessions: Arc<DashMap<String, SessionEntry>>,
    /// Per-team mutex serializing membership mutations with session startup so
    /// callers cannot read-modify-write the `agents` JSON or rebuild a runtime
    /// session from a stale roster snapshot.
    add_agent_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Per-team mutex serializing `ensure_session` so concurrent callers cannot
    /// race and start two sessions for the same team.
    ensure_session_locks: Arc<DashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Project-bind side branch (optional). `None` → team binding is a no-op,
    /// so team create/read behaves exactly as before.
    project_service: Arc<RwLock<Option<Arc<ProjectService>>>>,
    /// Sidebar ordering store (optional). Set → `remove_team` cascade-deletes the
    /// team's `user_order` rows (design §4.3, path 2). `None` → no-op, so team
    /// deletion behaves exactly as before.
    user_order: Arc<RwLock<Option<Arc<dyn IUserOrderStore>>>>,
    /// Back-pointer used by [`TeamSession::spawn_agent`] to reach DB-facing
    /// orchestration without threading the service through every session method.
    /// Stored as `Weak` so the session map does not create a strong cycle with
    /// the service that owns it. Set once during [`TeamSessionService::new`]
    /// via [`Arc::new_cyclic`].
    self_ref: Weak<TeamSessionService>,
}

impl TeamSessionService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repo: Arc<dyn ITeamRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
        assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
        assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
        provider_repo: Arc<dyn IProviderRepository>,
        conversation_port: Arc<dyn TeamConversationProvisioningPort>,
        projection_store: Arc<dyn TeamProjectionMessageStore>,
        broadcaster: Arc<dyn EventBroadcaster>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        turn_port: Arc<dyn AgentTurnExecutionPort>,
        cancellation_port: Arc<dyn AgentTurnCancellationPort>,
        backend_binary_path: Arc<PathBuf>,
    ) -> Arc<Self> {
        Self::new_with_prompt_dump(
            repo,
            agent_metadata_repo,
            assistant_catalog,
            assistant_definition_repo,
            assistant_overlay_repo,
            provider_repo,
            conversation_port,
            projection_store,
            broadcaster,
            task_manager,
            turn_port,
            cancellation_port,
            Arc::new(NoopNativeSlashCommandPort),
            backend_binary_path,
            TeamPromptDumpConfig::disabled(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_prompt_dump(
        repo: Arc<dyn ITeamRepository>,
        agent_metadata_repo: Arc<dyn IAgentMetadataRepository>,
        assistant_catalog: Arc<dyn TeamAssistantCatalogPort>,
        assistant_definition_repo: Arc<dyn IAssistantDefinitionRepository>,
        assistant_overlay_repo: Arc<dyn IAssistantOverlayRepository>,
        provider_repo: Arc<dyn IProviderRepository>,
        conversation_port: Arc<dyn TeamConversationProvisioningPort>,
        projection_store: Arc<dyn TeamProjectionMessageStore>,
        broadcaster: Arc<dyn EventBroadcaster>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        turn_port: Arc<dyn AgentTurnExecutionPort>,
        cancellation_port: Arc<dyn AgentTurnCancellationPort>,
        slash_command_port: Arc<dyn NativeSlashCommandPort>,
        backend_binary_path: Arc<PathBuf>,
        prompt_dump: TeamPromptDumpConfig,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            repo,
            agent_metadata_repo,
            assistant_catalog,
            assistant_definition_repo,
            assistant_overlay_repo,
            provider_repo,
            conversation_port,
            projection_store,
            broadcaster,
            task_manager,
            turn_port,
            cancellation_port,
            slash_command_port,
            backend_binary_path,
            prompt_dump,
            sessions: Arc::new(DashMap::new()),
            add_agent_locks: Arc::new(DashMap::new()),
            ensure_session_locks: Arc::new(DashMap::new()),
            project_service: Arc::new(RwLock::new(None)),
            user_order: Arc::new(RwLock::new(None)),
            self_ref: weak.clone(),
        })
    }

    pub(crate) fn provisioner(&self) -> TeamAgentProvisioner {
        TeamAgentProvisioner::new(
            self.repo.clone(),
            self.agent_metadata_repo.clone(),
            self.assistant_catalog.clone(),
            self.provider_repo.clone(),
            self.conversation_port.clone(),
        )
    }

    /// Inject the project-bind service (project-bind side branch). When unset,
    /// binding/backfill are no-ops.
    pub fn with_project_service(&self, project_service: Arc<ProjectService>) {
        if let Ok(mut guard) = self.project_service.write() {
            *guard = Some(project_service);
        }
    }

    /// Inject the sidebar ordering store so `remove_team` cascade-deletes the
    /// team's `user_order` rows (design §4.3, path 2). When unset, the cascade is
    /// a no-op. Member conversations are handled separately by the conversation
    /// delete hook (they route through `ConversationService::delete`).
    pub fn with_user_order_store(&self, user_order: Arc<dyn IUserOrderStore>) {
        if let Ok(mut guard) = self.user_order.write() {
            *guard = Some(user_order);
        }
    }

    /// Best-effort cascade of a removed team's `user_order` row (design §4.3,
    /// path 2). Store unset → no-op. An error is logged, not propagated: an
    /// orphan `team` row self-heals on read (the pinned group only emits teams
    /// present in the live aggregate), so it must never block team deletion.
    async fn remove_team_order_row(&self, user_id: &str, team_id: &str) {
        let store = self.user_order.read().ok().and_then(|guard| guard.clone());
        let Some(store) = store else { return };
        let item = OrderItemRef::new(OrderItemType::Team, team_id);
        if let Err(err) = store.remove_item(user_id, &item).await {
            warn!(
                user_id = %user_id,
                team_id = %team_id,
                error = %err,
                "sidebar: failed to cascade-delete user_order row for removed team"
            );
        }
    }

    /// Resolve a team workspace into `(project_id, folder_id)`. Best-effort:
    /// missing service / empty workspace / bad URI / resolve error → `(None, None)`,
    /// logged at `warn`. Never affects team create/read.
    async fn resolve_binding_best_effort(&self, user_id: &str, workspace: &str) -> (Option<String>, Option<String>) {
        let project_service = self.project_service.read().ok().and_then(|guard| guard.clone());
        let Some(project_service) = project_service else {
            return (None, None);
        };
        if workspace.trim().is_empty() {
            return (None, None);
        }
        let uri = match canonical::to_file_uri(Path::new(workspace)) {
            Ok(uri) => uri,
            Err(err) => {
                warn!(error = err.code(), "team project bind skipped: bad workspace uri");
                return (None, None);
            }
        };
        match project_service.resolve_existing(user_id, uri).await {
            Ok(out) => (Some(out.project.project_id), Some(out.folder.folder_id)),
            Err(err) => {
                warn!(error = err.code(), "team project bind skipped");
                (None, None)
            }
        }
    }

    /// Lazily backfill `teams.project_id`/`folder_id` on read. Best-effort;
    /// no-op when already bound, workspace empty, or service unset.
    async fn backfill_team_binding_best_effort(&self, row: &TeamRow) {
        if row.project_id.is_some() || row.workspace.trim().is_empty() {
            return;
        }
        let (Some(project_id), Some(folder_id)) = self.resolve_binding_best_effort(&row.user_id, &row.workspace).await
        else {
            return;
        };
        let params = UpdateTeamParams {
            project_id: Some(project_id),
            folder_id: Some(folder_id),
            ..Default::default()
        };
        if let Err(err) = self.repo.update_team(&row.user_id, &row.id, &params).await {
            warn!(team_id = %row.id, error = %err, "team project bind: backfill update failed");
        }
    }

    async fn load_owned_team(&self, user_id: &str, team_id: &str) -> Result<Team, TeamError> {
        let row = self.load_owned_team_row(user_id, team_id).await?;
        Ok(Team::from_row(&row)?)
    }

    async fn load_owned_team_row(&self, user_id: &str, team_id: &str) -> Result<TeamRow, TeamError> {
        self.repo
            .get_team(user_id, team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.into()))
    }

    pub(crate) async fn team_owner_user_id(&self, team_id: &str) -> Result<String, TeamError> {
        let row = self
            .repo
            .get_team_for_restore(team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.into()))?;
        Ok(row.user_id)
    }

    /// Returns the most recent team-wide mailbox messages (all recipients),
    /// newest first, for the read-only activity view. `limit` is clamped to
    /// `[1, MAX_ACTIVITY_LIMIT]`. Ownership is enforced first via a scoped
    /// lookup: a missing team and another user's team both surface as
    /// `TeamNotFound`, so team existence is never leaked across users.
    pub async fn list_team_mailbox(
        &self,
        user_id: &str,
        team_id: &str,
        limit: i64,
    ) -> Result<Vec<TeamMailboxMessageResponse>, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        let clamped = limit.clamp(1, MAX_ACTIVITY_LIMIT);
        let rows = self.repo.list_messages_by_team(team_id, clamped).await?;
        let responses: Vec<TeamMailboxMessageResponse> = rows.iter().map(mailbox_row_to_response).collect();
        info!(kind = "team", team_id, count = responses.len(), "team mailbox listed");
        Ok(responses)
    }

    /// Returns the team's tasks, newest first (`created_at` DESC, `id` as a
    /// stable secondary key), truncated to a clamped `limit`, for the
    /// read-only activity view. Reuses the existing ASC `list_tasks` and sorts
    /// in the service. Ownership is enforced first.
    pub async fn list_team_tasks(
        &self,
        user_id: &str,
        team_id: &str,
        limit: i64,
    ) -> Result<Vec<TeamTaskResponse>, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        let clamped = limit.clamp(1, MAX_ACTIVITY_LIMIT);
        let rows = self.repo.list_tasks(user_id, team_id).await?;
        let mut tasks: Vec<TeamTask> = rows.iter().filter_map(|r| TeamTask::from_row(r).ok()).collect();
        tasks.sort_by(|a, b| b.created_at.cmp(&a.created_at).then_with(|| b.id.cmp(&a.id)));
        tasks.truncate(clamped as usize);
        let responses: Vec<TeamTaskResponse> = tasks.iter().map(task_to_response).collect();
        info!(kind = "team", team_id, count = responses.len(), "team tasks listed");
        Ok(responses)
    }

    /// Returns the team's tasks matching `ids` (newest first), for resolving
    /// dependency (`blocked_by`) subjects that may lie outside the loaded
    /// activity page. Ownership is enforced first; `ids` is clamped to
    /// `MAX_TASK_ID_LOOKUP`. An empty `ids` yields an empty result.
    pub async fn list_team_tasks_by_ids(
        &self,
        user_id: &str,
        team_id: &str,
        ids: &[String],
    ) -> Result<Vec<TeamTaskResponse>, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let capped = &ids[..ids.len().min(MAX_TASK_ID_LOOKUP)];
        let rows = self.repo.list_tasks_by_ids(user_id, team_id, capped).await?;
        let tasks: Vec<TeamTask> = rows.iter().filter_map(|r| TeamTask::from_row(r).ok()).collect();
        let responses: Vec<TeamTaskResponse> = tasks.iter().map(task_to_response).collect();
        info!(
            kind = "team",
            team_id,
            count = responses.len(),
            "team tasks resolved by ids"
        );
        Ok(responses)
    }

    /// Returns one keyset-paginated page of the unified activity feed (messages
    /// and/or tasks per `kind`), ordered by `(created_at, id)` in `direction`.
    /// Ownership is enforced first, so another user's team is indistinguishable
    /// from a missing one (`TeamNotFound`). For `kind = All`, each stream is
    /// fetched up to `limit` rows and merged; the global top-`limit` is
    /// mathematically complete (any item newer/older than the cursor is within
    /// its own stream's top-`limit`). `has_more` is conservative: a full sub-
    /// query or a post-merge truncation both flag "possibly more".
    pub async fn list_team_activity(
        &self,
        user_id: &str,
        team_id: &str,
        cursor: Option<ActivityCursor>,
        direction: PageDirection,
        kind: ActivityKind,
        limit: i64,
    ) -> Result<TeamActivityPageResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        let limit = limit.clamp(1, MAX_ACTIVITY_LIMIT);

        let (mut items, mailbox_full, tasks_full) = match kind {
            ActivityKind::Message => {
                let rows = self
                    .repo
                    .list_messages_by_team_paged(team_id, cursor.clone(), direction, limit)
                    .await?;
                let full = rows.len() as i64 == limit;
                (
                    rows.iter().map(message_row_to_activity_item).collect::<Vec<_>>(),
                    full,
                    false,
                )
            }
            ActivityKind::Task => {
                let rows = self
                    .repo
                    .list_tasks_paged(user_id, team_id, cursor.clone(), direction, limit)
                    .await?;
                let full = rows.len() as i64 == limit;
                (
                    rows.iter().filter_map(task_row_to_activity_item).collect::<Vec<_>>(),
                    false,
                    full,
                )
            }
            ActivityKind::All => {
                let msgs = self
                    .repo
                    .list_messages_by_team_paged(team_id, cursor.clone(), direction, limit)
                    .await?;
                let tasks = self
                    .repo
                    .list_tasks_paged(user_id, team_id, cursor.clone(), direction, limit)
                    .await?;
                let mailbox_full = msgs.len() as i64 == limit;
                let tasks_full = tasks.len() as i64 == limit;
                let mut merged: Vec<_> = msgs
                    .iter()
                    .map(message_row_to_activity_item)
                    .chain(tasks.iter().filter_map(task_row_to_activity_item))
                    .collect();
                sort_activity_items(&mut merged, direction);
                (merged, mailbox_full, tasks_full)
            }
        };

        // Truncate to the top `limit`; whether we cut anything feeds `has_more`.
        let truncated = items.len() as i64 > limit;
        items.truncate(limit as usize);

        let has_more = mailbox_full || tasks_full || truncated;
        let next_cursor = if has_more {
            items.last().map(|i| TeamActivityCursor {
                ts: i.created_at,
                id: i.id.clone(),
            })
        } else {
            None
        };

        info!(
            kind = "team",
            team_id,
            count = items.len(),
            first_page = cursor.is_none(),
            "team activity listed"
        );

        Ok(TeamActivityPageResponse {
            items,
            next_cursor,
            has_more,
        })
    }

    pub async fn renew_active_lease(
        &self,
        user_id: &str,
        team_id: &str,
        active_leases: &ActiveLeaseRegistry,
    ) -> Result<(), TeamError> {
        let team = match self.repo.get_team(user_id, team_id).await {
            Ok(Some(row)) => Team::from_row(&row).map_err(TeamError::from),
            Ok(None) => Err(TeamError::TeamNotFound(team_id.to_owned())),
            Err(error) => Err(TeamError::Database(error)),
        };
        let team = match team {
            Ok(team) => team,
            Err(error @ TeamError::TeamNotFound(_)) => {
                debug!(
                    kind = "team",
                    team_id,
                    user_id,
                    error = %error,
                    "Team active lease renew rejected"
                );
                return Err(error);
            }
            Err(error) => {
                warn!(
                    kind = "team",
                    team_id,
                    user_id,
                    error = %error,
                    "Team active lease renew failed"
                );
                return Err(error);
            }
        };

        let conversation_ids = team
            .agents
            .iter()
            .map(|agent| agent.conversation_id.as_str())
            .filter(|conversation_id| !conversation_id.trim().is_empty());
        let (covered_count, expires_at) = active_leases.renew_many(conversation_ids);

        debug!(
            kind = "team",
            team_id, covered_count, expires_at, "Team active lease renewed"
        );
        Ok(())
    }

    /// Restore sessions for all existing teams. Called once at app startup
    /// so that MCP servers are available before any user sends a message.
    pub async fn restore_all_sessions(&self) {
        let teams = match self.repo.list_teams_for_restore().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list teams for session restore");
                return;
            }
        };
        for team in &teams {
            if let Err(e) = self.ensure_session_inner(&team.id, None).await {
                tracing::warn!(team_id = %team.id, error = %e, "failed to restore session on startup");
                continue;
            }
        }
        if !teams.is_empty() {
            tracing::info!(count = teams.len(), "team sessions restored on startup");
        }
    }

    pub async fn create_team(&self, user_id: &str, req: CreateTeamRequest) -> Result<TeamResponse, TeamError> {
        if req.agents.is_empty() {
            return Err(TeamError::InvalidRequest("at least one agent is required".into()));
        }
        if req
            .agents
            .iter()
            .any(|agent| agent.conversation_id.as_deref().is_some_and(|id| !id.trim().is_empty()))
        {
            return Err(TeamError::InvalidRequest(
                "creating Team agents from existing conversations are no longer supported; omit agents[].conversation_id"
                    .into(),
            ));
        }

        let shared_workspace = match req.workspace.as_deref() {
            Some(workspace) if !workspace.is_empty() => Some(validate_create_workspace_path(workspace)?),
            _ => None,
        };

        let team_id = generate_id();
        let now = now_ms();

        let provisioned = self
            .provisioner()
            .provision_initial_agents(user_id, &team_id, &req.agents, shared_workspace.as_deref())
            .await?;
        let agents = provisioned.agents;
        let lead_agent_id = provisioned.lead_agent_id;
        let team_workspace = provisioned.team_workspace;
        let agents_json = serde_json::to_string(&agents)?;

        // Project-bind side branch (best-effort; never affects team creation).
        let (project_id, folder_id) = self.resolve_binding_best_effort(user_id, &team_workspace).await;

        let row = TeamRow {
            id: team_id.clone(),
            user_id: user_id.to_owned(),
            name: req.name.clone(),
            workspace: team_workspace.clone(),
            workspace_mode: "shared".into(),
            agents: agents_json,
            lead_agent_id: lead_agent_id.clone(),
            session_mode: None,
            agents_version: "1.0.1".into(),
            created_at: now,
            updated_at: now,
            project_id,
            folder_id,
        };
        self.repo.create_team(&row).await?;

        let team = Team {
            id: team_id,
            name: req.name,
            workspace: team_workspace,
            agents,
            lead_agent_id,
            created_at: now,
            updated_at: now,
        };

        info!(
            team_id = %team.id,
            workspace_source = if shared_workspace.is_some() {
                "user_supplied"
            } else {
                "auto_from_leader"
            },
            agent_count = team.agents.len(),
            "Team created"
        );

        self.broadcast_team_created(user_id, &team.id, &team.name);

        self.build_team_response(user_id, &team).await
    }

    pub async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamResponse>, TeamError> {
        let rows = self.repo.list_teams_by_user(user_id).await?;
        let mut teams = Vec::with_capacity(rows.len());
        for row in &rows {
            match Team::from_row(row) {
                Ok(team) => match self.build_team_response(user_id, &team).await {
                    Ok(resp) => teams.push(resp),
                    Err(e) => {
                        tracing::warn!(team_id = %row.id, error = %e, "skipping team with build error");
                    }
                },
                Err(e) => {
                    tracing::warn!(team_id = %row.id, error = %e, "skipping team with invalid agents JSON");
                }
            }
        }
        Ok(teams)
    }

    pub async fn get_team(&self, user_id: &str, team_id: &str) -> Result<TeamResponse, TeamError> {
        let row = self.load_owned_team_row(user_id, team_id).await?;
        // Project-bind side branch: lazily backfill binding only when a single
        // team is opened (never during list_teams / lease renew).
        self.backfill_team_binding_best_effort(&row).await;
        let team = Team::from_row(&row)?;
        self.build_team_response(user_id, &team).await
    }

    pub async fn remove_team(&self, user_id: &str, team_id: &str) -> Result<(), TeamError> {
        let team = self.load_owned_team(user_id, team_id).await?;

        self.stop_session_unchecked(team_id);

        let kill_futures: Vec<_> = team
            .agents
            .iter()
            .map(|agent| {
                self.task_manager
                    .kill_and_wait(&agent.conversation_id, Some(AgentKillReason::TeamDeleted))
            })
            .collect();

        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            futures_util::future::join_all(kill_futures),
        )
        .await;

        for agent in &team.agents {
            let _ = self
                .conversation_port
                .delete_team_conversation(user_id, &agent.conversation_id)
                .await;
        }

        self.repo.delete_mailbox_by_team(user_id, team_id).await?;
        self.repo.delete_tasks_by_team(user_id, team_id).await?;
        self.repo.delete_team(user_id, team_id).await?;

        // Cascade the team's sidebar ordering row (design §4.3, path 2). Members'
        // conversation rows are dropped by the conversation delete hook via the
        // `delete_team_conversation` calls above. Best-effort: an orphan `team`
        // row self-heals on read (the pinned group only emits teams present in
        // the live aggregate), so it never blocks deletion.
        self.remove_team_order_row(user_id, team_id).await;

        self.add_agent_locks.remove(team_id);

        info!(team_id = %team_id, "Team removed");
        self.broadcast_team_removed(user_id, team_id);
        Ok(())
    }

    pub async fn rename_team(&self, user_id: &str, team_id: &str, name: &str) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;

        self.repo
            .update_team(
                user_id,
                team_id,
                &UpdateTeamParams {
                    name: Some(name.to_owned()),
                    ..Default::default()
                },
            )
            .await?;
        self.broadcast_team_renamed(user_id, team_id, name);
        Ok(())
    }

    pub async fn add_agent(
        &self,
        user_id: &str,
        team_id: &str,
        req: AddAgentRequest,
    ) -> Result<TeamAgentResponse, TeamError> {
        let lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let row = self.load_owned_team_row(user_id, team_id).await?;
        let mut team = Team::from_row(&row)?;
        let agent = self.provisioner().add_agent(user_id, &row, &mut team, req).await?;

        if let Some(session) = self.sessions.get(team_id).map(|e| Arc::clone(&e.session)) {
            let reservation = session.reserve_dynamic_member_attach(&agent);
            session.add_manual_agent(&agent).await?;
            let service = self
                .self_ref
                .upgrade()
                .ok_or_else(|| TeamError::InvalidRequest("add_agent requires a live TeamSessionService".into()))?;
            self.broadcast_agent_runtime_status(user_id, team_id, &agent, TeamAgentRuntimeStatus::Pending, None);
            spawn_attach_agent_process_bg(
                service,
                session,
                user_id.to_owned(),
                agent.clone(),
                self.task_manager.clone(),
                reservation,
                // User-initiated add: failures surface inline, do not wake leader.
                false,
            );
            info!(
                team_id = %team_id,
                slot_id = %agent.slot_id,
                assistant_id = %agent.assistant_id.as_deref().unwrap_or(""),
                role = %agent.role,
                notification_written = true,
                wake_requested = true,
                "manual teammate added"
            );
        } else {
            TeamEventEmitter::new(team_id.to_owned(), user_id.to_owned(), self.broadcaster.clone())
                .broadcast_agent_spawned(&agent);
            info!(
                team_id = %team_id,
                slot_id = %agent.slot_id,
                assistant_id = %agent.assistant_id.as_deref().unwrap_or(""),
                role = %agent.role,
                notification_written = false,
                wake_requested = false,
                "manual teammate added"
            );
        }

        self.build_agent_response(user_id, &agent).await
    }

    pub async fn remove_agent(&self, user_id: &str, team_id: &str, slot_id: &str) -> Result<(), TeamError> {
        let lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let (removed, session, removal_lease) = {
            let _guard = lock.lock().await;
            let team = self.load_owned_team(user_id, team_id).await?;
            let removed = team
                .agents
                .iter()
                .find(|agent| agent.slot_id == slot_id)
                .cloned()
                .ok_or_else(|| TeamError::AgentNotFound(slot_id.into()))?;
            if removed.role == crate::types::TeammateRole::Lead {
                return Err(TeamError::InvalidRequest("cannot remove the team lead".into()));
            }
            let session = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session));
            let removal = session
                .as_ref()
                .map(|session| session.member_runtimes().begin_remove(slot_id));
            let removal_lease = match removal {
                Some(BeginRemove::Start(lease)) => Some(lease),
                Some(BeginRemove::Join(waiter)) => {
                    drop(_guard);
                    return match waiter.wait().await {
                        AttachOutcome::Removed => Ok(()),
                        AttachOutcome::Failed(failure) => Err(TeamError::MemberRuntimeFailed {
                            team_id: team_id.to_owned(),
                            slot_id: removed.slot_id,
                            conversation_id: removed.conversation_id,
                            public_reason: failure.public_reason,
                        }),
                        AttachOutcome::Ready | AttachOutcome::SessionStopped => {
                            Err(TeamError::SessionNotFound(team_id.to_owned()))
                        }
                    };
                }
                Some(BeginRemove::Absent | BeginRemove::SessionStopped) | None => None,
            };
            (removed, session, removal_lease)
        };

        // Cancellation and process cleanup intentionally happen without the
        // membership lock. Concurrent ensure calls observe Removing and join
        // the same registry operation instead of starting a replacement.
        if let Some(session) = &session {
            session.event_loops().remove(slot_id);
        }
        self.task_manager
            .kill_and_wait(&removed.conversation_id, Some(AgentKillReason::TeamDeleted))
            .await;

        let persist_result = {
            let _guard = lock.lock().await;
            let mut current = self.load_owned_team(user_id, team_id).await?;
            current.agents.retain(|agent| agent.slot_id != slot_id);
            let agents_json = serde_json::to_string(&current.agents)?;
            self.repo
                .update_team(
                    user_id,
                    team_id,
                    &UpdateTeamParams {
                        agents: Some(agents_json),
                        ..Default::default()
                    },
                )
                .await
        };

        if let Err(error) = persist_result {
            if let (Some(session), Some(lease)) = (&session, removal_lease.as_ref()) {
                session
                    .member_runtimes()
                    .restore_attach_required_after_remove_persist_error(
                        lease,
                        MemberRuntimeFailure {
                            classification: "membership_persist_failed",
                            public_reason: "Agent runtime needs to restart after membership update failed".to_owned(),
                        },
                    );
                self.refresh_member_runtime_status(session).await;
            }
            return Err(error.into());
        }

        let published_session = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session));
        let active_session = if let Some(current) = published_session {
            let current_removal_lease = if session.as_ref().is_some_and(|captured| Arc::ptr_eq(captured, &current)) {
                removal_lease
            } else {
                match current.member_runtimes().begin_remove(slot_id) {
                    BeginRemove::Start(lease) => Some(lease),
                    BeginRemove::Join(waiter) => {
                        let _ = waiter.wait().await;
                        None
                    }
                    BeginRemove::Absent | BeginRemove::SessionStopped => None,
                }
            };
            current.event_loops().remove(slot_id);
            self.task_manager
                .kill_and_wait(&removed.conversation_id, Some(AgentKillReason::TeamDeleted))
                .await;
            match current.scheduler().remove_agent(slot_id).await {
                Ok(_) | Err(TeamError::AgentNotFound(_)) => {}
                Err(error) => return Err(error),
            }
            if let Some(lease) = current_removal_lease.as_ref() {
                current.member_runtimes().finish_remove(lease);
            }
            Some(current)
        } else {
            None
        };

        if let Err(error) = self
            .conversation_port
            .delete_team_conversation(user_id, &removed.conversation_id)
            .await
        {
            warn!(
                team_id,
                slot_id,
                conversation_id = %removed.conversation_id,
                error = %error,
                "removed team member conversation cleanup failed"
            );
        }

        if let Some(session) = active_session.filter(|session| self.capture_published_session(session).is_some()) {
            session.notify_leader_membership_removed(&removed).await?;
            self.refresh_member_runtime_status(&session).await;
            info!(
                team_id = %team_id,
                slot_id = %removed.slot_id,
                assistant_id = %removed.assistant_id.as_deref().unwrap_or(""),
                role = %removed.role,
                notification_written = true,
                wake_requested = true,
                "manual teammate removed"
            );
        } else {
            TeamEventEmitter::new(team_id.to_owned(), user_id.to_owned(), self.broadcaster.clone())
                .broadcast_agent_removed(slot_id);
            info!(
                team_id = %team_id,
                slot_id = %removed.slot_id,
                assistant_id = %removed.assistant_id.as_deref().unwrap_or(""),
                role = %removed.role,
                notification_written = false,
                wake_requested = false,
                "manual teammate removed"
            );
        }

        Ok(())
    }

    pub async fn rename_agent(&self, user_id: &str, team_id: &str, slot_id: &str, name: &str) -> Result<(), TeamError> {
        let lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let mut team = self.load_owned_team(user_id, team_id).await?;

        let normalized = crate::scheduler::normalize_name(name);
        if normalized.is_empty() {
            return Err(TeamError::InvalidRequest(
                "rename_agent.name is empty after normalization".into(),
            ));
        }

        // Uniqueness check against all other agents in the team.
        let has_conflict = team
            .agents
            .iter()
            .any(|a| a.slot_id != slot_id && crate::scheduler::normalize_name(&a.name) == normalized);
        if has_conflict {
            return Err(TeamError::DuplicateAgentName(name.to_owned()));
        }

        let agent = team
            .agents
            .iter_mut()
            .find(|a| a.slot_id == slot_id)
            .ok_or_else(|| TeamError::AgentNotFound(slot_id.into()))?;
        agent.name = name.to_owned();

        let agents_json = serde_json::to_string(&team.agents)?;
        self.repo
            .update_team(
                user_id,
                team_id,
                &UpdateTeamParams {
                    agents: Some(agents_json),
                    ..Default::default()
                },
            )
            .await?;

        if let Some(session) = self.sessions.get(team_id).map(|e| Arc::clone(&e.session)) {
            let _ = session.rename_agent(slot_id, name).await;
        }

        Ok(())
    }

    /// Start the team's MCP server and rebuild every agent process so it
    /// carries a fresh `team_mcp_stdio_config` pointing at the new server.
    ///
    /// Flow (mcp.md §4.3):
    /// 1. Start `TeamSession` (opens the MCP TCP server).
    /// 2. For each agent: persist `team_mcp_stdio_config` into
    ///    `conversation.extra` → `task_manager.kill_and_wait(conv_id, TeamMcpRebuild)`
    ///    → `TeamConversationProvisioningPort::warmup_agent_process(...)`
    ///    rebuilds the ACP process with
    ///    the new extra.
    /// 3. Spawn per-agent event loops that drain the mailbox whenever notified.
    /// 4. Only insert into `sessions` after every step above succeeds — on
    ///    any failure, stop the session and leave the map untouched so a
    ///    retry can start cleanly.
    pub async fn ensure_session(&self, user_id: &str, team_id: &str) -> Result<(), TeamError> {
        self.load_owned_team_row(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await
    }

    async fn ensure_session_inner(&self, team_id: &str, requested_user_id: Option<&str>) -> Result<(), TeamError> {
        let membership_lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let membership_guard = membership_lock.lock().await;

        let row = match self.repo.get_team_for_restore(team_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                if let Some(user_id) = requested_user_id {
                    self.broadcast_session_status(
                        user_id,
                        team_id,
                        TeamSessionStatus::Failed,
                        Some(TeamSessionPhase::LoadingTeam),
                        |p| {
                            p.error = Some(format!("team not found: {team_id}"));
                        },
                    );
                }
                return Err(TeamError::TeamNotFound(team_id.into()));
            }
            Err(e) => {
                if let Some(user_id) = requested_user_id {
                    self.broadcast_session_status(
                        user_id,
                        team_id,
                        TeamSessionStatus::Failed,
                        Some(TeamSessionPhase::LoadingTeam),
                        |p| {
                            p.error = Some(e.to_string());
                        },
                    );
                }
                return Err(e.into());
            }
        };
        let user_id = row.user_id.clone();
        let team = Team::from_row(&row)?;
        let agents_snapshot: Vec<TeamAgent> = team.agents.clone();

        if let Some(session) = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session)) {
            let work = self
                .reserve_member_runtime_reconciliation(&session, &agents_snapshot)
                .await?;
            drop(membership_guard);
            return self
                .complete_member_runtime_reconciliation(team_id, &user_id, session, work)
                .await;
        }

        let lock = self
            .ensure_session_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let ensure_guard = lock.lock().await;

        if let Some(session) = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session)) {
            let work = self
                .reserve_member_runtime_reconciliation(&session, &agents_snapshot)
                .await?;
            drop(membership_guard);
            drop(ensure_guard);
            return self
                .complete_member_runtime_reconciliation(team_id, &user_id, session, work)
                .await;
        }

        self.broadcast_session_status(
            &user_id,
            team_id,
            TeamSessionStatus::Starting,
            Some(TeamSessionPhase::LoadingTeam),
            |_| {},
        );

        self.broadcast_session_status(
            &user_id,
            team_id,
            TeamSessionStatus::Starting,
            Some(TeamSessionPhase::StartingBridge),
            |_| {},
        );

        let session = match TeamSession::start_with_prompt_dump(
            team,
            self.repo.clone(),
            self.broadcaster.clone(),
            self.backend_binary_path.clone(),
            self.task_manager.clone(),
            self.turn_port.clone(),
            self.cancellation_port.clone(),
            self.projection_store.clone(),
            user_id.clone(),
            self.self_ref.clone(),
            self.prompt_dump.clone(),
        )
        .await
        {
            Ok(session) => Arc::new(session.with_slash_command_port(self.slash_command_port.clone())),
            Err(e) => {
                self.broadcast_session_status(
                    &user_id,
                    team_id,
                    TeamSessionStatus::Failed,
                    Some(TeamSessionPhase::StartingBridge),
                    |p| {
                        p.error = Some(e.to_string());
                    },
                );
                return Err(e);
            }
        };

        self.broadcast_session_status(
            &user_id,
            team_id,
            TeamSessionStatus::Starting,
            Some(TeamSessionPhase::AttachingAgents),
            |_| {},
        );

        let service = self
            .self_ref
            .upgrade()
            .ok_or_else(|| TeamError::InvalidRequest("team service is shutting down".to_owned()))?;

        // Leader-only warmup: only the lead slot is attached at first start.
        // Teammates stay dormant (Absent in the registry) until a delivery
        // lazily wakes them (spec 5.1).
        let Some(leader) = agents_snapshot
            .iter()
            .find(|agent| agent.role == TeammateRole::Lead)
            .cloned()
        else {
            let error = TeamError::InvalidRequest("team has no lead agent".to_owned());
            self.broadcast_session_status(
                &user_id,
                team_id,
                TeamSessionStatus::Failed,
                Some(TeamSessionPhase::AttachingAgents),
                |p| p.error = Some(error.to_string()),
            );
            session.stop();
            return Err(error);
        };

        // Publish the session BEFORE attaching so the single attach path
        // (`attach_member_runtime`) observes it as the current published
        // session. Drop the startup guards before awaiting the attach: that
        // path re-acquires the membership lock (via `refresh_member_runtime_status`)
        // and the ensure lock (via `cleanup_stale_member_runtime_task` on
        // failure), so holding them here would deadlock. Concurrent ensures
        // that were blocked on membership_guard now observe the published
        // session and take the reconciliation path instead of cold-starting.
        let slow_monitor_handle = Self::spawn_slow_monitor(session.clone());
        let entry = SessionEntry {
            session: session.clone(),
            slow_monitor_handle,
        };
        self.sessions.insert(team_id.to_owned(), entry);
        drop(membership_guard);
        drop(ensure_guard);

        self.broadcast_agent_runtime_status(&user_id, team_id, &leader, TeamAgentRuntimeStatus::Pending, None);
        let leader_outcome = match session.member_runtimes().reserve_attach(&leader.slot_id, false) {
            ReserveAttach::Start(lease) => {
                attach_member_runtime(
                    Arc::clone(&service),
                    session.clone(),
                    user_id.clone(),
                    leader.clone(),
                    self.task_manager.clone(),
                    lease,
                    // Leader cold-start failure bubbles to a session-level Failed
                    // (full-screen card), not an inline per-member notice.
                    false,
                )
                .await
            }
            ReserveAttach::Join(waiter) | ReserveAttach::Removing(waiter) => waiter.wait().await,
            ReserveAttach::AlreadyReady => AttachOutcome::Ready,
            ReserveAttach::SessionStopped => AttachOutcome::SessionStopped,
        };

        match leader_outcome {
            AttachOutcome::Ready | AttachOutcome::Removed => {}
            AttachOutcome::Failed(failure) => {
                self.broadcast_session_status(
                    &user_id,
                    team_id,
                    TeamSessionStatus::Failed,
                    Some(TeamSessionPhase::AttachingAgents),
                    |p| p.error = Some(failure.public_reason.clone()),
                );
                session.stop();
                self.sessions.remove(team_id);
                return Err(TeamError::MemberRuntimeFailed {
                    team_id: team_id.to_owned(),
                    slot_id: leader.slot_id.clone(),
                    conversation_id: leader.conversation_id.clone(),
                    public_reason: failure.public_reason,
                });
            }
            AttachOutcome::SessionStopped => {
                session.stop();
                self.sessions.remove(team_id);
                return Err(TeamError::InvalidRequest(
                    "team session stopped during leader warmup".to_owned(),
                ));
            }
        }

        // Teammates start dormant; the leader's Ready was already broadcast by
        // its successful attach.
        for agent in agents_snapshot.iter().filter(|a| a.role != TeammateRole::Lead) {
            self.broadcast_agent_runtime_status(&user_id, team_id, agent, TeamAgentRuntimeStatus::Dormant, None);
        }

        self.broadcast_session_status(
            &user_id,
            team_id,
            TeamSessionStatus::Starting,
            Some(TeamSessionPhase::Recovering),
            |_| {},
        );

        if let Err(err) = session.try_start_recovery_drain("ensure_session_ready").await {
            warn!(
                team_id,
                error = %err,
                "team recovery scan failed after session ensure"
            );
        }

        self.broadcast_session_status(&user_id, team_id, TeamSessionStatus::Ready, None, |p| {
            p.server_count = Some(agents_snapshot.len());
        });

        Ok(())
    }

    async fn reserve_member_runtime_reconciliation(
        &self,
        session: &Arc<TeamSession>,
        agents: &[TeamAgent],
    ) -> Result<Vec<MemberRuntimeReconcileWork>, TeamError> {
        let scheduler_slots = session
            .scheduler()
            .list_agents()
            .await
            .into_iter()
            .map(|agent| agent.slot_id)
            .collect::<HashSet<_>>();
        let mut work = Vec::new();

        for agent in agents {
            if !scheduler_slots.contains(&agent.slot_id) {
                session.scheduler().add_agent(agent).await;
            }
            let snapshot = session.member_runtimes().snapshot(&agent.slot_id);
            // Skip dormant teammates: an Absent non-lead member was never
            // triggered, so a re-ensure (second warmupSession, model switch,
            // retry) must NOT wake it, or it would punch through lazy warmup
            // (spec 5.1). The leader, Ready-repair, Failed-retry, and in-flight
            // members still reconcile.
            if matches!(snapshot, MemberRuntimeSnapshot::Absent) && agent.role != TeammateRole::Lead {
                continue;
            }
            let reservation = match snapshot {
                MemberRuntimeSnapshot::Ready if self.task_manager.get_task(&agent.conversation_id).is_none() => {
                    session.member_runtimes().reserve_repair(&agent.slot_id)
                }
                MemberRuntimeSnapshot::Ready => ReserveAttach::AlreadyReady,
                _ => session.member_runtimes().reserve_attach(&agent.slot_id, true),
            };

            match reservation {
                ReserveAttach::Start(owner) => {
                    self.broadcast_agent_runtime_status(
                        session.user_id(),
                        session.team_id(),
                        agent,
                        TeamAgentRuntimeStatus::Pending,
                        None,
                    );
                    work.push(MemberRuntimeReconcileWork {
                        agent: agent.clone(),
                        waiter: owner.waiter(),
                        owner: Some(owner),
                    });
                }
                ReserveAttach::Join(waiter) | ReserveAttach::Removing(waiter) => {
                    info!(
                        team_id = session.team_id(),
                        slot_id = agent.slot_id,
                        conversation_id = agent.conversation_id,
                        operation_id = waiter.operation_id(),
                        generation = session.generation(),
                        duration_ms = 0,
                        error_classification = "none",
                        "team member runtime reconciliation waiting"
                    );
                    work.push(MemberRuntimeReconcileWork {
                        agent: agent.clone(),
                        waiter,
                        owner: None,
                    });
                }
                ReserveAttach::AlreadyReady => {}
                ReserveAttach::SessionStopped => {
                    return Err(TeamError::InvalidRequest(
                        "team session stopped during reconciliation reservation".to_owned(),
                    ));
                }
            }
        }
        Ok(work)
    }

    async fn complete_member_runtime_reconciliation(
        &self,
        team_id: &str,
        user_id: &str,
        session: Arc<TeamSession>,
        work: Vec<MemberRuntimeReconcileWork>,
    ) -> Result<(), TeamError> {
        // Session-level `Starting` is leader-scoped (spec 5.4/5.5): only a
        // leader (re)attach may raise the overlay. Reconciliation that touches
        // only teammates (Ready-repair, Failed-retry of a non-lead member) keeps
        // its progress inline via `agentRuntimeStatusChanged`.
        if work.iter().any(|item| item.agent.role == TeammateRole::Lead) {
            self.publish_member_runtime_starting_if_current(&session);
        }
        let mut waiters = Vec::with_capacity(work.len());
        for item in work {
            let waiter = item.waiter;
            if let Some(owner) = item.owner {
                tokio::spawn(attach_member_runtime(
                    self.self_ref
                        .upgrade()
                        .ok_or_else(|| TeamError::InvalidRequest("team service is shutting down".to_owned()))?,
                    Arc::clone(&session),
                    user_id.to_owned(),
                    item.agent.clone(),
                    self.task_manager.clone(),
                    owner,
                    // Reconciliation repairs runtimes without a fresh delivery to
                    // re-delegate; failures surface via runtime status only.
                    false,
                ));
            }
            waiters.push((item.agent, waiter));
        }

        let outcomes = futures_util::future::join_all(
            waiters
                .into_iter()
                .map(|(agent, waiter)| async move { (agent, waiter.wait().await) }),
        )
        .await;

        let membership_lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _membership_guard = membership_lock.lock().await;
        let current_agents = match self.repo.get_team(user_id, team_id).await? {
            Some(row) => Team::from_row(&row)?.agents,
            None => return Err(TeamError::TeamNotFound(team_id.to_owned())),
        };
        let current_slots = current_agents
            .iter()
            .map(|agent| agent.slot_id.as_str())
            .collect::<HashSet<_>>();
        let current_session = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.to_owned()))?;
        if !Arc::ptr_eq(&current_session.session, &session) {
            return Err(TeamError::SessionNotFound(team_id.to_owned()));
        }

        for (agent, outcome) in outcomes {
            if !current_slots.contains(agent.slot_id.as_str()) {
                continue;
            }
            match outcome {
                AttachOutcome::Ready | AttachOutcome::Removed => {}
                AttachOutcome::Failed(failure) => {
                    // Session-level Failed / the whole-team error are leader-scoped
                    // (spec 5.4/5.5): only a leader failure raises the full-screen
                    // failure card and blocks the team. A teammate reconciliation
                    // failure stays inline — its `agentRuntimeStatusChanged=failed`
                    // already fired in `attach_member_runtime` — and the team stays
                    // usable because the leader is ready. `ensure_session` (invoked
                    // on mount, model switches, and before sends via warmupSession)
                    // must not fail just because an unrelated teammate is broken.
                    if agent.role == TeammateRole::Lead {
                        self.broadcast_session_status(
                            user_id,
                            team_id,
                            TeamSessionStatus::Failed,
                            Some(TeamSessionPhase::AttachingAgents),
                            |payload| payload.error = Some(failure.public_reason.clone()),
                        );
                        return Err(TeamError::MemberRuntimeFailed {
                            team_id: team_id.to_owned(),
                            slot_id: agent.slot_id,
                            conversation_id: agent.conversation_id,
                            public_reason: failure.public_reason,
                        });
                    }
                }
                AttachOutcome::SessionStopped => {
                    return Err(TeamError::InvalidRequest(
                        "team session stopped during reconciliation".to_owned(),
                    ));
                }
            }
        }

        self.broadcast_session_status(user_id, team_id, TeamSessionStatus::Ready, None, |payload| {
            payload.server_count = Some(current_agents.len());
        });
        Ok(())
    }

    pub(crate) async fn cleanup_stale_member_runtime_task(
        &self,
        captured_session: &TeamSession,
        conversation_id: &str,
    ) {
        let lock = self
            .ensure_session_locks
            .entry(captured_session.team_id().to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;
        if self
            .sessions
            .get(captured_session.team_id())
            .is_some_and(|entry| !std::ptr::eq(entry.session.as_ref(), captured_session))
        {
            return;
        }
        self.task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::TeamMcpRebuild))
            .await;
    }

    pub async fn get_conversation_config_options(
        &self,
        user_id: &str,
        team_id: &str,
        conversation_id: &str,
    ) -> Result<GetConfigOptionsResponse, TeamError> {
        let row = self.load_owned_team_row(user_id, team_id).await?;

        let team = Team::from_row(&row)?;
        let member = team.agents.iter().any(|agent| agent.conversation_id == conversation_id);
        if !member {
            return Err(TeamError::AgentNotFound(conversation_id.to_owned()));
        }

        self.conversation_port.get_config_options(conversation_id).await
    }

    fn broadcast_session_status<F>(
        &self,
        user_id: &str,
        team_id: &str,
        status: TeamSessionStatus,
        phase: Option<TeamSessionPhase>,
        customize: F,
    ) where
        F: FnOnce(&mut TeamSessionStatusPayload),
    {
        let mut payload = TeamSessionStatusPayload {
            team_id: team_id.to_owned(),
            status,
            phase,
            server_count: None,
            error: None,
        };
        customize(&mut payload);
        // Session-level status drives the full-screen warmup overlay (leader-only,
        // spec 5.4/5.5). This is a low-volume lifecycle boundary per team, so log
        // at info for production diagnosability — the reason is already the
        // sanitized public failure text, never a raw payload.
        info!(
            team_id = %payload.team_id,
            status = ?payload.status,
            phase = ?payload.phase,
            server_count = ?payload.server_count,
            error = payload.error.as_deref().unwrap_or(""),
            "team session status broadcast"
        );
        // Keep per-user scoping so the overlay event reaches only the owning
        // user's WebSocket subscribers.
        let mut value = serde_json::to_value(payload).expect("serialize team session status payload");
        value["user_id"] = serde_json::Value::String(user_id.to_owned());
        let event = WebSocketMessage::new(TEAM_SESSION_STATUS_CHANGED_EVENT, value);
        self.broadcaster.broadcast(event);
    }

    fn spawn_slow_monitor(session: Arc<TeamSession>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let snapshot = session.work_coordinator().snapshot();
                session.team_run_manager().publish_snapshot_update(&snapshot);
            }
        })
    }

    fn broadcast_team_created(&self, user_id: &str, team_id: &str, team_name: &str) {
        info!(team_id = %team_id, event_name = TEAM_CREATED_EVENT, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            TEAM_CREATED_EVENT,
            serde_json::json!({ "user_id": user_id, "team_id": team_id, "team_name": team_name }),
        ));
        self.broadcast_team_list_changed(user_id, team_id, "created");
    }

    fn broadcast_team_removed(&self, user_id: &str, team_id: &str) {
        info!(team_id = %team_id, event_name = TEAM_REMOVED_EVENT, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            TEAM_REMOVED_EVENT,
            serde_json::json!({ "user_id": user_id, "team_id": team_id }),
        ));
        self.broadcast_team_list_changed(user_id, team_id, "removed");
    }

    fn broadcast_team_renamed(&self, user_id: &str, team_id: &str, team_name: &str) {
        info!(team_id = %team_id, event_name = TEAM_RENAMED_EVENT, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            TEAM_RENAMED_EVENT,
            serde_json::json!({ "user_id": user_id, "team_id": team_id, "team_name": team_name }),
        ));
        self.broadcast_team_list_changed(user_id, team_id, "renamed");
    }

    fn broadcast_team_list_changed(&self, user_id: &str, team_id: &str, action: &str) {
        info!(team_id = %team_id, event_name = crate::events::TEAM_LIST_CHANGED_EVENT, action, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            crate::events::TEAM_LIST_CHANGED_EVENT,
            serde_json::json!({ "user_id": user_id, "team_id": team_id, "action": action }),
        ));
    }

    pub(crate) fn broadcast_agent_runtime_status(
        &self,
        user_id: &str,
        team_id: &str,
        agent: &TeamAgent,
        status: TeamAgentRuntimeStatus,
        error: Option<String>,
    ) {
        TeamEventEmitter::new(team_id.to_owned(), user_id.to_owned(), self.broadcaster.clone())
            .broadcast_agent_runtime_status(agent, status, error);
    }

    /// Register an event loop for an attaching agent.
    ///
    /// Called from `attach_member_runtime` (the single attach path used by
    /// leader cold-start, reconciliation, `add_agent`, `spawn_agent`, and lazy
    /// wakeup) after the agent process warms up, so it gets its own drain loop.
    pub(crate) fn register_event_loop(
        &self,
        session: &Arc<TeamSession>,
        slot_id: &str,
    ) -> Result<bool, EventLoopRegistrationError> {
        let registry = session.event_loops();

        let ctx = AgentLoopContext {
            team_id: session.team_id().to_owned(),
            slot_id: slot_id.to_owned(),
            user_id: session.user_id().to_owned(),
            session: session.clone(),
            scheduler: session.scheduler().clone(),
            mailbox: session.mailbox().clone(),
            turn_port: self.turn_port.clone(),
            registry: registry.clone(),
        };
        match registry.spawn(slot_id, ctx) {
            Ok(()) => {
                info!(
                    team_id = session.team_id(),
                    slot_id,
                    generation = session.generation(),
                    "agent event loop registered"
                );
                Ok(true)
            }
            Err(EventLoopRegistrationError::Duplicate) => Ok(false),
            Err(error) => Err(error),
        }
    }

    pub async fn get_session_user_id(&self, team_id: &str) -> Option<String> {
        self.sessions.get(team_id).map(|e| e.session.user_id().to_owned())
    }

    pub(crate) fn capture_published_session(&self, expected: &TeamSession) -> Option<Arc<TeamSession>> {
        self.sessions
            .get(expected.team_id())
            .and_then(|entry| std::ptr::eq(entry.session.as_ref(), expected).then(|| Arc::clone(&entry.session)))
    }

    /// Run a synchronous side effect only while `expected` is still the
    /// published session. Keeping the map guard alive through `action`
    /// serializes the effect with session removal/replacement.
    pub(crate) fn with_published_session<R>(
        &self,
        expected: &TeamSession,
        action: impl FnOnce(&TeamSession) -> R,
    ) -> Option<R> {
        let entry = self.sessions.get(expected.team_id())?;
        std::ptr::eq(entry.session.as_ref(), expected).then(|| action(&entry.session))
    }

    pub(crate) fn publish_member_runtime_ready_if_current(&self, expected: &TeamSession, agent: &TeamAgent) -> bool {
        self.with_published_session(expected, |_| {
            self.broadcast_agent_runtime_status(
                expected.user_id(),
                expected.team_id(),
                agent,
                TeamAgentRuntimeStatus::Ready,
                None,
            );
        })
        .is_some()
    }

    pub(crate) fn publish_member_runtime_starting_if_current(&self, expected: &TeamSession) -> bool {
        self.with_published_session(expected, |_| {
            self.broadcast_session_status(
                expected.user_id(),
                expected.team_id(),
                TeamSessionStatus::Starting,
                Some(TeamSessionPhase::AttachingAgents),
                |_| {},
            );
        })
        .is_some()
    }

    pub(crate) fn publish_member_runtime_failed_if_current(&self, expected: &TeamSession, reason: &str) -> bool {
        self.with_published_session(expected, |_| {
            self.broadcast_session_status(
                expected.user_id(),
                expected.team_id(),
                TeamSessionStatus::Failed,
                Some(TeamSessionPhase::AttachingAgents),
                |payload| payload.error = Some(reason.to_owned()),
            );
        })
        .is_some()
    }

    pub(crate) async fn refresh_member_runtime_status(&self, expected: &TeamSession) {
        let membership_lock = self
            .add_agent_locks
            .entry(expected.team_id().to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _membership_guard = membership_lock.lock().await;
        let Ok(Some(row)) = self.repo.get_team(expected.user_id(), expected.team_id()).await else {
            return;
        };
        let Ok(team) = Team::from_row(&row) else {
            return;
        };

        // Session-level status is leader-scoped: "Ready = leader ready = team
        // usable" (spec 5.2). It drives the full-screen warmup overlay, which
        // must reflect the leader only (spec 5.4/5.5). Teammate runtimes
        // (dormant/pending/ready/failed) are surfaced per-member via
        // `agentRuntimeStatusChanged` and must NOT flip the session status, or
        // the overlay would resurface on lazy wakeup / add-member and a teammate
        // failure would raise the full-screen failure card.
        let Some(leader) = team.agents.iter().find(|agent| agent.role == TeammateRole::Lead) else {
            // No lead in the roster is a malformed team; bootstrap already
            // reports it. Nothing to publish here.
            return;
        };

        match expected.member_runtimes().snapshot(&leader.slot_id) {
            MemberRuntimeSnapshot::Ready => {
                let _ = self.with_published_session(expected, |_| {
                    self.broadcast_session_status(
                        expected.user_id(),
                        expected.team_id(),
                        TeamSessionStatus::Ready,
                        None,
                        |payload| {
                            payload.server_count = Some(team.agents.len());
                        },
                    );
                });
            }
            MemberRuntimeSnapshot::Failed { failure, .. } => {
                self.publish_member_runtime_failed_if_current(expected, &failure.public_reason);
            }
            // An in-flight leader attach/remove (cold start, repair, retry) is
            // the only case that legitimately raises the overlay again. The lead
            // cannot be removed, so `Removing` is defensive.
            MemberRuntimeSnapshot::Attaching { .. } | MemberRuntimeSnapshot::Removing { .. } => {
                self.publish_member_runtime_starting_if_current(expected);
            }
            // The leader is attached at bootstrap and never dormant in steady
            // state; treat a stray Absent defensively as in-flight rather than
            // prematurely declaring Ready.
            MemberRuntimeSnapshot::Absent => {
                self.publish_member_runtime_starting_if_current(expected);
            }
            MemberRuntimeSnapshot::SessionStopped => {}
        }
    }

    pub async fn get_run_state(&self, user_id: &str, team_id: &str) -> Result<TeamRunStateResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        let session = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session));
        let Some(session) = session else {
            return Ok(TeamRunStateResponse {
                session_generation: None,
                active_run: None,
                slot_work: Vec::new(),
            });
        };
        let snapshot = session.work_coordinator().snapshot();
        let active_run = session.team_run_manager().current_payload(&snapshot).filter(|run| {
            matches!(
                run.status,
                aionui_api_types::TeamRunStatus::Accepted
                    | aionui_api_types::TeamRunStatus::Running
                    | aionui_api_types::TeamRunStatus::Cancelling
            )
        });
        let slot_work = snapshot.slots.iter().map(TeamRunManager::slot_payload).collect();
        Ok(TeamRunStateResponse {
            session_generation: Some(snapshot.session_generation),
            active_run,
            slot_work,
        })
    }

    pub fn get_session_scheduler(&self, team_id: &str) -> Option<Arc<crate::scheduler::TeammateManager>> {
        self.sessions.get(team_id).map(|e| e.session.scheduler().clone())
    }

    pub async fn resolve_team_tool_context(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<ResolvedTeamToolContext, TeamToolErrorPayload> {
        let Some(binding_lookup) = self
            .conversation_port
            .lookup_team_binding_by_conversation(conversation_id)
            .await
            .map_err(|error| error_payload(TeamToolErrorCode::RuntimeContextMissing, error.to_string()))?
        else {
            return Err(error_payload(
                TeamToolErrorCode::ConversationNotFound,
                "conversation not found",
            ));
        };

        if binding_lookup.user_id != user_id {
            return Err(error_payload(
                TeamToolErrorCode::PermissionDenied,
                "conversation does not belong to user",
            ));
        }

        let Some(team_id) = binding_lookup.team_id.clone() else {
            return Ok(ResolvedTeamToolContext {
                response: TeamToolContextResponse {
                    in_team: false,
                    conversation_id: conversation_id.to_owned(),
                    team_id: None,
                    team_name: None,
                    slot_id: None,
                    role: None,
                    agent_name: None,
                    transport: None,
                    allowed_tools: Vec::new(),
                },
                context: None,
            });
        };

        let team_row = self
            .repo
            .get_team(user_id, &team_id)
            .await
            .map_err(|error| error_payload(TeamToolErrorCode::RuntimeContextMissing, error.to_string()))?
            .ok_or_else(|| error_payload(TeamToolErrorCode::TeamNotFound, "team not found"))?;

        let binding = TeamSessionBinding {
            team_id: team_id.clone(),
            slot_id: binding_lookup.slot_id,
            role: binding_lookup.role,
            runtime_seed: Default::default(),
            mcp: None,
        };
        let agents: Vec<crate::types::TeamAgent> = serde_json::from_str(&team_row.agents)
            .map_err(|error| error_payload(TeamToolErrorCode::RuntimeContextMissing, error.to_string()))?;
        let agent = agent_for_conversation(&agents, conversation_id, &binding)?;
        let context = crate::tool_executor::TeamToolContext {
            team_id: team_id.clone(),
            caller_slot_id: agent.slot_id.clone(),
            caller_role: agent.role,
            user_id: Some(user_id.to_owned()),
            conversation_id: Some(conversation_id.to_owned()),
            transport: TeamToolTransport::CliAssumed,
        };
        let allowed_tools = aionui_api_types::team_tool_descriptors_for_role(role_to_tool_role(agent.role))
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        Ok(ResolvedTeamToolContext {
            response: TeamToolContextResponse {
                in_team: true,
                conversation_id: conversation_id.to_owned(),
                team_id: Some(team_id),
                team_name: Some(team_row.name),
                slot_id: Some(agent.slot_id.clone()),
                role: Some(role_to_tool_role(agent.role)),
                agent_name: Some(agent.name.clone()),
                transport: Some(TeamToolTransport::CliAssumed),
                allowed_tools,
            },
            context: Some(context),
        })
    }

    pub async fn execute_team_tool(
        &self,
        context: &crate::tool_executor::TeamToolContext,
        call: TeamToolCall,
    ) -> Result<serde_json::Value, TeamToolErrorPayload> {
        let scheduler = self
            .get_session_scheduler(&context.team_id)
            .ok_or_else(|| error_payload(TeamToolErrorCode::TeamNotFound, "active team session not found"))?;
        execute_with_scheduler(&scheduler, &self.self_ref, context, call).await
    }

    #[cfg(test)]
    fn session_has_slow_monitor(&self, team_id: &str) -> bool {
        self.sessions
            .get(team_id)
            .map(|entry| !entry.slow_monitor_handle.is_finished())
            .unwrap_or(false)
    }

    #[cfg(test)]
    fn session_count_for_test(&self) -> usize {
        self.sessions.len()
    }

    pub async fn stop_session(&self, user_id: &str, team_id: &str) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.stop_session_unchecked(team_id);
        Ok(())
    }

    pub fn stop_sessions_for_user(&self, user_id: &str) -> usize {
        let team_ids: Vec<String> = self
            .sessions
            .iter()
            .filter(|entry| entry.session.user_id() == user_id)
            .map(|entry| entry.key().clone())
            .collect();
        let stopped = team_ids.len();
        for team_id in team_ids {
            self.stop_session_unchecked(&team_id);
        }
        stopped
    }

    fn stop_session_unchecked(&self, team_id: &str) {
        if let Some((_, entry)) = self.sessions.remove(team_id) {
            entry.slow_monitor_handle.abort();
            entry.session.stop();
        }
    }

    pub async fn cleanup_idle_team_runtime_tasks(
        &self,
        idle_conversation_ids: Vec<String>,
        active_leases: &ActiveLeaseRegistry,
        idle_threshold_ms: TimestampMs,
    ) -> Vec<String> {
        if idle_conversation_ids.is_empty() {
            return Vec::new();
        }

        let idle_conversation_set: HashSet<String> = idle_conversation_ids.iter().cloned().collect();
        let now = now_ms();
        let mut handled_conversations = HashSet::new();
        let mut cleanup_teams = Vec::new();

        for entry in self.sessions.iter() {
            let team_id = entry.key().clone();
            let session = Arc::clone(&entry.session);
            let agents = session.scheduler().list_agents().await;
            let matched_idle_count = agents
                .iter()
                .filter(|agent| idle_conversation_set.contains(&agent.conversation_id))
                .count();
            if matched_idle_count == 0 {
                continue;
            }

            for agent in &agents {
                handled_conversations.insert(agent.conversation_id.clone());
            }

            if session.team_run_manager().current_active_run_id().is_some() {
                debug!(
                    team_id,
                    matched_idle_count, "team idle cleanup skipped because team run is active"
                );
                continue;
            }

            if agents
                .iter()
                .any(|agent| active_leases.active_until(&agent.conversation_id).is_some())
            {
                debug!(
                    team_id,
                    matched_idle_count, "team idle cleanup skipped because at least one member has an active lease"
                );
                continue;
            }

            if !agents.iter().all(|agent| {
                self.task_manager
                    .get_task(&agent.conversation_id)
                    .map(|task| is_idle_collectable_team_member(&task, now, idle_threshold_ms))
                    .unwrap_or(true)
            }) {
                debug!(
                    team_id,
                    matched_idle_count, "team idle cleanup skipped because at least one member runtime task is active"
                );
                continue;
            }

            cleanup_teams.push((team_id, agents, matched_idle_count));
        }

        for (team_id, agents, matched_idle_count) in cleanup_teams {
            info!(
                team_id,
                matched_idle_count,
                member_count = agents.len(),
                "team idle cleanup stopping idle team session"
            );
            info!(team_id, reason = "idle_cleanup", "broadcasting team session stopped");
            if let Some(entry) = self.sessions.get(&team_id) {
                self.broadcast_session_status(
                    entry.session.user_id(),
                    &team_id,
                    TeamSessionStatus::Stopped,
                    None,
                    |_| {},
                );
            }
            self.stop_session_unchecked(&team_id);
            for agent in agents {
                self.task_manager
                    .kill_and_wait(&agent.conversation_id, Some(AgentKillReason::IdleTimeout))
                    .await;
            }
        }

        idle_conversation_ids
            .into_iter()
            .filter(|conversation_id| !handled_conversations.contains(conversation_id))
            .collect()
    }

    pub async fn send_message(
        &self,
        user_id: &str,
        team_id: &str,
        content: &str,
        files: Option<Vec<ChatFileRef>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await?;
        let (content, files) = self.resolve_message_attachments(user_id, content, files).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.send_message(&content, files).await
    }

    pub async fn send_message_to_agent(
        &self,
        user_id: &str,
        team_id: &str,
        slot_id: &str,
        content: &str,
        files: Option<Vec<ChatFileRef>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await?;
        let (content, files) = self.resolve_message_attachments(user_id, content, files).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.send_message_to_agent(slot_id, &content, files).await
    }

    /// Resolve a send's attachments to absolute paths and re-inline them into
    /// the content (`[[AION_FILES]]` form) at the team send boundary. Atomic;
    /// empty/absent `files` is a no-op needing no project service.
    async fn resolve_message_attachments(
        &self,
        user_id: &str,
        content: &str,
        files: Option<Vec<ChatFileRef>>,
    ) -> Result<(String, Option<Vec<String>>), TeamError> {
        let files = match files {
            Some(files) if !files.is_empty() => files,
            _ => return Ok((content.to_owned(), None)),
        };
        let project = self
            .project_service
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| {
                TeamError::InvalidRequest("project service unavailable; cannot resolve file attachments".into())
            })?;
        let upload_root = std::env::temp_dir().join("aionui");
        let resolved = project
            .resolve_chat_message(user_id, content, &files, &upload_root)
            .await
            .map_err(|err| TeamError::InvalidRequest(err.to_string()))?;
        Ok((resolved.content, Some(resolved.files)))
    }

    /// Directed retry/wakeup for a single member runtime (dormant or failed),
    /// reusing the one attach path. Backs the send-box "retry start" entry.
    /// `reserve_attach(slot, true)` retries a `Failed` member; a dormant
    /// (`Absent`) member is attached fresh. Non-blocking: the attach runs in
    /// the background and any preserved unread mailbox rows are re-drained by
    /// the member's event loop via `reconcile_mailbox`.
    pub async fn attach_agent_runtime(&self, user_id: &str, team_id: &str, slot_id: &str) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        let agent = session.scheduler().get_agent(slot_id).await?;
        let service = self
            .self_ref
            .upgrade()
            .ok_or_else(|| TeamError::InvalidRequest("team service is shutting down".to_owned()))?;
        let reservation = session.member_runtimes().reserve_attach(slot_id, true);
        self.broadcast_agent_runtime_status(user_id, team_id, &agent, TeamAgentRuntimeStatus::Pending, None);
        spawn_attach_agent_process_bg(
            service,
            Arc::clone(&session),
            user_id.to_owned(),
            agent,
            self.task_manager.clone(),
            reservation,
            // Directed user retry: failures surface inline, do not wake the leader.
            false,
        );
        Ok(())
    }

    pub async fn cancel_run(
        &self,
        user_id: &str,
        team_id: &str,
        team_run_id: &str,
        target_slot_id: Option<String>,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.cancel_run(team_run_id, target_slot_id, reason).await
    }

    pub async fn cancel_child_turn(
        &self,
        user_id: &str,
        team_id: &str,
        team_run_id: &str,
        slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.cancel_child_turn(team_run_id, slot_id, reason).await
    }

    pub async fn pause_slot_work(
        &self,
        user_id: &str,
        team_id: &str,
        team_run_id: &str,
        slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id, Some(user_id)).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.pause_slot_work(team_run_id, slot_id, reason).await
    }

    pub async fn set_session_mode(&self, user_id: &str, team_id: &str, mode: &str) -> Result<(), TeamError> {
        let team = self.load_owned_team(user_id, team_id).await?;
        let provisioner = self.provisioner();
        self.repo
            .update_team(
                user_id,
                team_id,
                &UpdateTeamParams {
                    session_mode: Some(mode.to_owned()),
                    ..Default::default()
                },
            )
            .await?;

        for agent in &team.agents {
            let mode_applied = match self.task_manager.get_task(&agent.conversation_id) {
                Some(instance) => match set_active_agent_session_mode(&instance, mode).await {
                    Ok(()) => true,
                    Err(e) => {
                        warn!(
                            team_id,
                            slot_id = %agent.slot_id,
                            conversation_id = %agent.conversation_id,
                            error = %e,
                            "failed to set session mode on agent"
                        );
                        false
                    }
                },
                None => true,
            };
            if mode_applied && let Err(e) = provisioner.update_session_mode_seed(agent, mode).await {
                warn!(
                    team_id,
                    slot_id = %agent.slot_id,
                    conversation_id = %agent.conversation_id,
                    error = %e,
                    "failed to persist team session mode seed"
                );
            }
        }

        Ok(())
    }

    pub async fn send_agent_message_from_agent(
        &self,
        team_id: &str,
        from_slot_id: &str,
        to_slot_id: &str,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<AgentMessageQueueResult, TeamError> {
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session
            .send_agent_message_from_agent(from_slot_id, to_slot_id, content, files)
            .await
    }

    pub async fn shutdown_agent_in_session(
        &self,
        team_id: &str,
        caller_slot_id: &str,
        target_slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.shutdown_agent(caller_slot_id, target_slot_id, reason).await
    }

    pub(crate) async fn wake_leader_after_recovery_message(
        &self,
        team_id: &str,
        source_slot_id: &str,
        source: WorkSource,
    ) -> Result<(), TeamError> {
        let entry = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
        entry
            .session
            .wake_leader_after_recovery_message(source_slot_id, source)
            .await
    }
}

async fn set_active_agent_session_mode(instance: &AgentInstance, mode: &str) -> Result<(), AgentError> {
    #[allow(unreachable_patterns)]
    match instance {
        AgentInstance::Acp(_) => instance.set_config_option("mode", mode).await.map(|_| ()),
        AgentInstance::Aionrs(manager) => manager.set_mode(mode).await,
        _ => instance.set_config_option("mode", mode).await.map(|_| ()),
    }
}

fn is_idle_collectable_team_member(task: &AgentInstance, now: TimestampMs, idle_threshold_ms: TimestampMs) -> bool {
    if !matches!(
        task.status(),
        None | Some(ConversationStatus::Pending | ConversationStatus::Finished)
    ) {
        return false;
    }
    now.saturating_sub(task.last_activity_at()) > idle_threshold_ms
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
    use aionui_ai_agent::{
        ActiveLeaseRegistry, AgentError, AgentInstance, AgentSendError, AgentStreamEvent, IAgentTask, IMockAgent,
        IWorkerTaskManager, IdleCleanupCoordinator,
    };
    use aionui_api_types::{AddAgentRequest, ConfigOptionConfirmation, SetConfigOptionResponse};
    use aionui_common::{AgentKillReason, AgentType, ConversationStatus, TimestampMs, now_ms};
    use aionui_db::{IConversationRepository, ITeamRepository};
    use tokio::sync::broadcast;

    use super::TeamIdleCleanupCoordinator;
    use crate::test_utils::workspace_harness::{
        setup_with_factory_metadata_team_repo_and_conversation_repo,
        setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster,
        setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager,
        single_agent_team_request,
    };

    struct ModeSettingAgent {
        conversation_id: String,
        agent_type: AgentType,
        mode_result: Mutex<Result<(), String>>,
        event_tx: broadcast::Sender<AgentStreamEvent>,
        status: Option<ConversationStatus>,
        last_activity_at: TimestampMs,
    }

    impl ModeSettingAgent {
        fn accepts_mode(conversation_id: &str) -> Self {
            Self::new(conversation_id, Ok(()))
        }

        fn rejects_mode(conversation_id: &str, message: &str) -> Self {
            Self::new(conversation_id, Err(message.to_owned()))
        }

        fn new(conversation_id: &str, mode_result: Result<(), String>) -> Self {
            let (event_tx, _) = broadcast::channel(1);
            Self {
                conversation_id: conversation_id.to_owned(),
                agent_type: AgentType::Acp,
                mode_result: Mutex::new(mode_result),
                event_tx,
                status: None,
                last_activity_at: now_ms(),
            }
        }

        fn idle_finished(conversation_id: &str) -> Self {
            Self::accepts_mode(conversation_id)
                .with_status(Some(ConversationStatus::Finished))
                .with_last_activity(now_ms() - 600_000)
        }

        fn idle_pending_aionrs(conversation_id: &str) -> Self {
            Self::accepts_mode(conversation_id)
                .with_agent_type(AgentType::Aionrs)
                .with_status(Some(ConversationStatus::Pending))
                .with_last_activity(now_ms() - 600_000)
        }

        fn with_agent_type(mut self, agent_type: AgentType) -> Self {
            self.agent_type = agent_type;
            self
        }

        fn with_status(mut self, status: Option<ConversationStatus>) -> Self {
            self.status = status;
            self
        }

        fn with_last_activity(mut self, last_activity_at: TimestampMs) -> Self {
            self.last_activity_at = last_activity_at;
            self
        }
    }

    #[async_trait::async_trait]
    impl IAgentTask for ModeSettingAgent {
        fn agent_type(&self) -> AgentType {
            self.agent_type
        }

        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }

        fn workspace(&self) -> &str {
            "/tmp/aioncore-team-mode-test"
        }

        fn status(&self) -> Option<ConversationStatus> {
            self.status
        }

        fn last_activity_at(&self) -> TimestampMs {
            self.last_activity_at
        }

        fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
            self.event_tx.subscribe()
        }

        async fn send_message(&self, _data: SendMessageData) -> Result<(), AgentSendError> {
            Ok(())
        }

        async fn cancel(&self) -> Result<(), AgentError> {
            Ok(())
        }

        fn kill(&self, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl IMockAgent for ModeSettingAgent {
        async fn set_config_option(&self, option_id: &str, value: &str) -> Result<SetConfigOptionResponse, AgentError> {
            assert_eq!(option_id, "mode");
            assert_eq!(value, "read-only");
            match self.mode_result.lock().unwrap().clone() {
                Ok(()) => Ok(SetConfigOptionResponse {
                    confirmation: ConfigOptionConfirmation::Observed,
                    config_options: None,
                }),
                Err(message) => Err(AgentError::bad_request(message)),
            }
        }
    }

    struct StaticTaskManager {
        tasks: HashMap<String, AgentInstance>,
    }

    impl StaticTaskManager {
        fn new(tasks: HashMap<String, AgentInstance>) -> Self {
            Self { tasks }
        }
    }

    #[async_trait::async_trait]
    impl IWorkerTaskManager for StaticTaskManager {
        fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
            self.tasks.get(conversation_id).cloned()
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, AgentError> {
            Err(AgentError::internal("static task manager does not build tasks"))
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
            self.tasks.len()
        }

        fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    struct MutableTaskManager {
        tasks: Mutex<HashMap<String, AgentInstance>>,
        kills: Mutex<Vec<String>>,
    }

    impl MutableTaskManager {
        fn new() -> Self {
            Self {
                tasks: Mutex::new(HashMap::new()),
                kills: Mutex::new(Vec::new()),
            }
        }

        fn insert_mode_agent(&self, conversation_id: &str) {
            self.tasks.lock().unwrap().insert(
                conversation_id.to_owned(),
                AgentInstance::Mock(Arc::new(ModeSettingAgent::accepts_mode(conversation_id))),
            );
        }

        fn insert_idle_finished_agent(&self, conversation_id: &str) {
            self.tasks.lock().unwrap().insert(
                conversation_id.to_owned(),
                AgentInstance::Mock(Arc::new(ModeSettingAgent::idle_finished(conversation_id))),
            );
        }

        fn insert_idle_pending_aionrs_agent(&self, conversation_id: &str) {
            self.tasks.lock().unwrap().insert(
                conversation_id.to_owned(),
                AgentInstance::Mock(Arc::new(ModeSettingAgent::idle_pending_aionrs(conversation_id))),
            );
        }

        fn remove(&self, conversation_id: &str) {
            self.tasks.lock().unwrap().remove(conversation_id);
        }

        fn reset_kills(&self) {
            self.kills.lock().unwrap().clear();
        }

        fn kills(&self) -> Vec<String> {
            self.kills.lock().unwrap().clone()
        }
    }

    fn two_agent_team_request(name: &str) -> aionui_api_types::CreateTeamRequest {
        aionui_api_types::CreateTeamRequest {
            name: name.into(),
            agents: vec![
                aionui_api_types::TeamAgentInput {
                    name: "Lead".into(),
                    role: "lead".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                },
                aionui_api_types::TeamAgentInput {
                    name: "Worker".into(),
                    role: "teammate".into(),
                    backend: Some("acp".into()),
                    model: "claude".into(),
                    assistant_id: None,
                    conversation_id: None,
                },
            ],
            workspace: None,
        }
    }

    fn team_with_aionrs_worker_request(name: &str) -> aionui_api_types::CreateTeamRequest {
        let mut request = two_agent_team_request(name);
        request.agents.push(aionui_api_types::TeamAgentInput {
            name: "Butler".into(),
            role: "teammate".into(),
            backend: Some("aionrs".into()),
            model: "claude-sonnet".into(),
            assistant_id: None,
            conversation_id: None,
        });
        request
    }

    #[async_trait::async_trait]
    impl IWorkerTaskManager for MutableTaskManager {
        fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
            self.tasks.lock().unwrap().get(conversation_id).cloned()
        }

        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, AgentError> {
            Err(AgentError::internal("mutable task manager does not build tasks"))
        }

        fn kill(&self, conversation_id: &str, _reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            self.kills.lock().unwrap().push(conversation_id.to_owned());
            self.remove(conversation_id);
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
            self.tasks.lock().unwrap().clear();
        }

        fn active_count(&self) -> usize {
            self.tasks.lock().unwrap().len()
        }

        fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    #[tokio::test]
    async fn session_has_slow_monitor() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Slow Monitor"))
            .await
            .unwrap();

        svc.ensure_session("user-test", &created.id).await.unwrap();

        assert!(svc.session_has_slow_monitor(&created.id));
        svc.stop_session("user-test", &created.id).await.unwrap();
    }

    #[tokio::test]
    async fn stop_sessions_for_user_keeps_other_user_sessions() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let owned = svc
            .create_team("user-test", single_agent_team_request("Owned Session"))
            .await
            .unwrap();
        let other = svc
            .create_team("user-other", single_agent_team_request("Other Session"))
            .await
            .unwrap();

        svc.ensure_session("user-test", &owned.id).await.unwrap();
        svc.ensure_session("user-other", &other.id).await.unwrap();

        assert_eq!(svc.stop_sessions_for_user("user-test"), 1);
        assert_eq!(svc.session_count_for_test(), 1);
        assert!(!svc.session_has_slow_monitor(&owned.id));
        assert!(svc.session_has_slow_monitor(&other.id));

        svc.stop_session("user-other", &other.id).await.unwrap();
    }

    #[tokio::test]
    async fn ensure_session_emits_agent_runtime_ready_after_member_warmup() {
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster();
        let created = svc
            .create_team("user-test", single_agent_team_request("Runtime Events"))
            .await
            .unwrap();
        let assistant = created.assistants.first().expect("team assistant");

        svc.ensure_session("user-test", &created.id).await.unwrap();

        let events = broadcaster.events_by_name("team.agentRuntimeStatusChanged");
        let statuses: Vec<&str> = events
            .iter()
            .map(|event| event.data.get("status").and_then(serde_json::Value::as_str).unwrap())
            .collect();

        assert_eq!(statuses, vec!["pending", "ready"]);
        assert_eq!(
            events[0].data.get("team_id").and_then(serde_json::Value::as_str),
            Some(created.id.as_str())
        );
        assert_eq!(
            events[0].data.get("slot_id").and_then(serde_json::Value::as_str),
            Some(assistant.slot_id.as_str())
        );
        assert_eq!(
            events[0]
                .data
                .get("conversation_id")
                .and_then(serde_json::Value::as_str),
            Some(assistant.conversation_id.as_str())
        );
    }

    #[tokio::test]
    async fn ensure_session_repairs_only_missing_member_runtime_in_place() {
        let task_manager = Arc::new(MutableTaskManager::new());
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager.clone());
        let created = svc
            .create_team("user-test", two_agent_team_request("Runtime Repair"))
            .await
            .unwrap();
        let lead = created.assistants.iter().find(|agent| agent.role == "lead").unwrap();
        let worker = created
            .assistants
            .iter()
            .find(|agent| agent.role == "teammate")
            .unwrap();

        svc.ensure_session("user-test", &created.id).await.unwrap();
        // Leader-only warmup: only the lead runtime exists after first start;
        // the worker stays dormant (spec 5.1), so repair now targets the lead.
        task_manager.insert_mode_agent(&lead.conversation_id);
        task_manager.reset_kills();
        let original_session = Arc::clone(&svc.sessions.get(&created.id).expect("session").session);
        let original_generation = original_session.generation();
        // Simulate the lead runtime disappearing so reconciliation repairs it in place.
        task_manager.remove(&lead.conversation_id);

        svc.ensure_session("user-test", &created.id).await.unwrap();

        let current_session = Arc::clone(&svc.sessions.get(&created.id).expect("session").session);
        assert!(Arc::ptr_eq(&original_session, &current_session));
        assert_eq!(current_session.generation(), original_generation);
        assert_eq!(task_manager.kills(), vec![lead.conversation_id.clone()]);
        assert!(current_session.event_loops().has(&lead.slot_id));
        // The dormant worker is never woken by reconciliation (spec 5.1).
        assert!(!current_session.event_loops().has(&worker.slot_id));

        let events = broadcaster.events_by_name("team.agentRuntimeStatusChanged");
        let lead_statuses: Vec<&str> = events
            .iter()
            .filter(|event| {
                event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(lead.slot_id.as_str())
            })
            .map(|event| event.data.get("status").and_then(serde_json::Value::as_str).unwrap())
            .collect();
        assert_eq!(lead_statuses, vec!["pending", "ready", "pending", "ready"]);

        let worker_statuses: Vec<&str> = events
            .iter()
            .filter(|event| {
                event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(worker.slot_id.as_str())
            })
            .map(|event| event.data.get("status").and_then(serde_json::Value::as_str).unwrap())
            .collect();
        assert_eq!(worker_statuses, vec!["dormant"]);
    }

    #[tokio::test]
    async fn no_work_reconciliation_rejects_replaced_session() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("No work replacement"))
            .await
            .unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();
        let old = Arc::clone(&svc.sessions.get(&created.id).unwrap().session);
        svc.stop_session("user-test", &created.id).await.unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();

        let result = svc
            .complete_member_runtime_reconciliation(&created.id, "user-test", old, Vec::new())
            .await;
        assert!(matches!(result, Err(crate::TeamError::SessionNotFound(_))));
    }

    #[tokio::test]
    async fn replaced_session_cannot_publish_dynamic_runtime_ready() {
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster();
        let created = svc
            .create_team("user-test", single_agent_team_request("Runtime ready generation fence"))
            .await
            .unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();
        let old = Arc::clone(&svc.sessions.get(&created.id).unwrap().session);
        let agent = old.scheduler().list_agents().await.remove(0);

        svc.stop_session("user-test", &created.id).await.unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();
        let ready_before = broadcaster
            .events_by_name("team.agentRuntimeStatusChanged")
            .into_iter()
            .filter(|event| event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready"))
            .count();

        assert!(!svc.publish_member_runtime_ready_if_current(&old, &agent));
        let ready_after = broadcaster
            .events_by_name("team.agentRuntimeStatusChanged")
            .into_iter()
            .filter(|event| event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready"))
            .count();
        assert_eq!(ready_after, ready_before, "old generations must not publish Ready");
    }

    #[tokio::test]
    async fn stale_cleanup_does_not_kill_replacement_runtime() {
        let task_manager = Arc::new(MutableTaskManager::new());
        let (svc, _repo, _task_manager, _conv_repo, _broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager.clone());
        let created = svc
            .create_team("user-test", single_agent_team_request("Cleanup fence"))
            .await
            .unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();
        let old = Arc::clone(&svc.sessions.get(&created.id).unwrap().session);
        svc.stop_session("user-test", &created.id).await.unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();
        let conversation_id = created.assistants[0].conversation_id.clone();
        task_manager.insert_mode_agent(&conversation_id);

        svc.cleanup_stale_member_runtime_task(&old, &conversation_id).await;

        assert!(task_manager.get_task(&conversation_id).is_some());
    }

    #[tokio::test]
    async fn idle_cleanup_stops_team_session_and_kills_all_members_when_team_is_collectable() {
        let task_manager = Arc::new(MutableTaskManager::new());
        let (svc, _repo, _task_manager, _conv_repo, _broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager.clone());
        let created = svc
            .create_team("user-test", two_agent_team_request("Idle Cleanup"))
            .await
            .unwrap();
        let lead = created.assistants.iter().find(|agent| agent.role == "lead").unwrap();
        let worker = created
            .assistants
            .iter()
            .find(|agent| agent.role == "teammate")
            .unwrap();
        task_manager.insert_idle_finished_agent(&lead.conversation_id);
        task_manager.insert_idle_finished_agent(&worker.conversation_id);

        svc.ensure_session("user-test", &created.id).await.unwrap();

        let unhandled = svc
            .cleanup_idle_team_runtime_tasks(vec![lead.conversation_id.clone()], &ActiveLeaseRegistry::new(), 300_000)
            .await;

        assert!(unhandled.is_empty());
        assert_eq!(svc.session_count_for_test(), 0);
        assert_eq!(task_manager.active_count(), 0);
    }

    #[tokio::test]
    async fn idle_cleanup_broadcasts_team_session_stopped() {
        let task_manager = Arc::new(MutableTaskManager::new());
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager.clone());
        let created = svc
            .create_team("user-test", two_agent_team_request("Idle Cleanup Stopped Broadcast"))
            .await
            .unwrap();
        let lead = created.assistants.iter().find(|agent| agent.role == "lead").unwrap();
        let worker = created
            .assistants
            .iter()
            .find(|agent| agent.role == "teammate")
            .unwrap();
        task_manager.insert_idle_finished_agent(&lead.conversation_id);
        task_manager.insert_idle_finished_agent(&worker.conversation_id);

        svc.ensure_session("user-test", &created.id).await.unwrap();

        let unhandled = svc
            .cleanup_idle_team_runtime_tasks(vec![lead.conversation_id.clone()], &ActiveLeaseRegistry::new(), 300_000)
            .await;

        assert!(unhandled.is_empty());
        assert_eq!(svc.session_count_for_test(), 0);
        assert_eq!(task_manager.active_count(), 0);

        let stopped_events: Vec<_> = broadcaster
            .events_by_name("team.sessionStatusChanged")
            .into_iter()
            .filter(|event| event.data.get("status").and_then(serde_json::Value::as_str) == Some("stopped"))
            .collect();
        assert_eq!(
            stopped_events.len(),
            1,
            "idle cleanup must broadcast exactly one stopped status"
        );
        assert_eq!(
            stopped_events[0]
                .data
                .get("team_id")
                .and_then(serde_json::Value::as_str),
            Some(created.id.as_str())
        );
    }

    #[tokio::test]
    async fn explicit_stop_session_does_not_broadcast_team_session_stopped() {
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster();
        let created = svc
            .create_team(
                "user-test",
                single_agent_team_request("Explicit Stop No Stopped Broadcast"),
            )
            .await
            .unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();

        svc.stop_session("user-test", &created.id).await.unwrap();

        let stopped_count = broadcaster
            .events_by_name("team.sessionStatusChanged")
            .into_iter()
            .filter(|event| event.data.get("status").and_then(serde_json::Value::as_str) == Some("stopped"))
            .count();
        assert_eq!(stopped_count, 0, "explicit stop must not broadcast a stopped status");
    }

    #[test]
    fn idle_collectable_team_member_accepts_idle_pending_aionrs_runtime() {
        let task = AgentInstance::Mock(Arc::new(ModeSettingAgent::idle_pending_aionrs("aionrs-idle")));

        assert!(super::is_idle_collectable_team_member(&task, now_ms(), 300_000));
    }

    #[test]
    fn idle_collectable_team_member_rejects_running_aionrs_runtime() {
        let task = AgentInstance::Mock(Arc::new(
            ModeSettingAgent::accepts_mode("aionrs-running")
                .with_agent_type(AgentType::Aionrs)
                .with_status(Some(ConversationStatus::Running))
                .with_last_activity(now_ms() - 600_000),
        ));

        assert!(!super::is_idle_collectable_team_member(&task, now_ms(), 300_000));
    }

    #[tokio::test]
    async fn idle_cleanup_stops_team_session_when_aionrs_member_is_idle_pending() {
        let task_manager = Arc::new(MutableTaskManager::new());
        let (svc, _repo, _task_manager, _conv_repo, _broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager.clone());
        let created = svc
            .create_team("user-test", team_with_aionrs_worker_request("Idle Aionrs Cleanup"))
            .await
            .unwrap();
        let lead = created.assistants.iter().find(|agent| agent.role == "lead").unwrap();
        let acp_worker = created.assistants.iter().find(|agent| agent.name == "Worker").unwrap();
        let aionrs_worker = created.assistants.iter().find(|agent| agent.name == "Butler").unwrap();
        task_manager.insert_idle_finished_agent(&lead.conversation_id);
        task_manager.insert_idle_finished_agent(&acp_worker.conversation_id);
        task_manager.insert_idle_pending_aionrs_agent(&aionrs_worker.conversation_id);

        svc.ensure_session("user-test", &created.id).await.unwrap();

        let unhandled = svc
            .cleanup_idle_team_runtime_tasks(
                vec![lead.conversation_id.clone(), acp_worker.conversation_id.clone()],
                &ActiveLeaseRegistry::new(),
                300_000,
            )
            .await;

        assert!(unhandled.is_empty());
        assert_eq!(svc.session_count_for_test(), 0);
        assert_eq!(task_manager.active_count(), 0);
    }

    #[tokio::test]
    async fn team_idle_cleanup_coordinator_delegates_to_team_service() {
        let task_manager = Arc::new(MutableTaskManager::new());
        let (svc, _repo, _task_manager, _conv_repo, _broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager.clone());
        let created = svc
            .create_team("user-test", two_agent_team_request("Idle Coordinator"))
            .await
            .unwrap();
        for agent in &created.assistants {
            task_manager.insert_idle_finished_agent(&agent.conversation_id);
        }

        svc.ensure_session("user-test", &created.id).await.unwrap();
        let coordinator = TeamIdleCleanupCoordinator::new(svc.clone(), Arc::new(ActiveLeaseRegistry::new()));

        let unhandled = coordinator
            .cleanup_idle_conversations(vec![created.assistants[0].conversation_id.clone()], 300_000)
            .await;

        assert!(unhandled.is_empty());
        assert_eq!(svc.session_count_for_test(), 0);
        assert_eq!(task_manager.active_count(), 0);
    }

    #[tokio::test]
    async fn manual_add_agent_in_active_session_emits_runtime_ready_after_background_attach() {
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster();
        let created = svc
            .create_team("user-test", single_agent_team_request("Manual Runtime Events"))
            .await
            .unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();

        let added = svc
            .add_agent(
                "user-test",
                &created.id,
                AddAgentRequest {
                    name: "Worker".to_owned(),
                    role: "teammate".to_owned(),
                    backend: Some("acp".to_owned()),
                    model: "claude".to_owned(),
                    assistant_id: None,
                },
            )
            .await
            .unwrap();

        let events = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let events = broadcaster.events_by_name("team.agentRuntimeStatusChanged");
                let added_events: Vec<_> = events
                    .into_iter()
                    .filter(|event| {
                        event.data.get("slot_id").and_then(serde_json::Value::as_str) == Some(added.slot_id.as_str())
                    })
                    .collect();
                if added_events
                    .iter()
                    .any(|event| event.data.get("status").and_then(serde_json::Value::as_str) == Some("ready"))
                {
                    break added_events;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime ready event should be emitted");
        let statuses: Vec<&str> = events
            .iter()
            .map(|event| event.data.get("status").and_then(serde_json::Value::as_str).unwrap())
            .collect();

        assert_eq!(statuses, vec!["pending", "ready"]);
        let session = Arc::clone(&svc.sessions.get(&created.id).expect("session").session);
        assert_eq!(session.event_loops().len(), 2, "new slot must own one event loop");
        assert!(session.event_loops().has(&added.slot_id));
        assert_eq!(
            session.member_runtimes().snapshot(&added.slot_id),
            crate::member_runtime::MemberRuntimeSnapshot::Ready
        );
    }

    #[tokio::test]
    async fn dynamic_member_is_reserved_before_membership_event() {
        let (svc, _repo, _task_manager, _conv_repo, broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster();
        let created = svc
            .create_team("user-test", single_agent_team_request("Reservation ordering"))
            .await
            .unwrap();
        svc.ensure_session("user-test", &created.id).await.unwrap();
        let session = Arc::clone(&svc.sessions.get(&created.id).unwrap().session);
        let observed = Arc::new(std::sync::Mutex::new(None));
        let observed_for_event = Arc::clone(&observed);
        broadcaster.set_observer(Arc::new(move |event| {
            if event.name != crate::events::TEAM_AGENT_SPAWNED_EVENT {
                return;
            }
            let slot_id = event
                .data
                .get("assistant")
                .and_then(|assistant| assistant.get("slot_id"))
                .and_then(serde_json::Value::as_str);
            if let Some(slot_id) = slot_id {
                *observed_for_event.lock().unwrap() = Some(session.member_runtimes().snapshot(slot_id));
            }
        }));

        svc.add_agent(
            "user-test",
            &created.id,
            AddAgentRequest {
                name: "Worker".to_owned(),
                role: "teammate".to_owned(),
                backend: Some("acp".to_owned()),
                model: "claude".to_owned(),
                assistant_id: None,
            },
        )
        .await
        .unwrap();

        assert!(matches!(
            observed.lock().unwrap().as_ref(),
            Some(
                crate::member_runtime::MemberRuntimeSnapshot::Attaching { .. }
                    | crate::member_runtime::MemberRuntimeSnapshot::Ready
            )
        ));
    }

    #[tokio::test]
    async fn set_session_mode_persists_team_mode_and_new_agents_inherit_it() {
        let (svc, repo, _task_manager, conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Team Mode Seed"))
            .await
            .unwrap();

        svc.set_session_mode("user-test", &created.id, "full_auto")
            .await
            .unwrap();

        let row = repo
            .get_team("user-test", &created.id)
            .await
            .unwrap()
            .expect("team row");
        assert_eq!(row.session_mode.as_deref(), Some("full_auto"));

        let added = svc
            .add_agent(
                "user-test",
                &created.id,
                AddAgentRequest {
                    name: "Worker".to_owned(),
                    role: "teammate".to_owned(),
                    backend: Some("acp".to_owned()),
                    model: "claude".to_owned(),
                    assistant_id: None,
                },
            )
            .await
            .unwrap();
        let extra = conv_repo
            .get_extra(&added.conversation_id)
            .expect("added conversation extra");

        assert_eq!(
            extra.get("session_mode").and_then(serde_json::Value::as_str),
            Some("full_auto")
        );
    }

    #[tokio::test]
    async fn set_session_mode_does_not_persist_agent_seed_when_active_runtime_rejects_mode() {
        let accepting_conversation_id = "conv-accepts";
        let rejecting_conversation_id = "conv-rejects";
        let task_manager = Arc::new(StaticTaskManager::new(HashMap::from([
            (
                accepting_conversation_id.to_owned(),
                AgentInstance::Mock(Arc::new(ModeSettingAgent::accepts_mode(accepting_conversation_id))),
            ),
            (
                rejecting_conversation_id.to_owned(),
                AgentInstance::Mock(Arc::new(ModeSettingAgent::rejects_mode(
                    rejecting_conversation_id,
                    "Value 'read-only' is not selectable for config option 'mode'",
                ))),
            ),
        ])));
        let (svc, repo, _task_manager, conv_repo, _broadcaster) =
            setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager(task_manager);
        let created = svc
            .create_team("user-test", single_agent_team_request("Partial Mode Seed"))
            .await
            .unwrap();
        let mut row = repo
            .get_team("user-test", &created.id)
            .await
            .unwrap()
            .expect("team row");
        row.agents = serde_json::json!([
            {
                "slot_id": "slot-accepts",
                "name": "Codex CLI",
                "role": "lead",
                "conversation_id": accepting_conversation_id,
                "backend": "codex",
                "model": "openai.gpt-5.5",
                "assistant_id": "bare:codex"
            },
            {
                "slot_id": "slot-rejects",
                "name": "Claude Code",
                "role": "teammate",
                "conversation_id": rejecting_conversation_id,
                "backend": "claude",
                "model": "global.anthropic.claude-opus-4-8",
                "assistant_id": "bare:claude"
            }
        ])
        .to_string();
        repo.update_team(
            "user-test",
            &created.id,
            &aionui_db::UpdateTeamParams {
                agents: Some(row.agents),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        conv_repo
            .create(&aionui_db::models::ConversationRow {
                id: accepting_conversation_id.to_owned(),
                user_id: "user-test".to_owned(),
                name: "Codex CLI".to_owned(),
                r#type: AgentType::Acp.serde_name().to_owned(),
                extra: serde_json::json!({
                    "current_mode_id": "default",
                    "session_mode": "default"
                })
                .to_string(),
                model: None,
                status: Some("pending".to_owned()),
                source: None,
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: now_ms(),
                updated_at: now_ms(),
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();
        conv_repo
            .create(&aionui_db::models::ConversationRow {
                id: rejecting_conversation_id.to_owned(),
                user_id: "user-test".to_owned(),
                name: "Claude Code".to_owned(),
                r#type: AgentType::Acp.serde_name().to_owned(),
                extra: serde_json::json!({
                    "current_mode_id": "default",
                    "session_mode": "default"
                })
                .to_string(),
                model: None,
                status: Some("pending".to_owned()),
                source: None,
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: now_ms(),
                updated_at: now_ms(),
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();

        svc.set_session_mode("user-test", &created.id, "read-only")
            .await
            .unwrap();

        let team = repo
            .get_team("user-test", &created.id)
            .await
            .unwrap()
            .expect("team row");
        assert_eq!(team.session_mode.as_deref(), Some("read-only"));

        let accepting_extra = conv_repo.get_extra(accepting_conversation_id).unwrap();
        assert_eq!(
            accepting_extra.get("session_mode").and_then(serde_json::Value::as_str),
            Some("read-only")
        );

        let rejecting_extra = conv_repo.get_extra(rejecting_conversation_id).unwrap();
        assert_eq!(
            rejecting_extra.get("session_mode").and_then(serde_json::Value::as_str),
            Some("default")
        );
    }

    #[tokio::test]
    async fn run_state_returns_none_without_session_and_does_not_create_session() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Run State"))
            .await
            .unwrap();
        svc.stop_session("user-test", &created.id).await.unwrap();

        assert_eq!(svc.session_count_for_test(), 0);

        let state = svc.get_run_state("user-test", &created.id).await.unwrap();

        assert!(state.active_run.is_none());
        assert_eq!(svc.session_count_for_test(), 0);
    }

    #[tokio::test]
    async fn config_options_returns_snapshot_without_creating_team_session() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Config Options"))
            .await
            .unwrap();
        let conversation_id = &created.assistants[0].conversation_id;

        assert_eq!(svc.session_count_for_test(), 0);

        let options = svc
            .get_conversation_config_options("user-test", &created.id, conversation_id)
            .await
            .unwrap();

        assert_eq!(options.config_options[0].id, "model");
        assert_eq!(svc.session_count_for_test(), 0);
    }

    #[tokio::test]
    async fn run_state_returns_current_active_payload() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Active Run State"))
            .await
            .unwrap();

        let ack = svc.send_message("user-test", &created.id, "hello", None).await.unwrap();
        let state = svc.get_run_state("user-test", &created.id).await.unwrap();
        let active_run = state.active_run.expect("active run state");

        assert_eq!(active_run.team_id, created.id);
        assert_eq!(active_run.team_run_id, ack.run.team_run_id);
        assert_eq!(active_run.status, ack.run.status);
        assert_eq!(active_run.target_slot_id, ack.run.target_slot_id);
        assert_eq!(active_run.target_role, ack.run.target_role);
        assert_eq!(active_run.queued_intent_count, 1);
        assert_eq!(active_run.slot_work.len(), 1);
        assert_eq!(active_run.slot_work[0].slot_id, ack.run.slot_work[0].slot_id);
    }

    #[tokio::test]
    async fn config_options_return_member_runtime_snapshot() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Team Config"))
            .await
            .unwrap();
        let conversation_id = created.assistants[0].conversation_id.clone();

        let response = svc
            .get_conversation_config_options("user-test", &created.id, &conversation_id)
            .await
            .unwrap();

        let model = response
            .config_options
            .iter()
            .find(|option| option.id == "model")
            .expect("model config option");
        assert_eq!(model.current_value.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn config_options_reports_runtime_not_ready_for_member_conversation() {
        let (svc, _repo, _task_manager, conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Team Config Pending"))
            .await
            .unwrap();
        let conversation_id = created.assistants[0].conversation_id.clone();
        conv_repo.mark_runtime_not_ready(&conversation_id);

        let err = svc
            .get_conversation_config_options("user-test", &created.id, &conversation_id)
            .await
            .expect_err("member runtime readiness should be reported distinctly");

        assert!(matches!(
            err,
            crate::error::TeamError::RuntimeNotReady {
                conversation_id: ref id
            } if id == &conversation_id
        ));
    }

    #[tokio::test]
    async fn config_options_reject_non_member_conversation() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Team Config Reject"))
            .await
            .unwrap();

        let err = svc
            .get_conversation_config_options("user-test", &created.id, "other-conversation")
            .await
            .expect_err("non-member conversation must be rejected");

        assert!(matches!(err, crate::error::TeamError::AgentNotFound(_)));
    }

    #[tokio::test]
    async fn config_options_reject_cross_user_access() {
        let (svc, _repo, _task_manager, _conv_repo) = setup_with_factory_metadata_team_repo_and_conversation_repo();
        let created = svc
            .create_team("user-test", single_agent_team_request("Team Config Owner"))
            .await
            .unwrap();
        let conversation_id = created.assistants[0].conversation_id.clone();

        let err = svc
            .get_conversation_config_options("other-user", &created.id, &conversation_id)
            .await
            .expect_err("team config options must reject cross-user access");

        assert!(matches!(err, crate::error::TeamError::TeamNotFound(_)));
    }
}
