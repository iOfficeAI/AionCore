use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Instant;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    TeamAgentRuntimeStatus, TeamChildTurnPayload, TeamMessageEnqueueStatus, TeamRunAckResponse, TeamRunStatus,
    TeamRunTargetRole, TeamSlotWorkPayload, TeamToolTransport,
};
use aionui_common::{AgentKillReason, generate_id};
use aionui_db::ITeamRepository;
use aionui_realtime::EventBroadcaster;
use tracing::{info, warn};

use crate::error::TeamError;
use crate::event_loop::EventLoopRegistry;
use crate::events::{TEAM_CHILD_TURN_CANCELLED_EVENT, TeamEventEmitter};
use crate::mailbox::Mailbox;
use crate::mcp::{TeamMcpServer, TeamMcpStdioConfig, TeamMcpStdioServerSpec};
use crate::member_runtime::{
    AttachLease, AttachOutcome, BeginRemove, MemberRuntimeFailure, MemberRuntimeRegistry, MemberRuntimeSnapshot,
    ReserveAttach,
};
use crate::message_projection::{
    TeamMessageProjection, TeamProjectionMessageStore, TeamProjectionRequest, TeamProjectionSource, teammate_dedupe_key,
};
use crate::ports::{AgentTurnCancellationPort, AgentTurnExecutionPort};
use crate::prompt_dump::{TeamPromptDumpConfig, TeamWakePromptDump, dump_team_wake_prompt};
use crate::prompts::{build_lead_prompt_for_transport, build_teammate_prompt_for_transport, build_wake_payload};
use crate::provisioning::PersistSpawnedAgentRequest;
use crate::scheduler::{TeammateManager, normalize_name};
use crate::service::TeamSessionService;
use crate::task_board::TaskBoard;
use crate::team_run::{TeamRunManager, target_role_for};
use crate::types::{MailboxMessageType, Team, TeamAgent, TeammateRole, TeammateStatus};
use crate::work_coordinator::{
    CausalBinding, CommitResult, EnqueueCommit, EnqueueDisposition, EnqueueLease, EnqueueRequest, ReconcileDecision,
    RuntimeConstraint, SlotWorkCoordinator, WorkBatch,
};
use crate::work_source::WorkSource;

