use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    TeamRunAckResponse, TeamRunTargetRole, TeamSendMessageDelivery, TeamSendMessageReason,
    TeamSendMessageTargetQueueState, TeamSlotRuntimeHealth, TeamSlotWorkPayload,
};
use aionui_common::{AgentKillReason, generate_id};
use aionui_db::ITeamRepository;
use aionui_realtime::EventBroadcaster;
use tracing::{info, warn};

use crate::error::TeamError;
use crate::event_loop::EventLoopRegistry;
use crate::events::TeamEventEmitter;
use crate::mailbox::Mailbox;
use crate::mcp::{TeamMcpServer, TeamMcpStdioConfig, TeamMcpStdioServerSpec};
use crate::message_projection::{
    TeamMessageProjection, TeamProjectionMessageStore, TeamProjectionRequest, TeamProjectionSource, teammate_dedupe_key,
};
use crate::ports::{AgentTurnCancellationPort, AgentTurnExecutionPort};
use crate::prompts::{build_lead_prompt, build_teammate_prompt, build_wake_payload};
use crate::provisioning::PersistSpawnedAgentRequest;
use crate::scheduler::{TeammateManager, normalize_name};
use crate::service::TeamSessionService;
use crate::task_board::TaskBoard;
use crate::team_run::{
    ChildCancelTarget, RecoveryBacklogResult, RecoveryWakeCandidate, TeamRunManager, TeamRunWakeAcquireOutcome,
    WakeRecordDecision, target_role_for,
};
use crate::types::{MailboxMessageType, Team, TeamAgent, TeammateRole, TeammateStatus};
use crate::wake::TeamWakeSource;

/// Input for the wake path. Produced by [`TeamSession::compute_wake_input`],
/// consumed by D7b's `send_message` / `send_message_to_agent` (not implemented
/// in D7a). `first_message` includes the role prompt on cold starts.
#[derive(Debug, Clone)]
pub struct WakeInput {
    pub team_run_id: Option<String>,
    pub conversation_id: String,
    pub first_message: String,
    /// `false` when the mailbox is empty — caller should skip wake and
    /// leave the agent idle.
    pub should_send: bool,
    /// Unread mailbox rows used to build `first_message`. Returned so the
    /// caller can mirror non-user senders into the target agent's conversation
    /// as left bubbles (matches AionUi `TeammateManager.wake()`). These are
    /// **not** yet marked as read — the caller must call
    /// `mailbox.mark_read_batch` after successful delivery.
    pub unread: Vec<crate::types::MailboxMessage>,
    /// Role of the wake target.
    pub agent_role: TeammateRole,
    pub(crate) wake_source: Option<TeamWakeSource>,
    pub(crate) trigger_message_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessageQueueResult {
    pub team_run_id: String,
    pub delivery: TeamSendMessageDelivery,
    pub target: TeamSendMessageTargetQueueState,
}

/// Input for [`TeamSession::spawn_agent`]. Populated by the lead agent when
/// it calls the `spawn_agent` MCP tool.
#[derive(Debug, Clone)]
pub struct SpawnAgentRequest {
    pub name: String,
    pub assistant_id: Option<String>,
    pub model: Option<String>,
}

enum SpawnWakePlan {
    RunScoped(TeamRunTargetRole),
    MailboxOnly,
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
        let mailbox = Arc::new(Mailbox::new(repo.clone()));
        let task_board = Arc::new(TaskBoard::new(repo));
        let team_run_manager = Arc::new(TeamRunManager::new(
            team.id.clone(),
            Arc::new(TeamEventEmitter::new(team.id.clone(), broadcaster.clone())),
        ));

        let scheduler = Arc::new(TeammateManager::new(
            team.id.clone(),
            &team.agents,
            mailbox.clone(),
            task_board.clone(),
            broadcaster.clone(),
        ));

        let auth_token = aionui_common::generate_id();
        let mcp_server = TeamMcpServer::start(
            auth_token,
            scheduler.clone(),
            team.id.clone(),
            broadcaster.clone(),
            service.clone(),
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
            user_id,
            service,
            broadcaster,
            event_loops,
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

    pub fn turn_port(&self) -> &Arc<dyn AgentTurnExecutionPort> {
        &self.turn_port
    }

    pub fn cancellation_port(&self) -> &Arc<dyn AgentTurnCancellationPort> {
        &self.cancellation_port
    }

    pub fn team_run_manager(&self) -> &Arc<TeamRunManager> {
        &self.team_run_manager
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

    /// Assemble the payload that will drive the next wake of `slot_id`.
    ///
    /// - Reads status, unread messages and tasks.
    /// - Cold-start agents (no prior status, or last status was `Error`)
    ///   receive the full role prompt prepended to the wake payload.
    /// - When the mailbox is empty, returns `WakeInput { should_send: false, .. }`
    ///   so the caller can skip the wake and mark the agent idle.
    /// - Filters out messages where `from_agent_id == slot_id` (prevent self-trigger).
    ///
    /// Messages are **not** marked as read here. The caller is responsible for
    /// calling `mailbox.mark_read_batch` after successful delivery.
    pub async fn compute_wake_input(&self, slot_id: &str) -> Result<Option<WakeInput>, TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        let all_unread = self.mailbox.peek_unread(&self.team.id, slot_id).await?;
        let next_wake = self.team_run_manager.peek_next_pending_wake(slot_id).await;
        // Filter out self-messages to prevent an agent from triggering itself.
        // User interventions consume only the triggering foreground message in
        // this turn, leaving background backlog unread for follow-up drain.
        let unread: Vec<_> = if matches!(
            next_wake.as_ref().map(|wake| wake.source),
            Some(TeamWakeSource::UserIntervention)
        ) {
            match next_wake.as_ref().and_then(|wake| wake.message_id.as_ref()) {
                Some(message_id) => all_unread
                    .into_iter()
                    .filter(|m| m.id == *message_id && m.from_agent_id != slot_id)
                    .collect(),
                None => all_unread.into_iter().filter(|m| m.from_agent_id == "user").collect(),
            }
        } else {
            all_unread.into_iter().filter(|m| m.from_agent_id != slot_id).collect()
        };
        let active_team_run_id = self.team_run_manager.active_run_id().await;
        if !unread.is_empty() && (active_team_run_id.is_none() || next_wake.is_none()) {
            warn!(
                team_id = %self.team.id,
                slot_id,
                unread_count = unread.len(),
                has_active_team_run = active_team_run_id.is_some(),
                has_pending_wake = next_wake.is_some(),
                reason = "unowned_mailbox_backlog",
                "team wake input skipped because unread mailbox is not owned by a TeamRun wake"
            );
            return Ok(None);
        }
        let tasks = self.scheduler.list_tasks().await?;

        let mut wake_body = build_wake_payload(&agent, &tasks, &unread);
        if matches!(
            next_wake.as_ref().map(|wake| wake.source),
            Some(TeamWakeSource::UserIntervention)
        ) {
            wake_body = format!(
                "## Turn Context\n\nThis turn was triggered by a user intervention. Prioritize the user's message in this turn. Do not infer exact queue or backlog state from this notice.\n\n{wake_body}"
            );
        }

        let needs_role_prompt = self.scheduler.take_needs_role_prompt(slot_id).await;

        let first_message = if needs_role_prompt {
            let role_prompt = match agent.role {
                TeammateRole::Lead => {
                    let available_assistants = match self.service.upgrade() {
                        Some(svc) => svc.list_team_selectable_assistants().await,
                        None => Vec::new(),
                    };
                    build_lead_prompt(
                        &self.team.name,
                        &self.scheduler.list_agents().await,
                        &available_assistants,
                    )
                }
                TeammateRole::Teammate => {
                    let members = self.scheduler.list_agents().await;
                    build_teammate_prompt(&agent, &self.team.name, &members)
                }
            };
            format!("{role_prompt}\n\n{wake_body}")
        } else {
            wake_body
        };

        let should_send = !unread.is_empty();

        Ok(Some(WakeInput {
            team_run_id: active_team_run_id,
            conversation_id: agent.conversation_id,
            first_message,
            should_send,
            unread,
            agent_role: agent.role,
            wake_source: next_wake.as_ref().map(|wake| wake.source),
            trigger_message_id: next_wake.and_then(|wake| wake.message_id),
        }))
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

        let lead_conv_id = self.scheduler.get_agent(&lead_slot_id).await?.conversation_id;
        let (mut ack, lease) = self
            .team_run_manager
            .acquire_user_message_wake(&lead_slot_id, TeamRunTargetRole::Lead)
            .await?;

        let mailbox_message = match self
            .mailbox
            .write_with_files(
                &self.team.id,
                &lead_slot_id,
                "user",
                MailboxMessageType::Message,
                content,
                None,
                files.as_deref(),
            )
            .await
        {
            Ok(message) => message,
            Err(err) => {
                let _ = self
                    .team_run_manager
                    .abort_operation_lease(&lease.lease_id, "mailbox_write_failed")
                    .await;
                return Err(err);
            }
        };
        ack.message_id = Some(mailbox_message.id.clone());

        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let request = TeamProjectionRequest::user_visible(
            &self.team.id,
            &lead_slot_id,
            &lead_conv_id,
            content,
            files.clone().unwrap_or_default(),
        );
        if let Err(e) = projection.project(request).await {
            warn!(
                team_id = %self.team.id,
                slot_id = %lead_slot_id,
                conversation_id = %lead_conv_id,
                error = %e,
                "failed to project user right bubble for leader (non-fatal)"
            );
        }

        let _ = files;
        self.team_run_manager
            .commit_operation_lease(&lease.lease_id, Some(mailbox_message.id.clone()))
            .await?;
        self.notify_reserved_wake_for_team_work(&lead_slot_id, lease.role.clone(), lease.wake_source);
        Ok(ack)
    }

    /// Write a user message to the specified agent's mailbox and trigger a wake.
    ///
    /// Same log-not-throw behaviour as [`send_message`]; see that method for
    /// rationale.
    pub async fn send_message_to_agent(
        &self,
        slot_id: &str,
        content: &str,
        files: Option<Vec<String>>,
    ) -> Result<TeamRunAckResponse, TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        let (mut ack, lease) = self
            .team_run_manager
            .acquire_user_message_wake(slot_id, target_role_for(agent.role))
            .await?;

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
            Err(err) => {
                let _ = self
                    .team_run_manager
                    .abort_operation_lease(&lease.lease_id, "mailbox_write_failed")
                    .await;
                return Err(err);
            }
        };
        ack.message_id = Some(mailbox_message.id.clone());

        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let request = TeamProjectionRequest::user_visible(
            &self.team.id,
            slot_id,
            &agent.conversation_id,
            content,
            files.clone().unwrap_or_default(),
        );
        if let Err(e) = projection.project(request).await {
            warn!(
                team_id = %self.team.id,
                slot_id,
                conversation_id = %agent.conversation_id,
                error = %e,
                "failed to project user right bubble (non-fatal)"
            );
        }

