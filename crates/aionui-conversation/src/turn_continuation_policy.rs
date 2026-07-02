use tracing::warn;

use crate::runtime_state::RuntimeLifecycleState;
use crate::stream_relay::RelayOutcome;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContinuationDecision {
    Continue { content: String, next_count: usize },
    Stop(ContinuationStopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuationStopReason {
    NoSystemResponses,
    TerminalNotFinish,
    LifecycleNotActive,
    LimitReached,
}

pub struct TurnContinuationPolicy {
    max_continuations: usize,
}

impl TurnContinuationPolicy {
    pub fn new(max_continuations: usize) -> Self {
        Self { max_continuations }
    }

    pub fn decide(
        &self,
        conversation_id: &str,
        continuation_count: usize,
        outcome: &RelayOutcome,
        lifecycle: RuntimeLifecycleState,
    ) -> ContinuationDecision {
        if lifecycle != RuntimeLifecycleState::Active {
            return ContinuationDecision::Stop(ContinuationStopReason::LifecycleNotActive);
        }
        if outcome.terminal.is_error() {
            return ContinuationDecision::Stop(ContinuationStopReason::TerminalNotFinish);
        }
        if outcome.system_responses.is_empty() {
            return ContinuationDecision::Stop(ContinuationStopReason::NoSystemResponses);
        }
        if continuation_count >= self.max_continuations {
            warn!(
                conversation_id,
                max = self.max_continuations,
                "Reached system response continuation limit; ending turn early"
            );
            return ContinuationDecision::Stop(ContinuationStopReason::LimitReached);
        }

        ContinuationDecision::Continue {
            content: outcome.system_responses.join("\n"),
            next_count: continuation_count + 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use aionui_api_types::AgentErrorCode;

    use super::*;
    use crate::stream_relay::RelayTerminal;

    #[test]
    fn finish_with_system_responses_continues() {
        let policy = TurnContinuationPolicy::new(4);
        let outcome = RelayOutcome {
            system_responses: vec!["one".into(), "two".into()],
            terminal: RelayTerminal::Finish,
            ..RelayOutcome::default()
        };

        assert_eq!(
            policy.decide("conv-1", 0, &outcome, RuntimeLifecycleState::Active),
            ContinuationDecision::Continue {
                content: "one\ntwo".into(),
                next_count: 1,
            }
        );
    }

    #[test]
    fn terminal_error_stops() {
        let policy = TurnContinuationPolicy::new(4);
        let outcome = RelayOutcome {
            system_responses: vec!["next".into()],
            terminal: RelayTerminal::Error {
                code: Some(AgentErrorCode::UnknownUpstreamError),
                retryable: Some(true),
            },
            ..RelayOutcome::default()
        };

        assert_eq!(
            policy.decide("conv-1", 0, &outcome, RuntimeLifecycleState::Active),
            ContinuationDecision::Stop(ContinuationStopReason::TerminalNotFinish)
        );
    }

    #[test]
    fn deleting_stops() {
        let policy = TurnContinuationPolicy::new(4);
        let outcome = RelayOutcome {
            system_responses: vec!["next".into()],
            terminal: RelayTerminal::Finish,
            ..RelayOutcome::default()
        };

        assert_eq!(
            policy.decide("conv-1", 0, &outcome, RuntimeLifecycleState::Deleting),
            ContinuationDecision::Stop(ContinuationStopReason::LifecycleNotActive)
        );
    }

    #[test]
    fn limit_stops() {
        let policy = TurnContinuationPolicy::new(1);
        let outcome = RelayOutcome {
            system_responses: vec!["next".into()],
            terminal: RelayTerminal::Finish,
            ..RelayOutcome::default()
        };

        assert_eq!(
            policy.decide("conv-1", 1, &outcome, RuntimeLifecycleState::Active),
            ContinuationDecision::Stop(ContinuationStopReason::LimitReached)
        );
    }
}
