use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{ConfigOptionConfirmation, SetConfigOptionRequest, SetConfigOptionResponse};
use aionui_common::{AgentKillReason, ErrorChain};
use aionui_conversation::{
    ConversationService, ConversationTurnAdmission, ConversationTurnAdmissionRequest, ConversationTurnGuard,
    ConversationTurnObservation, ConversationTurnObserver,
};
use aionui_development::{
    BudgetEvaluation, DevelopmentBudgetAdmission, DevelopmentOperationsService, DevelopmentUsageIngestor,
    ObservedAgentTurnUsage,
};
use tracing::{info, warn};

#[derive(Debug, Clone, PartialEq, Eq)]
enum BudgetRuntimeAction {
    None,
    DowngradeModel(String),
    StopAgent,
}

fn runtime_action(evaluation: Option<&BudgetEvaluation>) -> BudgetRuntimeAction {
    let Some(evaluation) = evaluation.filter(|evaluation| !evaluation.reasons.is_empty()) else {
        return BudgetRuntimeAction::None;
    };
    match evaluation.action.as_str() {
        "downgrade_model" => evaluation
            .replacement_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(|model| BudgetRuntimeAction::DowngradeModel(model.to_owned()))
            .unwrap_or(BudgetRuntimeAction::StopAgent),
        "pause" | "terminate" => BudgetRuntimeAction::StopAgent,
        "notify" => BudgetRuntimeAction::None,
        _ => BudgetRuntimeAction::StopAgent,
    }
}

fn turn_admission(admission: Option<&DevelopmentBudgetAdmission>) -> ConversationTurnAdmission {
    let Some(admission) = admission else {
        return ConversationTurnAdmission::Allowed;
    };
    if matches!(admission.run_status.as_str(), "paused" | "cancelled" | "integrating") {
        return ConversationTurnAdmission::Denied {
            reason: format!(
                "Development run {} is {}; wait for recovery/completion or create a new run before sending more Agent work",
                admission.run_id, admission.run_status
            ),
        };
    }
    if admission.evaluation.reasons.is_empty()
        || matches!(admission.evaluation.action.as_str(), "notify" | "downgrade_model")
    {
        ConversationTurnAdmission::Allowed
    } else {
        ConversationTurnAdmission::Denied {
            reason: admission.evaluation.reasons.join("; "),
        }
    }
}

fn observed_model_matches(response: &SetConfigOptionResponse, expected_model: &str) -> bool {
    response.confirmation == ConfigOptionConfirmation::Observed
        && response.config_options.as_ref().is_some_and(|options| {
            options
                .iter()
                .any(|option| option.id == "model" && option.current_value.as_deref() == Some(expected_model))
        })
}

fn pre_turn_runtime_action(admission: &DevelopmentBudgetAdmission) -> BudgetRuntimeAction {
    if matches!(admission.run_status.as_str(), "paused" | "cancelled" | "integrating") {
        BudgetRuntimeAction::StopAgent
    } else {
        runtime_action(Some(&admission.evaluation))
    }
}

pub(crate) struct DevelopmentTurnObserver {
    ingestor: DevelopmentUsageIngestor,
    operations: DevelopmentOperationsService,
    conversation_service: ConversationService,
    task_manager: Arc<dyn IWorkerTaskManager>,
}

impl DevelopmentTurnObserver {
    pub(crate) fn new(
        ingestor: DevelopmentUsageIngestor,
        operations: DevelopmentOperationsService,
        conversation_service: ConversationService,
        task_manager: Arc<dyn IWorkerTaskManager>,
    ) -> Self {
        Self {
            ingestor,
            operations,
            conversation_service,
            task_manager,
        }
    }

    async fn apply_runtime_action(
        &self,
        user_id: &str,
        run_id: Option<&str>,
        conversation_id: &str,
        evaluation: Option<&BudgetEvaluation>,
    ) -> bool {
        match runtime_action(evaluation) {
            BudgetRuntimeAction::None => true,
            BudgetRuntimeAction::DowngradeModel(model) => {
                match self
                    .conversation_service
                    .set_config_option(
                        conversation_id,
                        "model",
                        SetConfigOptionRequest { value: model.clone() },
                    )
                    .await
                {
                    Ok(response) if observed_model_matches(&response, &model) => {
                        info!(
                            conversation_id,
                            model, "development budget downgraded the active agent model"
                        );
                        true
                    }
                    result => {
                        let detail = match result {
                            Ok(response) => format!(
                                "Agent acknowledged model change without an observed fallback snapshot ({:?})",
                                response.confirmation
                            ),
                            Err(error) => ErrorChain(&error).to_string(),
                        };
                        warn!(
                            conversation_id,
                            model,
                            error = %detail,
                            "development budget model downgrade failed; stopping the active agent to prevent unbounded execution"
                        );
                        if let Some(run_id) = run_id
                            && let Err(pause_error) = self
                                .operations
                                .pause_after_runtime_action_failure(
                                    user_id,
                                    run_id,
                                    "configured budget model downgrade failed; run paused fail-closed",
                                )
                                .await
                        {
                            warn!(
                                run_id,
                                error = %ErrorChain(&pause_error),
                                "failed to persist fail-closed pause after budget model downgrade failure"
                            );
                        }
                        self.task_manager
                            .kill_and_wait(conversation_id, Some(AgentKillReason::BudgetExceeded))
                            .await;
                        false
                    }
                }
            }
            BudgetRuntimeAction::StopAgent => {
                self.task_manager
                    .kill_and_wait(conversation_id, Some(AgentKillReason::BudgetExceeded))
                    .await;
                false
            }
        }
    }
}

