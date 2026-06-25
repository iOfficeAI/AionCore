use aionui_api_types::AgentErrorCode;
use aionui_common::AgentType;

use crate::runtime_state::RuntimeLifecycleState;
use crate::stream_relay::RelayOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentHealthAction {
    Keep,
    EvictAcpTask {
        error_code: Option<AgentErrorCode>,
        retryable: Option<bool>,
        clear_model_seed: bool,
    },
}

pub struct AgentHealthPolicy;

impl AgentHealthPolicy {
    pub fn decide(
        agent_type: AgentType,
        outcome: &RelayOutcome,
        lifecycle: RuntimeLifecycleState,
    ) -> AgentHealthAction {
        if matches!(
            lifecycle,
            RuntimeLifecycleState::Deleting | RuntimeLifecycleState::ShuttingDown
        ) {
            return AgentHealthAction::Keep;
        }
        if agent_type != AgentType::Acp || !outcome.terminal.is_error() {
            return AgentHealthAction::Keep;
        }

        let error_code = outcome.terminal.code();
        AgentHealthAction::EvictAcpTask {
            error_code,
            retryable: outcome.terminal.retryable(),
            clear_model_seed: error_code == Some(AgentErrorCode::UserLlmProviderModelNotFound),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_relay::RelayTerminal;

    fn error_outcome(code: Option<AgentErrorCode>) -> RelayOutcome {
        RelayOutcome {
            terminal: RelayTerminal::Error {
                code,
                retryable: Some(true),
            },
            ..RelayOutcome::default()
        }
    }

    #[test]
    fn acp_finish_keeps_task() {
        let outcome = RelayOutcome::default();
        assert_eq!(
            AgentHealthPolicy::decide(AgentType::Acp, &outcome, RuntimeLifecycleState::Active),
            AgentHealthAction::Keep
        );
    }

    #[test]
    fn acp_terminal_error_evicts_task() {
        let outcome = error_outcome(Some(AgentErrorCode::UnknownUpstreamError));
        assert!(matches!(
            AgentHealthPolicy::decide(AgentType::Acp, &outcome, RuntimeLifecycleState::Active),
            AgentHealthAction::EvictAcpTask {
                clear_model_seed: false,
                ..
            }
        ));
    }

    #[test]
    fn acp_model_not_found_requests_model_seed_cleanup() {
        let outcome = error_outcome(Some(AgentErrorCode::UserLlmProviderModelNotFound));
        assert!(matches!(
            AgentHealthPolicy::decide(AgentType::Acp, &outcome, RuntimeLifecycleState::Active),
            AgentHealthAction::EvictAcpTask {
                clear_model_seed: true,
                ..
            }
        ));
    }

    #[test]
    fn non_acp_terminal_error_keeps_task() {
        let outcome = error_outcome(Some(AgentErrorCode::UnknownUpstreamError));
        assert_eq!(
            AgentHealthPolicy::decide(AgentType::Aionrs, &outcome, RuntimeLifecycleState::Active),
            AgentHealthAction::Keep
        );
    }

    #[test]
    fn channel_closed_does_not_evict_by_default() {
        let outcome = RelayOutcome {
            terminal: RelayTerminal::ChannelClosed,
            ..RelayOutcome::default()
        };
        assert_eq!(
            AgentHealthPolicy::decide(AgentType::Acp, &outcome, RuntimeLifecycleState::Active),
            AgentHealthAction::Keep
        );
    }

    #[test]
    fn cancelling_acp_terminal_error_evicts_task() {
        let outcome = error_outcome(Some(AgentErrorCode::UnknownUpstreamError));
        assert!(matches!(
            AgentHealthPolicy::decide(AgentType::Acp, &outcome, RuntimeLifecycleState::Cancelling),
            AgentHealthAction::EvictAcpTask {
                clear_model_seed: false,
                ..
            }
        ));
    }

    #[test]
    fn non_active_lifecycle_keeps_task() {
        let outcome = error_outcome(Some(AgentErrorCode::UserLlmProviderModelNotFound));
        assert_eq!(
            AgentHealthPolicy::decide(AgentType::Acp, &outcome, RuntimeLifecycleState::Deleting),
            AgentHealthAction::Keep
        );
    }
}
