use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};

use aionui_api_types::{CancellationState, ConversationRuntimeStateKind, ConversationRuntimeSummary};
use aionui_common::ConversationStatus;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::ConversationError;

#[derive(Debug, Default)]
pub struct ConversationRuntimeStateService {
    state: Mutex<ConversationRuntimeState>,
    release_notify: Notify,
}

#[derive(Debug, Default)]
struct ConversationRuntimeState {
    active_turns: HashMap<String, String>,
    active_tool_executions: HashMap<String, HashSet<String>>,
    deleting_conversations: HashSet<String>,
    cancelling_conversations: HashSet<String>,
    /// Cancels that arrived before the turn's agent registered, keyed by
    /// conversation and holding the turn they were meant for.
    deferred_cancels: HashMap<String, String>,
    cancellation_outcomes: HashMap<String, (String, CancellationState)>,
    restarting_conversations: HashSet<String>,
    /// The turn each (event, conversation) pair has already been reported for.
    logged_once_per_turn: HashMap<(OncePerTurn, String), String>,
    shutting_down: bool,
}

/// Events that are worth one log line per turn rather than one per attempt.
///
/// Both of these are reported from paths the cross-session drainer retries once
/// a second, so an unattended target turns each of them into hundreds of
/// identical lines. The FACT is per turn, not per attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OncePerTurn {
    /// A turn claim lost to the turn already running.
    ClaimRejected,
    /// A mid-turn write was refused because a confirmation card is pending.
    MidturnRefusal,
}

#[derive(Debug)]
pub struct TurnClaim {
    conversation_id: String,
    turn_id: String,
    state: Weak<ConversationRuntimeStateService>,
    released: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeLifecycleState {
    Active,
    Deleting,
    Cancelling,
    ShuttingDown,
}

impl ConversationRuntimeStateService {
    pub fn try_claim_turn(
        self: &Arc<Self>,
        conversation_id: &str,
        turn_id: &str,
    ) -> Result<TurnClaim, ConversationError> {
        let mut state = self.state.lock().map_err(|_| {
            warn!(
                conversation_id,
                turn_id, "conversation runtime state lock poisoned while claiming turn"
            );
            ConversationError::internal("conversation runtime state lock poisoned")
        })?;

        if state.shutting_down {
            info!(
                conversation_id,
                turn_id, "conversation runtime turn claim rejected because runtime is shutting down"
            );
            return Err(ConversationError::Busy {
                reason: "conversation runtime is shutting down".into(),
            });
        }

        if state.deleting_conversations.contains(conversation_id) {
            info!(
                conversation_id,
                turn_id, "conversation runtime turn claim rejected because conversation is deleting"
            );
            return Err(ConversationError::Busy {
                reason: format!("conversation {conversation_id} is being deleted"),
            });
        }

        if state.restarting_conversations.contains(conversation_id) {
            info!(
                conversation_id,
                turn_id, "conversation runtime turn claim rejected during restart"
            );
            return Err(ConversationError::RuntimeRestarting {
                conversation_id: conversation_id.to_owned(),
            });
        }

        if let Some(active_turn_id) = state.active_turns.get(conversation_id).cloned() {
            // Once per active turn, not once per attempt: the cross-session
            // drainer retries a queued delivery every second and loses the claim
            // identically each time, which is what turned this line into a
            // per-second stream.
            //
            // Keyed on the ACTIVE turn id, which is stable across those retries
            // — the REJECTED `turn_id` is freshly minted per attempt, so keying
            // on it would gate nothing. Inlined rather than calling
            // `should_log_once_for_turn`, which would deadlock on the lock this
            // scope is already holding.
            let worth_logging = {
                let key = (OncePerTurn::ClaimRejected, conversation_id.to_owned());
                let first = state
                    .logged_once_per_turn
                    .get(&key)
                    .is_none_or(|logged| *logged != active_turn_id);
                if first {
                    state.logged_once_per_turn.insert(key, active_turn_id.clone());
                }
                first
            };
            if worth_logging {
                info!(
                    conversation_id,
                    turn_id,
                    active_turn_id = %active_turn_id,
                    "conversation runtime turn claim rejected"
                );
            }
            return Err(ConversationError::Busy {
                reason: format!("conversation {conversation_id} is already running"),
            });
        }

        state
            .active_turns
            .insert(conversation_id.to_owned(), turn_id.to_owned());

        info!(conversation_id, turn_id, "conversation runtime turn claimed");

        Ok(TurnClaim {
            conversation_id: conversation_id.to_owned(),
            turn_id: turn_id.to_owned(),
            state: Arc::downgrade(self),
            released: false,
        })
    }

