use std::sync::Arc;

use tracing::debug;

use crate::runtime_state::{ConversationRuntimeStateService, RuntimeLifecycleState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeWriteKind {
    UserMessage,
    AssistantTextCreate,
    AssistantTextFlush,
    AssistantTextFinalize,
    AssistantThinkingFinalize,
    ToolCallPersist,
    AcpToolCallPersist,
    ToolGroupPersist,
    TerminalFinalize,
    ConversationFinished,
    SendFailureTip,
    SessionKey,
    AcpRecoveryCleanup,
    ResolvedWorkspace,
    StartupRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeWriteDecision {
    Allow,
    Skip(RuntimeLifecycleState),
}

#[derive(Clone)]
pub struct RuntimePersistenceCoordinator {
    runtime_state: Arc<ConversationRuntimeStateService>,
}

impl RuntimePersistenceCoordinator {
    pub fn new(runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        Self { runtime_state }
    }

    pub fn lifecycle_for(&self, conversation_id: &str) -> RuntimeLifecycleState {
        self.runtime_state.lifecycle_for(conversation_id)
    }

    pub fn decide(&self, conversation_id: &str, kind: RuntimeWriteKind) -> RuntimeWriteDecision {
        let lifecycle = self.lifecycle_for(conversation_id);
        let allow = match lifecycle {
            RuntimeLifecycleState::Active => true,
            RuntimeLifecycleState::Deleting => matches!(kind, RuntimeWriteKind::UserMessage),
            RuntimeLifecycleState::Cancelling => matches!(
                kind,
                RuntimeWriteKind::AssistantTextCreate
                    | RuntimeWriteKind::AssistantTextFlush
                    | RuntimeWriteKind::AssistantTextFinalize
                    | RuntimeWriteKind::AssistantThinkingFinalize
                    | RuntimeWriteKind::ToolCallPersist
                    | RuntimeWriteKind::AcpToolCallPersist
                    | RuntimeWriteKind::ToolGroupPersist
                    | RuntimeWriteKind::TerminalFinalize
                    | RuntimeWriteKind::ConversationFinished
            ),
            RuntimeLifecycleState::ShuttingDown => matches!(kind, RuntimeWriteKind::UserMessage),
        };

        if allow {
            RuntimeWriteDecision::Allow
        } else {
            debug!(
                conversation_id,
                write_kind = ?kind,
                lifecycle_state = ?lifecycle,
                "runtime persistence write skipped"
            );
            RuntimeWriteDecision::Skip(lifecycle)
        }
    }

    pub fn allows(&self, conversation_id: &str, kind: RuntimeWriteKind) -> bool {
        matches!(self.decide(conversation_id, kind), RuntimeWriteDecision::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_allows_all_expected_write_kinds() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        let coordinator = RuntimePersistenceCoordinator::new(state);

        for kind in [
            RuntimeWriteKind::UserMessage,
            RuntimeWriteKind::AssistantTextCreate,
            RuntimeWriteKind::AssistantTextFlush,
            RuntimeWriteKind::AssistantTextFinalize,
            RuntimeWriteKind::AssistantThinkingFinalize,
            RuntimeWriteKind::ToolCallPersist,
            RuntimeWriteKind::AcpToolCallPersist,
            RuntimeWriteKind::ToolGroupPersist,
            RuntimeWriteKind::TerminalFinalize,
            RuntimeWriteKind::ConversationFinished,
            RuntimeWriteKind::SendFailureTip,
            RuntimeWriteKind::SessionKey,
            RuntimeWriteKind::AcpRecoveryCleanup,
            RuntimeWriteKind::ResolvedWorkspace,
            RuntimeWriteKind::StartupRecovery,
        ] {
            assert!(coordinator.allows("conv-1", kind), "{kind:?} should be allowed");
        }
    }

    #[test]
    fn deleting_skips_runtime_finalization_and_recovery_writes() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.mark_deleting("conv-1");
        let coordinator = RuntimePersistenceCoordinator::new(state);

        assert!(coordinator.allows("conv-1", RuntimeWriteKind::UserMessage));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::AssistantTextFinalize));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::SendFailureTip));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::SessionKey));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::AcpRecoveryCleanup));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::ConversationFinished));
    }

    #[test]
    fn cancelling_allows_partial_finalization_but_skips_failure_and_recovery() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.mark_cancelling("conv-1");
        let coordinator = RuntimePersistenceCoordinator::new(state);

        assert!(coordinator.allows("conv-1", RuntimeWriteKind::AssistantTextFinalize));
        assert!(coordinator.allows("conv-1", RuntimeWriteKind::ConversationFinished));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::SendFailureTip));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::AcpRecoveryCleanup));
    }

    #[test]
    fn shutting_down_skips_new_runtime_finalization_failure_and_recovery() {
        let state = Arc::new(ConversationRuntimeStateService::default());
        state.mark_shutting_down();
        let coordinator = RuntimePersistenceCoordinator::new(state);

        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::ConversationFinished));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::SendFailureTip));
        assert!(!coordinator.allows("conv-1", RuntimeWriteKind::AcpRecoveryCleanup));
    }
}
