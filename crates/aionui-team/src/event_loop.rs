use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::mailbox::Mailbox;
use crate::ports::{
    AgentTurnExecutionPort, AgentTurnRequest, AgentTurnSource, AgentTurnStarted, AgentTurnStartedCallback,
};
use crate::scheduler::TeammateManager;
use crate::session::TeamSession;
use crate::team_run::{ActiveChildTurn, ChildStartDecision, target_role_for};
use crate::types::TeammateStatus;
use crate::wake::TeamWakeSource;
use aionui_api_types::TeamRunStatus;

/// Registry of per-agent Notify handles. Used by any trigger source to poke
/// an agent's event loop without needing to know its internals.
pub struct EventLoopRegistry {
    notifiers: DashMap<String, Arc<Notify>>,
    handles: DashMap<String, JoinHandle<()>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
}

impl Default for EventLoopRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLoopRegistry {
    pub fn new() -> Self {
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        Self {
            notifiers: DashMap::new(),
            handles: DashMap::new(),
            shutdown_tx,
            shutdown_rx,
        }
    }

    /// Check if an event loop is registered for this slot.
    pub fn has(&self, slot_id: &str) -> bool {
        self.notifiers.contains_key(slot_id)
    }

    /// Poke the named agent's event loop so it drains its mailbox.
    pub fn notify(&self, slot_id: &str) {
        if let Some(n) = self.notifiers.get(slot_id) {
            n.notify_one();
        }
    }

    /// Register and spawn an event loop for one agent.
    pub fn spawn(&self, slot_id: &str, ctx: AgentLoopContext) -> bool {
        let notify = Arc::new(Notify::new());
        match self.notifiers.entry(slot_id.to_owned()) {
            Entry::Occupied(_) => {
                debug!(
                    team_id = %ctx.team_id,
                    slot_id,
                    "agent event loop registration ignored because slot is already registered"
                );
                return false;
            }
            Entry::Vacant(entry) => {
                entry.insert(notify.clone());
            }
        }
        let handle = tokio::spawn(run_event_loop(notify, self.shutdown_rx.clone(), ctx));
        self.handles.insert(slot_id.to_owned(), handle);
        true
    }

    /// Remove an agent's event loop (agent removed from team).
    pub fn remove(&self, slot_id: &str) {
        self.notifiers.remove(slot_id);
        if let Some((_, handle)) = self.handles.remove(slot_id) {
            handle.abort();
        }
    }

    /// Shut down all event loops.
    pub fn shutdown(&self) {
        let _ = self.shutdown_tx.send(true);
        for entry in self.handles.iter() {
            entry.value().abort();
        }
        self.handles.clear();
        self.notifiers.clear();
    }
}

/// Context shared across all iterations of one agent's event loop.
pub struct AgentLoopContext {
    pub team_id: String,
    pub slot_id: String,
    pub user_id: String,
    pub session: Arc<TeamSession>,
    pub scheduler: Arc<TeammateManager>,
    pub mailbox: Arc<Mailbox>,
    pub turn_port: Arc<dyn AgentTurnExecutionPort>,
    /// Used to notify other agents' event loops (e.g. leader after all-settled).
    pub registry: Arc<EventLoopRegistry>,
}

struct TurnExecution {
    finish_ok: bool,
    team_run_id: Option<String>,
    turn_id: Option<String>,
}

/// The event loop for one agent slot. Spawned as a tokio task.
///
/// Flow:
/// 1. Wait for signal (notify) or shutdown.
/// 2. Drain loop: compute_wake_input → has messages → send_message (blocking) → finalize → repeat.
/// 3. When mailbox empty → back to step 1.
async fn run_event_loop(
    notify: Arc<Notify>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ctx: AgentLoopContext,
) {
    info!(
        team_id = %ctx.team_id,
        slot_id = %ctx.slot_id,
        "agent event loop started"
    );

    loop {
        // Step 1: wait for signal or shutdown
        tokio::select! {
            biased;
            _ = shutdown_rx.wait_for(|v| *v) => {
                info!(
                    team_id = %ctx.team_id,
                    slot_id = %ctx.slot_id,
                    "agent event loop shutting down"
                );
                return;
            }
            _ = notify.notified() => {}
        }

        // Drain loop: keep processing until mailbox is empty
        loop {
            if *shutdown_rx.borrow() {
                return;
            }

            let input = match ctx.session.compute_wake_input(&ctx.slot_id).await {
                Ok(Some(input)) => input,
                Ok(None) => break,
                Err(e) => {
                    warn!(
                        team_id = %ctx.team_id,
                        slot_id = %ctx.slot_id,
                        error = %e,
                        "event loop: compute_wake_input failed"
                    );
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    break;
                }
            };

            if !input.should_send {
                ctx.session
                    .team_run_manager()
                    .record_empty_wake_observed(&ctx.slot_id)
                    .await;
                break;
            }

            match execute_turn(&ctx, &input).await {
                Some(turn) => finalize_turn(&ctx, turn, &input).await,
                None => break, // Turn not started (guard/warmup); retry on next signal
            }
        }
    }
}

