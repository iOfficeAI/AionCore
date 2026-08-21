use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};

use aionui_api_types::{ConversationRuntimeStateKind, ConversationRuntimeSummary};
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
    deleting_conversations: HashSet<String>,
    cancelling_conversations: HashSet<String>,
    restarting_conversations: HashSet<String>,
    /// Cancels that arrived before the turn's agent registered, keyed by
    /// conversation and holding the turn they were meant for.
    deferred_cancels: HashMap<String, String>,
    /// The turn each (event, conversation) pair has already been reported for.
    /// One entry per pair, overwritten as turns advance, so it cannot grow with
    /// time. See [`ConversationRuntimeStateService::should_log_once_for_turn`].
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

    /// Claim a turn the AGENT started on its own (a CLI-initiated turn, or the
    /// follow-up turn claude opens for a mid-turn message it could not fold into
    /// the running one — verified in the design spec §6甲.2).
    ///
    /// Returns `None` when a turn is already claimed: for an agent-driven turn
    /// that is the NORMAL case (the message folded into the running turn), not an
    /// error, so it must not surface as 409.
    pub fn claim_for_agent_turn(self: &Arc<Self>, conversation_id: &str, turn_id: &str) -> Option<TurnClaim> {
        self.try_claim_turn(conversation_id, turn_id).ok()
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
            // Same condition the turn/config gates report, so it carries the same
            // code: a caller retrying on `runtime_restarting` needs one rule, not
            // one per entry point.
            return Err(ConversationError::RuntimeRestarting {
                conversation_id: conversation_id.to_owned(),
            });
        }
        info!(conversation_id, "conversation runtime marked restarting");
        Ok(())
    }

    /// Whether this event is still worth an `info` line for this turn.
    ///
    /// True once per (event, conversation, turn), then false. Both events it
    /// guards sit on paths the cross-session drainer retries every second, so
    /// an unattended target reprints them indefinitely: measured live, a single
    /// unanswered confirmation card produced exactly 600 identical lines over a
    /// 10-minute TTL. Repeats carry no information — the first line already
    /// names the conversation and the turn — so they are dropped here rather
    /// than downgraded to `debug`, which would merely move the flood into
    /// development, where that level is on.
    ///
    /// `turn_id` must be the ACTIVE turn, which is stable across retries. The
    /// claim rejection also has a rejected turn id, but the drainer mints a
    /// fresh one per attempt, so keying on that would gate nothing.
    ///
    /// A poisoned lock reports `true`: losing the gate means a noisy log, while
    /// losing the line means a silent rejection, and noisy beats silent.
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

    /// Clear turn-scoped state after the old process has been terminated while
    /// preserving the restart gate until the replacement runtime is ready.
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

    pub fn clear_conversation(&self, conversation_id: &str) {
        match self.state.lock() {
            Ok(mut state) => {
                let had_active_turn = state.active_turns.remove(conversation_id).is_some();
                let had_deleting = state.deleting_conversations.remove(conversation_id);
                let had_cancelling = state.cancelling_conversations.remove(conversation_id);
                let had_restarting = state.restarting_conversations.remove(conversation_id);
                state.deferred_cancels.remove(conversation_id);
                state
                    .logged_once_per_turn
                    .retain(|(_, conversation), _| conversation != conversation_id);
                if had_active_turn || had_deleting || had_cancelling || had_restarting {
                    info!(
                        conversation_id,
                        had_active_turn,
                        had_deleting,
                        had_cancelling,
                        had_restarting,
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
        supports_midturn_delivery: bool,
    ) -> ConversationRuntimeSummary {
        let (active_turn_id, cancelling, restarting) = self
            .state
            .lock()
            .map(|state| {
                (
                    state.active_turns.get(conversation_id).cloned(),
                    state.cancelling_conversations.contains(conversation_id),
                    state.restarting_conversations.contains(conversation_id),
                )
            })
            .unwrap_or((None, false, true));
        let claimed = active_turn_id.is_some();

        let state = if restarting {
            ConversationRuntimeStateKind::Restarting
        } else if pending_confirmations > 0 {
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
            supports_midturn_delivery,
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
                state.cancelling_conversations.remove(conversation_id);
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

        let summary = state.summary_from_parts("conv-1", None, false, 0, false);
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
    fn a_midturn_send_does_not_mint_a_phantom_turn_id() {
        let svc = Arc::new(ConversationRuntimeStateService::default());
        let _claim = svc.try_claim_turn("conv-1", "t-1").expect("first claim");
        // A second send while t-1 runs must NOT create a second claim, and the
        // summary must keep reporting the REAL active turn.
        assert!(svc.claim_for_agent_turn("conv-1", "t-2").is_none());
        assert_eq!(svc.active_turn_id_for("conv-1").as_deref(), Some("t-1"));
    }

    #[test]
    fn an_agent_started_turn_claims_after_the_previous_one_released() {
        let svc = Arc::new(ConversationRuntimeStateService::default());
        let claim = svc.try_claim_turn("conv-1", "t-1").expect("first claim");
        drop(claim);
        let adopted = svc.claim_for_agent_turn("conv-1", "t-2").expect("adopted");
        assert_eq!(svc.active_turn_id_for("conv-1").as_deref(), Some("t-2"));
        drop(adopted);
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
    fn restart_gate_rejects_turns_and_duplicate_restarts_until_released() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        state.begin_restart("conv-1").expect("first restart should claim gate");

        let duplicate = state
            .begin_restart("conv-1")
            .expect_err("duplicate restart should be rejected");
        assert!(matches!(duplicate, ConversationError::RuntimeRestarting { .. }));
        assert_eq!(duplicate.error_code(), "runtime_restarting");
        let turn = state
            .try_claim_turn("conv-1", "turn-1")
            .expect_err("send must be rejected while restart owns the runtime");
        // Asserted by CODE, not message text: clients gate on the code, so the
        // wording must stay free to change without breaking them.
        assert!(matches!(turn, ConversationError::RuntimeRestarting { .. }));
        assert_eq!(turn.error_code(), "runtime_restarting");

        let summary = state.summary_from_parts("conv-1", None, false, 0, false);
        assert_eq!(summary.state, ConversationRuntimeStateKind::Restarting);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);

        state.clear_restarting("conv-1");
        assert!(state.try_claim_turn("conv-1", "turn-2").is_ok());
    }

    #[test]
    fn clearing_old_turn_for_restart_preserves_restart_gate() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-before-restart")
            .expect("turn should be active before restart");
        state.mark_cancelling("conv-1");
        state.begin_restart("conv-1").expect("restart should claim gate");

        state.clear_turn_state_for_restart("conv-1");

        assert!(!state.is_claimed("conv-1"));
        assert!(!state.is_cancelling("conv-1"));
        assert!(state.is_restarting("conv-1"));
        assert!(state.try_claim_turn("conv-1", "turn-during-restart").is_err());
    }

    #[test]
    fn summary_uses_claim_as_starting_state() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", None, false, 0, false);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Starting);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    /// Task-1 brief: `summary_from_parts` must pass the caller-supplied
    /// `supports_midturn_delivery` straight through, unmodified by any other
    /// runtime-state derivation.
    #[test]
    fn summary_from_parts_carries_supports_midturn_delivery_through() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        let summary_true = state.summary_from_parts("conv-1", None, false, 0, true);
        assert!(summary_true.supports_midturn_delivery);

        let summary_false = state.summary_from_parts("conv-1", None, false, 0, false);
        assert!(!summary_false.supports_midturn_delivery);
    }

    #[test]
    fn summary_waiting_confirmation_takes_priority() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state
            .try_claim_turn("conv-1", "turn-1")
            .expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 1, false);

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

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 0, false);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Cancelling);
        assert_eq!(summary.turn_id.as_deref(), Some("turn-a"));
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn summary_uses_running_task_without_claim() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 0, false);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Running);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn summary_idle_when_no_claim_running_task_or_confirmation() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Finished), true, 0, false);

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