    pub fn is_claimed(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.active_turns.contains_key(conversation_id))
            .unwrap_or(false)
    }

    pub fn active_turn_id_for(&self, conversation_id: &str) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.active_turns.get(conversation_id).cloned())
    }

    pub fn update_tool_execution(&self, conversation_id: &str, execution_id: &str, active: bool) {
        let Ok(mut state) = self.state.lock() else {
            warn!(
                conversation_id,
                execution_id, "runtime state lock poisoned while tracking tool execution"
            );
            return;
        };
        if active {
            state
                .active_tool_executions
                .entry(conversation_id.to_owned())
                .or_default()
                .insert(execution_id.to_owned());
        } else if let Some(executions) = state.active_tool_executions.get_mut(conversation_id) {
            executions.remove(execution_id);
            if executions.is_empty() {
                state.active_tool_executions.remove(conversation_id);
            }
        }
    }

    pub fn has_active_tool_executions(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| {
                state
                    .active_tool_executions
                    .get(conversation_id)
                    .is_some_and(|ids| !ids.is_empty())
            })
            .unwrap_or(true)
    }

    pub fn mark_tools_force_terminated(&self, conversation_id: &str, turn_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                state.active_tool_executions.remove(conversation_id);
                state.cancelling_conversations.remove(conversation_id);
                state.cancellation_outcomes.insert(
                    conversation_id.to_owned(),
                    (turn_id.to_owned(), CancellationState::ForceTerminated),
                );
                drop(state);
                self.release_notify.notify_waiters();
            }
            Err(_) => warn!(
                conversation_id,
                turn_id, "runtime state lock poisoned while terminating tools"
            ),
        }
    }

    pub async fn wait_until_unclaimed(&self, conversation_id: &str) {
        loop {
            let notified = self.release_notify.notified();
            if !self.is_claimed(conversation_id) {
                return;
            }
            notified.await;
        }
    }

    pub fn mark_deleting(&self, conversation_id: &str) -> bool {
        match self.state.lock() {
            Ok(mut state) => {
                state.deleting_conversations.insert(conversation_id.to_owned());
                let active = state.active_turns.contains_key(conversation_id);
                info!(conversation_id, active, "conversation marked deleting");
                active
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while marking delete"
                );
                false
            }
        }
    }

    pub fn clear_deleting(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                state.deleting_conversations.remove(conversation_id);
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while clearing delete"
                );
            }
        }
    }

    pub fn is_deleting(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.deleting_conversations.contains(conversation_id))
            .unwrap_or(false)
    }

    pub fn mark_cancelling(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                state.cancelling_conversations.insert(conversation_id.to_owned());
                info!(conversation_id, "conversation marked cancelling");
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while marking cancel"
                );
            }
        }
    }

    pub fn clear_cancelling(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                state.cancelling_conversations.remove(conversation_id);
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while clearing cancel"
                );
            }
        }
    }

    /// Remember a cancel that arrived before the turn's agent registered.
    ///
    /// Deliberately NOT `mark_cancelling`: that flag is also set on the ordinary
    /// cancel path (where the agent was handed the request directly), so reusing
    /// it would make the orchestrator abort turns whose cancel is already being
    /// handled. This one is turn-scoped and consumed exactly once.
    pub fn defer_cancel(&self, conversation_id: &str, turn_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                state
                    .deferred_cancels
                    .insert(conversation_id.to_owned(), turn_id.to_owned());
                info!(conversation_id, turn_id, "cancel deferred until the agent registers");
            }
            Err(_) => warn!(
                conversation_id,
                "conversation runtime state lock poisoned while deferring cancel"
            ),
        }
    }

    /// Consume a deferred cancel for `turn_id`, if one is pending for it.
    ///
    /// A record left by an earlier turn must not stop a later one, so the turn
    /// id has to match.
    pub fn take_deferred_cancel(&self, conversation_id: &str, turn_id: &str) -> bool {
        match self.state.lock() {
            Ok(mut state) => match state.deferred_cancels.get(conversation_id) {
                Some(pending) if pending == turn_id => {
                    state.deferred_cancels.remove(conversation_id);
                    true
                }
                _ => false,
            },
            Err(_) => false,
        }
    }

    pub fn is_cancelling(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.cancelling_conversations.contains(conversation_id))
            .unwrap_or(false)
    }

    pub fn mark_force_terminated(&self, conversation_id: &str, turn_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                state.cancellation_outcomes.insert(
                    conversation_id.to_owned(),
                    (turn_id.to_owned(), CancellationState::ForceTerminated),
                );
            }
            Err(_) => warn!(
                conversation_id,
                turn_id, "conversation runtime state lock poisoned while marking forced cancellation"
            ),
        }
    }


    pub fn begin_restart(&self, conversation_id: &str) -> Result<(), ConversationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| ConversationError::internal("conversation runtime state lock poisoned"))?;
        if state.shutting_down {
            return Err(ConversationError::Busy {
                reason: "conversation runtime is shutting down".into(),
            });
        }
        if state.deleting_conversations.contains(conversation_id) {
            return Err(ConversationError::Busy {
                reason: format!("conversation {conversation_id} is being deleted"),
            });
        }
        if !state.restarting_conversations.insert(conversation_id.to_owned()) {
            return Err(ConversationError::RuntimeRestarting {
                conversation_id: conversation_id.to_owned(),
            });
        }
        info!(conversation_id, "conversation runtime marked restarting");
        Ok(())
    }

    pub fn should_log_once_for_turn(&self, event: OncePerTurn, conversation_id: &str, turn_id: &str) -> bool {
        match self.state.lock() {
            Ok(mut state) => {
                let key = (event, conversation_id.to_owned());
                if state
                    .logged_once_per_turn
                    .get(&key)
                    .is_some_and(|logged| logged == turn_id)
                {
                    return false;
                }
                state.logged_once_per_turn.insert(key, turn_id.to_owned());
                true
            }
            Err(_) => true,
        }
    }

    pub fn clear_restarting(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                if state.restarting_conversations.remove(conversation_id) {
                    info!(conversation_id, "conversation runtime restart gate released");
                    self.release_notify.notify_waiters();
                }
            }
            Err(_) => warn!(
                conversation_id,
                "conversation runtime state lock poisoned while clearing restart"
            ),
        }
    }

    pub fn is_restarting(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.restarting_conversations.contains(conversation_id))
            .unwrap_or(true)
    }

    pub fn clear_turn_state_for_restart(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                let had_active_turn = state.active_turns.remove(conversation_id).is_some();
                let had_cancelling = state.cancelling_conversations.remove(conversation_id);
                state.deferred_cancels.remove(conversation_id);
                if had_active_turn || had_cancelling {
                    info!(
                        conversation_id,
                        had_active_turn, had_cancelling, "conversation turn state cleared for restart"
                    );
                    self.release_notify.notify_waiters();
                }
            }
            Err(_) => warn!(
                conversation_id,
                "conversation runtime state lock poisoned while clearing restart turn"
            ),
        }
    }

    pub fn take_cancellation_outcome(&self, conversation_id: &str, turn_id: &str) -> Option<CancellationState> {
        let mut state = self.state.lock().ok()?;
        match state.cancellation_outcomes.get(conversation_id) {
            Some((recorded_turn_id, _)) if recorded_turn_id == turn_id => state
                .cancellation_outcomes
                .remove(conversation_id)
                .map(|(_, outcome)| outcome),
            _ => None,
        }
    }

    pub fn clear_conversation(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                let had_active_turn = state.active_turns.remove(conversation_id).is_some();
                let had_active_tools = state.active_tool_executions.remove(conversation_id).is_some();
                let had_deleting = state.deleting_conversations.remove(conversation_id);
                let had_cancelling = state.cancelling_conversations.remove(conversation_id);
                let had_restarting = state.restarting_conversations.remove(conversation_id);
                state.deferred_cancels.remove(conversation_id);
                state
                    .logged_once_per_turn
                    .retain(|(_, conversation), _| conversation != conversation_id);
                if had_active_turn || had_active_tools || had_deleting || had_cancelling || had_restarting {
                    info!(
                        conversation_id,
                        had_active_turn,
                        had_active_tools,
                        had_deleting,
                        had_cancelling,
                        "conversation runtime state cleared"
                    );
                    drop(state);
                    self.release_notify.notify_waiters();
                }
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while clearing conversation"
                );
            }
        }
    }

    pub fn mark_shutting_down(&self) -> usize {
        match self.state.lock() {
            Ok(mut state) => {
                state.shutting_down = true;
                let active_turn_count = state.active_turns.len();
                info!(active_turn_count, "conversation runtime marked shutting down");
                active_turn_count
            }
            Err(_) => {
                warn!("conversation runtime state lock poisoned while marking shutdown");
                0
            }
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.state.lock().map(|state| state.shutting_down).unwrap_or(true)
    }

    pub fn lifecycle_for(&self, conversation_id: &str) -> RuntimeLifecycleState {
        match self.state.lock() {
            Ok(state) => {
                if state.shutting_down {
                    RuntimeLifecycleState::ShuttingDown
                } else if state.deleting_conversations.contains(conversation_id) {
                    RuntimeLifecycleState::Deleting
                } else if state.cancelling_conversations.contains(conversation_id) {
                    RuntimeLifecycleState::Cancelling
                } else {
                    RuntimeLifecycleState::Active
                }
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while reading lifecycle"
                );
                RuntimeLifecycleState::ShuttingDown
            }
        }
    }

    pub fn summary_from_parts(
        &self,
        conversation_id: &str,
        task_status: Option<ConversationStatus>,
        has_task: bool,
        pending_confirmations: usize,
    ) -> ConversationRuntimeSummary {
        let (active_turn_id, cancelling) = self
            .state
            .lock()
            .map(|state| {
                (
                    state.active_turns.get(conversation_id).cloned(),
                    state.cancelling_conversations.contains(conversation_id),
                )
            })
            .unwrap_or((None, false));
        let claimed = active_turn_id.is_some();

        let state = if pending_confirmations > 0 {
            ConversationRuntimeStateKind::WaitingConfirmation
        } else if cancelling {
            ConversationRuntimeStateKind::Cancelling
        } else if claimed && task_status != Some(ConversationStatus::Running) {
            ConversationRuntimeStateKind::Starting
        } else if claimed || task_status == Some(ConversationStatus::Running) {
            ConversationRuntimeStateKind::Running
        } else {
            ConversationRuntimeStateKind::Idle
        };

        let is_processing = state != ConversationRuntimeStateKind::Idle;

        ConversationRuntimeSummary {
            state,
            can_send_message: !is_processing,
            has_task,
            task_status,
            is_processing,
            pending_confirmations,
            turn_id: active_turn_id,
        }
    }

    fn release(&self, conversation_id: &str, turn_id: &str) -> bool {
        match self.state.lock() {
            Ok(mut state) => {
                let removed = match state.active_turns.get(conversation_id) {
                    Some(active_turn_id) if active_turn_id == turn_id => {
                        state.active_turns.remove(conversation_id);
                        true
                    }
                    Some(active_turn_id) => {
                        info!(
                            conversation_id,
                            turn_id,
                            active_turn_id = %active_turn_id,
                            "conversation runtime turn claim release ignored because turn id mismatched"
                        );
                        false
                    }
                    None => false,
                };

                if !removed {
                    return false;
                }

                let was_deleting = state.deleting_conversations.remove(conversation_id);
                let was_cancelling = state.cancelling_conversations.remove(conversation_id);
                let tools_converged = state
                    .active_tool_executions
                    .get(conversation_id)
                    .is_none_or(HashSet::is_empty);
                if was_cancelling && tools_converged {
                    state
                        .cancellation_outcomes
                        .entry(conversation_id.to_owned())
                        .or_insert_with(|| (turn_id.to_owned(), CancellationState::ConvergedIdle));
                } else if was_cancelling {
                    state.cancelling_conversations.insert(conversation_id.to_owned());
                }
                info!(
                    conversation_id,
                    turn_id,
                    deleting = was_deleting,
                    "conversation runtime turn claim released"
                );
                drop(state);
                self.release_notify.notify_waiters();
                was_deleting
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    turn_id, "conversation runtime state lock poisoned while releasing turn"
                );
                false
            }
        }
    }
}

