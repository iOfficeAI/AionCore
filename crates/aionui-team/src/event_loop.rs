use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::mailbox::Mailbox;
use crate::ports::{
    AgentTurnExecutionPort, AgentTurnRequest, AgentTurnSource, AgentTurnStarted, AgentTurnStartedCallback,
};
use crate::scheduler::TeammateManager;
use crate::session::TeamSession;
use crate::team_run::{ActiveChildTurn, target_role_for};
use crate::types::TeammateStatus;
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
    pub fn spawn(&self, slot_id: &str, ctx: AgentLoopContext) {
        let notify = Arc::new(Notify::new());
        self.notifiers.insert(slot_id.to_owned(), notify.clone());
        let handle = tokio::spawn(run_event_loop(notify, self.shutdown_rx.clone(), ctx));
        self.handles.insert(slot_id.to_owned(), handle);
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
                ctx.session.team_run_manager().record_wake_consumed().await;
                ctx.session.team_run_manager().maybe_complete().await;
                break;
            }

            match execute_turn(&ctx, &input).await {
                Some(turn) => finalize_turn(&ctx, turn, &input.conversation_id).await,
                None => break, // Turn not started (guard/warmup); retry on next signal
            }
        }
    }
}

/// Execute one agent turn through the Team-defined port. Conversation/runtime
/// lifecycle remains behind the port; Team keeps projection, mark-read, and
/// scheduler finalization here.
async fn execute_turn(ctx: &AgentLoopContext, input: &crate::session::WakeInput) -> Option<TurnExecution> {
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
    let role = target_role_for(input.agent_role);
    let on_started: Option<AgentTurnStartedCallback> = input.team_run_id.as_ref().map(|_| {
        let team_run_manager = ctx.session.team_run_manager().clone();
        Arc::new(move |started: AgentTurnStarted| {
            let team_run_manager = team_run_manager.clone();
            Box::pin(async move {
                team_run_manager
                    .record_child_started(ActiveChildTurn {
                        team_run_id: started.team_run_id,
                        slot_id: started.slot_id,
                        role: started.role,
                        conversation_id: started.conversation_id,
                        turn_id: started.turn_id,
                    })
                    .await;
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
            if input.team_run_id.is_some() {
                ctx.session.team_run_manager().complete_failed().await;
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
async fn finalize_turn(ctx: &AgentLoopContext, turn: TurnExecution, _conversation_id: &str) {
    if !turn.finish_ok {
        let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Error).await;
    }
    match ctx.scheduler.finalize_turn(&ctx.slot_id, &[]).await {
        Ok(Some(wake_target)) => {
            if wake_target != ctx.slot_id {
                ctx.session.team_run_manager().record_pending_wake().await;
                ctx.registry.notify(&wake_target);
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
}
