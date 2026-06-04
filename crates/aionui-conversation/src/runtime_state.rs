use std::{
    collections::HashSet,
    sync::{Arc, Mutex, Weak},
};

use aionui_api_types::{ConversationRuntimeStateKind, ConversationRuntimeSummary};
use aionui_common::{AppError, ConversationStatus};
use tracing::{info, warn};

#[derive(Debug, Default)]
pub struct ConversationRuntimeStateService {
    state: Mutex<ConversationRuntimeState>,
}

#[derive(Debug, Default)]
struct ConversationRuntimeState {
    active_turns: HashSet<String>,
    deleting_conversations: HashSet<String>,
}

#[derive(Debug)]
pub struct TurnClaim {
    conversation_id: String,
    state: Weak<ConversationRuntimeStateService>,
    released: bool,
}

impl ConversationRuntimeStateService {
    pub fn try_claim_turn(self: &Arc<Self>, conversation_id: &str) -> Result<TurnClaim, AppError> {
        let mut state = self.state.lock().map_err(|_| {
            warn!(
                conversation_id,
                "conversation runtime state lock poisoned while claiming turn"
            );
            AppError::Internal("conversation runtime state lock poisoned".into())
        })?;

        if state.deleting_conversations.contains(conversation_id) {
            info!(
                conversation_id,
                "conversation runtime turn claim rejected because conversation is deleting"
            );
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is being deleted"
            )));
        }

        if !state.active_turns.insert(conversation_id.to_owned()) {
            info!(conversation_id, "conversation runtime turn claim rejected");
            return Err(AppError::Conflict(format!(
                "conversation {conversation_id} is already running"
            )));
        }

        info!(conversation_id, "conversation runtime turn claimed");

        Ok(TurnClaim {
            conversation_id: conversation_id.to_owned(),
            state: Arc::downgrade(self),
            released: false,
        })
    }

    pub fn is_claimed(&self, conversation_id: &str) -> bool {
        self.state
            .lock()
            .map(|state| state.active_turns.contains(conversation_id))
            .unwrap_or(false)
    }

    pub fn mark_deleting(&self, conversation_id: &str) -> bool {
        match self.state.lock() {
            Ok(mut state) => {
                state.deleting_conversations.insert(conversation_id.to_owned());
                let active = state.active_turns.contains(conversation_id);
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

    pub fn summary_from_parts(
        &self,
        conversation_id: &str,
        task_status: Option<ConversationStatus>,
        has_task: bool,
        pending_confirmations: usize,
    ) -> ConversationRuntimeSummary {
        let claimed = self.is_claimed(conversation_id);

        let state = if pending_confirmations > 0 {
            ConversationRuntimeStateKind::WaitingConfirmation
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
        }
    }

    fn release(&self, conversation_id: &str) -> bool {
        match self.state.lock() {
            Ok(mut state) => {
                state.active_turns.remove(conversation_id);
                let was_deleting = state.deleting_conversations.remove(conversation_id);
                info!(
                    conversation_id,
                    deleting = was_deleting,
                    "conversation runtime turn claim released"
                );
                was_deleting
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while releasing turn"
                );
                false
            }
        }
    }
}

impl TurnClaim {
    pub fn release(&mut self) -> bool {
        self.release_inner()
    }

    fn release_inner(&mut self) -> bool {
        if self.released {
            return false;
        }

        let was_deleting = self
            .state
            .upgrade()
            .map(|state| state.release(&self.conversation_id))
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
    fn claim_rejects_second_active_turn() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state.try_claim_turn("conv-1").expect("first claim should win");

        let err = state.try_claim_turn("conv-1").expect_err("second claim should fail");
        assert!(err.to_string().contains("already running"));
    }

    #[test]
    fn claim_releases_on_drop() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        {
            let _claim = state.try_claim_turn("conv-1").expect("claim should be created");
            assert!(state.is_claimed("conv-1"));
        }

        assert!(!state.is_claimed("conv-1"));
        assert!(state.try_claim_turn("conv-1").is_ok());
    }

    #[test]
    fn deleting_rejects_new_turn_claims() {
        let state = Arc::new(ConversationRuntimeStateService::default());

        state.mark_deleting("conv-1");

        let err = state
            .try_claim_turn("conv-1")
            .expect_err("deleting conversation should reject new turns");
        assert!(err.to_string().contains("being deleted"));
    }

    #[test]
    fn release_clears_deleting_flag_for_active_turn() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let mut claim = state.try_claim_turn("conv-1").expect("claim should be created");

        state.mark_deleting("conv-1");
        assert!(state.is_deleting("conv-1"));

        assert!(claim.release());

        assert!(!state.is_deleting("conv-1"));
    }

    #[test]
    fn summary_uses_claim_as_starting_state() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state.try_claim_turn("conv-1").expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", None, false, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Starting);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }

    #[test]
    fn summary_waiting_confirmation_takes_priority() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state.try_claim_turn("conv-1").expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", Some(ConversationStatus::Running), true, 1);

        assert_eq!(summary.state, ConversationRuntimeStateKind::WaitingConfirmation);
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
