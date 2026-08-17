use std::sync::Arc;

use aionui_api_types::AgentErrorCode;
use aionui_common::{AgentKillReason, AgentType, ConversationSource, now_ms};
use aionui_db::{ConversationRowUpdate, SaveRuntimeStateParams};
use tracing::{info, warn};

use crate::convert::string_to_enum;
use crate::service::ConversationService;
use crate::stream_relay::RelayOutcome;
use aionui_ai_agent::IWorkerTaskManager;

use crate::runtime_persistence::RuntimeWriteKind;

fn should_clear_persisted_model(error_code: Option<AgentErrorCode>) -> bool {
    matches!(
        error_code,
        Some(AgentErrorCode::UserLlmProviderModelNotFound | AgentErrorCode::UserLlmProviderUnsupportedModel)
    )
}

impl ConversationService {
    async fn clear_conversation_model_seed_after_model_error(
        &self,
        user_id: &str,
        conversation_id: &str,
        error_code: Option<AgentErrorCode>,
    ) {
        if !should_clear_persisted_model(error_code) {
            return;
        }
        if !self
            .runtime_persistence()
            .allows(conversation_id, RuntimeWriteKind::AcpRecoveryCleanup)
        {
            return;
        }

        let row = match self.conversation_repo().get(user_id, conversation_id).await {
            Ok(Some(row)) => row,
            Ok(None) => {
                warn!(
                    conversation_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Conversation ACP model seed clear skipped because conversation row is missing"
                );
                return;
            }
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to load conversation before clearing ACP model seed"
                );
                return;
            }
        };

        let mut extra: serde_json::Value = match serde_json::from_str(&row.extra) {
            Ok(extra) => extra,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Conversation ACP model seed clear skipped because extra JSON is invalid"
                );
                return;
            }
        };

        let Some(extra_obj) = extra.as_object_mut() else {
            warn!(
                conversation_id,
                error_code = ?error_code,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Conversation ACP model seed clear skipped because extra is not an object"
            );
            return;
        };
        let Some(previous_model_value) = extra_obj.remove("current_model_id") else {
            return;
        };
        let previous_model_id = previous_model_value.as_str().map(ToOwned::to_owned);
        if previous_model_id.is_none() {
            warn!(
                conversation_id,
                error_code = ?error_code,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Conversation ACP model seed was malformed and will be cleared"
            );
        }

        let extra_json = match serde_json::to_string(&extra) {
            Ok(json) => json,
            Err(err) => {
                warn!(
                    conversation_id,
                    ?previous_model_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to serialize conversation extra after clearing ACP model seed"
                );
                return;
            }
        };
        let update = ConversationRowUpdate {
            extra: Some(extra_json),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(err) = self.conversation_repo().update(user_id, conversation_id, &update).await {
            warn!(
                conversation_id,
                ?previous_model_id,
                error = %err,
                error_code = ?error_code,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Failed to clear conversation ACP model seed after provider model error"
            );
            return;
        }

        let source = row
            .source
            .as_deref()
            .and_then(|value| string_to_enum::<ConversationSource>(value).ok());
        self.broadcast_list_changed(user_id, conversation_id, "updated", source.as_ref());
        info!(
            conversation_id,
            ?previous_model_id,
            error_code = ?error_code,
            reason = ?AgentKillReason::AgentErrorRecovery,
            "Conversation ACP model seed cleared after provider model error"
        );
    }

    async fn clear_persisted_acp_model_after_model_error(
        &self,
        user_id: &str,
        conversation_id: &str,
        error_code: Option<AgentErrorCode>,
    ) {
        if !should_clear_persisted_model(error_code) {
            return;
        }
        if !self
            .runtime_persistence()
            .allows(conversation_id, RuntimeWriteKind::AcpRecoveryCleanup)
        {
            return;
        }

        let previous_model_id = match self
            .acp_session_repo()
            .load_runtime_state_for_user(user_id, conversation_id)
            .await
        {
            Ok(Some(state)) => state.current_model_id,
            Ok(None) => None,
            Err(err) => {
                warn!(
                    user_id,
                    conversation_id,
                    error = %err,
                    "Failed to load ACP persisted model before clearing after provider model error"
                );
                None
            }
        };

        let params = SaveRuntimeStateParams {
            current_model_id: Some(None),
            ..Default::default()
        };
        match self
            .acp_session_repo()
            .save_runtime_state_for_user(user_id, conversation_id, &params)
            .await
        {
            Ok(true) => {
                info!(
                    user_id,
                    conversation_id,
                    ?previous_model_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "ACP persisted model cleared after provider model error"
                );
            }
            Ok(false) => {
                warn!(
                    user_id,
                    conversation_id,
                    ?previous_model_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "ACP persisted model clear skipped because session row is missing"
                );
            }
            Err(err) => {
                warn!(
                    user_id,
                    conversation_id,
                    ?previous_model_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to clear ACP persisted model after provider model error"
                );
            }
        }
    }

    /// Drop the persisted ACP session id once the agent has disowned it.
    ///
    /// Called from BOTH failure paths: terminal-error eviction and agent task
    /// BUILD failure. The build path is the one that actually mattered for the
    /// reported loop — warmup fails there, so the eviction path never runs.
    ///
    /// The session lives INSIDE that process, so killing it destroys the session
    /// while the id stayed on disk. Every later turn then replayed the dead id at
    /// `session/new`-less warmup and the agent answered `Session not found`,
    /// which is not only wrong but PERMANENT: the real failure (live case: a
    /// missing `Z_AI_API_KEY`, reported once and then never again) became
    /// unreachable because warmup now failed before the prompt was ever sent.
    /// The user saw a retryable USER_AGENT_SESSION_NOT_FOUND that no amount of
    /// retrying could clear.
    ///
    /// Gated on `UserAgentSessionNotFound` — the one code where the agent itself
    /// has declared the id dead. It must NOT be cleared for every terminal error:
    /// a retryable failure triggers an auto-replay rebuild that deliberately
    /// carries the session id forward so the replay resumes the same session
    /// instead of losing the conversation's context (see
    /// `auto_replay_rebuild_keeps_existing_acp_session_id_in_build_options`).
    pub(crate) async fn clear_persisted_acp_session_after_disown(
        &self,
        user_id: &str,
        conversation_id: &str,
        error_code: Option<AgentErrorCode>,
    ) {
        if error_code != Some(AgentErrorCode::UserAgentSessionNotFound) {
            return;
        }
        if !self
            .runtime_persistence()
            .allows(conversation_id, RuntimeWriteKind::AcpRecoveryCleanup)
        {
            return;
        }

        match self
            .acp_session_repo()
            .clear_session_id_for_user(user_id, conversation_id)
            .await
        {
            Ok(true) => info!(
                conversation_id,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Cleared persisted ACP session id after task eviction"
            ),
            Ok(false) => {}
            Err(err) => warn!(
                user_id,
                conversation_id,
                error = %err,
                reason = ?AgentKillReason::AgentErrorRecovery,
                "Failed to clear persisted ACP session id after task eviction; \
                 the next turn may fail with Session not found"
            ),
        }
    }

    pub(crate) async fn evict_acp_task_after_terminal_error(
        &self,
        user_id: &str,
        conversation_id: &str,
        agent_type: AgentType,
        outcome: &RelayOutcome,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> bool {
        if agent_type != AgentType::Acp || !outcome.terminal.is_error() {
            return false;
        }

        let started_at = now_ms();
        let error_code = outcome.terminal.code();
        let retryable = outcome.terminal.retryable();
        info!(
            conversation_id,
            ?agent_type,
            error_code = ?error_code,
            retryable = ?retryable,
            reason = ?AgentKillReason::AgentErrorRecovery,
            "ACP task marked unhealthy after terminal error; evicting task"
        );
        task_manager
            .kill_and_wait(conversation_id, Some(AgentKillReason::AgentErrorRecovery))
            .await;
        self.clear_persisted_acp_session_after_disown(user_id, conversation_id, error_code)
            .await;
        self.clear_persisted_acp_model_after_model_error(user_id, conversation_id, error_code)
            .await;
        self.clear_conversation_model_seed_after_model_error(user_id, conversation_id, error_code)
            .await;
        info!(
            conversation_id,
            ?agent_type,
            error_code = ?error_code,
            retryable = ?retryable,
            elapsed_ms = now_ms().saturating_sub(started_at),
            reason = ?AgentKillReason::AgentErrorRecovery,
            "ACP task eviction completed after terminal error"
        );
        true
    }
}
