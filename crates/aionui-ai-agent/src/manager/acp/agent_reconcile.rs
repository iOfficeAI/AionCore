use crate::manager::acp::AcpAgentManager;

use crate::manager::acp::error_mapping::is_acp_session_not_found;
use crate::manager::acp::mode_normalize::normalize_requested_mode_for_available_values;
use crate::manager::acp::session::PendingStartupConfigSeedResult;
use crate::protocol::error::AcpError;
use crate::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId};
use agent_client_protocol::schema::{
    SessionId, SetSessionConfigOptionRequest, SetSessionModeRequest, SetSessionModelRequest,
};
use std::collections::VecDeque;
use tracing::{error, info, warn};

const MAX_RECONCILE_ACTIONS: usize = 8;

/// Actions the session driver must execute to align CLI state with user intent.
///
/// Produced by `AcpSession::plan_reconcile` — a pure function that compares
/// desired vs observed and returns a list of idempotent, order-independent ops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    SetMode { mode: ModeId },
    SetModel { model: ModelId },
    SetConfigOption { key: ConfigKey, value: ConfigValue },
}

impl AcpAgentManager {
    /// Execute reconcile actions produced by `AcpSession::plan_reconcile`.
    ///
    /// Compares the aggregate's desired state against what the CLI has
    /// reported as current, then issues the minimal set of SDK calls
    /// (set_mode, set_model, set_config_option) to bring the CLI into
    /// alignment.
    ///
    /// Failure handling:
    /// - `SessionNotFound`: returned as structured `AcpError::SessionNotFound` so callers
    ///   (e.g. `open_session_resume`) can drop the stale sid and rebuild
    ///   the session. ELECTRON-1HQ regressed because we silently swallowed
    ///   this case during warmup, leaving downstream `session/prompt` to
    ///   surface the same error to the user every turn.
    /// - Any other error: logged and skipped (best-effort), so a failed
    ///   `set_config_option` doesn't block a successful `set_mode`.
    pub(super) async fn reconcile_session(&self, session_id: &str) -> Result<(), AcpError> {
        use crate::manager::acp::ReconcileAction;

        let (startup_config_seed_results, invalid_mode, invalid_model, actions) = {
            let mut session = self.session.write().await;
            let startup_config_seed_results =
                session.resolve_pending_startup_config_seeds_with_mode_normalizer(|requested, available_values| {
                    normalize_requested_mode_for_available_values(
                        &self.params.metadata,
                        requested,
                        available_values.iter().copied(),
                    )
                });
            let invalid_mode = session.clear_invalid_desired_mode();
            let invalid_model = session.clear_invalid_desired_model();
            let actions = session.plan_reconcile();
            (startup_config_seed_results, invalid_mode, invalid_model, actions)
        };
        self.log_reconcile_session_plan_results(startup_config_seed_results, invalid_mode, invalid_model);
        let mut actions: VecDeque<_> = actions.into();
        let mut executed_actions = 0usize;
        while let Some(action) = actions.pop_front() {
            let executed_action = action.clone();
            executed_actions += 1;
            if executed_actions > MAX_RECONCILE_ACTIONS {
                warn!(
                    conversation_id = %self.params.conversation_id,
                    max_actions = MAX_RECONCILE_ACTIONS,
                    "reconcile_session: stopping after action limit"
                );
                break;
            }
            match action {
                ReconcileAction::SetMode { mode } => {
                    let normalized = {
                        let session = self.session.read().await;
                        let snapshot = session.config_snapshot();
                        normalize_requested_mode_for_available_values(
                            &self.params.metadata,
                            mode.as_str(),
                            snapshot.selectable_values("mode"),
                        )
                    };
                    if normalized.is_empty() {
                        continue;
                    }
                    if let Err(e) = self
                        .protocol
                        .set_mode(SetSessionModeRequest::new(
                            SessionId::new(session_id),
                            normalized.clone(),
                        ))
                        .await
                    {
                        if is_acp_session_not_found(&e) {
                            warn!(
                                conversation_id = %self.params.conversation_id,
                                mode_id = %normalized,
                                error = %e,
                                "reconcile_session: set_mode hit SessionNotFound; aborting reconcile"
                            );
                            return Err(e);
                        }
                        error!(
                            conversation_id = %self.params.conversation_id,
                            mode_id = %normalized,
                            error = %e,
                            "reconcile_session: set_mode failed"
                        );
                        continue;
                    }
                }

                ReconcileAction::SetModel { model } => {
                    if let Err(e) = self
                        .protocol
                        .set_model(SetSessionModelRequest::new(
                            SessionId::new(session_id),
                            model.as_str().to_owned(),
                        ))
                        .await
                    {
                        if is_acp_session_not_found(&e) {
                            warn!(
                                conversation_id = %self.params.conversation_id,
                                model_id = %model,
                                error = %e,
                                "reconcile_session: set_model hit SessionNotFound; aborting reconcile"
                            );
                            return Err(e);
                        }
                        error!(
                            conversation_id = %self.params.conversation_id,
                            model_id = %model,
                            error = %e,
                            "reconcile_session: set_model failed"
                        );
                        continue;
                    }
                }

                ReconcileAction::SetConfigOption { key, value } => {
                    let resolved_value = if key.as_str() == "mode" {
                        let session = self.session.read().await;
                        let snapshot = session.config_snapshot();
                        normalize_requested_mode_for_available_values(
                            &self.params.metadata,
                            value.as_str(),
                            snapshot.selectable_values("mode"),
                        )
                    } else {
                        value.as_str().trim().to_owned()
                    };
                    if key.as_str() == "mode" && resolved_value != value.as_str() {
                        let mut session = self.session.write().await;
                        session.set_desired_config(key.clone(), ConfigValue::new(resolved_value.clone()));
                    }
                    info!(
                        conversation_id = %self.params.conversation_id,
                        agent_backend = ?self.params.metadata.backend,
                        config_id = %key,
                        desired = %resolved_value,
                        "acp_reconcile_config_option_requested"
                    );
                    match self
                        .protocol
                        .set_config_option(SetSessionConfigOptionRequest::new(
                            SessionId::new(session_id),
                            key.as_str().to_owned(),
                            resolved_value.clone(),
                        ))
                        .await
                    {
                        Ok(response) => {
                            info!(
                                conversation_id = %self.params.conversation_id,
                                agent_backend = ?self.params.metadata.backend,
                                config_id = %key,
                                desired = %resolved_value,
                                "acp_reconcile_config_option_ack"
                            );
                            let (startup_config_seed_results, invalid_mode, invalid_model, followup_actions) = {
                                let mut session = self.session.write().await;
                                session.apply_advertised_config_options(response.config_options);
                                let startup_config_seed_results = session
                                    .resolve_pending_startup_config_seeds_with_mode_normalizer(
                                        |requested, available_values| {
                                            normalize_requested_mode_for_available_values(
                                                &self.params.metadata,
                                                requested,
                                                available_values.iter().copied(),
                                            )
                                        },
                                    );
                                let invalid_mode = session.clear_invalid_desired_mode();
                                let invalid_model = session.clear_invalid_desired_model();
                                let followup_actions = session.plan_reconcile();
                                self.commit_session_changes(&mut session).await;
                                (
                                    startup_config_seed_results,
                                    invalid_mode,
                                    invalid_model,
                                    followup_actions,
                                )
                            };
                            self.log_reconcile_session_plan_results(
                                startup_config_seed_results,
                                invalid_mode,
                                invalid_model,
                            );
                            let mut followup_actions = followup_actions;
                            followup_actions.retain(|candidate| candidate != &executed_action);
                            actions = followup_actions.into();
                        }
                        Err(err) => {
                            if is_acp_session_not_found(&err) {
                                warn!(
                                    conversation_id = %self.params.conversation_id,
                                    config_id = %key,
                                    desired = %resolved_value,
                                    error = %err,
                                    "reconcile_session: set_config_option hit SessionNotFound; aborting reconcile"
                                );
                                return Err(err);
                            }
                            info!(
                                conversation_id = %self.params.conversation_id,
                                config_id = %key,
                                desired = %resolved_value,
                                error = %err,
                                "reconcile_session: set_config_option failed; skipping"
                            );
                            continue;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn log_reconcile_session_plan_results(
        &self,
        startup_config_seed_results: Vec<PendingStartupConfigSeedResult>,
        invalid_mode: Option<ModeId>,
        invalid_model: Option<ModelId>,
    ) {
        if let Some(mode) = invalid_mode {
            warn!(
                conversation_id = %self.params.conversation_id,
                mode_id = %mode,
                "reconcile_session: dropped unavailable desired mode"
            );
        }
        if let Some(model) = invalid_model {
            warn!(
                conversation_id = %self.params.conversation_id,
                model_id = %model,
                "reconcile_session: dropped unavailable desired model"
            );
        }
        for result in startup_config_seed_results {
            match result {
                PendingStartupConfigSeedResult::Applied { .. } => {}
                PendingStartupConfigSeedResult::OptionNotAdvertised { category } => {
                    warn!(
                        conversation_id = %self.params.conversation_id,
                        agent_backend = ?self.params.metadata.backend,
                        category = ?category,
                        reason = "option_not_advertised",
                        "reconcile_session: startup config seed not applied"
                    );
                }
                PendingStartupConfigSeedResult::ValueNotSelectable { category } => {
                    warn!(
                        conversation_id = %self.params.conversation_id,
                        agent_backend = ?self.params.metadata.backend,
                        category = ?category,
                        reason = "value_not_selectable",
                        "reconcile_session: startup config seed not applied"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::acp::AcpSession;
    use std::collections::HashMap;

    #[test]
    fn reconcile_action_equality() {
        let a = ReconcileAction::SetMode {
            mode: ModeId::new("plan"),
        };
        let b = ReconcileAction::SetMode {
            mode: ModeId::new("plan"),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn reconcile_set_config_option_ack_must_not_be_modeled_as_observed_event() {
        let mut session = AcpSession::new(
            None,
            None,
            HashMap::from([(ConfigKey::new("reasoning_effort"), ConfigValue::new("high"))]),
        );

        session.apply_observed_config(ConfigKey::new("reasoning_effort"), ConfigValue::new("medium"));
        session.drain_events();

        let actions = session.plan_reconcile();
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], ReconcileAction::SetConfigOption { .. }));

        let events = session.drain_events();
        assert!(events.is_empty());
    }
}
