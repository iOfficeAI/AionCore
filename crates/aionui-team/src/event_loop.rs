use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use dashmap::DashMap;
use dashmap::mapref::entry::Entry;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use aionui_api_types::{TeamChildTurnPayload, TeamRunStatus};

use crate::events::{TEAM_CHILD_TURN_CANCELLED_EVENT, TEAM_CHILD_TURN_COMPLETED_EVENT, TEAM_CHILD_TURN_STARTED_EVENT};
use crate::mailbox::Mailbox;
use crate::ports::{
    AgentTurnExecutionError, AgentTurnExecutionPort, AgentTurnRequest, AgentTurnSource, AgentTurnStarted,
    AgentTurnStartedCallback,
};
use crate::scheduler::TeammateManager;
use crate::session::{PrepareBatchResult, TeamSession, WakeInput};
use crate::team_run::target_role_for;
use crate::types::TeammateStatus;
use crate::work_coordinator::{CommitResult, StartCommitResult, WorkBatch};
use crate::work_source::WorkSource;

pub struct EventLoopRegistry {
    notifiers: DashMap<String, Arc<Notify>>,
    handles: DashMap<String, JoinHandle<()>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
    lifecycle: Mutex<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventLoopRegistrationError {
    Duplicate,
    Stopped,
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
            lifecycle: Mutex::new(false),
        }
    }

    pub fn has(&self, slot_id: &str) -> bool {
        self.notifiers.contains_key(slot_id)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.notifiers.len()
    }

    pub fn notify(&self, slot_id: &str) {
        if let Some(notify) = self.notifiers.get(slot_id) {
            notify.notify_one();
        }
    }

    pub fn spawn(&self, slot_id: &str, ctx: AgentLoopContext) -> Result<(), EventLoopRegistrationError> {
        let stopped = self.lock_lifecycle();
        if *stopped {
            return Err(EventLoopRegistrationError::Stopped);
        }
        let notify = Arc::new(Notify::new());
        match self.notifiers.entry(slot_id.to_owned()) {
            Entry::Occupied(_) => {
                debug!(
                    team_id = %ctx.team_id,
                    slot_id,
                    "agent event loop registration ignored because slot is already registered"
                );
                return Err(EventLoopRegistrationError::Duplicate);
            }
            Entry::Vacant(entry) => {
                entry.insert(notify.clone());
            }
        }
        let handle = tokio::spawn(run_event_loop(notify, self.shutdown_rx.clone(), ctx));
        self.handles.insert(slot_id.to_owned(), handle);
        Ok(())
    }

    pub fn remove(&self, slot_id: &str) {
        let _lifecycle = self.lock_lifecycle();
        self.notifiers.remove(slot_id);
        if let Some((_, handle)) = self.handles.remove(slot_id) {
            handle.abort();
        }
    }

    pub fn shutdown(&self) {
        let mut stopped = self.lock_lifecycle();
        if *stopped {
            return;
        }
        *stopped = true;
        let _ = self.shutdown_tx.send(true);
        for entry in self.handles.iter() {
            entry.value().abort();
        }
        self.handles.clear();
        self.notifiers.clear();
    }

    fn lock_lifecycle(&self) -> MutexGuard<'_, bool> {
        self.lifecycle.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

pub struct AgentLoopContext {
    pub team_id: String,
    pub slot_id: String,
    pub user_id: String,
    pub session: Arc<TeamSession>,
    pub scheduler: Arc<TeammateManager>,
    pub mailbox: Arc<Mailbox>,
    pub turn_port: Arc<dyn AgentTurnExecutionPort>,
    pub registry: Arc<EventLoopRegistry>,
}

