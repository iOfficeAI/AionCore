use std::{
    collections::HashSet,
    sync::{Arc, Mutex, Weak},
};

use aionui_api_types::{ConversationRuntimeStateKind, ConversationRuntimeSummary};
use aionui_common::{AppError, ConversationStatus};
use tracing::{info, warn};

#[derive(Debug, Default)]
pub struct ConversationRuntimeStateService {
    active_turns: Mutex<HashSet<String>>,
}

#[derive(Debug)]
pub struct TurnClaim {
    conversation_id: String,
    state: Weak<ConversationRuntimeStateService>,
    released: bool,
}

impl ConversationRuntimeStateService {
    pub fn try_claim_turn(self: &Arc<Self>, conversation_id: &str) -> Result<TurnClaim, AppError> {
        let mut active_turns = self.active_turns.lock().map_err(|_| {
            warn!(
                conversation_id,
                "conversation runtime state lock poisoned while claiming turn"
            );
            AppError::Internal("conversation runtime state lock poisoned".into())
        })?;

        if !active_turns.insert(conversation_id.to_owned()) {
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
        self.active_turns
            .lock()
            .map(|active_turns| active_turns.contains(conversation_id))
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

    fn release(&self, conversation_id: &str) {
        match self.active_turns.lock() {
            Ok(mut active_turns) => {
                active_turns.remove(conversation_id);
                info!(conversation_id, "conversation runtime turn claim released");
            }
            Err(_) => {
                warn!(
                    conversation_id,
                    "conversation runtime state lock poisoned while releasing turn"
                );
            }
        }
    }
}

impl TurnClaim {
    pub fn release(&mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }

        if let Some(state) = self.state.upgrade() {
            state.release(&self.conversation_id);
        }
        self.released = true;
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
    fn summary_uses_claim_as_starting_state() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let _claim = state.try_claim_turn("conv-1").expect("claim should be created");

        let summary = state.summary_from_parts("conv-1", None, false, 0);

        assert_eq!(summary.state, ConversationRuntimeStateKind::Starting);
        assert!(summary.is_processing);
        assert!(!summary.can_send_message);
    }
}
