use std::sync::Arc;

use aionui_api_types::AgentErrorCode;
use aionui_common::{AgentKillReason, AgentType, now_ms};
use aionui_db::SaveRuntimeStateParams;
use tracing::{info, warn};

use crate::service::ConversationService;
use crate::stream_relay::RelayOutcome;
use aionui_ai_agent::IWorkerTaskManager;

impl ConversationService {
    async fn clear_persisted_acp_model_after_model_not_found(
        &self,
        conversation_id: &str,
        error_code: Option<AgentErrorCode>,
    ) {
        if error_code != Some(AgentErrorCode::UserLlmProviderModelNotFound) {
            return;
        }

        let previous_model_id = match self.acp_session_repo().load_runtime_state(conversation_id).await {
            Ok(Some(state)) => state.current_model_id,
            Ok(None) => None,
            Err(err) => {
                warn!(
                    conversation_id,
                    error = %err,
                    "Failed to load ACP persisted model before clearing after model_not_found"
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
            .save_runtime_state(conversation_id, &params)
            .await
        {
            Ok(true) => {
                info!(
                    conversation_id,
                    ?previous_model_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "ACP persisted model cleared after model_not_found"
                );
            }
            Ok(false) => {
                warn!(
                    conversation_id,
                    ?previous_model_id,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "ACP persisted model clear skipped because session row is missing"
                );
            }
            Err(err) => {
                warn!(
                    conversation_id,
                    ?previous_model_id,
                    error = %err,
                    error_code = ?error_code,
                    reason = ?AgentKillReason::AgentErrorRecovery,
                    "Failed to clear ACP persisted model after model_not_found"
                );
            }
        }
    }

    pub(crate) async fn evict_acp_task_after_terminal_error(
        &self,
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
        self.clear_persisted_acp_model_after_model_not_found(conversation_id, error_code)
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