impl TurnClaim {
    pub fn turn_id(&self) -> &str {
        &self.turn_id
    }

    pub fn release(&mut self) -> bool {
        self.release_inner()
    }

    pub fn release_for_turn(&mut self, turn_id: &str) -> bool {
        if self.turn_id != turn_id {
            return false;
        }
        self.release_inner()
    }

    fn release_inner(&mut self) -> bool {
        if self.released {
            return false;
        }

        let was_deleting = self
            .state
            .upgrade()
            .map(|state| state.release(&self.conversation_id, &self.turn_id))
            .unwrap_or(false);
        self.released = true;
        was_deleting
    }
}

impl Drop for TurnClaim {
    fn drop(&mut self) {
        self.release_inner();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn claim_records_active_turn_id_in_summary() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-a")
            .expect("claim should be created");

        assert_eq!(state.active_turn_id_for("conv-1").as_deref(), Some("turn-a"));

        let summary = state.summary_from_parts("conv-1", None, false, 0);
        assert_eq!(summary.turn_id.as_deref(), Some("turn-a"));
        assert_eq!(summary.state, ConversationRuntimeStateKind::Starting);
    }

    #[test]
    fn releasing_wrong_turn_does_not_clear_active_claim() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state
            .try_claim_turn("conv-1", "turn-a")
            .expect("claim should be created");

