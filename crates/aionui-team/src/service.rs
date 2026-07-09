mod describe_support;
mod response_builder;
pub(crate) mod spawn_support;

use std::path::PathBuf;
use std::sync::{Arc, Weak};
use std::time::Instant;

use aionui_ai_agent::{ActiveLeaseRegistry, AgentError, AgentInstance, IWorkerTaskManager};
use aionui_api_types::{
    AddAgentRequest, CreateTeamRequest, GetConfigOptionsResponse, TeamAgentResponse, TeamAgentRuntimeStatus,
    TeamMcpPhase, TeamMcpStatusPayload, TeamResponse, TeamRunAckResponse, TeamRunStateResponse, TeamRunTargetRole,
    TeamSlotRuntimeHealth, WebSocketMessage,
};
use aionui_common::{AgentKillReason, generate_id, now_ms};
use aionui_db::models::TeamRow;
use aionui_db::{
    IAgentMetadataRepository, IAssistantDefinitionRepository, IAssistantOverlayRepository, IProviderRepository,
    ITeamRepository, UpdateTeamParams,
};
use aionui_realtime::EventBroadcaster;
use dashmap::DashMap;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::error::TeamError;
use crate::event_loop::AgentLoopContext;
use crate::events::{
    TEAM_CREATED_EVENT, TEAM_MCP_STATUS_EVENT, TEAM_REMOVED_EVENT, TEAM_RENAMED_EVENT, TeamEventEmitter,
};
use crate::mcp::TeamMcpStdioConfig;
use crate::message_projection::TeamProjectionMessageStore;
use crate::ports::{AgentTurnCancellationPort, AgentTurnExecutionPort, TeamAssistantCatalogPort};
use crate::prompt_dump::TeamPromptDumpConfig;
use crate::provisioning::{TeamAgentProvisioner, TeamConversationProvisioningPort};
use crate::session::{AgentMessageQueueResult, TeamSession, spawn_attach_agent_process_bg};
use crate::types::{Team, TeamAgent, TeammateRole, TeammateStatus};
use crate::wake::TeamWakeSource;
use crate::workspace::validate_create_workspace_path;

pub(crate) fn inherit_team_workspace(extra: &mut serde_json::Value, workspace: &str) {
    if !workspace.trim().is_empty() {
        extra["workspace"] = serde_json::Value::String(workspace.to_owned());
    }
}

struct SessionEntry {
    session: Arc<TeamSession>,
    slow_monitor_handle: tokio::task::JoinHandle<()>,
}

struct TeamAgentRebuildOutcome {
    agent: TeamAgent,
    duration_ms: u128,
    result: Result<(), TeamError>,
}

const TEAM_REBUILD_MAX_CONCURRENCY: usize = 3;
const TEAM_REBUILD_START_STAGGER: std::time::Duration = std::time::Duration::from_secs(3);

fn format_rebuild_agent_identity(agent: &TeamAgent) -> String {
    format!(
        "{} (backend={}, model={}, role={}, slot_id={}, conversation_id={})",
        agent.name, agent.backend, agent.model, agent.role, agent.slot_id, agent.conversation_id
    )
}

