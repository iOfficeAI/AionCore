use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};

use aionui_api_types::{ConversationRuntimeStateKind, ConversationRuntimeSummary};
use aionui_common::ConversationStatus;
use tracing::{info, warn};

use crate::ConversationError;

#[derive(Debug, Default)]
pub struct ConversationRuntimeStateService {
    state: Mutex<ConversationRuntimeState>,
}

#[derive(Debug, Default)]
struct ConversationRuntimeState {
    active_turns: HashMap<String, String>,
    deleting_conversations: HashSet<String>,
    cancelling_conversations: HashSet<String>,
    shutting_down: bool,
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

        if state.active_turns.contains_key(conversation_id) {
            info!(
                conversation_id,
                turn_id,
                active_turn_id = state.active_turns.get(conversation_id).map(String::as_str),
                "conversation runtime turn claim rejected"
            );
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

    pub fn is_cancelling(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.cancelling_conversations.contains(conversation_id))
            .unwrap_or(false)
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
                state.cancelling_conversations.remove(conversation_id);
                info!(
                    conversation_id,
                    turn_id,
                    deleting = was_deleting,
                    "conversation runtime turn claim released"
                );
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
}