        let _ = files;
        self.team_run_manager
            .commit_operation_lease(&lease.lease_id, Some(mailbox_message.id.clone()))
            .await?;
        self.notify_reserved_wake_for_team_work(slot_id, lease.role.clone(), lease.wake_source);
        Ok(ack)
    }

    pub(crate) async fn send_agent_message_from_agent(
        &self,
        from_slot_id: &str,
        to_slot_id: &str,
        content: &str,
    ) -> Result<AgentMessageQueueResult, TeamError> {
        let to_agent = self.scheduler.get_agent(to_slot_id).await?;
        let from_agent = self.scheduler.get_agent(from_slot_id).await?;
        let to_role = target_role_for(to_agent.role);
        let outcome = self
            .team_run_manager
            .acquire_run_scoped_wake(to_slot_id, to_role.clone(), TeamWakeSource::McpSendMessage)
            .await?;
        let TeamRunWakeAcquireOutcome::Accepted(lease) = outcome else {
            let (team_run_id, work) = self
                .team_run_manager
                .slot_work_for_slot(to_slot_id)
                .await
                .ok_or_else(|| TeamError::InvalidRequest(format!("no active team run work for slot {to_slot_id}")))?;
            let queue_state = classify_send_message_queue_state(&work, true);
            let target = TeamSendMessageTargetQueueState {
                slot_id: work.slot_id,
                role: work.role,
                queue_state,
                pending_wake_count: work.pending_wake_count,
                starting_child_count: work.starting_child_count,
                active_turn_id: work.active_turn_id,
                suppressed_wake_count: work.suppressed_wake_count,
            };
            info!(
                team_id = %self.team.id,
                team_run_id = %team_run_id,
                caller_slot_id = from_slot_id,
                target_slot_id = to_slot_id,
                target_role = ?target.role,
                wake_source = %TeamWakeSource::McpSendMessage,
                suppressed_wake_count = target.suppressed_wake_count,
                reason = ?target.queue_state,
                "team_agent_message_wake_suppressed"
            );
            return Ok(AgentMessageQueueResult {
                team_run_id,
                delivery: TeamSendMessageDelivery::WakeSuppressed,
                target,
            });
        };

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
            Err(err) => {
                let _ = self
                    .team_run_manager
                    .abort_operation_lease(&lease.lease_id, "mailbox_write_failed")
                    .await;
                return Err(err);
            }
        };