#[async_trait::async_trait]
impl ConversationTurnObserver for DevelopmentTurnObserver {
    async fn observe(&self, observation: ConversationTurnObservation) {
        let conversation_id = observation.conversation_id.clone();
        let user_id = observation.user_id.clone();
        let team_id = observation.team_id.clone();
        let event = ObservedAgentTurnUsage {
            user_id: observation.user_id,
            conversation_id: observation.conversation_id,
            turn_id: observation.turn_id,
            agent_id: observation.agent_id,
            provider: observation.provider,
            model: observation.model,
            team_id: observation.team_id,
            slot_id: observation.slot_id,
            usage: observation.usage,
            duration_ms: observation.duration_ms,
            retry_count: observation.retry_count,
            occurred_at: observation.occurred_at,
        };
        match self.ingestor.record(event).await {
            Ok(Some(outcome)) => {
                let _ = self
                    .apply_runtime_action(
                        &outcome.row.user_id,
                        outcome.row.run_id.as_deref(),
                        &conversation_id,
                        outcome.budget.as_ref(),
                    )
                    .await;
            }
            Ok(_) => {}
            Err(error) => {
                warn!(
                    conversation_id,
                    error = %ErrorChain(&error),
                    "failed to persist or enforce observed agent usage"
                );
                match self
                    .ingestor
                    .pause_after_observation_failure(
                        &user_id,
                        &conversation_id,
                        team_id.as_deref(),
                        "observed Agent usage could not be safely persisted or enforced; run paused fail-closed",
                    )
                    .await
                {
                    Ok(None) => {}
                    Ok(Some(_)) | Err(_) => {
                        self.task_manager
                            .kill_and_wait(&conversation_id, Some(AgentKillReason::BudgetExceeded))
                            .await;
                    }
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl ConversationTurnGuard for DevelopmentTurnObserver {
    async fn authorize(&self, request: ConversationTurnAdmissionRequest) -> Result<ConversationTurnAdmission, String> {
        let admission = self
            .ingestor
            .admit(&request.user_id, &request.conversation_id, request.team_id.as_deref())
            .await
            .map_err(|error| error.to_string())?;
        let Some(admission) = admission else {
            return Ok(ConversationTurnAdmission::Allowed);
        };
        match pre_turn_runtime_action(&admission) {
            BudgetRuntimeAction::None => {}
            BudgetRuntimeAction::DowngradeModel(_) => {
                if !self
                    .apply_runtime_action(
                        &request.user_id,
                        Some(&admission.run_id),
                        &request.conversation_id,
                        Some(&admission.evaluation),
                    )
                    .await
                {
                    return Ok(ConversationTurnAdmission::Denied {
                        reason: "Development budget model downgrade was not observed; run paused fail-closed".into(),
                    });
                }
            }
            BudgetRuntimeAction::StopAgent => {
                self.task_manager
                    .kill_and_wait(&request.conversation_id, Some(AgentKillReason::BudgetExceeded))
                    .await;
                let reason = if matches!(admission.run_status.as_str(), "paused" | "cancelled") {
                    format!(
                        "Development run {} is {}; resume it or create a new run before sending more Agent work",
                        admission.run_id, admission.run_status
                    )
                } else if admission.evaluation.reasons.is_empty() {
                    "Development budget policy could not produce a safe runtime action".into()
                } else {
                    admission.evaluation.reasons.join("; ")
                };
                return Ok(ConversationTurnAdmission::Denied { reason });
            }
        }
        Ok(turn_admission(Some(&admission)))
    }
}

#[cfg(test)]
mod tests {
    use aionui_api_types::AcpConfigOptionDto;
    use aionui_db::models::DevelopmentUsageSummary;

    use super::*;

    fn evaluation(action: &str, replacement_model: Option<&str>, exceeded: bool) -> BudgetEvaluation {
        BudgetEvaluation {
            allowed: !exceeded || action == "notify",
            action: action.into(),
            reasons: exceeded.then(|| "token budget exceeded".into()).into_iter().collect(),
            usage: DevelopmentUsageSummary::default(),
            replacement_model: replacement_model.map(str::to_owned),
        }
    }

    #[test]
    fn runtime_action_applies_only_when_the_budget_is_exceeded() {
        assert_eq!(runtime_action(None), BudgetRuntimeAction::None);
        assert_eq!(
            runtime_action(Some(&evaluation("pause", None, false))),
            BudgetRuntimeAction::None
        );
        assert_eq!(
            runtime_action(Some(&evaluation("notify", None, true))),
            BudgetRuntimeAction::None
        );
        assert_eq!(
            runtime_action(Some(&evaluation("pause", None, true))),
            BudgetRuntimeAction::StopAgent
        );
        assert_eq!(
            runtime_action(Some(&evaluation("terminate", None, true))),
            BudgetRuntimeAction::StopAgent
        );
        assert_eq!(
            runtime_action(Some(&evaluation("downgrade_model", Some("fallback-model"), true))),
            BudgetRuntimeAction::DowngradeModel("fallback-model".into())
        );
    }

    #[test]
    fn malformed_persisted_budget_action_fails_closed() {
        assert_eq!(
            runtime_action(Some(&evaluation("downgrade_model", None, true))),
            BudgetRuntimeAction::StopAgent
        );
        assert_eq!(
            runtime_action(Some(&evaluation("unexpected", None, true))),
            BudgetRuntimeAction::StopAgent
        );
    }

    #[test]
    fn admission_blocks_non_executable_runs_but_allows_notify_and_downgrade() {
        let admission = |status: &str, action: &str, exceeded: bool| DevelopmentBudgetAdmission {
            run_id: "run-1".into(),
            run_status: status.into(),
            evaluation: evaluation(action, Some("fallback-model"), exceeded),
        };
        assert_eq!(
            turn_admission(Some(&admission("running", "notify", true))),
            ConversationTurnAdmission::Allowed
        );
        assert_eq!(
            turn_admission(Some(&admission("running", "downgrade_model", true))),
            ConversationTurnAdmission::Allowed
        );
        assert!(matches!(
            turn_admission(Some(&admission("paused", "pause", true))),
            ConversationTurnAdmission::Denied { .. }
        ));
        assert!(matches!(
            turn_admission(Some(&admission("cancelled", "terminate", true))),
            ConversationTurnAdmission::Denied { .. }
        ));
        assert!(matches!(
            turn_admission(Some(&admission("integrating", "notify", false))),
            ConversationTurnAdmission::Denied { .. }
        ));
    }

    #[test]
    fn model_downgrade_requires_an_observed_matching_snapshot() {
        let response = |confirmation, current_value: Option<&str>| SetConfigOptionResponse {
            confirmation,
            config_options: Some(vec![AcpConfigOptionDto {
                id: "model".into(),
                name: Some("Model".into()),
                label: None,
                description: None,
                category: Some("model".into()),
                option_type: "select".into(),
                current_value: current_value.map(str::to_owned),
                options: vec![],
            }]),
        };
        assert!(observed_model_matches(
            &response(ConfigOptionConfirmation::Observed, Some("fallback")),
            "fallback"
        ));
        assert!(!observed_model_matches(
            &response(ConfigOptionConfirmation::CommandAck, Some("fallback")),
            "fallback"
        ));
        assert!(!observed_model_matches(
            &response(ConfigOptionConfirmation::Observed, Some("expensive")),
            "fallback"
        ));
    }

    #[test]
    fn pre_turn_actions_stop_terminal_budget_runs_and_downgrade_every_open_status() {
        let admission = |status: &str, action: &str, exceeded: bool| DevelopmentBudgetAdmission {
            run_id: "run-1".into(),
            run_status: status.into(),
            evaluation: evaluation(action, Some("fallback-model"), exceeded),
        };
        assert_eq!(
            pre_turn_runtime_action(&admission("paused", "notify", false)),
            BudgetRuntimeAction::StopAgent
        );
        assert_eq!(
            pre_turn_runtime_action(&admission("cancelled", "notify", false)),
            BudgetRuntimeAction::StopAgent
        );
        assert_eq!(
            pre_turn_runtime_action(&admission("integrating", "notify", false)),
            BudgetRuntimeAction::StopAgent
        );
        for status in [
            "preflight",
            "running",
            "waiting_approval",
            "verifying",
            "reviewing",
            "rework",
        ] {
            assert_eq!(
                pre_turn_runtime_action(&admission(status, "downgrade_model", true)),
                BudgetRuntimeAction::DowngradeModel("fallback-model".into())
            );
        }
    }
}