fn spawn_rebuild_agent_process(
    jobs: &mut JoinSet<TeamAgentRebuildOutcome>,
    provisioner: TeamAgentProvisioner,
    task_manager: Arc<dyn IWorkerTaskManager>,
    user_id: String,
    agent: TeamAgent,
    cfg: TeamMcpStdioConfig,
) {
    jobs.spawn(async move {
        let team_id = cfg.team_id.clone();
        info!(
            team_id = %team_id,
            slot_id = %agent.slot_id,
            agent_name = %agent.name,
            conversation_id = %agent.conversation_id,
            backend = %agent.backend,
            model = %agent.model,
            role = %agent.role,
            "team agent rebuild attach started"
        );
        let attach_started_at = Instant::now();
        let result = provisioner
            .attach_agent_process(&user_id, &agent, cfg, &task_manager)
            .await;
        let duration_ms = attach_started_at.elapsed().as_millis();
        match &result {
            Ok(()) => info!(
                team_id = %team_id,
                slot_id = %agent.slot_id,
                agent_name = %agent.name,
                conversation_id = %agent.conversation_id,
                backend = %agent.backend,
                model = %agent.model,
                role = %agent.role,
                duration_ms,
                "team agent rebuild attach finished"
            ),
            Err(error) => warn!(
                team_id = %team_id,
                slot_id = %agent.slot_id,
                agent_name = %agent.name,
                conversation_id = %agent.conversation_id,
                backend = %agent.backend,
                model = %agent.model,
                role = %agent.role,
                duration_ms,
                error = %error,
                "team agent rebuild attach failed"
            ),
        }
        TeamAgentRebuildOutcome {
            agent,
            duration_ms,
            result,
        }
    });
}

