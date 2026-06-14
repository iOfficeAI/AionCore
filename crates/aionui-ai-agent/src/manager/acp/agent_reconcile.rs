use crate::manager::acp::AcpAgentManager;

use crate::manager::acp::error_mapping::is_acp_session_not_found;
use crate::manager::acp::mode_normalize::normalize_requested_mode;
use crate::manager::acp::session::PendingStartupConfigSeedResult;
use crate::protocol::error::AcpError;
use crate::shared_kernel::{ConfigKey, ConfigValue, ModeId, ModelId};
use agent_client_protocol::schema::{
    SessionId, SetSessionConfigOptionRequest, SetSessionModeRequest, SetSessionModelRequest,
};
use tracing::{error, info, warn};

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
            let startup_config_seed_results = session.resolve_pending_startup_config_seeds();
            let invalid_mode = session.clear_invalid_desired_mode();
            let invalid_model = session.clear_invalid_desired_model();
            let actions = session.plan_reconcile();
            (startup_config_seed_results, invalid_mode, invalid_model, actions)
        };
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
        for action in actions {
            match action {
                ReconcileAction::SetMode { mode } => {
                    let normalized = normalize_requested_mode(&self.params.metadata, mode.as_str());
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
                    match self
                        .protocol
                        .set_config_option(SetSessionConfigOptionRequest::new(
                            SessionId::new(session_id),
                            key.as_str().to_owned(),
                            value.as_str().to_owned(),
                        ))
                        .await
                    {
                        Ok(response) => {
                            let mut session = self.session.write().await;
                            session.apply_advertised_config_options(response.config_options);
                            self.commit_session_changes(&mut session).await;
                        }
                        Err(err) => {
                            if is_acp_session_not_found(&err) {
                                warn!(
                                    conversation_id = %self.params.conversation_id,
                                    config_id = %key,
                                    desired = %value,
                                    error = %err,
                                    "reconcile_session: set_config_option hit SessionNotFound; aborting reconcile"
                                );
                                return Err(err);
                            }
                            info!(
                                conversation_id = %self.params.conversation_id,
                                config_id = %key,
                                desired = %value,
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