/// Input for the wake path. Produced by [`TeamSession::compute_wake_input`],
/// consumed by D7b's `send_message` / `send_message_to_agent` (not implemented
/// in D7a). `first_message` includes the role prompt on cold starts.
#[derive(Debug, Clone)]
pub struct WakeInput {
    pub conversation_id: String,
    pub first_message: String,
    /// Unread mailbox rows used to build `first_message`. Returned so the
    /// caller can mirror non-user senders into the target agent's conversation
    /// as left bubbles (matches AionUi `TeammateManager.wake()`). These are
    /// **not** yet marked as read — the caller must call
    /// `mailbox.mark_read_batch` after successful delivery.
    pub unread: Vec<crate::types::MailboxMessage>,
    /// Role of the wake target.
    pub agent_role: TeammateRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageQueueResult {
    pub team_run_id: Option<String>,
    pub target: TeamSlotWorkPayload,
}

pub(crate) enum PrepareBatchResult {
    Execute { batch: Box<WorkBatch>, input: WakeInput },
    SettleSignals { intent_ids: Vec<String> },
    WaitingForCompletion,
    Blocked,
    Quiescent,
}

/// Input for [`TeamSession::spawn_agent`]. Populated by the lead agent when
/// it calls the `spawn_agent` MCP tool.
#[derive(Debug, Clone)]
pub struct SpawnAgentRequest {
    pub name: String,
    pub assistant_id: Option<String>,
}

pub struct TeamSession {
    team: Team,
    scheduler: Arc<TeammateManager>,
    mailbox: Arc<Mailbox>,
    task_board: Arc<TaskBoard>,
    mcp_server: TeamMcpServer,
    backend_binary_path: Arc<PathBuf>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    turn_port: Arc<dyn AgentTurnExecutionPort>,
    cancellation_port: Arc<dyn AgentTurnCancellationPort>,
    projection_store: Arc<dyn TeamProjectionMessageStore>,
    team_run_manager: Arc<TeamRunManager>,
    work_coordinator: Arc<SlotWorkCoordinator>,
    /// Owner user_id for this team — needed when spawn_agent creates a
    /// new conversation (conversations are scoped per user).
    user_id: String,
    /// Weak upward ref so `spawn_agent` can reach the DB-facing orchestration
    /// in `TeamSessionService` (conversation creation, persisted agent list)
    /// without creating a strong cycle with the session map that owns `self`.
    /// `None` in unit tests that don't exercise the DB path.
    service: Weak<TeamSessionService>,
    /// Used by the wake path to mirror non-user mailbox rows into the target
    /// agent's conversation as left bubbles (AionUi parity: see
    /// `TeammateManager.wake()`'s `teammate_message` emission).
    broadcaster: Arc<dyn EventBroadcaster>,
    /// Per-agent event loop registry. Each agent has a dedicated tokio task
    /// that drains its mailbox whenever notified.
    event_loops: Arc<EventLoopRegistry>,
    /// Per-member runtime lifecycle for this exact session incarnation.
    /// Created once with a fresh generation, seeded before publication, read
    /// by dynamic attach/reconcile operations, and permanently stopped before
    /// bridge/event-loop shutdown. A generation is never reused by another
    /// `TeamSession`, even for the same team id.
    member_runtimes: Arc<MemberRuntimeRegistry>,
    prompt_dump: TeamPromptDumpConfig,
    /// Set after the session lifecycle performs its system recovery mailbox scan.
    /// Written by `try_start_recovery_drain` and read by later scan attempts so
    /// ordinary event-loop notifications cannot repeatedly create recovery runs.
    /// Reset only by constructing a fresh `TeamSession` during a new restore,
    /// reconnect, or explicit re-ensure lifecycle.
    recovery_scan_completed: AtomicBool,
}

impl TeamSession {
    #[allow(clippy::too_many_arguments)]
    pub async fn start(
        team: Team,
        repo: Arc<dyn ITeamRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        backend_binary_path: Arc<PathBuf>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        turn_port: Arc<dyn AgentTurnExecutionPort>,
        cancellation_port: Arc<dyn AgentTurnCancellationPort>,
        projection_store: Arc<dyn TeamProjectionMessageStore>,
        user_id: String,
        service: Weak<TeamSessionService>,
    ) -> Result<Self, TeamError> {
        Self::start_with_prompt_dump(
            team,
            repo,
            broadcaster,
            backend_binary_path,
            task_manager,
            turn_port,
            cancellation_port,
            projection_store,
            user_id,
            service,
            TeamPromptDumpConfig::disabled(),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_prompt_dump(
        team: Team,
        repo: Arc<dyn ITeamRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        backend_binary_path: Arc<PathBuf>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        turn_port: Arc<dyn AgentTurnExecutionPort>,
        cancellation_port: Arc<dyn AgentTurnCancellationPort>,
        projection_store: Arc<dyn TeamProjectionMessageStore>,
        user_id: String,
        service: Weak<TeamSessionService>,
        prompt_dump: TeamPromptDumpConfig,
    ) -> Result<Self, TeamError> {
        let mailbox = Arc::new(Mailbox::new(repo.clone()));
        let task_board = Arc::new(TaskBoard::new(repo));
        let member_runtimes = Arc::new(MemberRuntimeRegistry::new(generate_id()));
        let team_run_manager = Arc::new(TeamRunManager::new(
            team.id.clone(),
            Arc::new(TeamEventEmitter::new(team.id.clone(), broadcaster.clone())),
        ));
        let work_coordinator = Arc::new(SlotWorkCoordinator::new(
            team.id.clone(),
            member_runtimes.generation().to_owned(),
            team_run_manager.clone(),
        ));

        let scheduler = Arc::new(TeammateManager::new(
            team.id.clone(),
            &team.agents,
            mailbox.clone(),
            task_board.clone(),
            broadcaster.clone(),
        ));

        let auth_token = aionui_common::generate_id();
        let mcp_server = TeamMcpServer::start_with_prompt_dump(
            auth_token,
            scheduler.clone(),
            team.id.clone(),
            broadcaster.clone(),
            service.clone(),
            Some(prompt_dump.clone()),
        )
        .await?;

        let event_loops = Arc::new(EventLoopRegistry::new());

        info!(
            team_id = %team.id,
            port = mcp_server.port(),
            "TeamSession started"
        );

        Ok(Self {
            team,
            scheduler,
            mailbox,
            task_board,
            mcp_server,
            backend_binary_path,
            task_manager,
            turn_port,
            cancellation_port,
            projection_store,
            team_run_manager,
            work_coordinator,
            user_id,
            service,
            broadcaster,
            event_loops,
            member_runtimes,
            prompt_dump,
            recovery_scan_completed: AtomicBool::new(false),
        })
    }

    pub fn team_id(&self) -> &str {
        &self.team.id
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn scheduler(&self) -> &Arc<TeammateManager> {
        &self.scheduler
    }

    pub fn event_loops(&self) -> &Arc<EventLoopRegistry> {
        &self.event_loops
    }

    pub(crate) fn member_runtimes(&self) -> &Arc<MemberRuntimeRegistry> {
        &self.member_runtimes
    }

    pub(crate) fn generation(&self) -> &str {
        self.member_runtimes.generation()
    }

    pub fn turn_port(&self) -> &Arc<dyn AgentTurnExecutionPort> {
        &self.turn_port
    }

    pub fn cancellation_port(&self) -> &Arc<dyn AgentTurnCancellationPort> {
        &self.cancellation_port
    }

    pub(crate) fn team_event_emitter(&self) -> Arc<TeamEventEmitter> {
        Arc::new(TeamEventEmitter::new(self.team.id.clone(), self.broadcaster.clone()))
    }

    pub fn team_run_manager(&self) -> &Arc<TeamRunManager> {
        &self.team_run_manager
    }

    pub(crate) fn work_coordinator(&self) -> &Arc<SlotWorkCoordinator> {
        &self.work_coordinator
    }

    pub fn mcp_stdio_config(&self, slot_id: &str) -> TeamMcpStdioConfig {
        TeamMcpStdioConfig {
            team_id: self.team.id.clone(),
            port: self.mcp_server.port(),
            token: self.mcp_server.auth_token().to_owned(),
            slot_id: slot_id.to_owned(),
            binary_path: self.backend_binary_path.to_string_lossy().into_owned(),
        }
    }

    /// Returns the stdio server spec that `TeamSessionService::ensure_session`
    /// (D9) persists into each agent's `conversation.extra` and that ACP
    /// `session/new` consumes via `mcp_servers`.
    pub fn stdio_spec(&self, slot_id: &str) -> TeamMcpStdioServerSpec {
        let binary_path = self.backend_binary_path.to_string_lossy();
        TeamMcpStdioServerSpec::from_config(binary_path.as_ref(), &self.mcp_stdio_config(slot_id))
    }

    async fn team_tool_transport_for_agent(&self, agent: &TeamAgent) -> Result<TeamToolTransport, TeamError> {
        let Some(service) = self.service.upgrade() else {
            return Ok(TeamToolTransport::Mcp);
        };
        service.provisioner().team_tool_transport(agent).await
    }

    pub(crate) async fn prepare_next_batch(&self, slot_id: &str) -> Result<PrepareBatchResult, TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        let runtime_constraint = match self.member_runtimes.snapshot(slot_id) {
            MemberRuntimeSnapshot::Absent if self.event_loops.has(slot_id) => RuntimeConstraint::Ready,
            MemberRuntimeSnapshot::Absent => RuntimeConstraint::Starting { operation_id: 0 },
            MemberRuntimeSnapshot::Attaching { operation_id } => RuntimeConstraint::Starting { operation_id },
            MemberRuntimeSnapshot::Ready => RuntimeConstraint::Ready,
            MemberRuntimeSnapshot::Failed { operation_id, failure } => RuntimeConstraint::Failed {
                operation_id,
                classification: failure.classification,
            },
            MemberRuntimeSnapshot::Removing { operation_id } => RuntimeConstraint::Removing { operation_id },
            MemberRuntimeSnapshot::SessionStopped => RuntimeConstraint::SessionStopped,
        };
        let update = self
            .work_coordinator
            .set_runtime_constraint(slot_id, runtime_constraint);
        if !update.terminal_message_ids.is_empty() {
            self.mailbox.mark_read_batch(&update.terminal_message_ids).await?;
        }

        let unread = self
            .mailbox
            .peek_unread(&self.team.id, slot_id)
            .await?
            .into_iter()
            .filter(|message| message.from_agent_id != slot_id)
            .collect::<Vec<_>>();
        let unread_ids = unread.iter().map(|message| message.id.clone()).collect::<Vec<_>>();
        self.work_coordinator
            .reconcile_mailbox(slot_id, &unread_ids, target_role_for(agent.role));

        match self.work_coordinator.next(slot_id) {
            ReconcileDecision::Claim(batch) => {
                let claimed_ids = batch
                    .mailbox_message_ids
                    .iter()
                    .map(String::as_str)
                    .collect::<std::collections::HashSet<_>>();
                let claimed_unread = unread
                    .into_iter()
                    .filter(|message| claimed_ids.contains(message.id.as_str()))
                    .collect::<Vec<_>>();
                let tasks = match self.scheduler.list_tasks().await {
                    Ok(tasks) => tasks,
                    Err(error) => {
                        self.work_coordinator.retry_start(&batch, "batch_prepare_failed");
                        return Err(error);
                    }
                };
                let current_slot_ids = self
                    .scheduler
                    .list_agents()
                    .await
                    .into_iter()
                    .map(|member| member.slot_id)
                    .collect();
                let wake_body = build_wake_payload(&agent, &tasks, &claimed_unread, &current_slot_ids);
                let needs_role_prompt = self.scheduler.take_needs_role_prompt(slot_id).await;
                let first_message = if needs_role_prompt {
                    let tool_transport = self.team_tool_transport_for_agent(&agent).await?;
                    let role_prompt = match agent.role {
                        TeammateRole::Lead => build_lead_prompt_for_transport(
                            &agent,
                            &self.team.name,
                            &self.scheduler.list_agents().await,
                            &[],
                            tool_transport,
                        ),
                        TeammateRole::Teammate => {
                            let members = self.scheduler.list_agents().await;
                            build_teammate_prompt_for_transport(&agent, &self.team.name, &members, tool_transport)
                        }
                    };
                    format!("{role_prompt}\n\n{wake_body}")
                } else {
                    wake_body
                };

                match dump_team_wake_prompt(
                    &self.prompt_dump,
                    TeamWakePromptDump {
                        team_id: &self.team.id,
                        slot_id,
                        conversation_id: &agent.conversation_id,
                        role: agent.role,
                        needs_role_prompt,
                        unread_count: claimed_unread.len(),
                        prompt: &first_message,
                    },
                ) {
                    Ok(Some(path)) => tracing::debug!(
                        team_id = %self.team.id,
                        slot_id,
                        path = %path.display(),
                        "team wake prompt dump written"
                    ),
                    Ok(None) => {}
                    Err(error) => warn!(
                        team_id = %self.team.id,
                        slot_id,
                        error = %error,
                        "team wake prompt dump failed"
                    ),
                }

                Ok(PrepareBatchResult::Execute {
                    batch: Box::new(batch),
                    input: WakeInput {
                        conversation_id: agent.conversation_id,
                        first_message,
                        unread: claimed_unread,
                        agent_role: agent.role,
                    },
                })
            }
            ReconcileDecision::SettleSignals(intent_ids) => Ok(PrepareBatchResult::SettleSignals { intent_ids }),
            ReconcileDecision::WaitingForCompletion => Ok(PrepareBatchResult::WaitingForCompletion),
            ReconcileDecision::Blocked(_) => Ok(PrepareBatchResult::Blocked),
            ReconcileDecision::Quiescent => Ok(PrepareBatchResult::Quiescent),
        }
    }

    pub(crate) async fn handle_signal_intents(&self, slot_id: &str, intent_ids: &[String]) {
        if self.work_coordinator.complete_signals(slot_id, intent_ids) != CommitResult::Committed {
            warn!(
                team_id = %self.team.id,
                slot_id,
                intent_count = intent_ids.len(),
                "team signal intents could not be settled"
            );
        }
    }

    /// Handle agent Finish/Error events. Delegates to the scheduler's
    /// `finalize_turn` with no parsed actions (phase1 does not parse the
    /// trailing message for scheduler directives). Returns the leader slot_id
    /// that the caller should re-wake, if any; D7b wires that return value
    /// into the wake path. `is_error` is reserved for future status handling.
    pub async fn on_agent_finish(&self, conversation_id: &str, is_error: bool) -> Result<Option<String>, TeamError> {
        // Dedup: skip if another finish event already claimed this conversation
        // within the 5-second window (W4-D19a).
        if !self.scheduler.begin_finalize(conversation_id) {
            return Ok(None);
        }

        let slot_id = {
            let agents = self.scheduler.list_agents().await;
            agents
                .into_iter()
                .find(|a| a.conversation_id == conversation_id)
                .map(|a| a.slot_id)
                .ok_or_else(|| TeamError::AgentNotFound(format!("no agent with conversation_id={conversation_id}")))?
        };

        // The event loop's `finalize_turn` handles most cases, but
        // `on_agent_finish` remains callable for aionrs resume and test scenarios.
        // `begin_finalize` dedup prevents double finalization.

        if is_error {
            self.scheduler.set_status(&slot_id, TeammateStatus::Error).await?;
        }

        let wake_target = self.scheduler.finalize_turn(&slot_id, &[]).await?;

        // Clear the dedup window unconditionally once finalize has run.
        self.scheduler.clear_finalized_turn(conversation_id);

        // Re-wake self if there are still unread messages in mailbox.
        // This handles the case where messages arrived while the agent was
        // working (e.g. shutdown_request). Mirrors Claude's useMailboxBridge:
        // when isLoading becomes false, poll mailbox and submit if non-empty.
        if wake_target.as_deref() != Some(&slot_id) {
            let has_unread = self.mailbox.has_unread(&self.team.id, &slot_id).await.unwrap_or(false);
            if has_unread {
                return Ok(Some(slot_id));
            }
        }

        Ok(wake_target)
    }

    /// Write a user message to the lead's mailbox and trigger a wake.
    ///
    /// Wake failures are logged but **not** propagated (D7b log-not-throw
    /// semantics — see backend-audit §3.5 #46): the mailbox row is already
    /// persisted, so surfacing an error to the HTTP caller would invite a
    /// retry that double-writes the message.
    pub async fn send_message(
        &self,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        let lead_slot_id = self
            .scheduler
            .find_lead_slot_id()
            .await
            .ok_or_else(|| TeamError::AgentNotFound("no lead agent in team".into()))?;
        self.enqueue_user_message(&lead_slot_id, TeamRunTargetRole::Lead, content, files)
            .await
    }

    pub async fn send_message_to_agent(
        &self,
        slot_id: &str,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        self.enqueue_user_message(slot_id, target_role_for(agent.role), content, files)
            .await
    }

    async fn enqueue_user_message(
        &self,
        slot_id: &str,
        role: TeamRunTargetRole,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        self.publish_runtime_constraint(slot_id).await?;
        let agent = self.scheduler.get_agent(slot_id).await?;
        let source = if self.team_run_manager.current_active_run_id().is_some() {
            WorkSource::UserIntervention
        } else {
            WorkSource::UserMessage
        };
        let lease = self.work_coordinator.acquire_enqueue(EnqueueRequest {
            slot_id: slot_id.to_owned(),
            role,
            source,
            binding: CausalBinding::UserVisible,
        })?;
        let mailbox_message = match self
            .mailbox
            .write_with_files(
                &self.team.id,
                slot_id,
                "user",
                MailboxMessageType::Message,
                content,
                None,
                files.as_deref(),
            )
            .await
        {
            Ok(message) => message,
            Err(error) => {
                self.work_coordinator.abort_enqueue(&lease, "mailbox_write_failed");
                return Err(error);
            }
        };

        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let request = TeamProjectionRequest::user_visible(
            &self.team.id,
            slot_id,
            &agent.conversation_id,
            content,
            files.unwrap_or_default(),
        );
        if let Err(error) = projection.project(request).await {
            warn!(
                team_id = %self.team.id,
                slot_id,
                conversation_id = %agent.conversation_id,
                error = %error,
                "failed to project user right bubble (non-fatal)"
            );
        }

        let commit = self
            .commit_persisted_enqueue(&lease, mailbox_message.id.clone())
            .await?;
        self.event_loops.notify(slot_id);
        let snapshot = self.work_coordinator.snapshot();
        let run = self
            .team_run_manager
            .current_payload(&snapshot)
            .ok_or_else(|| TeamError::InvalidRequest("user-visible enqueue lost its team run".into()))?;
        Ok(TeamRunAckResponse {
            enqueue_status: match commit.disposition {
                EnqueueDisposition::Accepted => TeamMessageEnqueueStatus::Accepted,
                EnqueueDisposition::Queued => TeamMessageEnqueueStatus::Queued,
                EnqueueDisposition::BlockedRuntimeStarting => TeamMessageEnqueueStatus::BlockedRuntimeStarting,
            },
            message_id: mailbox_message.id,
            run,
        })
    }

    async fn publish_runtime_constraint(&self, slot_id: &str) -> Result<(), TeamError> {
        let constraint = match self.member_runtimes.snapshot(slot_id) {
            MemberRuntimeSnapshot::Absent if self.event_loops.has(slot_id) => RuntimeConstraint::Ready,
            MemberRuntimeSnapshot::Absent => RuntimeConstraint::Starting { operation_id: 0 },
            MemberRuntimeSnapshot::Attaching { operation_id } => RuntimeConstraint::Starting { operation_id },
            MemberRuntimeSnapshot::Ready => RuntimeConstraint::Ready,
            MemberRuntimeSnapshot::Failed { operation_id, failure } => RuntimeConstraint::Failed {
                operation_id,
                classification: failure.classification,
            },
            MemberRuntimeSnapshot::Removing { operation_id } => RuntimeConstraint::Removing { operation_id },
            MemberRuntimeSnapshot::SessionStopped => RuntimeConstraint::SessionStopped,
        };
        let update = self.work_coordinator.set_runtime_constraint(slot_id, constraint);
        if !update.terminal_message_ids.is_empty() {
            self.mailbox.mark_read_batch(&update.terminal_message_ids).await?;
        }
        Ok(())
    }

    pub(crate) async fn send_agent_message_from_agent(
        &self,
        from_slot_id: &str,
        to_slot_id: &str,
        content: &str,
    ) -> Result<AgentMessageQueueResult, TeamError> {
        let to_agent = self.scheduler.get_agent(to_slot_id).await?;
        let from_agent = self.scheduler.get_agent(from_slot_id).await?;
        self.publish_runtime_constraint(to_slot_id).await?;
        let lease = self.work_coordinator.acquire_enqueue(EnqueueRequest {
            slot_id: to_slot_id.to_owned(),
            role: target_role_for(to_agent.role),
            source: WorkSource::McpSendMessage,
            binding: CausalBinding::InheritRunningBatch {
                caller_slot_id: from_slot_id.to_owned(),
            },
        })?;
        let mailbox_message = match self
            .mailbox
            .write(
                &self.team.id,
                to_slot_id,
                from_slot_id,
                MailboxMessageType::Message,
                content,
                None,
            )
            .await
        {
            Ok(message) => message,
            Err(error) => {
                self.work_coordinator.abort_enqueue(&lease, "mailbox_write_failed");
                return Err(error);
            }
        };

        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let request = TeamProjectionRequest {
            team_id: self.team.id.clone(),
            slot_id: to_slot_id.to_owned(),
            conversation_id: to_agent.conversation_id.clone(),
            source: TeamProjectionSource::Teammate {
                from_slot_id: from_slot_id.to_owned(),
                from_name: from_agent.name,
                sender_backend: Some(from_agent.backend),
                sender_conversation_id: Some(from_agent.conversation_id),
            },
            content: content.to_owned(),
            files: Vec::new(),
            visibility: crate::visibility::TeamVisibilityPolicy::teammate_message(),
            dedupe_key: Some(teammate_dedupe_key(
                &self.team.id,
                &mailbox_message.id,
                &to_agent.conversation_id,
            )),
        };
        if let Err(error) = projection.project(request).await {
            warn!(
                team_id = %self.team.id,
                from_slot_id,
                to_slot_id,
                mailbox_message_id = %mailbox_message.id,
                error = %error,
                "team agent message projection failed (non-fatal)"
            );
        }

        let commit = self.commit_persisted_enqueue(&lease, mailbox_message.id).await?;
        self.event_loops.notify(to_slot_id);
        Ok(AgentMessageQueueResult {
            team_run_id: commit.team_run_id,
            target: TeamRunManager::slot_payload(&commit.slot),
        })
    }

    pub(crate) async fn shutdown_agent(
        &self,
        caller_slot_id: &str,
        target_slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        let caller = self.scheduler.get_agent(caller_slot_id).await?;
        if caller.role != TeammateRole::Lead {
            return Err(TeamError::LeaderOnly("team_shutdown_agent".into()));
        }
        let target = self.scheduler.get_agent(target_slot_id).await?;
        if target.role == TeammateRole::Lead {
            return Err(TeamError::InvalidRequest("cannot shutdown the team lead".into()));
        }
        self.publish_runtime_constraint(target_slot_id).await?;
        let lease = self.work_coordinator.acquire_enqueue(EnqueueRequest {
            slot_id: target_slot_id.to_owned(),
            role: target_role_for(target.role),
            source: WorkSource::McpShutdownRequest,
            binding: CausalBinding::InheritRunningBatch {
                caller_slot_id: caller_slot_id.to_owned(),
            },
        })?;
        let shutdown_message = match self
            .scheduler
            .request_shutdown_agent(caller_slot_id, target_slot_id, reason.as_deref())
            .await
        {
            Ok(message) => message,
            Err(error) => {
                self.work_coordinator
                    .abort_enqueue(&lease, "shutdown_scheduler_action_failed");
                return Err(error);
            }
        };
        if shutdown_message.to_agent_id != target_slot_id {
            self.work_coordinator
                .abort_enqueue(&lease, "shutdown_mailbox_target_mismatch");
            return Err(TeamError::InvalidRequest(format!(
                "shutdown mailbox target mismatch: expected {target_slot_id}, got {}",
                shutdown_message.to_agent_id
            )));
        }
        self.commit_persisted_enqueue(&lease, shutdown_message.id).await?;
        self.event_loops.notify(target_slot_id);
        Ok(())
    }

    async fn enqueue_existing_work(
        &self,
        slot_id: &str,
        source: WorkSource,
        mailbox_message_id: Option<String>,
        binding: CausalBinding,
    ) -> Result<Option<String>, TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        self.publish_runtime_constraint(slot_id).await?;
        let lease = self.work_coordinator.acquire_enqueue(EnqueueRequest {
            slot_id: slot_id.to_owned(),
            role: target_role_for(agent.role),
            source,
            binding,
        })?;
        let commit = match mailbox_message_id {
            Some(message_id) => self.commit_persisted_enqueue(&lease, message_id).await?,
            None => match self.work_coordinator.commit_enqueue(&lease, None) {
                Ok(commit) => commit,
                Err(error) => {
                    self.work_coordinator.abort_enqueue(&lease, "enqueue_commit_failed");
                    return Err(error);
                }
            },
        };
        self.event_loops.notify(slot_id);
        Ok(commit.team_run_id)
    }

    async fn commit_persisted_enqueue(
        &self,
        lease: &EnqueueLease,
        mailbox_message_id: String,
    ) -> Result<EnqueueCommit, TeamError> {
        match self
            .work_coordinator
            .commit_enqueue(lease, Some(mailbox_message_id.clone()))
        {
            Ok(commit) => Ok(commit),
            Err(error) => {
                self.work_coordinator.abort_enqueue(lease, "enqueue_commit_failed");
                if let Err(mark_read_error) = self
                    .mailbox
                    .mark_read_batch(std::slice::from_ref(&mailbox_message_id))
                    .await
                {
                    warn!(
                        team_id = %self.team.id,
                        slot_id = %lease.slot_id,
                        mailbox_message_id,
                        error_classification = "enqueue_commit_compensation_failed",
                        error = %mark_read_error,
                        "persisted team message could not be settled after enqueue rejection"
                    );
                }
                Err(error)
            }
        }
    }

    pub(crate) async fn enqueue_leader_settle_signal(
        &self,
        slot_id: &str,
        source: WorkSource,
    ) -> Result<(), TeamError> {
        self.enqueue_existing_work(slot_id, source, None, CausalBinding::ActiveRunOrBackground)
            .await?;
        Ok(())
    }

    pub(crate) async fn try_start_recovery_drain(&self, reason: &'static str) -> Result<Vec<String>, TeamError> {
        if self.recovery_scan_completed.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                team_id = %self.team.id,
                reason,
                "team recovery scan skipped because it already ran for this session lifecycle"
            );
            return Ok(Vec::new());
        }