fn is_retryable_start_skip(error: &AgentTurnExecutionError) -> bool {
    matches!(error, AgentTurnExecutionError::Skipped { reason } if reason.contains("already running"))
}

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
        tokio::select! {
            biased;
            _ = shutdown_rx.wait_for(|stopped| *stopped) => return,
            _ = notify.notified() => {}
        }

        loop {
            if *shutdown_rx.borrow() {
                return;
            }
            match ctx.session.prepare_next_batch(&ctx.slot_id).await {
                Ok(PrepareBatchResult::Execute { batch, input }) => {
                    if execute_and_finalize(&ctx, *batch, input).await == ExecuteResult::WaitForSignal {
                        break;
                    }
                }
                Ok(PrepareBatchResult::SettleSignals { intent_ids }) => {
                    ctx.session.handle_signal_intents(&ctx.slot_id, &intent_ids).await;
                }
                Ok(
                    PrepareBatchResult::WaitingForCompletion
                    | PrepareBatchResult::Blocked
                    | PrepareBatchResult::Quiescent,
                ) => break,
                Err(error) => {
                    warn!(
                        team_id = %ctx.team_id,
                        slot_id = %ctx.slot_id,
                        error = %error,
                        "event loop reconcile failed"
                    );
                    break;
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecuteResult {
    ContinueDraining,
    WaitForSignal,
}

async fn execute_and_finalize(ctx: &AgentLoopContext, batch: WorkBatch, input: WakeInput) -> ExecuteResult {
    ctx.session.mirror_unread_to_conversation(&input).await;
    let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Working).await;

    let files = input
        .unread
        .iter()
        .filter_map(|message| message.files.as_ref())
        .flatten()
        .cloned()
        .collect();
    let unread_message_ids = input
        .unread
        .iter()
        .map(|message| message.id.clone())
        .collect::<Vec<_>>();
    let started_seen = Arc::new(AtomicBool::new(false));
    let coordinator = ctx.session.work_coordinator().clone();
    let cancellation_port = ctx.session.cancellation_port().clone();
    let user_id = ctx.user_id.clone();
    let conversation_id = input.conversation_id.clone();
    let callback_batch = batch.clone();
    let callback_started = started_seen.clone();
    let callback_emitter = ctx.session.team_event_emitter();
    let on_started: AgentTurnStartedCallback = Arc::new(move |started: AgentTurnStarted| {
        let coordinator = coordinator.clone();
        let cancellation_port = cancellation_port.clone();
        let user_id = user_id.clone();
        let conversation_id = conversation_id.clone();
        let batch = callback_batch.clone();
        let callback_started = callback_started.clone();
        let emitter = callback_emitter.clone();
        Box::pin(async move {
            callback_started.store(true, Ordering::SeqCst);
            match coordinator.mark_started(&batch, &started.turn_id) {
                StartCommitResult::Accepted => {
                    if let Some(team_run_id) = started.team_run_id {
                        emitter.broadcast_child_turn(
                            TEAM_CHILD_TURN_STARTED_EVENT,
                            TeamChildTurnPayload {
                                team_id: emitter.team_id().to_owned(),
                                team_run_id,
                                slot_id: started.slot_id,
                                role: started.role,
                                conversation_id: started.conversation_id,
                                turn_id: started.turn_id,
                                status: TeamRunStatus::Running,
                            },
                        );
                    }
                }
                StartCommitResult::CancelImmediately => {
                    if let Err(error) = cancellation_port
                        .cancel_agent_turn(&user_id, &conversation_id, &started.turn_id)
                        .await
                    {
                        warn!(
                            slot_id = %batch.slot_id,
                            batch_id = %batch.batch_id,
                            turn_id = %started.turn_id,
                            error = %error,
                            "late-start team batch cancellation failed"
                        );
                    }
                    coordinator.cancel_batch(&batch, "late_start_cancelled");
                    if let Some(team_run_id) = started.team_run_id {
                        emitter.broadcast_child_turn(
                            TEAM_CHILD_TURN_CANCELLED_EVENT,
                            TeamChildTurnPayload {
                                team_id: emitter.team_id().to_owned(),
                                team_run_id,
                                slot_id: started.slot_id,
                                role: started.role,
                                conversation_id: started.conversation_id,
                                turn_id: started.turn_id,
                                status: TeamRunStatus::Cancelled,
                            },
                        );
                    }
                }
                StartCommitResult::StaleOwner => {}
            }
        })
    });
    let request = AgentTurnRequest {
        team_run_id: batch.team_run_ids.first().cloned(),
        team_id: ctx.team_id.clone(),
        slot_id: ctx.slot_id.clone(),
        role: target_role_for(input.agent_role),
        conversation_id: input.conversation_id.clone(),
        user_id: ctx.user_id.clone(),
        content: input.first_message,
        files,
        source: AgentTurnSource::Mailbox {
            unread_count: unread_message_ids.len(),
            unread_message_ids,
        },
        on_started: Some(on_started),
    };

    let outcome = match ctx.turn_port.run_agent_turn(request).await {
        Ok(outcome) => outcome,
        Err(error) if !started_seen.load(Ordering::SeqCst) && is_retryable_start_skip(&error) => {
            ctx.session.work_coordinator().retry_start(&batch, "already_running");
            let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Idle).await;
            return ExecuteResult::WaitForSignal;
        }
        Err(error) => {
            warn!(
                team_id = %ctx.team_id,
                slot_id = %ctx.slot_id,
                batch_id = %batch.batch_id,
                error = %error,
                "agent turn start failed"
            );
            mark_batch_messages_read(ctx, &batch).await;
            ctx.session.work_coordinator().fail_batch(&batch, "turn_start_failed");
            let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Error).await;
            return ExecuteResult::ContinueDraining;
        }
    };

    mark_batch_messages_read(ctx, &batch).await;
    let terminal_status = if outcome.status.is_success() {
        (ctx.session.work_coordinator().complete_batch(&batch) == CommitResult::Committed)
            .then_some(TeamRunStatus::Completed)
    } else {
        let committed = ctx.session.work_coordinator().fail_batch(&batch, "turn_failed");
        let _ = ctx.scheduler.set_status(&ctx.slot_id, TeammateStatus::Error).await;
        (committed == CommitResult::Committed).then_some(TeamRunStatus::Failed)
    };
    if let Some(status) = terminal_status {
        let emitter = ctx.session.team_event_emitter();
        for team_run_id in &batch.team_run_ids {
            emitter.broadcast_child_turn(
                TEAM_CHILD_TURN_COMPLETED_EVENT,
                TeamChildTurnPayload {
                    team_id: ctx.team_id.clone(),
                    team_run_id: team_run_id.clone(),
                    slot_id: ctx.slot_id.clone(),
                    role: target_role_for(input.agent_role),
                    conversation_id: outcome.conversation_id.clone(),
                    turn_id: outcome.turn_id.clone(),
                    status: status.clone(),
                },
            );
        }
    }

    match ctx.scheduler.finalize_turn(&ctx.slot_id, &[]).await {
        Ok(Some(wake_target)) if wake_target != ctx.slot_id => {
            if let Err(error) = ctx
                .session
                .enqueue_leader_settle_signal(&wake_target, WorkSource::IdleNotification)
                .await
            {
                warn!(
                    team_id = %ctx.team_id,
                    slot_id = %ctx.slot_id,
                    wake_target,
                    error = %error,
                    "leader settle signal enqueue failed"
                );
            }
        }
        Ok(_) => {}
        Err(error) => warn!(
            team_id = %ctx.team_id,
            slot_id = %ctx.slot_id,
            error = %error,
            "scheduler turn finalization failed"
        ),
    }
    ExecuteResult::ContinueDraining
}

async fn mark_batch_messages_read(ctx: &AgentLoopContext, batch: &WorkBatch) {
    if batch.mailbox_message_ids.is_empty() {
        return;
    }
    if let Err(error) = ctx.mailbox.mark_read_batch(&batch.mailbox_message_ids).await {
        warn!(
            team_id = %ctx.team_id,
            slot_id = %ctx.slot_id,
            batch_id = %batch.batch_id,
            error = %error,
            "team batch mailbox terminal mark-read failed"
        );
    }
}

#[cfg(test)]
mod registry_lifecycle_tests {
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    use super::EventLoopRegistry;

    #[test]
    fn remove_waits_for_in_progress_registration_lifecycle() {
        let registry = Arc::new(EventLoopRegistry::new());
        let lifecycle = registry.lock_lifecycle();
        let (calling_tx, calling_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = Arc::clone(&registry);
        let thread = std::thread::spawn(move || {
            calling_tx.send(()).unwrap();
            worker.remove("worker-1");
            done_tx.send(()).unwrap();
        });
        calling_rx.recv().unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(lifecycle);
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        thread.join().unwrap();
    }
}