/// Execute one agent turn through the Team-defined port. Conversation/runtime
/// lifecycle remains behind the port; Team keeps projection, mark-read, and
/// scheduler finalization here.
async fn execute_turn(ctx: &AgentLoopContext, input: &crate::session::WakeInput) -> Option<TurnExecution> {
    let role = target_role_for(input.agent_role);
    let reservation = if input.team_run_id.is_some() {
        match ctx
            .session
            .team_run_manager()
            .claim_wake_for_turn(&ctx.slot_id, role.clone(), &input.conversation_id)
            .await
        {
            Some(reservation) => {
                if let Some(source) = input.wake_source {
                    info!(
                        team_id = %ctx.team_id,
                        team_run_id = ?input.team_run_id,
                        slot_id = %ctx.slot_id,
                        wake_source = %source,
                        trigger_message_id = ?input.trigger_message_id.as_deref(),
                        "team priority wake claimed"
                    );
                }
                Some(reservation)
            }
            None => {
                warn!(
                    team_id = %ctx.team_id,
                    team_run_id = ?input.team_run_id,
                    slot_id = %ctx.slot_id,
                    conversation_id = %input.conversation_id,
                    "event loop: team run wake skipped because reservation could not be claimed"
                );
                if let Err(e) = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Idle).await {
                    warn!(
                        team_id = %ctx.team_id,
                        slot_id = %ctx.slot_id,
                        error = %e,
                        "event loop: failed to roll back status after reservation claim failure"
                    );
                }
                return None;
            }
        }
    } else {
        None
    };

    ctx.session.mirror_unread_to_conversation(input).await;

    let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Working).await;

    let files: Vec<String> = input
        .unread
        .iter()
        .filter_map(|m| m.files.as_ref())
        .flatten()
        .cloned()
        .collect();

    let unread_message_ids = input.unread.iter().map(|m| m.id.clone()).collect::<Vec<_>>();
    let started_seen = Arc::new(AtomicBool::new(false));
    let on_started: Option<AgentTurnStartedCallback> = reservation.clone().map(|reservation| {
        let team_run_manager = ctx.session.team_run_manager().clone();
        let cancellation_port = ctx.session.cancellation_port().clone();
        let user_id = ctx.user_id.clone();
        let started_seen = started_seen.clone();
        Arc::new(move |started: AgentTurnStarted| {
            let team_run_manager = team_run_manager.clone();
            let cancellation_port = cancellation_port.clone();
            let user_id = user_id.clone();
            let reservation_id = reservation.reservation_id.clone();
            started_seen.store(true, Ordering::SeqCst);
            Box::pin(async move {
                let child = ActiveChildTurn {
                    team_run_id: started.team_run_id,
                    slot_id: started.slot_id,
                    role: started.role,
                    conversation_id: started.conversation_id,
                    turn_id: started.turn_id,
                };
                match team_run_manager
                    .record_child_started(&reservation_id, child.clone())
                    .await
                {
                    ChildStartDecision::Accepted => {}
                    ChildStartDecision::CancelImmediately(child) => {
                        if let Err(err) = cancellation_port
                            .cancel_agent_turn(&user_id, &child.conversation_id, &child.turn_id)
                            .await
                        {
                            warn!(
                                team_run_id = %child.team_run_id,
                                slot_id = %child.slot_id,
                                conversation_id = %child.conversation_id,
                                turn_id = %child.turn_id,
                                error = %err,
                                "event loop: late-start child cancellation failed"
                            );
                        }
                        team_run_manager.record_child_cancelled(&child).await;
                        team_run_manager.try_complete_cancelled().await;
                    }
                    ChildStartDecision::Ignored => {}
                }
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }) as AgentTurnStartedCallback
    });
    let request = AgentTurnRequest {
        team_run_id: input.team_run_id.clone(),
        team_id: ctx.team_id.clone(),
        slot_id: ctx.slot_id.clone(),
        role,
        conversation_id: input.conversation_id.clone(),
        user_id: ctx.user_id.clone(),
        content: input.first_message.clone(),
        files,
        source: AgentTurnSource::Mailbox {
            unread_count: input.unread.len(),
            unread_message_ids,
        },
        on_started,
    };

    info!(
        team_id = %ctx.team_id,
        team_run_id = ?input.team_run_id,
        slot_id = %ctx.slot_id,
        conversation_id = %input.conversation_id,
        "event loop: agent turn port call started"
    );
    let outcome = match ctx.turn_port.run_agent_turn(request).await {
        Ok(outcome) => outcome,
        Err(e) => {
            warn!(
                team_id = %ctx.team_id,
                team_run_id = ?input.team_run_id,
                slot_id = %ctx.slot_id,
                conversation_id = %input.conversation_id,
                error = %e,
                outcome = "failed",
                "event loop: agent turn port call failed"
            );
            if input.team_run_id.is_some()
                && let Some(reservation) = reservation.as_ref()
            {
                if started_seen.load(Ordering::SeqCst) {
                    ctx.session.team_run_manager().complete_failed().await;
                } else {
                    ctx.session
                        .team_run_manager()
                        .record_child_start_failed(&reservation.reservation_id, &e.to_string())
                        .await;
                }
            }
            return Some(TurnExecution {
                finish_ok: false,
                team_run_id: input.team_run_id.clone(),
                turn_id: None,
            });
        }
    };

    let turn_ok = outcome.status.is_success();
    info!(
        team_id = %ctx.team_id,
        team_run_id = ?input.team_run_id,
        slot_id = %ctx.slot_id,
        conversation_id = %outcome.conversation_id,
        turn_id = %outcome.turn_id,
        outcome = ?outcome.status,
        "event loop: agent turn port call completed"
    );

    let msg_ids: Vec<String> = input.unread.iter().map(|m| m.id.clone()).collect();
    if !msg_ids.is_empty()
        && let Err(e) = ctx.mailbox.mark_read_batch(&msg_ids).await
    {
        warn!(
            team_id = %ctx.team_id,
            slot_id = %ctx.slot_id,
            error = %e,
            "event loop: mark_read_batch failed (non-fatal)"
        );
    }

    Some(TurnExecution {
        finish_ok: turn_ok,
        team_run_id: input.team_run_id.clone(),
        turn_id: Some(outcome.turn_id),
    })
}

/// Finalize a completed turn: mark idle (or error), cascade to leader.
async fn finalize_turn(ctx: &AgentLoopContext, turn: TurnExecution, input: &crate::session::WakeInput) {
    if !turn.finish_ok {
        let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Error).await;
    }
    match ctx.scheduler.finalize_turn(&ctx.slot_id, &[]).await {
        Ok(Some(wake_target)) => {
            if wake_target != ctx.slot_id
                && let Err(e) = ctx
                    .session
                    .wake_agent_for_team_work(&wake_target, TeamWakeSource::IdleNotification, None)
                    .await
            {
                warn!(
                    team_id = %ctx.team_id,
                    slot_id = %ctx.slot_id,
                    wake_target = %wake_target,
                    error = %e,
                    "event loop: failed to wake leader after teammate finalize"
                );
            }
        }
        Ok(None) => {}
        Err(e) => {
            warn!(
                team_id = %ctx.team_id,
                slot_id = %ctx.slot_id,
                error = %e,
                "event loop: finalize_turn failed"
            );
        }
    }
    if let (Some(_team_run_id), Some(turn_id)) = (turn.team_run_id, turn.turn_id) {
        let status = if turn.finish_ok {
            TeamRunStatus::Completed
        } else {
            TeamRunStatus::Failed
        };
        ctx.session
            .team_run_manager()
            .record_child_completed(&ctx.slot_id, &turn_id, status)
            .await;
        ctx.session.team_run_manager().maybe_complete().await;
    }
    let should_release_suppressed_wake = input.wake_source.is_some_and(TeamWakeSource::resumes_paused_slot);
    if should_release_suppressed_wake {
        let role = target_role_for(input.agent_role);
        if ctx
            .session
            .team_run_manager()
            .release_suppressed_wake_if_resumed(&ctx.slot_id, role)
            .await
            .is_some()
        {
            ctx.registry.notify(&ctx.slot_id);
        }
    }
}