        let mut recovered_slots = Vec::new();
        for agent in self.scheduler.list_agents().await {
            let unread = self
                .mailbox
                .peek_unread(&self.team.id, &agent.slot_id)
                .await?
                .into_iter()
                .filter(|message| message.from_agent_id != agent.slot_id)
                .map(|message| message.id)
                .collect::<Vec<_>>();
            if unread.is_empty() {
                continue;
            }
            for message_id in unread {
                self.enqueue_existing_work(
                    &agent.slot_id,
                    WorkSource::RecoveryDrain,
                    Some(message_id),
                    CausalBinding::Background,
                )
                .await?;
            }
            recovered_slots.push(agent.slot_id);
        }
        info!(
            team_id = %self.team.id,
            recovered_slot_count = recovered_slots.len(),
            reason,
            "team recovery scan projected background work"
        );
        Ok(recovered_slots)
    }

    /// Mirror each non-user mailbox row into the target agent's conversation
    /// as a left bubble so the UI shows "who said what" when the user opens
    /// an agent's chat panel.
    ///
    /// Skipped for:
    /// - `from_agent_id == "user"`: user-originated messages are already
    ///   written to the conversation by the standard user-send path, and we
    ///   must not double-write them.
    /// - `IdleNotification`: internal mailbox wake/prompt signal, not a
    ///   teammate chat message.
    ///
    /// Failures per-message are logged and swallowed — the mailbox rows are
    /// already marked read, and we never let a conversation-write failure
    /// block the wake itself.
    pub(crate) async fn mirror_unread_to_conversation(&self, input: &WakeInput) {
        if input.unread.is_empty() {
            return;
        }
        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let agents = self.scheduler.list_agents().await;
        let total = input.unread.len();

        for msg in &input.unread {
            if msg.from_agent_id == "user" {
                continue;
            }
            if msg.msg_type == MailboxMessageType::IdleNotification {
                continue;
            }
            let sender = agents.iter().find(|a| a.slot_id == msg.from_agent_id);
            let sender_name = sender
                .map(|a| a.name.clone())
                .unwrap_or_else(|| msg.from_agent_id.clone());
            let sender_backend = sender.map(|a| a.backend.clone());
            let sender_conv_id = sender.map(|a| a.conversation_id.clone());
            let display_content = if total > 1 {
                format!("[{sender_name}] {}", msg.content)
            } else {
                msg.content.clone()
            };
            let request = TeamProjectionRequest {
                team_id: self.team.id.clone(),
                slot_id: msg.to_agent_id.clone(),
                conversation_id: input.conversation_id.clone(),
                source: TeamProjectionSource::Teammate {
                    from_slot_id: msg.from_agent_id.clone(),
                    from_name: sender_name,
                    sender_backend,
                    sender_conversation_id: sender_conv_id,
                },
                content: display_content,
                files: msg.files.clone().unwrap_or_default(),
                visibility: crate::visibility::TeamVisibilityPolicy::teammate_message(),
                dedupe_key: Some(teammate_dedupe_key(&self.team.id, &msg.id, &input.conversation_id)),
            };
            if let Err(err) = projection.project(request).await {
                warn!(
                    team_id = %self.team.id,
                    conversation_id = %input.conversation_id,
                    from = %msg.from_agent_id,
                    error = %err,
                    "mirror_unread_to_conversation: projection failed (non-fatal)"
                );
            }
        }
    }

    pub async fn cancel_run(
        &self,
        team_run_id: &str,
        _target_slot_id: Option<String>,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        if self.team_run_manager.current_active_run_id().as_deref() != Some(team_run_id) {
            return Err(TeamError::InvalidRequest(format!(
                "team run {team_run_id} is not active"
            )));
        }
        self.team_run_manager.begin_cancel(team_run_id, reason)?;
        let result = self.work_coordinator.cancel_run(team_run_id);
        if !result.terminal_message_ids.is_empty() {
            self.mailbox.mark_read_batch(&result.terminal_message_ids).await?;
        }
        for target in result.cancel_targets {
            let Some(turn_id) = target.turn_id else {
                continue;
            };
            let agent = self.scheduler.get_agent(&target.batch.slot_id).await?;
            if let Err(error) = self
                .cancellation_port
                .cancel_agent_turn(&self.user_id, &agent.conversation_id, &turn_id)
                .await
            {
                warn!(
                    team_id = %self.team.id,
                    team_run_id,
                    slot_id = %target.batch.slot_id,
                    turn_id,
                    error = %error,
                    "team run cancellation failed for active turn"
                );
            }
            self.team_event_emitter().broadcast_child_turn(
                TEAM_CHILD_TURN_CANCELLED_EVENT,
                TeamChildTurnPayload {
                    team_id: self.team.id.clone(),
                    team_run_id: team_run_id.to_owned(),
                    slot_id: target.batch.slot_id,
                    role: target_role_for(agent.role),
                    conversation_id: agent.conversation_id,
                    turn_id,
                    status: TeamRunStatus::Cancelled,
                },
            );
        }
        Ok(())
    }

    pub async fn cancel_child_turn(
        &self,
        team_run_id: &str,
        slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        let snapshot = self
            .work_coordinator
            .slot_snapshot(slot_id)
            .ok_or_else(|| TeamError::AgentNotFound(slot_id.to_owned()))?;
        let batch = snapshot
            .active_batch
            .ok_or_else(|| TeamError::InvalidRequest(format!("slot has no active batch: {slot_id}")))?;
        if !batch.team_run_ids.iter().any(|run_id| run_id == team_run_id) {
            return Err(TeamError::InvalidRequest(format!(
                "agent {slot_id} is not active in team run {team_run_id}"
            )));
        }
        if let Some(turn_id) = snapshot.active_turn_id {
            let agent = self.scheduler.get_agent(slot_id).await?;
            self.cancellation_port
                .cancel_agent_turn(&self.user_id, &agent.conversation_id, &turn_id)
                .await
                .map_err(|error| TeamError::InvalidRequest(error.to_string()))?;
            self.team_event_emitter().broadcast_child_turn(
                TEAM_CHILD_TURN_CANCELLED_EVENT,
                TeamChildTurnPayload {
                    team_id: self.team.id.clone(),
                    team_run_id: team_run_id.to_owned(),
                    slot_id: slot_id.to_owned(),
                    role: snapshot.role.clone(),
                    conversation_id: agent.conversation_id,
                    turn_id,
                    status: TeamRunStatus::Cancelled,
                },
            );
        }
        self.work_coordinator.cancel_batch(&batch, "child_cancelled");
        if snapshot.role == TeamRunTargetRole::Teammate {
            self.notify_leader_child_interrupted(slot_id, reason).await?;
        }
        Ok(())
    }