        assert!(!claim.release_for_turn("turn-b"));
        assert!(state.is_claimed("conv-1"));
        assert_eq!(state.active_turn_id_for("conv-1").as_deref(), Some("turn-a"));

        assert!(!claim.release_for_turn("turn-a"));
        assert!(!state.is_claimed("conv-1"));
    }

    #[test]
    fn claim_rejects_second_active_turn() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("first claim should win");

        let err = state
            .try_claim_turn("conv-1", "turn-2")
            .expect_err("second claim should fail");
        assert!(err.to_string().contains("already running"));
    }

    #[test]
    fn claim_releases_on_drop() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        {
            let _claim = state
                .try_claim_turn("conv-1", "turn-1")
                .expect("claim should be created");
            assert!(state.is_claimed("conv-1"));
        }

        assert!(!state.is_claimed("conv-1"));
        assert!(state.try_claim_turn("conv-1", "turn-2").is_ok());
    }

    #[tokio::test]
    async fn wait_until_unclaimed_completes_after_active_claim_releases() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        let waiter = {
            let state = state.clone();
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                state.wait_until_unclaimed("conv-1").await;
                let _ = tx.send(());
            });
            rx
        };
        tokio::pin!(waiter);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut waiter)
                .await
                .is_err(),
            "waiter must stay pending while the claim is active"
        );

        let _ = claim.release();
        assert!(!state.is_claimed("conv-1"));
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("waiter should finish after release")
            .expect("waiter task should send completion");
    }

    #[test]
    fn deleting_rejects_new_turn_claims() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        state.mark_deleting("conv-1");

        let err = state
            .try_claim_turn("conv-1", "turn-1")
            .expect_err("deleting conversation should reject new turns");
        assert!(err.to_string().contains("being deleted"));
    }

    #[test]
    fn release_clears_deleting_flag_for_active_turn() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        state.mark_deleting("conv-1");
        assert!(state.is_deleting("conv-1"));

        assert!(claim.release());

        assert!(!state.is_deleting("conv-1"));
    }

    #[test]
    fn claim_rejects_when_shutting_down() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        state.mark_shutting_down();

        let err = state
            .try_claim_turn("conv-1", "turn-1")
            .expect_err("shutting down runtime should reject new turns");
        assert!(err.to_string().contains("shutting down"));
    }

    #[test]
    fn lifecycle_prioritizes_shutdown_over_conversation_flags() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        state.mark_deleting("conv-1");
        state.mark_cancelling("conv-1");
        assert_eq!(state.lifecycle_for("conv-1"), RuntimeLifecycleState::Deleting);

        state.mark_shutting_down();
        assert_eq!(state.lifecycle_for("conv-1"), RuntimeLifecycleState::ShuttingDown);
    }

    #[test]
    fn release_clears_cancelling_flag_for_active_turn() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        state.mark_cancelling("conv-1");
        assert!(state.is_cancelling("conv-1"));

        assert!(!claim.release());

        assert!(!state.is_cancelling("conv-1"));
    }

    #[test]
    fn cancellation_release_records_terminal_outcome_once() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state.try_claim_turn("conv-1", "turn-a").unwrap();
        state.mark_cancelling("conv-1");
        claim.release();

        assert_eq!(
            state.take_cancellation_outcome("conv-1", "turn-a"),
            Some(CancellationState::ConvergedIdle)
        );
        assert_eq!(state.take_cancellation_outcome("conv-1", "turn-a"), None);
    }

    #[test]
    fn cancellation_does_not_converge_while_tool_execution_is_active() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state.try_claim_turn("conv-1", "turn-a").unwrap();
        state.update_tool_execution("conv-1", "exec-1", true);
        state.mark_cancelling("conv-1");
        claim.release();

        assert!(state.is_cancelling("conv-1"));
        assert_eq!(state.take_cancellation_outcome("conv-1", "turn-a"), None);
        state.mark_tools_force_terminated("conv-1", "turn-a");
        assert!(!state.has_active_tool_executions("conv-1"));
        assert_eq!(
            state.take_cancellation_outcome("conv-1", "turn-a"),
            Some(CancellationState::ForceTerminated)
        );
    }

    #[test]
    fn forced_cancellation_is_not_downgraded_when_claim_releases() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state.try_claim_turn("conv-1", "turn-a").unwrap();
        state.mark_cancelling("conv-1");
        state.mark_force_terminated("conv-1", "turn-a");
        claim.release();

        assert_eq!(
            state.take_cancellation_outcome("conv-1", "turn-a"),
            Some(CancellationState::ForceTerminated)
        );
    }

    #[test]
    fn clear_conversation_removes_active_turn_and_lifecycle_flags() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        state.mark_deleting("conv-1");
        state.mark_cancelling("conv-1");
        state.clear_conversation("conv-1");

        assert!(!state.is_claimed("conv-1"));
        assert!(!state.is_deleting("conv-1"));
        assert!(!state.is_cancelling("conv-1"));
        assert!(state.active_turn_id_for("conv-1").is_none());
    }

    #[test]
    fn summary_uses_claim_as_starting_state() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", None, false, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Starting);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn summary_waiting_confirmation_takes_priority() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 1);

        assert_eq!(summary.state, ConversationRuntimeStateKind::WaitingConfirmation);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn cancelling_summary_keeps_processing_and_disables_send() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-a")
            .expect("claim should be created");
        state.mark_cancelling("conv-1");

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Cancelling);
        assert_eq!(summary.turn_id.as_deref(), Some("turn-a"));
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn summary_uses_running_task_without_claim() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Running);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn summary_idle_when_no_claim_running_task_or_confirmation() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Finished), true, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Idle);
        assert!(!summary.is_processing);
        assert!(summary.can_send_message);
    }

    // ── once-per-turn log gate ─────────────────────────────────────────

    /// The cross-session drainer retries a queued delivery once a SECOND and
    /// each retry re-reports the same rejection. Measured live: one unanswered
    /// confirmation card produced exactly 600 identical `mid-turn delivery
    /// refused` lines over a 10-minute TTL, and the claim rejection underneath
    /// it flooded at the same rate. The event is worth one entry per occasion,
    /// so the gate lets a turn's first attempt through and silences the rest.
    #[test]
    fn an_events_first_attempt_in_a_turn_is_worth_logging_once() {
        let state = ConversationRuntimeStateService::default();

        assert!(
            state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"),
            "first attempt"
        );
        assert!(
            !state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"),
            "the drainer's retries add no information"
        );
        assert!(
            !state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"),
            "still silent however long the card stays unanswered"
        );
    }

    /// A new turn is a new occasion — otherwise a conversation that hits the
    /// same situation tomorrow would be silently un-diagnosable.
    #[test]
    fn a_later_turn_logs_again() {
        let state = ConversationRuntimeStateService::default();

        assert!(state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"));
        assert!(state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-2"));
        assert!(!state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-2"));
    }

    #[test]
    fn conversations_are_gated_independently() {
        let state = ConversationRuntimeStateService::default();

        assert!(state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"));
        assert!(
            state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-2", "turn-1"),
            "another conversation's turn id must not silence this one"
        );
    }

    /// Two different events during the SAME turn each deserve a line: "the claim
    /// lost to a turn already running" and "a mid-turn write was refused because
    /// a card is pending" are different facts about that turn.
    #[test]
    fn different_events_in_one_turn_are_gated_independently() {
        let state = ConversationRuntimeStateService::default();

        assert!(state.should_log_once_for_turn(OncePerTurn::ClaimRejected, "conv-1", "turn-1"));
        assert!(
            state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"),
            "a different event in the same turn is a different fact"
        );
        assert!(!state.should_log_once_for_turn(OncePerTurn::ClaimRejected, "conv-1", "turn-1"));
        assert!(!state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"));
    }

    /// `try_claim_turn` cannot call `should_log_once_for_turn` — it already holds
    /// the lock that method takes — so it inlines the same compare-and-set. This
    /// pins that copy: a rejected claim must SPEND the gate for the active turn,
    /// or the inlined logic has drifted and the line floods again.
    #[test]
    fn a_rejected_claim_spends_its_own_log_gate() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state.try_claim_turn("conv-1", "turn-1").expect("first claim wins");

        assert!(
            state.try_claim_turn("conv-1", "turn-2").is_err(),
            "a second claim loses while turn-1 runs"
        );

        assert!(
            !state.should_log_once_for_turn(OncePerTurn::ClaimRejected, "conv-1", "turn-1"),
            "the rejection already reported itself for this active turn"
        );
        assert!(
            state.should_log_once_for_turn(OncePerTurn::ClaimRejected, "conv-1", "turn-9"),
            "a different active turn is a new occasion"
        );
    }

    /// Clearing a conversation drops its gates with the rest of its state, so a
    /// deleted-then-recreated id does not inherit a stale one — and it must not
    /// take another conversation's gate with it.
    #[test]
    fn clearing_a_conversation_forgets_only_its_own_gates() {
        let state = ConversationRuntimeStateService::default();

        assert!(state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"));
        assert!(state.should_log_once_for_turn(OncePerTurn::ClaimRejected, "conv-2", "turn-1"));

        state.clear_conversation("conv-1");

        assert!(
            state.should_log_once_for_turn(OncePerTurn::MidturnRefusal, "conv-1", "turn-1"),
            "conv-1 forgot"
        );
        assert!(
            !state.should_log_once_for_turn(OncePerTurn::ClaimRejected, "conv-2", "turn-1"),
            "conv-2 still remembers"
        );
    }
}