        let projection = TeamMessageProjection::new(self.projection_store.clone(), self.broadcaster.clone());
        let request = TeamProjectionRequest {
            team_id: self.team.id.clone(),
            slot_id: to_slot_id.to_owned(),
            conversation_id: to_agent.conversation_id.clone(),
            source: TeamProjectionSource::Teammate {
                from_slot_id: from_slot_id.to_owned(),
                from_name: from_agent.name.clone(),
                sender_backend: Some(from_agent.backend.clone()),
                sender_conversation_id: Some(from_agent.conversation_id.clone()),
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

        if let Err(err) = projection.project(request).await {
            warn!(
                team_id = %self.team.id,
                from_slot_id,
                to_slot_id,
                conversation_id = %to_agent.conversation_id,
                mailbox_message_id = %mailbox_message.id,
                error = %err,
                "team agent message immediate projection failed (non-fatal)"
            );
        }

        self.team_run_manager
            .commit_operation_lease(&lease.lease_id, Some(mailbox_message.id.clone()))
            .await?;
        let target_event_loop_registered = self.event_loops.has(to_slot_id);
        if !target_event_loop_registered {
            warn!(
                team_id = %self.team.id,
                slot_id = to_slot_id,
                target_role = ?to_role,
                wake_source = %TeamWakeSource::McpSendMessage,
                "team wake recorded but event loop is not registered; pending wake retained"
            );
            self.team_run_manager
                .mark_slot_runtime_health(to_slot_id, TeamSlotRuntimeHealth::Unhealthy)
                .await;
        }

        let (team_run_id, work) = self
            .team_run_manager
            .slot_work_for_slot(to_slot_id)
            .await
            .ok_or_else(|| TeamError::InvalidRequest(format!("no active team run work for slot {to_slot_id}")))?;
        let queue_state = classify_send_message_queue_state(&work, target_event_loop_registered);
        let target = TeamSendMessageTargetQueueState {
            slot_id: work.slot_id,
            role: work.role,
            queue_state,
            pending_wake_count: work.pending_wake_count,
            starting_child_count: work.starting_child_count,
            active_turn_id: work.active_turn_id,
            suppressed_wake_count: work.suppressed_wake_count,
        };

        info!(
            team_id = %self.team.id,
            team_run_id = %team_run_id,
            caller_slot_id = from_slot_id,
            target_slot_id = to_slot_id,
            target_role = ?target.role,
            wake_source = %TeamWakeSource::McpSendMessage,
            message_id = %mailbox_message.id,
            slot_pending_wake_count = target.pending_wake_count,
            starting_child_count = target.starting_child_count,
            active_turn_id = ?target.active_turn_id.as_deref(),
            suppressed_wake_count = target.suppressed_wake_count,
            reason = ?target.queue_state,
            "team_agent_message_queued"
        );

        if target_event_loop_registered {
            self.notify_reserved_wake_for_team_work(to_slot_id, to_role, TeamWakeSource::McpSendMessage);
        }

        Ok(AgentMessageQueueResult {
            team_run_id,
            delivery: TeamSendMessageDelivery::WakeRecorded,
            target,
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

        let outcome = self
            .team_run_manager
            .acquire_run_scoped_wake(
                target_slot_id,
                target_role_for(target.role),
                TeamWakeSource::McpShutdownRequest,
            )
            .await?;
        let TeamRunWakeAcquireOutcome::Accepted(lease) = outcome else {
            return Err(TeamError::InvalidRequest("shutdown wake was suppressed".into()));
        };

        let shutdown_message = match self
            .scheduler
            .request_shutdown_agent(caller_slot_id, target_slot_id, reason.as_deref())
            .await
        {
            Ok(message) => message,
            Err(err) => {
                let _ = self
                    .team_run_manager
                    .abort_operation_lease(&lease.lease_id, "shutdown_scheduler_action_failed")
                    .await;
                return Err(err);
            }
        };

        if shutdown_message.to_agent_id != target_slot_id {
            let _ = self
                .team_run_manager
                .abort_operation_lease(&lease.lease_id, "shutdown_mailbox_target_mismatch")
                .await;
            return Err(TeamError::InvalidRequest(format!(
                "shutdown mailbox target mismatch: expected {target_slot_id}, got {}",
                shutdown_message.to_agent_id
            )));
        }

        self.team_run_manager
            .commit_operation_lease(&lease.lease_id, Some(shutdown_message.id.clone()))
            .await?;
        self.notify_reserved_wake_for_team_work(target_slot_id, lease.role.clone(), lease.wake_source);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn wake_agent_for_team_work(
        &self,
        slot_id: &str,
        source: TeamWakeSource,
        trigger_message_id: Option<String>,
    ) -> Result<(), TeamError> {
        let Some(target_role) = self
            .reserve_wake_for_team_work(slot_id, source, trigger_message_id)
            .await?
        else {
            return Ok(());
        };

        if self.event_loops.has(slot_id) {
            self.notify_reserved_wake_for_team_work(slot_id, target_role, source);
            return Ok(());
        }

        warn!(
            team_id = %self.team.id,
            slot_id,
            target_role = ?target_role,
            wake_source = %source,
            "team wake recorded but event loop is not registered; pending wake retained"
        );
        Ok(())
    }

    pub(crate) fn notify_mailbox_only_wake(&self, slot_id: &str, source: TeamWakeSource) {
        if self.event_loops.has(slot_id) {
            self.event_loops.notify(slot_id);
            info!(
                team_id = %self.team.id,
                slot_id,
                wake_source = %source,
                wake_policy = "mailbox_only",
                "team mailbox-only wake notified"
            );
            return;
        }

        info!(
            team_id = %self.team.id,
            slot_id,
            wake_source = %source,
            wake_policy = "mailbox_only_deferred",
            "team mailbox-only wake deferred because event loop is not registered"
        );
    }

    pub(crate) async fn reserve_wake_for_team_work(
        &self,
        slot_id: &str,
        source: TeamWakeSource,
        trigger_message_id: Option<String>,
    ) -> Result<Option<TeamRunTargetRole>, TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        let target_role = target_role_for(agent.role);
        let decision = self
            .team_run_manager
            .record_or_suppress_wake(slot_id, target_role.clone(), source, trigger_message_id)
            .await?;
        Ok(matches!(decision, WakeRecordDecision::Recorded).then_some(target_role))
    }

    pub(crate) fn notify_reserved_wake_for_team_work(
        &self,
        slot_id: &str,
        target_role: TeamRunTargetRole,
        source: TeamWakeSource,
    ) {
        if self.event_loops.has(slot_id) {
            self.event_loops.notify(slot_id);
            info!(
                team_id = %self.team.id,
                slot_id,
                target_role = ?target_role,
                wake_source = %source,
                "team reserved wake notified"
            );
            return;
        }

        warn!(
            team_id = %self.team.id,
            slot_id,
            target_role = ?target_role,
            wake_source = %source,
            "team reserved wake notification skipped because event loop is not registered"
        );
    }

    pub(crate) async fn scheduler_wake_agent_for_team_work(
        &self,
        slot_id: &str,
        source: TeamWakeSource,
    ) -> Result<(), TeamError> {
        let agent = self.scheduler.get_agent(slot_id).await?;
        let target_role = target_role_for(agent.role);
        let outcome = self
            .team_run_manager
            .acquire_scheduler_wake(slot_id, target_role.clone(), source)
            .await?;
        let TeamRunWakeAcquireOutcome::Accepted(lease) = outcome else {
            return Ok(());
        };

        self.team_run_manager
            .commit_operation_lease(&lease.lease_id, None)
            .await?;
        self.notify_reserved_wake_for_team_work(slot_id, target_role, source);
        Ok(())
    }

    pub(crate) async fn try_start_recovery_drain(
        &self,
        reason: &'static str,
    ) -> Result<Option<RecoveryBacklogResult>, TeamError> {
        if self.recovery_scan_completed.swap(true, Ordering::AcqRel) {
            tracing::debug!(
                team_id = %self.team.id,
                reason,
                "team recovery scan skipped because it already ran for this session lifecycle"
            );
            return Ok(None);
        }

        let agents = self.scheduler.list_agents().await;
        let mut candidates = Vec::new();
        let mut unread_total = 0usize;
        let mut missing_event_loops = Vec::new();

        for agent in agents {
            if !self.event_loops.has(&agent.slot_id) {
                missing_event_loops.push(agent.slot_id.clone());
                continue;
            }

            let unread = self.mailbox.peek_unread(&self.team.id, &agent.slot_id).await?;
            let recoverable_count = unread
                .into_iter()
                .filter(|message| message.from_agent_id != agent.slot_id)
                .count();
            if recoverable_count == 0 {
                continue;
            }
            unread_total += recoverable_count;
            candidates.push(RecoveryWakeCandidate {
                slot_id: agent.slot_id,
                role: target_role_for(agent.role),
                unread_count: recoverable_count,
            });
        }

        if !missing_event_loops.is_empty() {
            warn!(
                team_id = %self.team.id,
                reason,
                missing_event_loop_count = missing_event_loops.len(),
                unread_count = unread_total,
                "team recovery scan found slots without event loops; unread work is retained"
            );
        }

        if candidates.is_empty() {
            tracing::debug!(
                team_id = %self.team.id,
                reason,
                "team recovery scan found no recoverable mailbox backlog"
            );
            return Ok(None);
        }

        let result = self.team_run_manager.recover_mailbox_backlog(candidates).await;
        if let Some(result) = result.as_ref() {
            info!(
                team_id = %self.team.id,
                team_run_id = %result.team_run_id,
                source = "recovery_drain",
                slot_count = result.recorded_wakes.len(),
                pending_wake_count = result.pending_wake_count,
                reason,
                "team recovery scan recorded TeamRun wakes"
            );
            for slot_id in &result.recorded_wakes {
                self.event_loops.notify(slot_id);
            }
        } else {
            warn!(
                team_id = %self.team.id,
                source = "recovery_drain",
                unread_count = unread_total,
                reason,
                "team recovery scan retained unread mailbox backlog without recording wakes"
            );
        }

        Ok(result)
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
        target_slot_id: Option<String>,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        let current_run_id = self
            .team_run_manager
            .current_run_id()
            .await
            .ok_or_else(|| TeamError::InvalidRequest("no active team run to cancel".into()))?;
        if current_run_id != team_run_id {
            return Err(TeamError::InvalidRequest(format!(
                "team run {team_run_id} is not active"
            )));
        }

        self.team_run_manager.begin_cancel(target_slot_id, reason).await?;

        let agent_ids = self
            .scheduler
            .list_agents()
            .await
            .into_iter()
            .map(|agent| agent.slot_id)
            .collect::<Vec<_>>();
        let marked = self
            .mailbox
            .mark_all_unread_for_agents_read(&self.team.id, &agent_ids)
            .await?;
        info!(
            team_id = %self.team.id,
            team_run_id,
            agent_count = agent_ids.len(),
            marked_unread = marked,
            "team_run cancel drained unread mailbox rows"
        );

        let active_child_turns = self.team_run_manager.active_child_turns().await;
        let active_child_count = active_child_turns.len();
        for child in active_child_turns {
            if let Err(err) = self
                .cancellation_port
                .cancel_agent_turn(&self.user_id, &child.conversation_id, &child.turn_id)
                .await
            {
                warn!(
                    team_id = %self.team.id,
                    team_run_id,
                    slot_id = %child.slot_id,
                    turn_id = %child.turn_id,
                    error = %err,
                    "team_run cancel child turn failed (continuing)"
                );
            }
            self.team_run_manager.record_child_cancelled(&child).await;
        }

        match self.team_run_manager.try_complete_cancelled().await {
            Some(payload) => {
                info!(
                    team_id = %self.team.id,
                    team_run_id,
                    active_child_count,
                    starting_child_count = payload.starting_child_count,
                    pending_wake_count = payload.pending_wake_count,
                    marked_unread = marked,
                    "team_run cancel completed"
                );
            }
            None => {
                info!(
                    team_id = %self.team.id,
                    team_run_id,
                    active_child_count,
                    marked_unread = marked,
                    "team_run cancel completion pending"
                );
            }
        }
        Ok(())
    }

    pub async fn cancel_child_turn(
        &self,
        team_run_id: &str,
        slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        let target = self.team_run_manager.begin_cancel_child(slot_id).await?;
        match target {
            ChildCancelTarget::Active(child) => {
                if child.team_run_id != team_run_id {
                    return Err(TeamError::InvalidRequest(format!(
                        "agent {slot_id} is not active in team run {team_run_id}"
                    )));
                }
                self.cancellation_port
                    .cancel_agent_turn(&self.user_id, &child.conversation_id, &child.turn_id)
                    .await
                    .map_err(|err| TeamError::InvalidRequest(err.to_string()))?;
                self.team_run_manager.record_child_cancelled(&child).await;

                if child.role == TeamRunTargetRole::Teammate {
                    self.notify_leader_child_interrupted(slot_id, reason).await?;
                } else {
                    self.team_run_manager.maybe_complete().await;
                }
            }
            ChildCancelTarget::Starting(reservation) => {
                if reservation.team_run_id != team_run_id {
                    return Err(TeamError::InvalidRequest(format!(
                        "agent {slot_id} is not starting in team run {team_run_id}"
                    )));
                }
                if reservation.role == TeamRunTargetRole::Teammate {
                    self.notify_leader_child_interrupted(slot_id, reason).await?;
                }
                self.team_run_manager.try_complete_cancelled().await;
            }
        }
        Ok(())
    }

    pub async fn pause_slot_work(
        &self,
        team_run_id: &str,
        slot_id: &str,
        reason: Option<String>,
    ) -> Result<(), TeamError> {
        let current_run_id = self
            .team_run_manager
            .current_run_id()
            .await
            .ok_or_else(|| TeamError::InvalidRequest("no active team run to pause".into()))?;
        if current_run_id != team_run_id {
            return Err(TeamError::InvalidRequest(format!(
                "agent {slot_id} is not active in team run {team_run_id}"
            )));
        }

        let outcome = self.team_run_manager.pause_slot_work(slot_id, reason.clone()).await?;

        if let Some(target) = outcome.cancel_target {
            match target {
                ChildCancelTarget::Active(child) => {
                    if let Err(err) = self
                        .cancellation_port
                        .cancel_agent_turn(&self.user_id, &child.conversation_id, &child.turn_id)
                        .await
                    {
                        warn!(
                            team_id = %self.team.id,
                            team_run_id = %outcome.team_run_id,
                            slot_id = %child.slot_id,
                            turn_id = %child.turn_id,
                            error = %err,
                            "team slot pause child turn cancel failed"
                        );
                        return Err(TeamError::InvalidRequest(err.to_string()));
                    }
                    self.team_run_manager
                        .complete_pause_after_child_cancelled(&child, reason.clone())
                        .await;
                    if child.role == TeamRunTargetRole::Teammate {
                        self.notify_leader_child_interrupted(slot_id, reason).await?;
                    }
                }
                ChildCancelTarget::Starting(reservation) => {
                    if reservation.role == TeamRunTargetRole::Teammate {
                        self.notify_leader_child_interrupted(slot_id, reason).await?;
                    }
                }
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
            self.wake_leader_after_recovery_message(slot_id, TeamWakeSource::InterruptedNotification)
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
        self.wake_leader_after_recovery_message(failed_slot_id, TeamWakeSource::SpawnAttachFailure)
            .await
    }

    pub(crate) async fn wake_leader_after_recovery_message(
        &self,
        source_slot_id: &str,
        source: TeamWakeSource,
    ) -> Result<(), TeamError> {
        let Some(lead_slot_id) = self.scheduler.find_lead_slot_id().await else {
            return Err(TeamError::AgentNotFound("lead".into()));
        };
        let outcome = match self
            .team_run_manager
            .acquire_run_scoped_wake(&lead_slot_id, TeamRunTargetRole::Lead, source)
            .await
        {
            Ok(outcome) => outcome,
            Err(TeamError::InvalidRequest(message)) if message == "no active team run for run-scoped wake" => {
                info!(
                    team_id = %self.team.id,
                    slot_id = %lead_slot_id,
                    source_slot_id,
                    wake_source = %source,
                    wake_policy = "deferred_mailbox_only",
                    "leader recovery message deferred because no active team run exists"
                );
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        let TeamRunWakeAcquireOutcome::Accepted(lease) = outcome else {
            return Ok(());
        };

        self.team_run_manager
            .commit_operation_lease(&lease.lease_id, None)
            .await?;
        self.notify_reserved_wake_for_team_work(&lead_slot_id, TeamRunTargetRole::Lead, source);
        Ok(())
    }

    pub async fn add_agent(&self, agent: &TeamAgent) {
        self.scheduler.add_agent(agent).await;
    }

    pub async fn remove_agent(&self, slot_id: &str) -> Result<(), TeamError> {
        self.event_loops.remove(slot_id);
        let conversation_id = self.scheduler.remove_agent(slot_id).await?;
        if let Some(conv_id) = conversation_id
            && let Err(e) = self.task_manager.kill(&conv_id, Some(AgentKillReason::TeamDeleted))
        {
            warn!(
                team_id = %self.team.id,
                slot_id,
                conversation_id = %conv_id,
                error = %e,
                "remove_agent: task_manager.kill failed (non-fatal)"
            );
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
            .resolve_spawn_backend_and_model(
                Some(assistant_id),
                req.model.as_deref(),
                caller.backend.as_str(),
                caller.model.as_str(),
            )
            .await?;

        // Step 4: backend capability check. Hard whitelist passes immediately;
        // otherwise query persisted agent_capabilities for MCP support.
        if !crate::capability::TEAM_CAPABLE_BACKENDS.contains(&backend.as_str()) {
            let capable = service.is_backend_team_capable(&backend).await;
            if !capable {
                return Err(TeamError::BackendNotAllowed(backend));
            }
        }

        // Step 5: DB side-effects (new conversation + persisted agent slot).
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

        let spawn_wake_plan = if self.team_run_manager.active_run_id().await.is_some() {
            let spawn_welcome_role = self
                .reserve_wake_for_team_work(
                    &new_agent.slot_id,
                    TeamWakeSource::SpawnWelcome,
                    Some(welcome_message.id),
                )
                .await?
                .ok_or_else(|| TeamError::InvalidRequest("spawn welcome wake was suppressed".into()))?;
            info!(
                team_id = %self.team.id,
                slot_id = %new_agent.slot_id,
                target_role = ?spawn_welcome_role,
                wake_source = %TeamWakeSource::SpawnWelcome,
                "spawn welcome wake reserved before runtime attach"
            );
            SpawnWakePlan::RunScoped(spawn_welcome_role)
        } else {
            info!(
                team_id = %self.team.id,
                slot_id = %new_agent.slot_id,
                wake_source = %TeamWakeSource::SpawnWelcome,
                wake_policy = "mailbox_only",
                "spawn welcome wake will use mailbox-only delivery because no active team run exists"
            );
            SpawnWakePlan::MailboxOnly
        };

        // Step 7: attach the CLI process and register the finish subscriber
        // in a background task. This involves spawning the CLI process and
        // completing the ACP protocol handshake, which can take significant
        // time (10-30s). Running it asynchronously ensures `spawn_agent`
        // returns promptly so the MCP tool call completes without blocking
        // the leader's connection loop.
        {
            let team_id = self.team.id.clone();
            let user_id = self.user_id.clone();
            let agent_clone = new_agent.clone();
            let mcp_stdio_cfg = self.mcp_stdio_config(&new_agent.slot_id);
            let task_manager = self.task_manager.clone();
            tokio::spawn(async move {
                // Push the team MCP stdio config into the new conversation's
                // extras, then kill + rebuild the agent task so the freshly
                // spawned process boots with the MCP handshake pointing at
                // our session.
                if let Err(err) = Self::attach_spawned_agent_process_bg(
                    &service,
                    &agent_clone,
                    mcp_stdio_cfg,
                    &user_id,
                    &task_manager,
                )
                .await
                {
                    warn!(
                        team_id = %team_id,
                        slot_id = %agent_clone.slot_id,
                        error = %err,
                        "failed to attach spawned agent process; agent is persisted but not yet running"
                    );
                    if let Err(notify_err) = service
                        .notify_leader_spawn_attach_failed(&team_id, &agent_clone.slot_id, &err.to_string())
                        .await
                    {
                        warn!(
                            team_id = %team_id,
                            slot_id = %agent_clone.slot_id,
                            error = %notify_err,
                            "failed to notify leader about spawned agent attach failure"
                        );
                    }
                    return;
                }

                // Register the event loop for the newly spawned agent.
                service.register_event_loop(&team_id, &agent_clone.slot_id);

                match spawn_wake_plan {
                    SpawnWakePlan::RunScoped(spawn_welcome_role) => {
                        service.notify_reserved_wake_for_team_work(
                            &team_id,
                            &agent_clone.slot_id,
                            spawn_welcome_role,
                            TeamWakeSource::SpawnWelcome,
                        );
                    }
                    SpawnWakePlan::MailboxOnly => {
                        service.notify_mailbox_only_wake(&team_id, &agent_clone.slot_id, TeamWakeSource::SpawnWelcome);
                    }
                }
            });
        }

        Ok(new_agent)
    }

    /// Persist the team MCP stdio config into the spawned agent's conversation
    /// row, then kill any pre-existing task and warm up the new one.
    ///
    /// This is a static helper suitable for use inside `tokio::spawn` (no
    /// `&self` borrow). The caller passes all necessary context by value.
    async fn attach_spawned_agent_process_bg(
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
        info!(team_id = %self.team.id, "TeamSession stopping");
        self.mcp_server.stop();
    }

    pub fn mailbox(&self) -> &Arc<Mailbox> {
        &self.mailbox
    }

    pub fn task_board(&self) -> &Arc<TaskBoard> {
        &self.task_board
    }
}

fn classify_send_message_queue_state(
    work: &TeamSlotWorkPayload,
    target_event_loop_registered: bool,
) -> TeamSendMessageReason {
    if matches!(work.runtime_health, Some(TeamSlotRuntimeHealth::Disconnected)) {
        return TeamSendMessageReason::TargetDisconnected;
    }
    if !target_event_loop_registered || matches!(work.runtime_health, Some(TeamSlotRuntimeHealth::Unhealthy)) {
        return TeamSendMessageReason::TargetUnhealthy;
    }
    if work.paused && work.suppressed_wake_count > 0 {
        return TeamSendMessageReason::SuppressedByPause;
    }
    if work.active_turn_id.is_some() {
        return TeamSendMessageReason::BehindActiveTurn;
    }
    if work.starting_child_count > 0 {
        return TeamSendMessageReason::BehindStartingTurn;
    }
    TeamSendMessageReason::QueuedForIdle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_loop::AgentLoopContext;
    use crate::team_run::{ActiveChildTurn, RecoveryWakeCandidate};
    use crate::test_utils::MockTeamRepo;
    use crate::types::{Team, TeamAgent, TeammateRole};
    use aionui_ai_agent::AgentError;
    use aionui_ai_agent::agent_task::AgentInstance;
    use aionui_ai_agent::types::BuildTaskOptions;
    use aionui_api_types::{
        TeamRunSource, TeamRunStatus, TeamRunTargetRole, TeamSendMessageDelivery, TeamSendMessageReason,
        WebSocketMessage,
    };
    use aionui_common::{AgentKillReason, TimestampMs, now_ms};
    use std::sync::{Arc, Mutex};

    struct NullBroadcaster;
    impl EventBroadcaster for NullBroadcaster {
        fn broadcast(&self, _msg: WebSocketMessage<serde_json::Value>) {}
    }

    struct NoopTurnPort;

    #[async_trait::async_trait]
    impl crate::ports::AgentTurnExecutionPort for NoopTurnPort {
        async fn run_agent_turn(
            &self,
            request: crate::ports::AgentTurnRequest,
        ) -> Result<crate::ports::AgentTurnOutcome, crate::ports::AgentTurnExecutionError> {
            if let Some(on_started) = request.on_started.as_ref() {
                on_started(crate::ports::AgentTurnStarted {
                    team_run_id: request.team_run_id.clone().expect("team run id"),
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

    struct FailingCancellationPort;

    #[async_trait::async_trait]
    impl crate::ports::AgentTurnCancellationPort for FailingCancellationPort {
        async fn cancel_agent_turn(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _turn_id: &str,
        ) -> Result<(), crate::ports::AgentTurnExecutionError> {
            Err(crate::ports::AgentTurnExecutionError::Failed {
                reason: "cancel unavailable".into(),
            })
        }
    }

    fn failing_cancellation_port() -> Arc<dyn crate::ports::AgentTurnCancellationPort> {
        Arc::new(FailingCancellationPort)
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

    #[derive(Default)]
    struct RecordingProjectionStore {
        inserted: std::sync::Mutex<Vec<aionui_db::models::MessageRow>>,
    }

    #[async_trait::async_trait]
    impl TeamProjectionMessageStore for RecordingProjectionStore {
        fn mint_message_id(&self) -> String {
            "minted-message".into()
        }

        async fn find_projected_message(
            &self,
            _conversation_id: &str,
            _msg_id: &str,
            _msg_type: &str,
        ) -> Result<Option<aionui_db::models::MessageRow>, TeamError> {
            Ok(None)
        }

        async fn insert_projected_message(&self, row: &aionui_db::models::MessageRow) -> Result<(), TeamError> {
            self.inserted.lock().unwrap().push(row.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct CompletingProjectionStore {
        manager: Mutex<Option<Arc<TeamRunManager>>>,
    }

    impl CompletingProjectionStore {
        fn set_manager(&self, manager: Arc<TeamRunManager>) {
            *self.manager.lock().unwrap() = Some(manager);
        }
    }

    #[async_trait::async_trait]
    impl TeamProjectionMessageStore for CompletingProjectionStore {
        fn mint_message_id(&self) -> String {
            "projection-completes-run".into()
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
            let manager = self.manager.lock().unwrap().clone();
            if let Some(manager) = manager {
                manager.maybe_complete().await;
            }
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailingProjectionStore;

    #[async_trait::async_trait]
    impl TeamProjectionMessageStore for FailingProjectionStore {
        fn mint_message_id(&self) -> String {
            "projection-fails".into()
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
            Err(TeamError::InvalidRequest("projection failed for test".into()))
        }
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
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        TeamSession::start(
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
        )
        .await
        .unwrap()
    }

    async fn start_session_arc() -> Arc<TeamSession> {
        Arc::new(start_session().await)
    }

    async fn record_recovery_wake(session: &TeamSession, slot_id: &str, role: TeamRunTargetRole, unread_count: usize) {
        session
            .team_run_manager()
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: slot_id.to_owned(),
                role,
                unread_count,
            }])
            .await
            .expect("recovery wake");
    }

    fn register_test_event_loop(session: &Arc<TeamSession>, slot_id: &str) {
        session.event_loops().spawn(
            slot_id,
            AgentLoopContext {
                team_id: session.team_id().to_owned(),
                slot_id: slot_id.to_owned(),
                user_id: session.user_id().to_owned(),
                session: session.clone(),
                scheduler: session.scheduler().clone(),
                mailbox: session.mailbox().clone(),
                turn_port: session.turn_port().clone(),
                registry: session.event_loops().clone(),
            },
        );
    }

    async fn start_session_with_projection_store(store: Arc<dyn TeamProjectionMessageStore>) -> TeamSession {
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        TeamSession::start(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            store,
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap()
    }

    async fn start_session_with_repo_and_projection_store(
        repo: Arc<dyn ITeamRepository>,
        store: Arc<dyn TeamProjectionMessageStore>,
    ) -> TeamSession {
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        TeamSession::start(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            noop_cancellation_port(),
            store,
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap()
    }

    async fn start_session_with_cancellation_port(
        cancellation_port: Arc<dyn crate::ports::AgentTurnCancellationPort>,
    ) -> TeamSession {
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::new());
        let broadcaster: Arc<dyn EventBroadcaster> = Arc::new(NullBroadcaster);
        TeamSession::start(
            make_team(),
            repo,
            broadcaster,
            backend_path(),
            empty_task_manager(),
            noop_turn_port(),
            cancellation_port,
            noop_projection_store(),
            "user-test".into(),
            Weak::<TeamSessionService>::new(),
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn agent_send_message_projects_recipient_visible_bubble_immediately() {
        let store = Arc::new(RecordingProjectionStore::default());
        let session = start_session_with_projection_store(store.clone()).await;
        session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("active run");

        session
            .send_agent_message_from_agent("lead-1", "worker-1", "Do the implementation")
            .await
            .expect("agent send succeeds");

        let inserted = store.inserted.lock().unwrap();
        assert_eq!(inserted.len(), 1);
        assert_eq!(inserted[0].conversation_id, "c2");
        assert_eq!(inserted[0].position.as_deref(), Some("left"));
        assert!(
            inserted[0]
                .msg_id
                .as_deref()
                .unwrap_or_default()
                .contains("team:t1:mailbox:"),
            "projection must use mailbox-message dedupe key"
        );
        session.stop();
    }

    #[tokio::test]
    async fn agent_send_message_survives_completion_between_projection_and_commit() {
        let store = Arc::new(CompletingProjectionStore::default());
        let session = start_session_with_projection_store(store.clone()).await;
        store.set_manager(session.team_run_manager().clone());
        session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("active run before agent message");

        session
            .send_agent_message_from_agent("lead-1", "worker-1", "lease protected")
            .await
            .expect("agent message lease should retain run while projection completes it");

        let unread = session
            .mailbox()
            .peek_unread(session.team_id(), "worker-1")
            .await
            .unwrap();
        assert!(unread.iter().any(|msg| msg.content == "lease protected"));
        session.stop();
    }

    #[tokio::test]
    async fn agent_message_to_idle_target_returns_queued_for_idle() {
        let session = start_session_arc().await;
        register_test_event_loop(&session, "worker-1");
        let ack = session.send_message("start", None).await.unwrap();

        let result = session
            .send_agent_message_from_agent("lead-1", "worker-1", "Do the implementation")
            .await
            .unwrap();

        assert_eq!(result.team_run_id, ack.team_run_id);
        assert_eq!(result.delivery, TeamSendMessageDelivery::WakeRecorded);
        assert_eq!(result.target.queue_state, TeamSendMessageReason::QueuedForIdle);
        assert_eq!(result.target.pending_wake_count, 1);
        session.event_loops().shutdown();
        session.stop();
    }

    #[tokio::test]
    async fn agent_message_to_active_target_returns_behind_active_turn() {
        let session = start_session_arc().await;
        register_test_event_loop(&session, "worker-1");
        let ack = session
            .team_run_manager()
            .accept_user_message("worker-1", TeamRunTargetRole::Teammate, false, None)
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_pending_wake("worker-1", TeamRunTargetRole::Teammate, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "conv-worker")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "worker-1".into(),
                    role: TeamRunTargetRole::Teammate,
                    conversation_id: "conv-worker".into(),
                    turn_id: "turn-worker".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        let result = session
            .send_agent_message_from_agent("lead-1", "worker-1", "Second item")
            .await
            .unwrap();

        assert_eq!(result.target.queue_state, TeamSendMessageReason::BehindActiveTurn);
        assert_eq!(result.target.pending_wake_count, 1);
        assert_eq!(result.target.active_turn_id.as_deref(), Some("turn-worker"));
        session.event_loops().shutdown();
        session.stop();
    }

    #[tokio::test]
    async fn leader_message_reuses_active_run_when_leader_slot_is_free() {
        let session = start_session().await;
        let first = session
            .send_message("First leader message", None)
            .await
            .expect("first leader send creates run");

        session
            .wake_agent_for_team_work("worker-1", TeamWakeSource::McpSendMessage, None)
            .await
            .expect("worker wake keeps run active");
        session.team_run_manager().record_empty_wake_observed("lead-1").await;

        let second = session
            .send_message("Second leader message", None)
            .await
            .expect("leader slot free should accept active-run intervention");

        assert_eq!(second.team_run_id, first.team_run_id);
        assert_eq!(second.target_slot_id, "lead-1");
        assert_eq!(second.accepted_slot_id, "lead-1");
        assert_eq!(second.accepted_role, TeamRunTargetRole::Lead);
        session.stop();
    }

    #[tokio::test]
    async fn leader_message_queues_when_leader_slot_has_pending_wake() {
        let session = start_session().await;
        let first = session
            .send_message("First leader message", None)
            .await
            .expect("first leader send creates run");

        let second = session
            .send_message("Second leader message", None)
            .await
            .expect("leader pending wake should accept additional foreground message");

        assert_eq!(second.team_run_id, first.team_run_id);
        let payload = session.team_run_manager().current_payload().await.unwrap();
        assert_eq!(payload.pending_wake_count, 2);
        session.stop();
    }

    #[tokio::test]
    async fn cancelling_leader_child_does_not_cancel_active_teammate_child() {
        let session = start_session().await;
        let ack = session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        let leader = ActiveChildTurn {
            team_run_id: ack.team_run_id.clone(),
            slot_id: "lead-1".into(),
            role: TeamRunTargetRole::Lead,
            conversation_id: "c1".into(),
            turn_id: "turn-lead".into(),
            started_at_ms: now_ms(),
            last_slow_notified_at_ms: None,
        };
        let worker = ActiveChildTurn {
            team_run_id: ack.team_run_id.clone(),
            slot_id: "worker-1".into(),
            role: TeamRunTargetRole::Teammate,
            conversation_id: "c2".into(),
            turn_id: "turn-worker".into(),
            started_at_ms: now_ms(),
            last_slow_notified_at_ms: None,
        };

        session
            .team_run_manager()
            .record_pending_wake("lead-1", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let leader_reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(&leader_reservation.reservation_id, leader)
            .await;

        session
            .team_run_manager()
            .record_pending_wake("worker-1", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();
        let worker_reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(&worker_reservation.reservation_id, worker)
            .await;

        session
            .cancel_child_turn(&ack.team_run_id, "lead-1", Some("user stopped leader".into()))
            .await
            .expect("leader child cancel succeeds");

        let active = session.team_run_manager().active_child_turns().await;
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].slot_id, "worker-1");
        assert_eq!(active[0].turn_id, "turn-worker");
        session.stop();
    }

    #[tokio::test]
    async fn cancel_run_still_cancels_leader_and_teammate_children() {
        let session = start_session().await;
        let ack = session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        session
            .team_run_manager()
            .record_pending_wake("lead-1", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let leader_reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(
                &leader_reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "lead-1".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "c1".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        session
            .team_run_manager()
            .record_pending_wake("worker-1", TeamRunTargetRole::Teammate, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();
        let worker_reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(
                &worker_reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "worker-1".into(),
                    role: TeamRunTargetRole::Teammate,
                    conversation_id: "c2".into(),
                    turn_id: "turn-worker".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        assert_eq!(session.team_run_manager().active_child_turns().await.len(), 2);

        session
            .cancel_run(&ack.team_run_id, None, Some("stop all".into()))
            .await
            .expect("stop-all run cancel succeeds");

        assert!(
            session.team_run_manager().active_child_turns().await.is_empty(),
            "cancel_run remains the explicit stop-all capability"
        );
        session.stop();
    }

    #[tokio::test]
    async fn wake_agent_for_team_work_records_pending_wake_without_registered_loop() {
        let session = start_session().await;
        session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        session
            .wake_agent_for_team_work("worker-1", TeamWakeSource::McpSendMessage, None)
            .await
            .expect("pending wake is recorded even before loop registration");

        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .expect("wake should be claimable");

        assert_eq!(reservation.slot_id, "worker-1");
        session.stop();
    }

    #[tokio::test]
    async fn pause_active_child_cancel_success_marks_slot_paused_and_removes_child() {
        let session = start_session().await;
        let ack = session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_pending_wake("lead-1", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "lead-1".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "c1".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;
        session
            .team_run_manager()
            .record_pending_wake("lead-1", TeamRunTargetRole::Lead, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();

        session
            .pause_slot_work(&ack.team_run_id, "lead-1", Some("user stopped".into()))
            .await
            .unwrap();

        let payload = session.team_run_manager().current_payload().await.unwrap();
        let lead = payload
            .slot_work
            .iter()
            .find(|work| work.slot_id == "lead-1")
            .expect("lead slot work");
        assert!(lead.paused);
        assert_eq!(lead.pending_wake_count, 0);
        assert_eq!(lead.suppressed_wake_count, 1);
        assert_eq!(lead.active_turn_id, None);
        session.stop();
    }

    #[tokio::test]
    async fn pause_active_child_cancel_error_keeps_child_and_does_not_pause_slot() {
        let session = start_session_with_cancellation_port(failing_cancellation_port()).await;
        let ack = session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_pending_wake("lead-1", TeamRunTargetRole::Lead, TeamWakeSource::UserMessage)
            .await
            .unwrap();
        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id.clone(),
                    slot_id: "lead-1".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "c1".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;
        session
            .team_run_manager()
            .record_pending_wake("lead-1", TeamRunTargetRole::Lead, TeamWakeSource::McpSendMessage)
            .await
            .unwrap();

        let err = session
            .pause_slot_work(&ack.team_run_id, "lead-1", Some("user stopped".into()))
            .await
            .unwrap_err();
        assert!(matches!(err, TeamError::InvalidRequest(message) if message.contains("cancel unavailable")));

        let payload = session.team_run_manager().current_payload().await.unwrap();
        let lead = payload
            .slot_work
            .iter()
            .find(|work| work.slot_id == "lead-1")
            .expect("lead slot work");
        assert!(!lead.paused);
        assert_eq!(lead.pending_wake_count, 1);
        assert_eq!(lead.suppressed_wake_count, 0);
        assert_eq!(lead.active_turn_id.as_deref(), Some("turn-lead"));
        session.stop();
    }

    #[tokio::test]
    async fn paused_slot_suppresses_mcp_send_without_mailbox_write() {
        let session = start_session().await;
        let ack = session.send_message("start", None).await.unwrap();
        session
            .pause_slot_work(&ack.team_run_id, "lead-1", Some("user stopped".into()))
            .await
            .unwrap();

        session
            .send_agent_message_from_agent("worker-1", "lead-1", "background update")
            .await
            .unwrap();

        let payload = session.team_run_manager().current_payload().await.expect("payload");
        let lead = payload.slot_work.iter().find(|work| work.slot_id == "lead-1").unwrap();
        assert!(lead.paused);
        assert_eq!(lead.pending_wake_count, 0);
        assert_eq!(lead.suppressed_wake_count, 2);

        let unread = session
            .mailbox()
            .peek_unread(session.team_id(), "lead-1")
            .await
            .unwrap();
        assert!(
            unread.iter().all(|msg| msg.content != "background update"),
            "suppressed acquire must not write mailbox"
        );
        session.stop();
    }

    #[tokio::test]
    async fn user_intervention_wake_input_prioritizes_only_user_message() {
        let session = start_session().await;
        let ack = session.send_message("start", None).await.unwrap();
        session
            .pause_slot_work(&ack.team_run_id, "lead-1", Some("user stopped".into()))
            .await
            .unwrap();
        session
            .send_agent_message_from_agent("worker-1", "lead-1", "old background")
            .await
            .unwrap();

        let user_ack = session
            .send_message_to_agent("lead-1", "please answer this first", None)
            .await
            .unwrap();

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("wake input");
        assert_eq!(input.wake_source, Some(TeamWakeSource::UserIntervention));
        assert_eq!(input.trigger_message_id, user_ack.message_id);
        assert!(input.first_message.contains("## Turn Context"));
        assert!(input.first_message.contains("Prioritize the user's message"));
        assert!(input.first_message.contains("please answer this first"));
        assert!(!input.first_message.contains("old background"));
        assert_eq!(input.unread.len(), 1);
        session.stop();
    }

    #[tokio::test]
    async fn reserved_spawn_welcome_survives_leader_empty_wake_until_teammate_registers() {
        let session = start_session().await;
        let ack = session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        let role = session
            .reserve_wake_for_team_work("worker-1", TeamWakeSource::SpawnWelcome, None)
            .await
            .expect("reserve spawn welcome")
            .expect("spawn welcome should be recorded");
        assert_eq!(role, TeamRunTargetRole::Teammate);

        assert!(
            session
                .team_run_manager()
                .record_empty_wake_observed("lead-1")
                .await
                .is_none(),
            "lead empty wake must not complete while worker spawn welcome is pending"
        );
        assert_eq!(
            session.team_run_manager().active_run_id().await.as_deref(),
            Some(ack.team_run_id.as_str())
        );

        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .expect("worker should claim reserved spawn welcome");

        assert_eq!(reservation.slot_id, "worker-1");
        session.stop();
    }

    #[tokio::test]
    async fn reserve_wake_for_team_work_rejects_without_active_run() {
        let session = start_session().await;

        let err = session
            .reserve_wake_for_team_work("worker-1", TeamWakeSource::SpawnWelcome, None)
            .await
            .expect_err("run-scoped reserve without active run must fail");

        assert!(matches!(
            err,
            TeamError::InvalidRequest(message)
                if message == "no active team run for run-scoped wake"
        ));
        session.stop();
    }

    #[tokio::test]
    async fn wake_agent_for_team_work_rejects_orphan_mcp_work() {
        let session = start_session().await;

        let err = session
            .wake_agent_for_team_work("worker-1", TeamWakeSource::McpSendMessage, None)
            .await
            .expect_err("MCP work without active run must fail");

        assert!(matches!(
            err,
            TeamError::InvalidRequest(message)
                if message == "no active team run for run-scoped wake"
        ));
        session.stop();
    }

    #[tokio::test]
    async fn crash_notification_in_active_run_wakes_leader() {
        let session = start_session().await;
        session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        session
            .wake_leader_after_recovery_message("worker-1", TeamWakeSource::CrashNotification)
            .await
            .expect("active run recovery should wake leader");

        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .expect("leader recovery wake should be claimable");

        assert_eq!(reservation.slot_id, "lead-1");
        session.stop();
    }

    #[tokio::test]
    async fn recovery_wake_without_active_run_is_deferred_without_active_run_snapshot_branch() {
        let session = start_session().await;

        session
            .wake_leader_after_recovery_message("worker-1", TeamWakeSource::CrashNotification)
            .await
            .expect("deferred recovery notification should not fail");

        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await;

        assert!(
            reservation.is_none(),
            "deferred recovery must not create Team Run reservation"
        );
        session.stop();
    }

    #[tokio::test]
    async fn inactivity_notification_in_active_run_wakes_leader() {
        let session = start_session().await;
        session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("accept run");

        session
            .wake_leader_after_recovery_message("worker-1", TeamWakeSource::InactivityTimeout)
            .await
            .expect("active run inactivity recovery should wake leader");

        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .expect("leader inactivity wake should be claimable");

        assert_eq!(reservation.slot_id, "lead-1");
        session.stop();
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
    async fn shutdown_without_active_run_does_not_write_mailbox() {
        let session = start_session().await;

        let err = session
            .shutdown_agent("lead-1", "worker-1", Some("done".into()))
            .await
            .expect_err("shutdown is run-scoped");

        assert!(matches!(
            err,
            TeamError::InvalidRequest(message)
                if message == "no active team run for run-scoped wake"
        ));
        let unread = session.mailbox().peek_unread("t1", "worker-1").await.unwrap();
        assert!(unread.is_empty());
        session.stop();
    }

    #[tokio::test]
    async fn shutdown_busy_target_is_accepted_as_lifecycle_wake() {
        let session = start_session().await;
        let ack = session.send_message("start", None).await.unwrap();
        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("lead-1", TeamRunTargetRole::Lead, "c1")
            .await
            .unwrap();
        session
            .team_run_manager()
            .record_child_started(
                &reservation.reservation_id,
                ActiveChildTurn {
                    team_run_id: ack.team_run_id,
                    slot_id: "lead-1".into(),
                    role: TeamRunTargetRole::Lead,
                    conversation_id: "c1".into(),
                    turn_id: "turn-lead".into(),
                    started_at_ms: now_ms(),
                    last_slow_notified_at_ms: None,
                },
            )
            .await;

        session
            .shutdown_agent("lead-1", "worker-1", Some("done".into()))
            .await
            .expect("shutdown lifecycle wake should be accepted");

        let unread = session.mailbox().peek_unread("t1", "worker-1").await.unwrap();
        assert_eq!(unread.len(), 1);
        assert_eq!(unread[0].msg_type, MailboxMessageType::ShutdownRequest);
        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .expect("shutdown wake should be pending");
        assert_eq!(reservation.wake_source, TeamWakeSource::McpShutdownRequest);
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
            model: None,
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
    async fn compute_wake_input_cold_start_injects_lead_role_prompt() {
        let session = start_session().await;
        // Seed one unread message. `send_message` flips status to Working —
        // that is the post-send path; here we want to exercise cold-start
        // detection, so write directly to the mailbox instead.
        session
            .mailbox
            .write("t1", "lead-1", "user", MailboxMessageType::Message, "kick off", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "lead-1", TeamRunTargetRole::Lead, 1).await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");

        assert_eq!(input.conversation_id, "c1");
        assert!(input.should_send);
        assert!(
            input.first_message.contains("You are the Team Leader"),
            "expected lead role prompt, got: {}",
            input.first_message
        );
        assert!(input.first_message.contains("kick off"));
        session.stop();
    }

    #[tokio::test]
    async fn compute_wake_input_cold_start_injects_teammate_role_prompt() {
        let session = start_session().await;
        session
            .mailbox
            .write("t1", "worker-1", "user", MailboxMessageType::Message, "do X", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "worker-1", TeamRunTargetRole::Teammate, 1).await;

        let input = session
            .compute_wake_input("worker-1")
            .await
            .unwrap()
            .expect("WakeInput");

        assert!(
            input.first_message.contains("## Team Governance"),
            "expected Team Governance in teammate role prompt, got: {}",
            input.first_message
        );
        assert!(
            input.first_message.contains("# You are a Team Member"),
            "expected teammate role prompt, got: {}",
            input.first_message
        );
        assert!(input.first_message.contains("do X"));
        session.stop();
    }

    #[tokio::test]
    async fn teammate_first_wake_uses_canonical_prompt() {
        let session = start_session().await;
        session
            .mailbox
            .write("t1", "worker-1", "user", MailboxMessageType::Message, "do X", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "worker-1", TeamRunTargetRole::Teammate, 1).await;

        let input = session
            .compute_wake_input("worker-1")
            .await
            .unwrap()
            .expect("WakeInput");
        let first_message = input.first_message;

        assert!(first_message.contains("## Team Governance"));
        assert!(first_message.contains("You MUST use the `team_*` MCP tools for ALL team coordination."));
        assert!(first_message.contains("Use team_send_message to report results to the leader"));
        assert!(first_message.contains("STOP GENERATING"));
        assert!(!first_message.contains(
            "You execute tasks assigned by the Lead Agent. Focus on completing your assigned work thoroughly and reporting back."
        ));
        assert!(first_message.contains("do X"));
        session.stop();
    }

    #[tokio::test]
    async fn compute_wake_input_warm_agent_skips_role_prompt() {
        let session = start_session().await;
        // Exit cold-start by setting a status once; any non-Error status
        // means the scheduler has seen this agent before.
        session
            .scheduler
            .set_status("lead-1", TeammateStatus::Idle)
            .await
            .unwrap();
        session
            .mailbox
            .write("t1", "lead-1", "user", MailboxMessageType::Message, "follow-up", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "lead-1", TeamRunTargetRole::Lead, 1).await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");

        assert!(input.should_send);
        assert!(
            !input.first_message.contains("Lead Agent of team"),
            "should not re-inject role prompt, got: {}",
            input.first_message
        );
        assert!(input.first_message.contains("follow-up"));
        session.stop();
    }

    #[tokio::test]
    async fn compute_wake_input_empty_mailbox_should_not_send() {
        let session = start_session().await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");

        assert!(!input.should_send);
        session.stop();
    }

    #[tokio::test]
    async fn compute_wake_input_refuses_unowned_mailbox_backlog() {
        let session = start_session().await;
        let lead = session.scheduler().find_lead_slot_id().await.expect("lead");
        session
            .mailbox()
            .write(
                session.team_id(),
                &lead,
                "worker-1",
                MailboxMessageType::Message,
                "recover this",
                None,
            )
            .await
            .expect("mailbox write");

        let input = session
            .compute_wake_input(&lead)
            .await
            .expect("compute should not fail");

        assert!(input.is_none(), "unowned unread mailbox must not start a turn");
    }

    #[tokio::test]
    async fn compute_wake_input_accepts_recovery_pending_wake() {
        let session = start_session().await;
        let lead = session.scheduler().find_lead_slot_id().await.expect("lead");
        session
            .mailbox()
            .write(
                session.team_id(),
                &lead,
                "worker-1",
                MailboxMessageType::Message,
                "recover this",
                None,
            )
            .await
            .expect("mailbox write");
        session
            .team_run_manager()
            .recover_mailbox_backlog(vec![RecoveryWakeCandidate {
                slot_id: lead.clone(),
                role: TeamRunTargetRole::Lead,
                unread_count: 1,
            }])
            .await
            .expect("recovery run");

        let input = session
            .compute_wake_input(&lead)
            .await
            .expect("compute")
            .expect("owned wake input");

        assert!(input.team_run_id.is_some());
        assert!(input.should_send);
        assert_eq!(input.wake_source, Some(TeamWakeSource::RecoveryDrain));
        assert!(input.trigger_message_id.is_none());
        assert_eq!(input.unread.len(), 1);
    }

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
            .expect("scan should not fail")
            .expect("recovery result");

        assert_eq!(result.recorded_wakes, vec![lead.clone()]);
        assert_eq!(result.source, TeamRunSource::RecoveryDrain);
        assert_eq!(result.pending_wake_count, 1);
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
        assert!(first.is_none(), "empty first scan must not create recovery work");

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
            second.is_none(),
            "ordinary new work must not create a second recovery scan"
        );
    }

    #[tokio::test]
    async fn compute_wake_input_returns_unread_rows_and_role_for_teammate() {
        let session = start_session().await;
        session
            .mailbox
            .write(
                "t1",
                "worker-1",
                "lead-1",
                MailboxMessageType::Message,
                "from lead",
                None,
            )
            .await
            .unwrap();
        session
            .mailbox
            .write("t1", "worker-1", "user", MailboxMessageType::Message, "from user", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "worker-1", TeamRunTargetRole::Teammate, 2).await;

        let input = session
            .compute_wake_input("worker-1")
            .await
            .unwrap()
            .expect("WakeInput");

        assert_eq!(input.unread.len(), 2);
        assert!(matches!(input.agent_role, TeammateRole::Teammate));
        assert!(input.unread.iter().any(|m| m.from_agent_id == "lead-1"));
        assert!(input.unread.iter().any(|m| m.from_agent_id == "user"));
        session.stop();
    }

    #[tokio::test]
    async fn compute_wake_input_returns_lead_role() {
        let session = start_session().await;
        session
            .mailbox
            .write("t1", "lead-1", "user", MailboxMessageType::Message, "hi lead", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "lead-1", TeamRunTargetRole::Lead, 1).await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");

        assert!(matches!(input.agent_role, TeammateRole::Lead));
        assert_eq!(input.unread.len(), 1);
        session.stop();
    }

    #[tokio::test]
    async fn mirror_unread_to_conversation_skips_when_service_weak_is_dangling_for_leader() {
        let session = start_session().await;
        session
            .mailbox
            .write(
                "t1",
                "lead-1",
                "worker-1",
                MailboxMessageType::Message,
                "lead-gets-this",
                None,
            )
            .await
            .unwrap();
        record_recovery_wake(&session, "lead-1", TeamRunTargetRole::Lead, 1).await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");

        // In unit tests, `service` is a dangling Weak — the mirror helper must
        // skip gracefully even for leader targets.
        session.mirror_unread_to_conversation(&input).await;
        session.stop();
    }

    #[tokio::test]
    async fn mirror_unread_to_conversation_skips_idle_notification_bubbles() {
        let store = Arc::new(RecordingProjectionStore::default());
        let session = start_session_with_projection_store(store.clone()).await;
        session
            .mailbox
            .write(
                "t1",
                "lead-1",
                "worker-1",
                MailboxMessageType::IdleNotification,
                "idle",
                Some("idle"),
            )
            .await
            .unwrap();
        record_recovery_wake(&session, "lead-1", TeamRunTargetRole::Lead, 1).await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");
        assert_eq!(
            input.unread.len(),
            1,
            "idle notification must still be delivered to wake payload"
        );

        session.mirror_unread_to_conversation(&input).await;

        assert!(
            store.inserted.lock().unwrap().is_empty(),
            "idle notification must not become a visible chat bubble"
        );
        session.stop();
    }

    #[tokio::test]
    async fn mirror_unread_to_conversation_still_projects_teammate_messages() {
        let store = Arc::new(RecordingProjectionStore::default());
        let session = start_session_with_projection_store(store.clone()).await;
        session
            .mailbox
            .write(
                "t1",
                "lead-1",
                "worker-1",
                MailboxMessageType::Message,
                "work is done",
                None,
            )
            .await
            .unwrap();
        record_recovery_wake(&session, "lead-1", TeamRunTargetRole::Lead, 1).await;

        let input = session.compute_wake_input("lead-1").await.unwrap().expect("WakeInput");
        session.mirror_unread_to_conversation(&input).await;

        let inserted = store.inserted.lock().unwrap();
        assert_eq!(inserted.len(), 1, "normal teammate message must remain visible");
        let content: serde_json::Value = serde_json::from_str(&inserted[0].content).unwrap();
        assert_eq!(content["content"], "work is done");
        session.stop();
    }

    #[tokio::test]
    async fn send_message_to_agent_survives_completion_between_projection_and_commit() {
        let store = Arc::new(CompletingProjectionStore::default());
        let session = start_session_with_projection_store(store.clone()).await;
        store.set_manager(session.team_run_manager().clone());
        session
            .team_run_manager()
            .accept_user_message("lead-1", TeamRunTargetRole::Lead, false, None)
            .await
            .expect("active run before user intervention");

        let ack = session
            .send_message_to_agent("worker-1", "race window message", None)
            .await
            .expect("lease should retain run while projection completes it");

        assert_eq!(ack.accepted_slot_id, "worker-1");
        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .expect("committed user intervention should be pending");
        assert_eq!(reservation.message_id, ack.message_id);
        session.stop();
    }

    #[tokio::test]
    async fn send_message_projection_failure_still_commits_wake() {
        let session = start_session_with_projection_store(Arc::new(FailingProjectionStore)).await;

        let ack = session
            .send_message_to_agent("worker-1", "projection can fail", None)
            .await
            .expect("projection failure is non-fatal");

        let reservation = session
            .team_run_manager()
            .claim_wake_for_turn("worker-1", TeamRunTargetRole::Teammate, "c2")
            .await
            .expect("wake should still be committed");
        assert_eq!(reservation.message_id, ack.message_id);
        session.stop();
    }

    #[tokio::test]
    async fn send_message_mailbox_failure_aborts_lease_and_allows_completion() {
        let repo: Arc<dyn ITeamRepository> = Arc::new(MockTeamRepo::with_message_write_failure());
        let session = start_session_with_repo_and_projection_store(repo, noop_projection_store()).await;

        let err = session
            .send_message("mailbox fails", None)
            .await
            .expect_err("mailbox write failure must be returned");
        assert!(
            err.to_string().contains("forced mailbox write failure"),
            "unexpected error: {err}"
        );

        let completed = session
            .team_run_manager()
            .maybe_complete()
            .await
            .expect("aborted lease should allow empty run to complete");
        assert_eq!(completed.status, TeamRunStatus::Completed);
        session.stop();
    }

    #[tokio::test]
    async fn mirror_unread_to_conversation_skips_when_service_weak_is_dangling() {
        let session = start_session().await;
        session
            .mailbox
            .write("t1", "worker-1", "lead-1", MailboxMessageType::Message, "do it", None)
            .await
            .unwrap();
        record_recovery_wake(&session, "worker-1", TeamRunTargetRole::Teammate, 1).await;

        let input = session
            .compute_wake_input("worker-1")
            .await
            .unwrap()
            .expect("WakeInput");

        // In unit tests, `service` is a dangling Weak — the mirror helper must
        // skip gracefully (no panic, no broadcast), leaving the wake path to
        // still forward `first_message` to the agent.
        session.mirror_unread_to_conversation(&input).await;
        session.stop();
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
            model: None,
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