    pub async fn pause_slot_work(
        &self,
        team_run_id: &str,
        slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        if self.team_run_manager.current_active_run_id().as_deref() != Some(team_run_id) {
            return Err(TeamError::InvalidRequest(format!(
                "team run {team_run_id} is not active"
            )));
        }
        let outcome = self.work_coordinator.pause_slot(slot_id);
        if let Some(target) = outcome.cancel_target {
            if let Some(turn_id) = target.turn_id {
                let agent = self.scheduler.get_agent(slot_id).await?;
                self.cancellation_port
                    .cancel_agent_turn(&self.user_id, &agent.conversation_id, &turn_id)
                    .await
                    .map_err(|error| TeamError::InvalidRequest(error.to_string()))?;
                self.team_event_emitter().broadcast_child_turn(
                    TEAM_CHILD_TURN_CANCELLED_EVENT,
                    TeamChildTurnPayload {
                        team_id: self.team.id.clone(),
                        team_run_id: team_run_id.to_owned(),
                        slot_id: slot_id.to_owned(),
                        role: outcome.slot.role.clone(),
                        conversation_id: agent.conversation_id,
                        turn_id,
                        status: TeamRunStatus::Cancelled,
                    },
                );
            }
            self.work_coordinator.cancel_batch(&target.batch, "slot_paused");
            if outcome.slot.role == TeamRunTargetRole::Teammate {
                self.notify_leader_child_interrupted(slot_id, reason).await?;
            }
        }
        Ok(())
    }