async fn join_next_rebuild_outcome(
    jobs: &mut JoinSet<TeamAgentRebuildOutcome>,
) -> Result<Option<TeamAgentRebuildOutcome>, TeamError> {
    match jobs.join_next().await {
        Some(Ok(outcome)) => Ok(Some(outcome)),
        Some(Err(error)) => Err(TeamError::InvalidRequest(format!(
            "team agent rebuild task failed: {error}"
        ))),
        None => Ok(None),
    }
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
            backend_binary_path,
            prompt_dump,
            sessions: Arc::new(DashMap::new()),
            add_agent_locks: Arc::new(DashMap::new()),
            ensure_session_locks: Arc::new(DashMap::new()),
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

    async fn load_owned_team(&self, user_id: &str, team_id: &str) -> Result<Team, TeamError> {
        let row = self
            .repo
            .get_team(team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.into()))?;
        if row.user_id != user_id {
            return Err(TeamError::Forbidden(format!(
                "team {team_id} is not owned by current user"
            )));
        }
        Ok(Team::from_row(&row)?)
    }

    pub async fn renew_active_lease(
        &self,
        user_id: &str,
        team_id: &str,
        active_leases: &ActiveLeaseRegistry,
    ) -> Result<(), TeamError> {
        let team = match self.load_owned_team(user_id, team_id).await {
            Ok(team) => team,
            Err(error @ (TeamError::TeamNotFound(_) | TeamError::Forbidden(_))) => {
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
        let teams = match self.repo.list_teams().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(error = %e, "failed to list teams for session restore");
                return;
            }
        };
        for team in &teams {
            if let Err(e) = self.ensure_session_inner(&team.id).await {
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

        self.broadcast_team_created(&team.id, &team.name);

        self.build_team_response(&team).await
    }

    pub async fn list_teams(&self, user_id: &str) -> Result<Vec<TeamResponse>, TeamError> {
        let rows = self.repo.list_teams_by_user(user_id).await?;
        let mut teams = Vec::with_capacity(rows.len());
        for row in &rows {
            match Team::from_row(row) {
                Ok(team) => match self.build_team_response(&team).await {
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
        let team = self.load_owned_team(user_id, team_id).await?;
        self.build_team_response(&team).await
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

        self.repo.delete_mailbox_by_team(team_id).await?;
        self.repo.delete_tasks_by_team(team_id).await?;
        self.repo.delete_team(team_id).await?;

        self.add_agent_locks.remove(team_id);

        info!(team_id = %team_id, "Team removed");
        self.broadcast_team_removed(team_id);
        Ok(())
    }

    pub async fn rename_team(&self, user_id: &str, team_id: &str, name: &str) -> Result<(), TeamError> {
        self.load_owned_team(user_id, team_id).await?;

        self.repo
            .update_team(
                team_id,
                &UpdateTeamParams {
                    name: Some(name.to_owned()),
                    ..Default::default()
                },
            )
            .await?;
        self.broadcast_team_renamed(team_id, name);
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

        let row = self
            .repo
            .get_team(team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.into()))?;
        if row.user_id != user_id {
            return Err(TeamError::Forbidden(format!(
                "team {team_id} is not owned by current user"
            )));
        }
        let mut team = Team::from_row(&row)?;
        let agent = self.provisioner().add_agent(user_id, &row, &mut team, req).await?;

        if let Some(session) = self.sessions.get(team_id).map(|e| Arc::clone(&e.session)) {
            let wake_plan = session.add_manual_agent(&agent).await?;
            let service = self
                .self_ref
                .upgrade()
                .ok_or_else(|| TeamError::InvalidRequest("add_agent requires a live TeamSessionService".into()))?;
            self.broadcast_agent_runtime_status(team_id, &agent, TeamAgentRuntimeStatus::Pending, None);
            spawn_attach_agent_process_bg(
                service,
                team_id.to_owned(),
                user_id.to_owned(),
                agent.clone(),
                session.mcp_stdio_config(&agent.slot_id),
                self.task_manager.clone(),
                wake_plan,
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
            TeamEventEmitter::new(team_id.to_owned(), self.broadcaster.clone()).broadcast_agent_spawned(&agent);
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

        self.build_agent_response(&agent).await
    }

    pub async fn remove_agent(&self, user_id: &str, team_id: &str, slot_id: &str) -> Result<(), TeamError> {
        let lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        let mut team = self.load_owned_team(user_id, team_id).await?;

        let idx = team
            .agents
            .iter()
            .position(|a| a.slot_id == slot_id)
            .ok_or_else(|| TeamError::AgentNotFound(slot_id.into()))?;

        if team.agents[idx].role == crate::types::TeammateRole::Lead {
            return Err(TeamError::InvalidRequest("cannot remove the team lead".into()));
        }

        let removed = team.agents.remove(idx);

        let _ = self
            .conversation_port
            .delete_team_conversation(user_id, &removed.conversation_id)
            .await;

        let agents_json = serde_json::to_string(&team.agents)?;
        self.repo
            .update_team(
                team_id,
                &UpdateTeamParams {
                    agents: Some(agents_json),
                    ..Default::default()
                },
            )
            .await?;

        if let Some(session) = self.sessions.get(team_id).map(|e| Arc::clone(&e.session)) {
            session.remove_agent(slot_id).await?;
            session.notify_leader_membership_removed(&removed).await?;
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
            TeamEventEmitter::new(team_id.to_owned(), self.broadcaster.clone()).broadcast_agent_removed(slot_id);
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
        let row = match self.repo.get_team(team_id).await {
            Ok(Some(row)) => row,
            Ok(None) | Err(_) => return self.ensure_session_inner(team_id).await,
        };
        if row.user_id != user_id {
            return Err(TeamError::Forbidden(format!(
                "team {team_id} is not owned by current user"
            )));
        }
        self.ensure_session_inner(team_id).await
    }

    async fn ensure_session_inner(&self, team_id: &str) -> Result<(), TeamError> {
        if self.sessions.contains_key(team_id) {
            return Ok(());
        }

        let membership_lock = self
            .add_agent_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _membership_guard = membership_lock.lock().await;

        // Re-check after acquiring the membership lock. A preceding add/remove
        // may have completed while this call was waiting, and another ensure
        // caller may also have finished startup.
        if self.sessions.contains_key(team_id) {
            return Ok(());
        }

        let lock = self
            .ensure_session_locks
            .entry(team_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let _guard = lock.lock().await;

        // Re-check after acquiring lock (another caller may have completed).
        if self.sessions.contains_key(team_id) {
            return Ok(());
        }

        let row = match self.repo.get_team(team_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                self.broadcast_mcp_phase(team_id, "", TeamMcpPhase::LoadFailed, None, |p| {
                    p.error = Some(format!("team not found: {team_id}"));
                });
                return Err(TeamError::TeamNotFound(team_id.into()));
            }
            Err(e) => {
                self.broadcast_mcp_phase(team_id, "", TeamMcpPhase::LoadFailed, None, |p| {
                    p.error = Some(e.to_string());
                });
                return Err(e.into());
            }
        };
        let user_id = row.user_id.clone();
        let team = Team::from_row(&row)?;
        let agents_snapshot: Vec<TeamAgent> = team.agents.clone();

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
            Ok(session) => session,
            Err(e) => {
                self.broadcast_mcp_phase(team_id, "", TeamMcpPhase::SessionError, None, |p| {
                    p.error = Some(e.to_string());
                });
                return Err(e);
            }
        };

        self.broadcast_mcp_phase(team_id, "", TeamMcpPhase::SessionInjecting, None, |_| {});

        if let Err(e) = self
            .rebuild_agent_processes(team_id, &session, &user_id, &agents_snapshot)
            .await
        {
            session.stop();
            return Err(e);
        }

        let session = Arc::new(session);

        // Spawn per-agent event loops
        self.spawn_event_loops(&session, &user_id, &agents_snapshot);

        let slow_monitor_handle = Self::spawn_slow_monitor(session.clone());
        let entry = SessionEntry {
            session: session.clone(),
            slow_monitor_handle,
        };
        self.sessions.insert(team_id.to_owned(), entry);

        if let Err(err) = session.try_start_recovery_drain("ensure_session_ready").await {
            warn!(
                team_id,
                error = %err,
                "team recovery scan failed after session ensure"
            );
        }

        self.broadcast_mcp_phase(team_id, "", TeamMcpPhase::SessionReady, None, |p| {
            p.server_count = Some(agents_snapshot.len());
        });

        Ok(())
    }

    pub async fn get_conversation_config_options(
        &self,
        user_id: &str,
        team_id: &str,
        conversation_id: &str,
    ) -> Result<GetConfigOptionsResponse, TeamError> {
        let row = self
            .repo
            .get_team(team_id)
            .await?
            .ok_or_else(|| TeamError::TeamNotFound(team_id.to_owned()))?;
        if row.user_id != user_id {
            return Err(TeamError::Forbidden(format!(
                "team {team_id} is not owned by current user"
            )));
        }

        let team = Team::from_row(&row)?;
        let member = team.agents.iter().any(|agent| agent.conversation_id == conversation_id);
        if !member {
            return Err(TeamError::AgentNotFound(conversation_id.to_owned()));
        }

        self.conversation_port.get_config_options(conversation_id).await
    }

    fn broadcast_mcp_phase<F>(&self, team_id: &str, slot_id: &str, phase: TeamMcpPhase, port: Option<u16>, customize: F)
    where
        F: FnOnce(&mut TeamMcpStatusPayload),
    {
        let mut payload = TeamMcpStatusPayload {
            team_id: team_id.to_owned(),
            slot_id: slot_id.to_owned(),
            phase,
            port,
            server_count: None,
            error: None,
        };
        customize(&mut payload);
        let event = WebSocketMessage::new(
            TEAM_MCP_STATUS_EVENT,
            serde_json::to_value(payload).expect("serialize mcp status payload"),
        );
        self.broadcaster.broadcast(event);
    }

    fn spawn_slow_monitor(session: Arc<TeamSession>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                session.team_run_manager().observe_slow_child_turns(now_ms()).await;
            }
        })
    }

    fn broadcast_team_created(&self, team_id: &str, team_name: &str) {
        info!(team_id = %team_id, event_name = TEAM_CREATED_EVENT, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            TEAM_CREATED_EVENT,
            serde_json::json!({ "team_id": team_id, "team_name": team_name }),
        ));
        self.broadcast_team_list_changed(team_id, "created");
    }

    fn broadcast_team_removed(&self, team_id: &str) {
        info!(team_id = %team_id, event_name = TEAM_REMOVED_EVENT, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            TEAM_REMOVED_EVENT,
            serde_json::json!({ "team_id": team_id }),
        ));
        self.broadcast_team_list_changed(team_id, "removed");
    }

    fn broadcast_team_renamed(&self, team_id: &str, team_name: &str) {
        info!(team_id = %team_id, event_name = TEAM_RENAMED_EVENT, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            TEAM_RENAMED_EVENT,
            serde_json::json!({ "team_id": team_id, "team_name": team_name }),
        ));
        self.broadcast_team_list_changed(team_id, "renamed");
    }

    fn broadcast_team_list_changed(&self, team_id: &str, action: &str) {
        info!(team_id = %team_id, event_name = crate::events::TEAM_LIST_CHANGED_EVENT, action, "team event broadcast");
        self.broadcaster.broadcast(WebSocketMessage::new(
            crate::events::TEAM_LIST_CHANGED_EVENT,
            serde_json::json!({ "team_id": team_id, "action": action }),
        ));
    }

    pub(crate) fn broadcast_agent_runtime_status(
        &self,
        team_id: &str,
        agent: &TeamAgent,
        status: TeamAgentRuntimeStatus,
        error: Option<String>,
    ) {
        TeamEventEmitter::new(team_id.to_owned(), self.broadcaster.clone())
            .broadcast_agent_runtime_status(agent, status, error);
    }

    async fn rebuild_agent_processes(
        &self,
        team_id: &str,
        session: &TeamSession,
        user_id: &str,
        agents: &[TeamAgent],
    ) -> Result<(), TeamError> {
        let provisioner = self.provisioner();
        let task_manager = self.task_manager.clone();
        let started_at = Instant::now();
        let mut rebuild_jobs: Vec<TeamAgent> = agents.to_vec();
        rebuild_jobs.sort_by_key(|agent| match agent.role {
            TeammateRole::Lead => 0,
            TeammateRole::Teammate => 1,
        });

        info!(
            team_id,
            agent_count = agents.len(),
            max_concurrency = TEAM_REBUILD_MAX_CONCURRENCY,
            start_stagger_ms = TEAM_REBUILD_START_STAGGER.as_millis(),
            "team agent rebuild started"
        );

        let mut outcomes = Vec::new();
        let mut jobs = JoinSet::new();
        let mut failed = false;

        for (launched_count, agent) in rebuild_jobs.into_iter().enumerate() {
            while jobs.len() >= TEAM_REBUILD_MAX_CONCURRENCY {
                if let Some(outcome) = join_next_rebuild_outcome(&mut jobs).await? {
                    failed = outcome.result.is_err();
                    outcomes.push(outcome);
                }
                if failed {
                    break;
                }
            }
            if failed {
                break;
            }

            if launched_count > 0 {
                let stagger = tokio::time::sleep(TEAM_REBUILD_START_STAGGER);
                tokio::pin!(stagger);
                loop {
                    tokio::select! {
                        _ = &mut stagger => break,
                        outcome = join_next_rebuild_outcome(&mut jobs), if !jobs.is_empty() => {
                            if let Some(outcome) = outcome? {
                                failed = outcome.result.is_err();
                                outcomes.push(outcome);
                            }
                            if failed {
                                break;
                            }
                        }
                    }
                }
                if failed {
                    break;
                }
            }

            let cfg = session.mcp_stdio_config(&agent.slot_id);
            self.broadcast_agent_runtime_status(team_id, &agent, TeamAgentRuntimeStatus::Pending, None);
            spawn_rebuild_agent_process(
                &mut jobs,
                provisioner.clone(),
                task_manager.clone(),
                user_id.to_owned(),
                agent,
                cfg,
            );
        }

        while let Some(outcome) = join_next_rebuild_outcome(&mut jobs).await? {
            outcomes.push(outcome);
        }

        let mut success_count = 0usize;
        let mut failures: Vec<&TeamAgentRebuildOutcome> = Vec::new();
        for outcome in &outcomes {
            match &outcome.result {
                Ok(()) => success_count += 1,
                Err(_) => failures.push(outcome),
            }
        }

        info!(
            team_id,
            agent_count = agents.len(),
            success_count,
            failure_count = failures.len(),
            duration_ms = started_at.elapsed().as_millis(),
            max_concurrency = TEAM_REBUILD_MAX_CONCURRENCY,
            start_stagger_ms = TEAM_REBUILD_START_STAGGER.as_millis(),
            "team agent rebuild completed"
        );

        if failures.is_empty() {
            for success in outcomes.iter().filter(|outcome| outcome.result.is_ok()) {
                self.broadcast_agent_runtime_status(team_id, &success.agent, TeamAgentRuntimeStatus::Ready, None);
            }
            return Ok(());
        }

        let first_error = failures
            .first()
            .map(|outcome| {
                let error = outcome
                    .result
                    .as_ref()
                    .err()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown rebuild failure".to_owned());
                format!("{}: {error}", format_rebuild_agent_identity(&outcome.agent))
            })
            .unwrap_or_else(|| "unknown rebuild failure".to_owned());

        for failure in &failures {
            let error = failure
                .result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "unknown rebuild failure".to_owned());
            self.broadcast_agent_runtime_status(
                team_id,
                &failure.agent,
                TeamAgentRuntimeStatus::Failed,
                Some(error.clone()),
            );
            let agent_identity = format_rebuild_agent_identity(&failure.agent);
            let msg = format!("failed to attach rebuilt agent {agent_identity}: {error}");
            self.broadcast_mcp_phase(team_id, &failure.agent.slot_id, TeamMcpPhase::SessionError, None, |p| {
                p.error = Some(msg);
            });
            warn!(
                team_id,
                slot_id = %failure.agent.slot_id,
                agent_name = %failure.agent.name,
                conversation_id = %failure.agent.conversation_id,
                backend = %failure.agent.backend,
                model = %failure.agent.model,
                role = %failure.agent.role,
                duration_ms = failure.duration_ms,
                error = %error,
                "warmup failed during rebuild"
            );
        }

        for success in outcomes.iter().filter(|outcome| outcome.result.is_ok()) {
            info!(
                team_id,
                slot_id = %success.agent.slot_id,
                agent_name = %success.agent.name,
                conversation_id = %success.agent.conversation_id,
                backend = %success.agent.backend,
                model = %success.agent.model,
                role = %success.agent.role,
                "cleaning up successfully attached agent after rebuild failure"
            );
            self.task_manager
                .kill_and_wait(&success.agent.conversation_id, Some(AgentKillReason::TeamMcpRebuild))
                .await;
        }

        Err(TeamError::InvalidRequest(format!(
            "failed to attach rebuilt agent: {first_error}"
        )))
    }

    /// Spawn per-agent event loops that drain the mailbox whenever notified.
    /// Each agent gets its own tokio task that runs until the session shuts down.
    fn spawn_event_loops(&self, session: &Arc<TeamSession>, user_id: &str, agents: &[TeamAgent]) {
        let registry = session.event_loops();

        for agent in agents {
            let ctx = AgentLoopContext {
                team_id: session.team_id().to_owned(),
                slot_id: agent.slot_id.clone(),
                user_id: user_id.to_owned(),
                session: session.clone(),
                scheduler: session.scheduler().clone(),
                mailbox: session.mailbox().clone(),
                turn_port: self.turn_port.clone(),
                registry: registry.clone(),
            };
            let _ = registry.spawn(&agent.slot_id, ctx);
        }
    }

    /// Register an event loop for a dynamically spawned agent.
    ///
    /// Called by [`TeamSession::spawn_agent`] after `attach_spawned_agent_process`
    /// succeeds so the newly booted agent gets its own drain loop — exactly as
    /// `spawn_event_loops` does for the initial members during `ensure_session`.
    pub(crate) fn register_event_loop(&self, team_id: &str, slot_id: &str) {
        let Some(entry) = self.sessions.get(team_id) else {
            return;
        };
        let session = Arc::clone(&entry.session);
        let registry = session.event_loops();

        let ctx = AgentLoopContext {
            team_id: team_id.to_owned(),
            slot_id: slot_id.to_owned(),
            user_id: session.user_id().to_owned(),
            session: session.clone(),
            scheduler: session.scheduler().clone(),
            mailbox: session.mailbox().clone(),
            turn_port: self.turn_port.clone(),
            registry: registry.clone(),
        };
        let registered = registry.spawn(slot_id, ctx);
        if registered {
            info!(team_id, slot_id, "agent event loop registered");
        }
    }

    pub async fn get_session_user_id(&self, team_id: &str) -> Option<String> {
        self.sessions.get(team_id).map(|e| e.session.user_id().to_owned())
    }

    pub async fn get_run_state(&self, user_id: &str, team_id: &str) -> Result<TeamRunStateResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        let session = self.sessions.get(team_id).map(|entry| Arc::clone(&entry.session));
        let active_run = match session {
            Some(session) => session.team_run_manager().current_payload().await,
            None => None,
        };

        Ok(TeamRunStateResponse { active_run })
    }

    pub fn get_session_scheduler(&self, team_id: &str) -> Option<Arc<crate::scheduler::TeammateManager>> {
        self.sessions.get(team_id).map(|e| e.session.scheduler().clone())
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

    fn stop_session_unchecked(&self, team_id: &str) {
        if let Some((_, entry)) = self.sessions.remove(team_id) {
            entry.slow_monitor_handle.abort();
            entry.session.event_loops().shutdown();
            entry.session.stop();
        }
    }

    pub async fn send_message(
        &self,
        user_id: &str,
        team_id: &str,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.send_message(content, files).await
    }

    pub async fn send_message_to_agent(
        &self,
        user_id: &str,
        team_id: &str,
        slot_id: &str,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        self.load_owned_team(user_id, team_id).await?;
        self.ensure_session_inner(team_id).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session.send_message_to_agent(slot_id, content, files).await
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
        self.ensure_session_inner(team_id).await?;
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
        self.ensure_session_inner(team_id).await?;
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
        self.ensure_session_inner(team_id).await?;
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
    ) -> Result<AgentMessageQueueResult, TeamError> {
        self.require_active_team_run_for_team_work(team_id).await?;
        let session = {
            let entry = self
                .sessions
                .get(team_id)
                .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
            Arc::clone(&entry.session)
        };
        session
            .send_agent_message_from_agent(from_slot_id, to_slot_id, content)
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

    pub(crate) fn notify_reserved_wake_for_team_work(
        &self,
        team_id: &str,
        slot_id: &str,
        target_role: TeamRunTargetRole,
        source: TeamWakeSource,
    ) {
        let Some(entry) = self.sessions.get(team_id) else {
            warn!(
                team_id,
                slot_id,
                target_role = ?target_role,
                wake_source = %source,
                "reserved wake notify skipped because session is missing"
            );
            return;
        };
        entry
            .session
            .notify_reserved_wake_for_team_work(slot_id, target_role, source);
    }

    pub(crate) fn notify_mailbox_only_wake(&self, team_id: &str, slot_id: &str, source: TeamWakeSource) {
        let Some(entry) = self.sessions.get(team_id) else {
            warn!(
                team_id,
                slot_id,
                wake_source = %source,
                "mailbox-only wake notify skipped because session is missing"
            );
            return;
        };
        entry.session.notify_mailbox_only_wake(slot_id, source);
    }

    /// Friendly pre-check used before invoking run-scoped team tools. This is
    /// not a concurrency guarantee; any operation
    /// that writes mailbox, projection, scheduler, spawn, shutdown, or wake state
    /// must still acquire a TeamRun operation lease in TeamSession/TeamRunManager.
    pub(crate) async fn require_active_team_run_for_team_work(&self, team_id: &str) -> Result<(), TeamError> {
        let entry = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
        if entry.session.team_run_manager().active_run_id().await.is_some() {
            return Ok(());
        }
        Err(TeamError::InvalidRequest(
            "no active team run for run-scoped wake".into(),
        ))
    }

    pub(crate) async fn notify_leader_spawn_attach_failed(
        &self,
        team_id: &str,
        failed_slot_id: &str,
        error: &str,
    ) -> Result<(), TeamError> {
        let entry = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
        entry
            .session
            .notify_leader_spawn_attach_failed(failed_slot_id, error)
            .await
    }

    pub(crate) async fn mark_agent_attach_failed(&self, team_id: &str, slot_id: &str) -> Result<(), TeamError> {
        let entry = self
            .sessions
            .get(team_id)
            .ok_or_else(|| TeamError::SessionNotFound(team_id.into()))?;
        entry
            .session
            .scheduler()
            .set_status(slot_id, TeammateStatus::Error)
            .await?;
        entry
            .session
            .team_run_manager()
            .mark_slot_runtime_health(slot_id, TeamSlotRuntimeHealth::Unhealthy)
            .await;
        Ok(())
    }

    pub(crate) async fn wake_leader_after_recovery_message(
        &self,
        team_id: &str,
        source_slot_id: &str,
        source: TeamWakeSource,
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use aionui_ai_agent::types::{BuildTaskOptions, SendMessageData};
    use aionui_ai_agent::{
        AgentError, AgentInstance, AgentSendError, AgentStreamEvent, IAgentTask, IMockAgent, IWorkerTaskManager,
    };
    use aionui_api_types::{AddAgentRequest, ConfigOptionConfirmation, SetConfigOptionResponse};
    use aionui_common::{AgentKillReason, AgentType, ConversationStatus, TimestampMs, now_ms};
    use aionui_db::{IConversationRepository, ITeamRepository};
    use tokio::sync::broadcast;

    use crate::test_utils::workspace_harness::{
        setup_with_factory_metadata_team_repo_and_conversation_repo,
        setup_with_factory_metadata_team_repo_conversation_repo_and_broadcaster,
        setup_with_factory_metadata_team_repo_conversation_repo_broadcaster_and_task_manager,
        single_agent_team_request,
    };

    struct ModeSettingAgent {
        conversation_id: String,
        mode_result: Mutex<Result<(), String>>,
        event_tx: broadcast::Sender<AgentStreamEvent>,
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
                mode_result: Mutex::new(mode_result),
                event_tx,
            }
        }
    }

    #[async_trait::async_trait]
    impl IAgentTask for ModeSettingAgent {
        fn agent_type(&self) -> AgentType {
            AgentType::Acp
        }

        fn conversation_id(&self) -> &str {
            &self.conversation_id
        }

        fn workspace(&self) -> &str {
            "/tmp/aioncore-team-mode-test"
        }

        fn status(&self) -> Option<ConversationStatus> {
            None
        }

        fn last_activity_at(&self) -> TimestampMs {
            now_ms()
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

        let row = repo.get_team(&created.id).await.unwrap().expect("team row");
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
        let mut row = repo.get_team(&created.id).await.unwrap().expect("team row");
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
            })
            .await
            .unwrap();

        svc.set_session_mode("user-test", &created.id, "read-only")
            .await
            .unwrap();

        let team = repo.get_team(&created.id).await.unwrap().expect("team row");
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
        assert_eq!(active_run.team_run_id, ack.team_run_id);
        assert_eq!(active_run.status, ack.status);
        assert_eq!(active_run.target_slot_id, ack.target_slot_id);
        assert_eq!(active_run.target_role, ack.target_role);
        assert_eq!(active_run.pending_wake_count, 1);
        assert_eq!(active_run.slot_work.len(), 1);
        assert_eq!(active_run.slot_work[0].slot_id, ack.accepted_slot_id);
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

        assert!(matches!(err, crate::error::TeamError::Forbidden(_)));
    }
}