    async fn notify_leader_child_interrupted(&self, slot_id: &str, reason: Option<String>) -> Result<(), TeamError> {
        if let Some(lead_slot_id) = self.scheduler.find_lead_slot_id().await {
            let content = reason.unwrap_or_else(|| format!("Agent {slot_id} was interrupted by the user."));
            self.mailbox
                .write(
                    &self.team.id,
                    &lead_slot_id,
                    slot_id,
                    MailboxMessageType::IdleNotification,
                    &content,
                    Some("Interrupted by user"),
                )
                .await?;
            self.wake_leader_after_recovery_message(slot_id, WorkSource::InterruptedNotification)
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn notify_leader_spawn_attach_failed(
        &self,
        failed_slot_id: &str,
        error: &str,
    ) -> Result<(), TeamError> {
        let Some(lead_slot_id) = self.scheduler.find_lead_slot_id().await else {
            return Err(TeamError::AgentNotFound("lead".into()));
        };
        let content = format!("Spawned teammate {failed_slot_id} failed to attach its runtime. Error: {error}");
        self.mailbox
            .write(
                &self.team.id,
                &lead_slot_id,
                failed_slot_id,
                MailboxMessageType::Message,
                &content,
                None,
            )
            .await?;
        self.wake_leader_after_recovery_message(failed_slot_id, WorkSource::SpawnAttachFailure)
            .await
    }

    pub(crate) async fn wake_leader_after_recovery_message(
        &self,
        source_slot_id: &str,
        source: WorkSource,
    ) -> Result<(), TeamError> {
        let lead_slot_id = self
            .scheduler
            .find_lead_slot_id()
            .await
            .ok_or_else(|| TeamError::AgentNotFound("lead".into()))?;
        let message_id = self
            .mailbox
            .peek_unread(&self.team.id, &lead_slot_id)
            .await?
            .into_iter()
            .rev()
            .find(|message| message.from_agent_id == source_slot_id)
            .map(|message| message.id);
        let Some(message_id) = message_id else {
            return Ok(());
        };
        self.enqueue_existing_work(
            &lead_slot_id,
            source,
            Some(message_id),
            CausalBinding::InheritRunningBatch {
                caller_slot_id: source_slot_id.to_owned(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn add_agent(&self, agent: &TeamAgent) {
        self.scheduler.add_agent(agent).await;
    }

    /// Reserve the runtime lifecycle before a newly persisted member becomes
    /// visible through the scheduler or membership events.
    pub(crate) fn reserve_dynamic_member_attach(&self, agent: &TeamAgent) -> ReserveAttach {
        self.member_runtimes.reserve_attach(&agent.slot_id, false)
    }

    pub(crate) async fn add_manual_agent(&self, agent: &TeamAgent) -> Result<(), TeamError> {
        self.scheduler.add_agent(agent).await;
        let lead_slot_id = self
            .scheduler
            .find_lead_slot_id()
            .await
            .ok_or_else(|| TeamError::InvalidRequest("team lead not found".into()))?;
        let welcome = self
            .mailbox
            .write(
                &self.team.id,
                &agent.slot_id,
                "team_system",
                MailboxMessageType::Message,
                "You were manually added to this team. Read your mailbox and wait for instructions.",
                None,
            )
            .await?;
        let leader_notice = self
            .mailbox
            .write(
                &self.team.id,
                &lead_slot_id,
                "team_system",
                MailboxMessageType::Message,
                &format!(
                    "The user manually added teammate '{}' (slot_id={}, assistant_id={}, model={}). Call team_members to get the latest roster before delegating work.",
                    agent.name,
                    agent.slot_id,
                    agent.assistant_id.as_deref().unwrap_or(""),
                    agent.model
                ),
                None,
            )
            .await?;
        self.project_team_system_message(&agent.slot_id, &agent.conversation_id, &welcome.id, &welcome.content)
            .await;
        let lead_agent = self.scheduler.get_agent(&lead_slot_id).await?;
        self.project_team_system_message(
            &lead_slot_id,
            &lead_agent.conversation_id,
            &leader_notice.id,
            &leader_notice.content,
        )
        .await;

        self.enqueue_existing_work(
            &agent.slot_id,
            WorkSource::SpawnWelcome,
            Some(welcome.id),
            CausalBinding::ActiveRunOrBackground,
        )
        .await?;
        self.enqueue_existing_work(
            &lead_slot_id,
            WorkSource::TeamMembershipChanged,
            Some(leader_notice.id),
            CausalBinding::ActiveRunOrBackground,
        )
        .await?;
        Ok(())
    }

    pub async fn notify_leader_membership_removed(&self, removed: &TeamAgent) -> Result<(), TeamError> {
        let lead_slot_id = self
            .scheduler
            .find_lead_slot_id()
            .await
            .ok_or_else(|| TeamError::InvalidRequest("team lead not found".into()))?;
        let notice = self
            .mailbox
            .write(
                &self.team.id,
                &lead_slot_id,
                "team_system",
                MailboxMessageType::Message,
                &format!(
                    "Teammate '{}' was removed from the team (slot_id={}, assistant_id={}, model={}). Call team_members to get the latest roster before delegating work.",
                    removed.name,
                    removed.slot_id,
                    removed.assistant_id.as_deref().unwrap_or(""),
                    removed.model
                ),
                None,
            )
            .await?;
        let lead_agent = self.scheduler.get_agent(&lead_slot_id).await?;
        self.project_team_system_message(&lead_slot_id, &lead_agent.conversation_id, &notice.id, &notice.content)
            .await;

        self.enqueue_existing_work(
            &lead_slot_id,
            WorkSource::TeamMembershipChanged,
            Some(notice.id),
            CausalBinding::ActiveRunOrBackground,
        )
        .await?;

        Ok(())
    }

    async fn project_team_system_message(
        &self,
        slot_id: &str,
        conversation_id: &str,
        mailbox_message_id: &str,
        content: &str,
    ) {
        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let request = TeamProjectionRequest::team_system_visible(
            &self.team.id,
            slot_id,
            conversation_id,
            content,
            mailbox_message_id,
        );
        if let Err(err) = projection.project(request).await {
            warn!(
                team_id = %self.team.id,
                slot_id = %slot_id,
                conversation_id = %conversation_id,
                mailbox_message_id = %mailbox_message_id,
                error = %err,
                "team system message projection failed (non-fatal)"
            );
        }
    }

    pub async fn remove_agent(&self, slot_id: &str) -> Result<(), TeamError> {
        let removal = match self.member_runtimes.begin_remove(slot_id) {
            BeginRemove::Join(waiter) => {
                let _ = waiter.wait().await;
                return Ok(());
            }
            removal => removal,
        };
        if let BeginRemove::Start(lease) = &removal {
            self.work_coordinator.set_runtime_constraint(
                slot_id,
                RuntimeConstraint::Removing {
                    operation_id: lease.operation_id(),
                },
            );
        }
        let work = self.work_coordinator.remove_slot(slot_id);
        if !work.terminal_message_ids.is_empty() {
            self.mailbox.mark_read_batch(&work.terminal_message_ids).await?;
        }
        if let Some(target) = work.cancel_target
            && let Some(turn_id) = target.turn_id
        {
            let agent = self.scheduler.get_agent(slot_id).await?;
            let _ = self
                .cancellation_port
                .cancel_agent_turn(&self.user_id, &agent.conversation_id, &turn_id)
                .await;
        }
        self.event_loops.remove(slot_id);
        let conversation_id = self.scheduler.remove_agent(slot_id).await?;
        if let Some(conversation_id) = conversation_id {
            self.task_manager
                .kill_and_wait(&conversation_id, Some(AgentKillReason::TeamDeleted))
                .await;
        }
        if let BeginRemove::Start(lease) = removal {
            self.member_runtimes.finish_remove(&lease);
        }
        Ok(())
    }

    pub async fn rename_agent(&self, slot_id: &str, new_name: &str) -> Result<(), TeamError> {
        self.scheduler.rename_agent(slot_id, new_name).await
    }

    /// Spawn a new teammate at the Lead's request (backing of `team_spawn_agent`).
    ///
    /// Validation chain mirrors the assistant-first team contract:
    /// 1. Caller must exist and carry `TeammateRole::Lead`.
    /// 2. `name` is normalized and must not collide with any live agent.
    /// 3. `assistant_id` must be present and resolve to a team-capable backend.
    ///
    /// On success, a new conversation is created, the agent slot is persisted
    /// into the team row, the MCP stdio config is written into the conversation
    /// extras, the agent task is launched, and a welcome message is dropped
    /// into the new mailbox so the first wake reaches the spawned teammate
    /// with its role prompt.
    pub async fn spawn_agent(&self, caller_slot_id: &str, req: SpawnAgentRequest) -> Result<TeamAgent, TeamError> {
        // Step 1: caller must be a Lead. MCP dispatch already gates by role,
        // but this method is exposed on TeamSession so every entry point
        // (including future direct service callers) re-checks.
        let caller = self.scheduler.get_agent(caller_slot_id).await?;
        if caller.role != TeammateRole::Lead {
            return Err(TeamError::LeaderOnly("spawn_agent".into()));
        }

        // Step 2: normalize + uniqueness check against live scheduler state.
        let requested_name = req.name.trim().to_owned();
        if requested_name.is_empty() {
            return Err(TeamError::InvalidRequest("spawn_agent.name must not be empty".into()));
        }
        let normalized = normalize_name(&requested_name);
        if normalized.is_empty() {
            return Err(TeamError::InvalidRequest(
                "spawn_agent.name is empty after normalization".into(),
            ));
        }
        let existing = self.scheduler.list_agents().await;
        if existing.iter().any(|a| normalize_name(&a.name) == normalized) {
            return Err(TeamError::DuplicateAgentName(requested_name));
        }

        let assistant_id = req
            .assistant_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| TeamError::InvalidRequest("spawn_agent.assistant_id is required".into()))?;

        let service = self
            .service
            .upgrade()
            .ok_or_else(|| TeamError::InvalidRequest("spawn_agent requires a live TeamSessionService".into()))?;

        // Step 3: resolve the effective assistant/backend/model target before
        // capability checks. Assistant spawns derive backend from the preset
        // identity rather than inheriting the caller backend.
        let (backend, model) = service
            .resolve_spawn_backend_and_model(Some(assistant_id), None, caller.backend.as_str(), caller.model.as_str())
            .await?;

        // Step 4: DB side-effects (new conversation + persisted agent slot).
        // Assistant-first spawn has already passed the team-selectable catalog
        // gate while resolving the backend/model above.
        let new_slot_id = generate_id();
        let new_agent = service
            .persist_spawned_agent(PersistSpawnedAgentRequest {
                team_id: self.team.id.clone(),
                user_id: self.user_id.clone(),
                slot_id: new_slot_id.clone(),
                name: requested_name,
                backend,
                model,
                assistant_id: Some(assistant_id.to_owned()),
            })
            .await?;

        let reservation = self.reserve_dynamic_member_attach(&new_agent);

        // Step 5: attach to the in-memory scheduler so wake-from-lead finds
        // the new slot immediately.
        self.scheduler.add_agent(&new_agent).await;

        // Step 6: welcome message. The mailbox write is the source of truth —
        // if the wake never fires (e.g. warmup raced), the next caller-triggered
        // wake will still drain this entry.
        let welcome_message = match self
            .mailbox
            .write(
                &self.team.id,
                &new_agent.slot_id,
                caller_slot_id,
                MailboxMessageType::Message,
                "You have been spawned as a teammate. Read your mailbox and wait for instructions.",
                None,
            )
            .await
        {
            Ok(message) => message,
            Err(err) => {
                if let Some(service) = self.service.upgrade() {
                    let _ = service
                        .remove_agent(&self.user_id, &self.team.id, &new_agent.slot_id)
                        .await;
                } else {
                    let _ = self.scheduler.remove_agent(&new_agent.slot_id).await;
                }
                return Err(err);
            }
        };

        self.enqueue_existing_work(
            &new_agent.slot_id,
            WorkSource::SpawnWelcome,
            Some(welcome_message.id),
            CausalBinding::InheritRunningBatch {
                caller_slot_id: caller_slot_id.to_owned(),
            },
        )
        .await?;

        // Step 7: attach the CLI process and register the finish subscriber
        // in a background task. This involves spawning the CLI process and
        // completing the ACP protocol handshake, which can take significant
        // time (10-30s). Running it asynchronously ensures `spawn_agent`
        // returns promptly so the MCP tool call completes without blocking
        // the leader's connection loop.
        let captured_session = service
            .capture_published_session(self)
            .ok_or_else(|| TeamError::SessionNotFound(self.team.id.clone()))?;
        spawn_attach_agent_process_bg(
            service,
            captured_session,
            self.user_id.clone(),
            new_agent.clone(),
            self.task_manager.clone(),
            reservation,
        );

        Ok(new_agent)
    }

    /// Persist the team MCP stdio config into the spawned agent's conversation
    /// row, then kill any pre-existing task and warm up the new one.
    ///
    /// This is a static helper suitable for use inside `tokio::spawn` (no
    /// `&self` borrow). The caller passes all necessary context by value.
    async fn attach_spawned_agent_process(
        service: &TeamSessionService,
        agent: &TeamAgent,
        mcp_stdio_cfg: crate::mcp::TeamMcpStdioConfig,
        user_id: &str,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(), TeamError> {
        service
            .provisioner()
            .attach_agent_process(user_id, agent, mcp_stdio_cfg, task_manager)
            .await
    }

    pub fn stop(&self) {
        info!(team_id = %self.team.id, generation = %self.generation(), "TeamSession stopping");
        self.work_coordinator.stop();
        self.member_runtimes.stop();
        self.event_loops.shutdown();
        self.mcp_server.stop();
    }

    pub fn mailbox(&self) -> &Arc<Mailbox> {
        &self.mailbox
    }

    pub fn task_board(&self) -> &Arc<TaskBoard> {
        &self.task_board
    }
}

pub(crate) fn spawn_attach_agent_process_bg(
    service: Arc<TeamSessionService>,
    session: Arc<TeamSession>,
    user_id: String,
    agent: TeamAgent,
    task_manager: Arc<dyn IWorkerTaskManager>,
    reservation: ReserveAttach,
) {
    tokio::spawn(async move {
        let outcome = match reservation {
            ReserveAttach::Start(lease) => {
                attach_member_runtime(
                    Arc::clone(&service),
                    Arc::clone(&session),
                    user_id,
                    agent.clone(),
                    task_manager,
                    lease,
                )
                .await
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
                    "team member runtime attach waiting"
                );
                waiter.wait().await
            }
            ReserveAttach::AlreadyReady => AttachOutcome::Ready,
            ReserveAttach::SessionStopped => AttachOutcome::SessionStopped,
        };

        if outcome != AttachOutcome::Ready {
            return;
        }

        let _ = service.with_published_session(&session, |current| {
            current
                .work_coordinator
                .set_runtime_constraint(&agent.slot_id, RuntimeConstraint::Ready);
            current.event_loops.notify(&agent.slot_id);
        });
    });
}

pub(crate) async fn attach_member_runtime(
    service: Arc<TeamSessionService>,
    session: Arc<TeamSession>,
    user_id: String,
    agent: TeamAgent,
    task_manager: Arc<dyn IWorkerTaskManager>,
    lease: AttachLease,
) -> AttachOutcome {
    let started_at = Instant::now();
    let operation_id = lease.operation_id();
    let generation = session.generation().to_owned();
    session
        .work_coordinator
        .set_runtime_constraint(&agent.slot_id, RuntimeConstraint::Starting { operation_id });
    service.publish_member_runtime_starting_if_current(&session);
    info!(
        team_id = session.team_id(),
        slot_id = agent.slot_id,
        conversation_id = agent.conversation_id,
        operation_id,
        generation,
        duration_ms = 0,
        error_classification = "none",
        "team member runtime attach started"
    );

    let attach_result = TeamSession::attach_spawned_agent_process(
        &service,
        &agent,
        session.mcp_stdio_config(&agent.slot_id),
        &user_id,
        &task_manager,
    )
    .await;
    if let Err(error) = attach_result {
        service
            .cleanup_stale_member_runtime_task(&session, &agent.conversation_id)
            .await;
        let failure = sanitize_member_runtime_failure(&error);
        if session.member_runtimes.commit_failed(&lease, failure.clone()) {
            service.broadcast_agent_runtime_status(
                session.team_id(),
                &agent,
                TeamAgentRuntimeStatus::Failed,
                Some(failure.public_reason.clone()),
            );
            let _ = session
                .scheduler
                .set_status(&agent.slot_id, TeammateStatus::Error)
                .await;
            let update = session.work_coordinator.set_runtime_constraint(
                &agent.slot_id,
                RuntimeConstraint::Failed {
                    operation_id,
                    classification: failure.classification,
                },
            );
            if !update.terminal_message_ids.is_empty() {
                let _ = session.mailbox.mark_read_batch(&update.terminal_message_ids).await;
            }
            service.refresh_member_runtime_status(&session).await;
            if let Err(notify_error) = session
                .notify_leader_spawn_attach_failed(&agent.slot_id, &failure.public_reason)
                .await
            {
                warn!(
                    team_id = session.team_id(),
                    slot_id = agent.slot_id,
                    conversation_id = agent.conversation_id,
                    operation_id,
                    generation,
                    duration_ms = started_at.elapsed().as_millis(),
                    error_classification = "leader_notification_failed",
                    error = %notify_error,
                    "team member runtime attach failure notification failed"
                );
            }
        } else {
            warn!(
                team_id = session.team_id(),
                slot_id = agent.slot_id,
                conversation_id = agent.conversation_id,
                operation_id,
                generation,
                duration_ms = started_at.elapsed().as_millis(),
                error_classification = "stale_attach_failure",
                "stale team member runtime attach failure ignored"
            );
        }
        info!(
            team_id = session.team_id(),
            slot_id = agent.slot_id,
            conversation_id = agent.conversation_id,
            operation_id,
            generation,
            duration_ms = started_at.elapsed().as_millis(),
            error_classification = failure.classification,
            "team member runtime attach completed"
        );
        return lease.waiter().wait().await;
    }

    if !session.member_runtimes.owns_attach(&lease) {
        cleanup_stale_attach(&service, &session, &agent, operation_id, &generation, started_at).await;
        return lease.waiter().wait().await;
    }

    let registered_event_loop = if session.event_loops.has(&agent.slot_id) {
        false
    } else {
        match service.register_event_loop(&session, &agent.slot_id) {
            Ok(registered) => registered,
            Err(error) => {
                service
                    .cleanup_stale_member_runtime_task(&session, &agent.conversation_id)
                    .await;
                let failure = MemberRuntimeFailure {
                    classification: "event_loop_registration_failed",
                    public_reason: "Agent runtime event loop could not be registered".to_owned(),
                };
                session.member_runtimes.commit_failed(&lease, failure.clone());
                session.work_coordinator.set_runtime_constraint(
                    &agent.slot_id,
                    RuntimeConstraint::Failed {
                        operation_id,
                        classification: failure.classification,
                    },
                );
                service.refresh_member_runtime_status(&session).await;
                warn!(
                    team_id = session.team_id(),
                    slot_id = agent.slot_id,
                    conversation_id = agent.conversation_id,
                    operation_id,
                    generation,
                    duration_ms = started_at.elapsed().as_millis(),
                    error_classification = failure.classification,
                    error = ?error,
                    "team member runtime attach event loop registration failed"
                );
                return lease.waiter().wait().await;
            }
        }
    };

    if !session.member_runtimes.commit_ready(&lease) {
        if registered_event_loop {
            session.event_loops.remove(&agent.slot_id);
        }
        cleanup_stale_attach(&service, &session, &agent, operation_id, &generation, started_at).await;
        return lease.waiter().wait().await;
    }
    session
        .work_coordinator
        .set_runtime_constraint(&agent.slot_id, RuntimeConstraint::Ready);
    session.event_loops.notify(&agent.slot_id);

    if !service.publish_member_runtime_ready_if_current(&session, &agent) {
        if registered_event_loop {
            session.event_loops.remove(&agent.slot_id);
        }
        cleanup_stale_attach(&service, &session, &agent, operation_id, &generation, started_at).await;
        return AttachOutcome::SessionStopped;
    }
    service.refresh_member_runtime_status(&session).await;
    info!(
        team_id = session.team_id(),
        slot_id = agent.slot_id,
        conversation_id = agent.conversation_id,
        operation_id,
        generation,
        duration_ms = started_at.elapsed().as_millis(),
        error_classification = "none",
        "team member runtime attach completed"
    );
    AttachOutcome::Ready
}

fn sanitize_member_runtime_failure(error: &TeamError) -> MemberRuntimeFailure {
    let classification = match error {
        TeamError::Database(_) => "runtime_configuration_failed",
        TeamError::SessionNotFound(_) => "session_stopped",
        _ => "runtime_start_failed",
    };
    MemberRuntimeFailure {
        classification,
        public_reason: "Agent runtime failed to start".to_owned(),
    }
}

async fn cleanup_stale_attach(
    service: &TeamSessionService,
    session: &TeamSession,
    agent: &TeamAgent,
    operation_id: u64,
    generation: &str,
    started_at: Instant,
) {
    service
        .cleanup_stale_member_runtime_task(session, &agent.conversation_id)
        .await;
    info!(
        team_id = session.team_id(),
        slot_id = agent.slot_id,
        conversation_id = agent.conversation_id,
        operation_id,
        generation,
        duration_ms = started_at.elapsed().as_millis(),
        error_classification = "attach_cancelled",
        "team member runtime attach cancelled"
    );
    warn!(
        team_id = session.team_id(),
        slot_id = agent.slot_id,
        conversation_id = agent.conversation_id,
        operation_id,
        generation,
        duration_ms = started_at.elapsed().as_millis(),
        error_classification = "stale_attach_completion",
        "stale team member runtime attach completion rejected"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_loop::AgentLoopContext;
    use crate::test_utils::MockTeamRepo;
    use crate::types::{Team, TeamAgent, TeammateRole};
    use crate::work_coordinator::SlotPhase;
    use aionui_ai_agent::AgentError;
    use aionui_ai_agent::agent_task::AgentInstance;
    use aionui_ai_agent::types::BuildTaskOptions;
    use aionui_api_types::WebSocketMessage;
    use aionui_common::{AgentKillReason, TimestampMs};
    use std::sync::{Arc, Mutex};

    struct NullBroadcaster;
    impl EventBroadcaster for NullBroadcaster {
        fn broadcast(&self, _msg: WebSocketMessage<serde_json::Value>) {}
    }

    struct NoopTurnPort;

    struct BlockingRunningTurnPort {
        started_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release_rx: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    impl BlockingRunningTurnPort {
        fn new(started_tx: tokio::sync::oneshot::Sender<()>, release_rx: tokio::sync::oneshot::Receiver<()>) -> Self {
            Self {
                started_tx: Mutex::new(Some(started_tx)),
                release_rx: Mutex::new(Some(release_rx)),
            }
        }
    }

    #[async_trait::async_trait]
    impl crate::ports::AgentTurnExecutionPort for BlockingRunningTurnPort {
        async fn run_agent_turn(
            &self,
            request: crate::ports::AgentTurnRequest,
        ) -> Result<crate::ports::AgentTurnOutcome, crate::ports::AgentTurnExecutionError> {
            if let Some(on_started) = request.on_started.as_ref() {
                on_started(crate::ports::AgentTurnStarted {
                    team_run_id: request.team_run_id.clone(),
                    slot_id: request.slot_id.clone(),
                    role: request.role.clone(),
                    conversation_id: request.conversation_id.clone(),
                    turn_id: "turn-background".into(),
                })
                .await;
            }
            if let Some(tx) = self.started_tx.lock().unwrap().take() {
                let _ = tx.send(());
            }
            let release_rx = self.release_rx.lock().unwrap().take();
            if let Some(rx) = release_rx {
                let _ = rx.await;
            }
            Ok(crate::ports::AgentTurnOutcome {
                conversation_id: request.conversation_id,
                turn_id: "turn-background".into(),
                status: crate::ports::AgentTurnStatus::Completed,
                runtime: None,
            })
        }
    }

    #[async_trait::async_trait]
    impl crate::ports::AgentTurnExecutionPort for NoopTurnPort {
        async fn run_agent_turn(
            &self,
            request: crate::ports::AgentTurnRequest,
        ) -> Result<crate::ports::AgentTurnOutcome, crate::ports::AgentTurnExecutionError> {
            if let Some(on_started) = request.on_started.as_ref() {
                on_started(crate::ports::AgentTurnStarted {
                    team_run_id: request.team_run_id.clone(),
                    slot_id: request.slot_id.clone(),
                    role: request.role.clone(),
                    conversation_id: request.conversation_id.clone(),
                    turn_id: "turn-test".into(),
                })
                .await;
            }
            Ok(crate::ports::AgentTurnOutcome {
                conversation_id: request.conversation_id,
                turn_id: "turn-test".into(),
                status: crate::ports::AgentTurnStatus::Completed,
                runtime: None,
            })
        }
    }

    fn noop_turn_port() -> Arc<dyn crate::ports::AgentTurnExecutionPort> {
        Arc::new(NoopTurnPort)
    }

    struct NoopCancellationPort;

    #[async_trait::async_trait]
    impl crate::ports::AgentTurnCancellationPort for NoopCancellationPort {
        async fn cancel_agent_turn(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _turn_id: &str,
        ) -> Result<(), crate::ports::AgentTurnExecutionError> {
            Ok(())
        }
    }

    fn noop_cancellation_port() -> Arc<dyn crate::ports::AgentTurnCancellationPort> {
        Arc::new(NoopCancellationPort)
    }

    #[derive(Default)]
    struct NoopProjectionStore;

    #[async_trait::async_trait]
    impl TeamProjectionMessageStore for NoopProjectionStore {
        fn mint_message_id(&self) -> String {
            "msg-test".into()
        }

        async fn find_projected_message(
            &self,
            _conversation_id: &str,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<aionui_db::models::MessageRow>, TeamError> {
            Ok(None)
        }

        async fn insert_projected_message(&self, _row: &aionui_db::models::MessageRow) -> Result<(), TeamError> {
            Ok(())
        }
    }

    fn noop_projection_store() -> Arc<dyn TeamProjectionMessageStore> {
        Arc::new(NoopProjectionStore)
    }

    /// RecordingBroadcaster used by the D29d-1 ratification test below to
    /// assert that `team.agentSpawned` is *not* emitted on failed spawns.
    #[derive(Default)]
    struct RecordingBroadcaster {
        events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
    }

    impl RecordingBroadcaster {
        fn new() -> Self {
            Self::default()
        }

        fn names(&self) -> Vec<String> {
            self.events.lock().unwrap().iter().map(|e| e.name.clone()).collect()
        }
    }

    impl EventBroadcaster for RecordingBroadcaster {
        fn broadcast(&self, msg: WebSocketMessage<serde_json::Value>) {
            self.events.lock().unwrap().push(msg);
        }
    }

    fn backend_path() -> Arc<PathBuf> {
        Arc::new(PathBuf::from("/tmp/aioncore-test"))
    }

    /// In-memory stub for [`IWorkerTaskManager`]. Only `get_task` is
    /// exercised by D7b; the other methods are unreachable in these tests
    /// and panic to surface drift early.
    struct StubTaskManager {
        tasks: Mutex<std::collections::HashMap<String, AgentInstance>>,
        kill_calls: Mutex<Vec<(String, Option<AgentKillReason>)>>,
        kill_error: Option<String>,
    }

    impl StubTaskManager {
        fn new() -> Self {
            Self {
                tasks: Mutex::new(std::collections::HashMap::new()),
                kill_calls: Mutex::new(Vec::new()),
                kill_error: None,
            }
        }

        /// Build a stub whose `kill` always fails with `AgentError::NotFound` so
        /// tests can exercise the non-fatal kill branch in `remove_agent`.
        fn with_kill_error(msg: &str) -> Self {
            Self {
                tasks: Mutex::new(std::collections::HashMap::new()),
                kill_calls: Mutex::new(Vec::new()),
                kill_error: Some(msg.to_owned()),
            }
        }

        fn kill_calls(&self) -> Vec<(String, Option<AgentKillReason>)> {
            self.kill_calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl IWorkerTaskManager for StubTaskManager {
        fn get_task(&self, conversation_id: &str) -> Option<AgentInstance> {
            self.tasks.lock().unwrap().get(conversation_id).cloned()
        }
        async fn get_or_build_task(
            &self,
            _conversation_id: &str,
            _options: BuildTaskOptions,
        ) -> Result<AgentInstance, AgentError> {
            panic!("get_or_build_task should not be called in D7b tests")
        }
        fn kill(&self, conversation_id: &str, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
            self.kill_calls
                .lock()
                .unwrap()
                .push((conversation_id.to_owned(), reason));
            if let Some(msg) = &self.kill_error {
                return Err(AgentError::not_found(msg.clone()));
            }
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
            self.tasks.lock().unwrap().len()
        }
        fn collect_idle(&self, _idle_threshold_ms: TimestampMs) -> Vec<String> {
            Vec::new()
        }
    }

    /// Empty task_manager — `get_task` returns `None` for every conversation.
    fn empty_task_manager() -> Arc<dyn IWorkerTaskManager> {
        Arc::new(StubTaskManager::new())
    }

    fn make_team() -> Team {
        Team {
            id: "t1".into(),
            name: "Test Team".into(),
            workspace: "/tmp/test-team".into(),
            agents: vec![
                TeamAgent {
                    slot_id: "lead-1".into(),
                    name: "Lead".into(),
                    role: TeammateRole::Lead,
                    conversation_id: "c1".into(),
                    backend: "acp".into(),
                    model: "claude".into(),
                    assistant_id: None,
                    status: None,
                    conversation_type: None,
                    cli_path: None,
                },
                TeamAgent {
                    slot_id: "worker-1".into(),
                    name: "Worker".into(),
                    role: TeammateRole::Teammate,
                    conversation_id: "c2".into(),
                    backend: "acp".into(),
                    model: "claude".into(),
                    assistant_id: None,
                    status: None,
                    conversation_type: None,
                    cli_path: None,
                },
            ],
            lead_agent_id: Some("lead-1".into()),
            created_at: 1000,
            updated_at: 1000,
        }
    }

    async fn start_session() -> TeamSession {
        start_session_with_prompt_dump(crate::prompt_dump::TeamPromptDumpConfig::disabled()).await
    }

    async fn start_session_with_prompt_dump(prompt_dump: crate::prompt_dump::TeamPromptDumpConfig) -> TeamSession {
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        TeamSession::start_with_prompt_dump(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
            prompt_dump,
        )
        .await
        .unwrap()
    }

    async fn start_session_arc() -> Arc<TeamSession> {
        Arc::new(start_session().await)
    }

    fn register_test_event_loop(session: &Arc<TeamSession>, slot_id: &str) {
        session
            .event_loops()
            .spawn(slot_id, test_loop_context(session, slot_id))
            .expect("test event loop registration");
    }

    fn test_loop_context(session: &Arc<TeamSession>, slot_id: &str) -> AgentLoopContext {
        AgentLoopContext {
            team_id: session.team_id().to_owned(),
            slot_id: slot_id.to_owned(),
            user_id: session.user_id().to_owned(),
            session: session.clone(),
            scheduler: session.scheduler().clone(),
            mailbox: session.mailbox().clone(),
            turn_port: session.turn_port().clone(),
            registry: session.event_loops().clone(),
        }
    }

    #[tokio::test]
    async fn event_loop_registry_rejects_duplicate_and_stopped_registration() {
        let session = Arc::new(start_session().await);
        register_test_event_loop(&session, "worker-1");

        assert_eq!(
            session
                .event_loops()
                .spawn("worker-1", test_loop_context(&session, "worker-1")),
            Err(crate::event_loop::EventLoopRegistrationError::Duplicate)
        );

        session.stop();
        assert_eq!(
            session
                .event_loops()
                .spawn("lead-1", test_loop_context(&session, "lead-1")),
            Err(crate::event_loop::EventLoopRegistrationError::Stopped)
        );
    }

    #[tokio::test]
    async fn start_and_stop() {
        let session = start_session().await;
        assert_eq!(session.team_id(), "t1");
        assert!(session.mcp_server.port() > 0);
        session.stop();
    }

    #[tokio::test]
    async fn mcp_stdio_config_for_agent() {
        let session = start_session().await;
        let config = session.mcp_stdio_config("lead-1");
        assert_eq!(config.team_id, "t1");
        assert_eq!(config.slot_id, "lead-1");
        assert_eq!(config.port, session.mcp_server.port());
        session.stop();
    }

    #[tokio::test]
    async fn stdio_spec_uses_fixed_name_and_binary_path() {
        let session = start_session().await;
        let spec = session.stdio_spec("lead-1");
        assert_eq!(spec.name, crate::mcp::TEAM_MCP_SERVER_NAME);
        assert_eq!(spec.command, "/tmp/aioncore-test");
        assert_eq!(spec.args, vec!["mcp-bridge".to_string()]);
        assert!(spec.env.iter().any(|(k, v)| k == "TEAM_AGENT_SLOT_ID" && v == "lead-1"));
        session.stop();
    }

    #[tokio::test]
    async fn send_message_writes_to_lead_mailbox() {
        let repo = Arc::new(MockTeamRepo::new());
        let repo_dyn: Arc<dyn ITeamRepository> = repo.clone();
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let session = TeamSession::start(
            make_team(),
            repo_dyn,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();
        session.send_message("Hello team", None).await.unwrap();

        let state = repo.state.lock().unwrap();
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].to_agent_id, "lead-1");
        assert_eq!(state.messages[0].from_agent_id, "user");
        assert_eq!(state.messages[0].content, "Hello team");
        session.stop();
    }

    #[tokio::test]
    async fn send_message_to_agent_writes_to_mailbox() {
        let repo = Arc::new(MockTeamRepo::new());
        let repo_dyn: Arc<dyn ITeamRepository> = repo.clone();
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let session = TeamSession::start(
            make_team(),
            repo_dyn,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();
        session
            .send_message_to_agent("worker-1", "Do this task", None)
            .await
            .unwrap();

        let state = repo.state.lock().unwrap();
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].to_agent_id, "worker-1");
        assert_eq!(state.messages[0].content, "Do this task");
        session.stop();
    }

    #[tokio::test]
    async fn send_message_to_unknown_agent_returns_error() {
        let session = start_session().await;
        let result = session.send_message_to_agent("nonexistent", "Hello", None).await;
        assert!(result.is_err());
        session.stop();
    }

    #[tokio::test]
    async fn shutdown_without_active_run_queues_background_work() {
        let session = start_session().await;

        session
            .shutdown_agent("lead-1", "worker-1", Some("done".into()))
            .await
            .expect("background caller may queue shutdown work");

        let unread = session.mailbox().peek_unread("t1", "worker-1").await.unwrap();
        assert_eq!(unread.len(), 1);
        assert!(session.team_run_manager().current_active_run_id().is_none());
        session.stop();
    }

    #[tokio::test]
    async fn add_and_remove_agent() {
        let session = start_session().await;

        let new_agent = TeamAgent {
            slot_id: "new-1".into(),
            name: "NewAgent".into(),
            role: TeammateRole::Teammate,
            conversation_id: "c3".into(),
            backend: "acp".into(),
            model: "claude".into(),
            assistant_id: None,
            status: None,
            conversation_type: None,
            cli_path: None,
        };
        session.add_agent(&new_agent).await;

        let agents = session.scheduler.list_agents().await;
        assert_eq!(agents.len(), 3);

        session.remove_agent("new-1").await.unwrap();
        let agents = session.scheduler.list_agents().await;
        assert_eq!(agents.len(), 2);

        session.stop();
    }

    // -- W5-D30d-1: remove_agent kills the agent process ---------------------

    #[tokio::test]
    async fn remove_agent_calls_task_manager_kill() {
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let stub = Arc::new(StubTaskManager::new());
        let stub_dyn: Arc<dyn IWorkerTaskManager> = stub.clone();
        let session = TeamSession::start(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            stub_dyn,
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();

        session.remove_agent("worker-1").await.unwrap();

        let calls = stub.kill_calls();
        assert_eq!(calls.len(), 1, "kill invoked exactly once");
        assert_eq!(calls[0].0, "c2", "kill targets removed slot's conversation_id");
        assert!(
            matches!(calls[0].1, Some(AgentKillReason::TeamDeleted)),
            "kill reason carries AgentKillReason"
        );
        session.stop();
    }

    #[tokio::test]
    async fn remove_agent_is_non_fatal_when_kill_fails() {
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let stub = Arc::new(StubTaskManager::with_kill_error("task not found"));
        let stub_dyn: Arc<dyn IWorkerTaskManager> = stub.clone();
        let session = TeamSession::start(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            stub_dyn,
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();

        // kill returns Err(AgentError::NotFound) but remove_agent must still
        // succeed — NotFound means the worker already died, which is OK.
        session.remove_agent("worker-1").await.unwrap();

        let agents = session.scheduler.list_agents().await;
        assert_eq!(agents.len(), 1, "slot still removed even after kill failure");
        assert_eq!(stub.kill_calls().len(), 1);
        session.stop();
    }

    #[tokio::test]
    async fn rename_agent_in_session() {
        let session = start_session().await;
        session.rename_agent("worker-1", "Senior Worker").await.unwrap();

        let agent = session.scheduler.get_agent("worker-1").await.unwrap();
        assert_eq!(agent.name, "Senior Worker");

        session.stop();
    }

    #[tokio::test]
    async fn rename_unknown_agent_returns_error() {
        let session = start_session().await;
        let result = session.rename_agent("nonexistent", "X").await;
        assert!(result.is_err());
        session.stop();
    }

    #[tokio::test]
    async fn rename_agent_rejects_duplicate_in_session() {
        let session = start_session().await;
        let agents = session.scheduler.list_agents().await;
        let lead_name = agents.iter().find(|a| a.slot_id == "lead-1").unwrap().name.clone();

        // Rename worker-1 to the lead's name — should collide.
        let result = session.rename_agent("worker-1", &lead_name).await;
        assert!(result.is_err());

        session.stop();
    }

    // -- spawn_agent helpers + guard tests -----------------------------------

    fn sample_spawn_req() -> SpawnAgentRequest {
        SpawnAgentRequest {
            name: "Helper".into(),
            assistant_id: Some("word-creator".into()),
        }
    }

    #[tokio::test]
    async fn spawn_agent_rejects_unknown_caller() {
        let session = start_session().await;
        let result = session.spawn_agent("nonexistent", sample_spawn_req()).await;
        assert!(
            matches!(&result, Err(TeamError::AgentNotFound(_))),
            "unknown caller must surface AgentNotFound, got {result:?}"
        );
        session.stop();
    }

    // -- D7a new method tests ------------------------------------------------

    #[tokio::test]
    async fn recovery_scan_creates_one_wake_per_slot_with_non_self_unread() {
        let session = start_session_arc().await;
        let lead = session.scheduler().find_lead_slot_id().await.expect("lead");
        let worker = session
            .scheduler()
            .list_agents()
            .await
            .into_iter()
            .find(|agent| agent.slot_id != lead)
            .expect("worker")
            .slot_id;
        register_test_event_loop(&session, &lead);
        register_test_event_loop(&session, &worker);

        session
            .mailbox()
            .write(
                session.team_id(),
                &lead,
                "worker-1",
                MailboxMessageType::Message,
                "m1",
                None,
            )
            .await
            .expect("lead mailbox write 1");
        session
            .mailbox()
            .write(
                session.team_id(),
                &lead,
                "worker-2",
                MailboxMessageType::Message,
                "m2",
                None,
            )
            .await
            .expect("lead mailbox write 2");
        session
            .mailbox()
            .write(
                session.team_id(),
                &worker,
                &worker,
                MailboxMessageType::Message,
                "self",
                None,
            )
            .await
            .expect("self mailbox write");

        let result = session
            .try_start_recovery_drain("test_restore")
            .await
            .expect("scan should not fail");

        assert_eq!(result, vec![lead.clone()]);
        assert!(session.team_run_manager().current_active_run_id().is_none());
    }

    #[tokio::test]
    async fn recovery_scan_is_one_shot_per_session_lifecycle() {
        let session = start_session_arc().await;
        let lead = session.scheduler().find_lead_slot_id().await.expect("lead");
        register_test_event_loop(&session, &lead);

        let first = session
            .try_start_recovery_drain("test_restore_no_work")
            .await
            .expect("first scan");
        assert!(first.is_empty(), "empty first scan must not create recovery work");

        session
            .mailbox()
            .write(
                session.team_id(),
                &lead,
                "worker-1",
                MailboxMessageType::Message,
                "late",
                None,
            )
            .await
            .expect("late mailbox write");

        let second = session
            .try_start_recovery_drain("test_restore_second")
            .await
            .expect("second scan should not fail");

        assert!(
            second.is_empty(),
            "ordinary new work must not create a second recovery scan"
        );
    }

    #[tokio::test]
    async fn on_agent_finish_marks_idle_and_returns_lead_when_all_settled() {
        let session = start_session().await;

        // Worker is Working; on finish → mark idle → since the lead is the
        // only remaining non-idle member (actually also idle), all-idle
        // check returns the lead slot_id.
        session
            .scheduler
            .set_status("worker-1", TeammateStatus::Working)
            .await
            .unwrap();

        let result = session.on_agent_finish("c2", false).await.unwrap();
        assert_eq!(result.as_deref(), Some("lead-1"));

        let status = session.scheduler.get_status("worker-1").await.unwrap();
        assert_eq!(status, TeammateStatus::Idle);
        session.stop();
    }

    #[tokio::test]
    async fn on_agent_finish_lead_returns_none() {
        let session = start_session().await;
        session
            .scheduler
            .set_status("lead-1", TeammateStatus::Working)
            .await
            .unwrap();

        let result = session.on_agent_finish("c1", false).await.unwrap();
        assert!(result.is_none());
        session.stop();
    }

    #[tokio::test]
    async fn on_agent_finish_unknown_conversation_returns_error() {
        let session = start_session().await;
        let result = session.on_agent_finish("nope", false).await;
        assert!(result.is_err());
        session.stop();
    }

    // -- D7b wake-path tests -------------------------------------------------

    async fn start_session_with(task_manager: Arc<dyn IWorkerTaskManager>) -> (TeamSession, Arc<MockTeamRepo>) {
        let repo = Arc::new(MockTeamRepo::new());
        let repo_dyn: Arc<dyn ITeamRepository> = repo.clone();
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let session = TeamSession::start(
            make_team(),
            repo_dyn,
            broadcaster,
            backend_path(),
            task_manager,
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();
        (session, repo)
    }

    #[tokio::test]
    async fn send_message_persists_files_in_mailbox_without_inline_wake() {
        let (session, _repo) = start_session_with(empty_task_manager()).await;

        session
            .send_message("Hello", Some(vec!["/tmp/a.txt".into(), "/tmp/b.txt".into()]))
            .await
            .unwrap();

        let unread = session.mailbox.peek_unread("t1", "lead-1").await.unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(
            unread[0].files.as_deref(),
            Some(&["/tmp/a.txt".into(), "/tmp/b.txt".into()][..])
        );
        assert_eq!(unread[0].content, "Hello");
        session.stop();
    }

    #[tokio::test]
    async fn rejected_commit_releases_lease_and_settles_the_persisted_message() {
        let (session, _repo) = start_session_with(empty_task_manager()).await;
        session
            .work_coordinator
            .set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
        let lease = session
            .work_coordinator
            .acquire_enqueue(EnqueueRequest {
                slot_id: "lead-1".into(),
                role: TeamRunTargetRole::Lead,
                source: WorkSource::UserMessage,
                binding: CausalBinding::UserVisible,
            })
            .unwrap();
        let message = session
            .mailbox
            .write(
                "t1",
                "lead-1",
                "user",
                MailboxMessageType::Message,
                "persisted before removal",
                None,
            )
            .await
            .unwrap();
        session.work_coordinator.remove_slot("lead-1");

        let error = session
            .commit_persisted_enqueue(&lease, message.id.clone())
            .await
            .expect_err("removed slot must reject the commit");

        assert!(matches!(error, TeamError::InvalidRequest(_)));
        assert!(session.team_run_manager.current_active_run_id().is_none());
        assert!(session.mailbox.peek_unread("t1", "lead-1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn prepare_next_batch_requeues_claim_when_task_snapshot_fails() {
        let (session, repo) = start_session_with(empty_task_manager()).await;
        let session = Arc::new(session);
        session.send_message("retry after task read", None).await.unwrap();
        repo.state.lock().unwrap().fail_task_lists = true;
        register_test_event_loop(&session, "lead-1");

        let error = match session.prepare_next_batch("lead-1").await {
            Err(error) => error,
            Ok(_) => panic!("task snapshot failure must be returned"),
        };

        assert!(matches!(error, TeamError::Database(_)));
        let slot = session.work_coordinator.slot_snapshot("lead-1").unwrap();
        assert_eq!(slot.state, SlotPhase::Queued);
        assert!(slot.active_batch.is_none());
        assert_eq!(slot.queued_foreground_count, 1);
        session.stop();
    }

    #[tokio::test]
    async fn background_turn_is_reported_running_without_a_team_run() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let session = TeamSession::start(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            Arc::new(BlockingRunningTurnPort::new(started_tx, release_rx)),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();
        let session = Arc::new(session);
        register_test_event_loop(&session, "worker-1");

        session
            .shutdown_agent("lead-1", "worker-1", Some("background control".into()))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("background turn should start")
            .expect("start signal should be sent");

        let slot = session.work_coordinator.slot_snapshot("worker-1").unwrap();
        assert_eq!(slot.state, SlotPhase::Running);
        assert_eq!(slot.active_turn_id.as_deref(), Some("turn-background"));
        assert!(session.team_run_manager.current_active_run_id().is_none());

        release_tx.send(()).unwrap();
        session.stop();
    }

    #[tokio::test]
    async fn busy_lead_user_message_is_persisted_and_acknowledged_as_queued() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let repo = Arc::new(MockTeamRepo::new());
        let repo_dyn: Arc<dyn ITeamRepository> = repo.clone();
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let session = TeamSession::start(
            make_team(),
            repo_dyn,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            Arc::new(BlockingRunningTurnPort::new(started_tx, release_rx)),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();
        let session = Arc::new(session);
        register_test_event_loop(&session, "lead-1");

        let first = session.send_message("first", None).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("first turn should start")
            .expect("start signal should be sent");
        let queued = session.send_message("queued", None).await.unwrap();

        assert_eq!(queued.enqueue_status, TeamMessageEnqueueStatus::Queued);
        assert_eq!(queued.run.team_run_id, first.run.team_run_id);
        assert!(
            session
                .mailbox
                .peek_unread("t1", "lead-1")
                .await
                .unwrap()
                .iter()
                .any(|message| message.id == queued.message_id)
        );
        release_tx.send(()).unwrap();
        session.stop();
    }

    #[tokio::test]
    async fn mailbox_failure_aborts_enqueue_lease_and_allows_run_completion() {
        let (session, repo) = start_session_with(empty_task_manager()).await;
        repo.state.lock().unwrap().fail_message_writes = true;

        let error = session.send_message("will fail", None).await.unwrap_err();

        assert!(error.to_string().contains("forced mailbox write failure"));
        assert!(session.team_run_manager.current_active_run_id().is_none());
        assert!(session.work_coordinator.snapshot().active_run_summary.is_none());
        session.stop();
    }

    #[tokio::test]
    async fn message_to_runtime_starting_member_returns_blocked_runtime_starting() {
        let session = start_session().await;

        let ack = session
            .send_message_to_agent("worker-1", "wait for runtime", None)
            .await
            .unwrap();

        assert_eq!(ack.enqueue_status, TeamMessageEnqueueStatus::BlockedRuntimeStarting);
        assert_eq!(
            ack.run.slot_work[0].blocked_reason,
            Some(aionui_api_types::TeamSlotBlockedReason::RuntimeStarting)
        );
        session.stop();
    }

    #[tokio::test]
    async fn runtime_failed_removing_and_stopped_members_reject_before_mailbox_write() {
        let (failed_session, failed_repo) = start_session_with(empty_task_manager()).await;
        let ReserveAttach::Start(failed_lease) = failed_session.member_runtimes.reserve_attach("lead-1", false) else {
            panic!("failed runtime setup must reserve attach");
        };
        assert!(failed_session.member_runtimes.commit_failed(
            &failed_lease,
            MemberRuntimeFailure {
                classification: "test_runtime_failed",
                public_reason: "test failure".into(),
            },
        ));
        let failed_error = failed_session.send_message("rejected", None).await.unwrap_err();
        assert!(failed_error.to_string().contains("runtime failed"));
        assert!(failed_repo.state.lock().unwrap().messages.is_empty());
        failed_session.stop();

        let (removing_session, removing_repo) = start_session_with(empty_task_manager()).await;
        let ReserveAttach::Start(removing_lease) = removing_session.member_runtimes.reserve_attach("lead-1", false)
        else {
            panic!("removing runtime setup must reserve attach");
        };
        assert!(removing_session.member_runtimes.commit_ready(&removing_lease));
        assert!(matches!(
            removing_session.member_runtimes.begin_remove("lead-1"),
            BeginRemove::Start(_)
        ));
        let removing_error = removing_session.send_message("rejected", None).await.unwrap_err();
        assert!(removing_error.to_string().contains("being removed"));
        assert!(removing_repo.state.lock().unwrap().messages.is_empty());
        removing_session.stop();

        let (stopped_session, stopped_repo) = start_session_with(empty_task_manager()).await;
        stopped_session.member_runtimes.stop();
        let stopped_error = stopped_session.send_message("rejected", None).await.unwrap_err();
        assert!(stopped_error.to_string().contains("session stopped"));
        assert!(stopped_repo.state.lock().unwrap().messages.is_empty());
        stopped_session.stop();
    }

    #[test]
    fn session_replacement_rejects_old_generation_batch_and_attach_completion() {
        let old_broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let old_emitter = Arc::new(TeamEventEmitter::new("t1".into(), old_broadcaster));
        let old_runs = Arc::new(TeamRunManager::new("t1".into(), old_emitter));
        let old_coordinator = SlotWorkCoordinator::new("t1".into(), "old-generation".into(), old_runs);
        old_coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
        let lease = old_coordinator
            .acquire_enqueue(EnqueueRequest {
                slot_id: "lead-1".into(),
                role: TeamRunTargetRole::Lead,
                source: WorkSource::RecoveryDrain,
                binding: CausalBinding::Background,
            })
            .unwrap();
        old_coordinator.commit_enqueue(&lease, Some("m-old".into())).unwrap();
        let ReconcileDecision::Claim(old_batch) = old_coordinator.next("lead-1") else {
            panic!("old generation batch must be claimed");
        };

        let new_broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        let new_emitter = Arc::new(TeamEventEmitter::new("t1".into(), new_broadcaster));
        let new_runs = Arc::new(TeamRunManager::new("t1".into(), new_emitter));
        let new_coordinator = SlotWorkCoordinator::new("t1".into(), "new-generation".into(), new_runs);
        new_coordinator.set_runtime_constraint("lead-1", RuntimeConstraint::Ready);
        assert_eq!(
            new_coordinator.mark_started(&old_batch, "turn-old"),
            crate::work_coordinator::StartCommitResult::StaleOwner
        );
        assert!(new_coordinator.slot_snapshot("lead-1").unwrap().active_batch.is_none());

        let old_registry = MemberRuntimeRegistry::new("old-generation");
        let ReserveAttach::Start(old_attach) = old_registry.reserve_attach("lead-1", false) else {
            panic!("old attach must be reserved");
        };
        let new_registry = MemberRuntimeRegistry::new("new-generation");
        assert!(!new_registry.commit_ready(&old_attach));
        assert_eq!(new_registry.snapshot("lead-1"), MemberRuntimeSnapshot::Absent);
    }

    #[tokio::test]
    async fn send_message_without_active_task_does_not_error() {
        // Empty task_manager → get_task returns None → log-not-throw: the
        // mailbox write must still succeed and the call must return Ok.
        let (session, repo) = start_session_with(empty_task_manager()).await;

        session
            .send_message("queued", None)
            .await
            .expect("send_message must return Ok even when no task is active");

        let state = repo.state.lock().unwrap();
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].content, "queued");
        session.stop();
    }

    #[tokio::test]
    async fn send_message_without_event_loop_retains_mailbox_message() {
        let (session, repo) = start_session_with(empty_task_manager()).await;

        session
            .send_message("payload", None)
            .await
            .expect("send_message must persist mailbox row without inline wake");

        let state = repo.state.lock().unwrap();
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].content, "payload");
        session.stop();
    }

    #[tokio::test]
    async fn send_message_to_agent_persists_files_for_target_mailbox() {
        let (session, _repo) = start_session_with(empty_task_manager()).await;

        session
            .send_message_to_agent("worker-1", "do X", Some(vec!["/tmp/x.md".into()]))
            .await
            .unwrap();

        let unread = session.mailbox.peek_unread("t1", "worker-1").await.unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].files.as_deref(), Some(&["/tmp/x.md".into()][..]));
        assert_eq!(unread[0].content, "do X");
        session.stop();
    }

    #[tokio::test]
    async fn send_message_with_empty_content_still_persists_mailbox_row() {
        let (session, _repo) = start_session_with(empty_task_manager()).await;

        session.send_message("", None).await.unwrap();

        let unread = session.mailbox.peek_unread("t1", "lead-1").await.unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].content, "");
        session.stop();
    }

    async fn start_session_with_lead_backend(backend: &str) -> TeamSession {
        let mut team = make_team();
        team.agents[0].backend = backend.to_string();
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        TeamSession::start(
            team,
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap()
    }

    async fn start_session_with_recorder(backend: &str) -> (TeamSession, Arc<RecordingBroadcaster>) {
        let mut team = make_team();
        team.agents[0].backend = backend.to_string();
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let recorder = Arc::new(RecordingBroadcaster::new());
        let broadcaster: Arc<dyn EventBroadcaster> = recorder.clone();
        let session = TeamSession::start(
            team,
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap();
        (session, recorder)
    }

    fn spawn_req(assistant_id: Option<&str>) -> SpawnAgentRequest {
        SpawnAgentRequest {
            name: "Helper".into(),
            assistant_id: assistant_id.map(str::to_owned),
        }
    }

    /// After all guards pass, the unit-test sessions have a null `service`
    /// Weak — so the spawn path must bail with InvalidRequest instead of
    /// panicking. This is the "validation passed, DB step not reachable"
    /// shape exercised below.
    fn assert_reached_db_step(err: TeamError) {
        match err {
            TeamError::InvalidRequest(msg) if msg.contains("live TeamSessionService") => {}
            other => panic!("expected service-unavailable error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn spawn_agent_requires_assistant_identity() {
        let session = start_session_with_lead_backend("claude").await;
        let err = session
            .spawn_agent("lead-1", spawn_req(None))
            .await
            .expect_err("assistant_id must be required");
        assert!(
            matches!(&err, TeamError::InvalidRequest(msg) if msg.contains("assistant_id is required")),
            "expected InvalidRequest about missing assistant_id, got {err:?}"
        );
        session.stop();
    }

    #[tokio::test]
    async fn spawn_agent_rejects_non_lead_caller() {
        let session = start_session_with_lead_backend("claude").await;
        let err = session
            .spawn_agent("worker-1", spawn_req(Some("word-creator")))
            .await
            .expect_err("non-lead caller must be rejected");
        assert!(
            matches!(&err, TeamError::LeaderOnly(what) if what == "spawn_agent"),
            "expected LeaderOnly(\"spawn_agent\"), got {err:?}"
        );
        session.stop();
    }

    #[tokio::test]
    async fn spawn_agent_rejects_duplicate_name() {
        let session = start_session_with_lead_backend("claude").await;
        // The seeded team already has an agent named "Worker". Case + trim
        // normalization means "  worker " collides.
        let mut req = spawn_req(Some("word-creator"));
        req.name = "  worker ".into();
        let err = session
            .spawn_agent("lead-1", req)
            .await
            .expect_err("duplicate name must be rejected");
        assert!(
            matches!(&err, TeamError::DuplicateAgentName(_)),
            "expected DuplicateAgentName, got {err:?}"
        );
        session.stop();
    }

    #[tokio::test]
    async fn spawn_agent_rejects_empty_name() {
        let session = start_session_with_lead_backend("claude").await;
        let mut req = spawn_req(Some("word-creator"));
        req.name = "   ".into();
        let err = session
            .spawn_agent("lead-1", req)
            .await
            .expect_err("empty name must be rejected");
        assert!(
            matches!(&err, TeamError::InvalidRequest(msg) if msg.contains("empty")),
            "expected InvalidRequest about empty name, got {err:?}"
        );
        session.stop();
    }

    // -- W5-D29d-1 ratification: spawn emit-order contract ------------------
    //
    // The success-path emission of `team.agentSpawned` is exercised by
    // `scheduler::tests::add_agent_broadcasts_spawned_event` — `spawn_agent`
    // reaches that emission via `scheduler.add_agent(&new_agent)` after
    // `persist_spawned_agent` returns. This ratification test locks the
    // *ordering* half of the contract: the event must NOT be published
    // before the DB step succeeds. If a future refactor hoists broadcast
    // above the persist/add_agent boundary (so the frontend sees a spawned
    // agent that never persisted), this test regresses.

    #[tokio::test]
    async fn spawn_agent_does_not_emit_before_db_step() {
        let (session, recorder) = start_session_with_recorder("claude").await;
        let err = session
            .spawn_agent("lead-1", spawn_req(Some("word-creator")))
            .await
            .expect_err("unit test has no service wire; spawn stops at DB step");
        assert_reached_db_step(err);
        assert!(
            !recorder.names().iter().any(|n| n == "team.agentSpawned"),
            "team.agentSpawned must not be emitted when spawn fails before add_agent; saw {:?}",
            recorder.names()
        );
        session.stop();
    }

    #[tokio::test]
    async fn spawn_agent_does_not_emit_on_guard_rejection() {
        let (session, recorder) = start_session_with_recorder("claude").await;
        let err = session
            .spawn_agent("worker-1", spawn_req(Some("word-creator")))
            .await
            .expect_err("non-lead caller must be rejected");
        assert!(matches!(&err, TeamError::LeaderOnly(what) if what == "spawn_agent"));
        assert!(
            !recorder.names().iter().any(|n| n == "team.agentSpawned"),
            "team.agentSpawned must not be emitted when guard rejects the caller; saw {:?}",
            recorder.names()
        );
        session.stop();
    }
}
