use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::engine::AgentEngine;
use aion_agent::output::OutputSink;
use aion_agent::session::Session;
use aion_config::config::{CliArgs, Config};
use aion_mcp::manager::McpManager;
use aion_protocol::commands::SessionMode;
use aion_protocol::{ToolApprovalManager, ToolApprovalResult};
use aionui_api_types::{AgentModeResponse, SlashCommandItem};
use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, ErrorChain, TimestampMs, now_ms};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::{debug, error, info, warn};

use crate::agent_runtime::AgentRuntime;
use crate::capability::backend_output_sink::BackendOutputSink;
use crate::capability::backend_protocol_sink::BackendProtocolSink;
use crate::error::AgentError;
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::{AionrsResolvedConfig, SendMessageData};

use super::error::aionrs_engine_error_to_send_error;

pub struct AionrsAgentManager {
    runtime: AgentRuntime,
    engine: Mutex<AgentEngine>,
    /// Static slash command metadata captured at bootstrap so UI lookups do
    /// not wait behind an active `engine.run()` turn.
    slash_commands: Vec<SlashCommandItem>,
    /// Holds `Arc<McpManager>` instances alive for the duration of this agent's
    /// lifetime. The managers are not accessed after construction — they exist
    /// solely so their underlying MCP connections outlive the engine's event
    /// loop. Rust drops them here, in field-declaration order, after `engine`
    /// and `runtime` are dropped. See the explicit `Drop` impl below.
    #[allow(dead_code)] // intentional: lifetime-extension only; see Drop impl
    mcp_managers: Vec<Arc<McpManager>>,
    approval_manager: Arc<ToolApprovalManager>,
    confirmations: Arc<std::sync::RwLock<Vec<Confirmation>>>,
    /// Signalled by `cancel()` to abort an in-flight `engine.run()` via
    /// `tokio::select!` in `send_message()`.
    cancel_notify: Arc<Notify>,
    /// Signalled after an in-flight turn emits its terminal event.
    turn_finished_notify: Arc<Notify>,
}

impl Drop for AionrsAgentManager {
    fn drop(&mut self) {
        // McpManagers are held alive by the `mcp_managers` field specifically
        // so they outlive the agent's event loop. No explicit cleanup is needed
        // here — the Arc drop path releases each McpManager's underlying MCP
        // connection. This impl exists to document the intentional Drop-order
        // semantics rather than as a lint escape hatch.
    }
}

impl AionrsAgentManager {
    pub async fn new(
        conversation_id: String,
        workspace: String,
        config_extra: AionrsResolvedConfig,
        resume_session: Option<Session>,
    ) -> Result<Self, AgentError> {
        let runtime = AgentRuntime::new(conversation_id.clone(), workspace.clone(), 128);
        let sink: Arc<dyn OutputSink> = Arc::new(BackendOutputSink::new(runtime.event_sender()));

        let cli_args = CliArgs {
            provider: Some(config_extra.provider.clone()),
            api_key: Some(config_extra.api_key.clone()),
            base_url: config_extra.base_url.clone(),
            model: Some(config_extra.model.clone()),
            max_tokens: Some(config_extra.max_tokens),
            max_turns: config_extra.max_turns,
            max_malformed_tool_call_turns: config_extra.max_malformed_tool_call_turns,
            system_prompt: config_extra.system_prompt.clone(),
            profile: None,
            auto_approve: config_extra.session_mode.as_deref() == Some("yolo"),
            project_dir: Some(PathBuf::from(&workspace)),
        };

        let mut config =
            Config::resolve(&cli_args).map_err(|e| AgentError::internal(format!("Config resolve failed: {e}")))?;

        // Backend-specific overrides
        config.bedrock = config_extra.bedrock_config;
        config.session.enabled = true;
        config.session.directory = config_extra.session_directory.to_string_lossy().into_owned();

        if let Some(field) = config_extra.compat_overrides.max_tokens_field {
            config.compat.max_tokens_field = Some(field);
        }
        if let Some(path) = config_extra.compat_overrides.api_path {
            config.compat.api_path = Some(path);
        }

        if !config_extra.extra_mcp_servers.is_empty() {
            config.mcp.servers.extend(config_extra.extra_mcp_servers.clone());
        }

        let is_resume = resume_session.is_some();
        let provider_label = config.provider_label.clone();

        let mut bootstrap = AgentBootstrap::new(config, &workspace, sink);
        if let Some(session) = resume_session {
            info!(
                conversation_id = %conversation_id,
                session_id = %session.id,
                message_count = session.messages.len(),
                "Resuming aionrs session"
            );
            bootstrap = bootstrap.resume(session);
        }

        let result = bootstrap
            .build()
            .await
            .map_err(|e| AgentError::internal(format!("Agent bootstrap failed: {e}")))?;

        let mut engine = result.engine;
        if !is_resume && let Err(e) = engine.init_session(&provider_label, &workspace, Some(&conversation_id)) {
            error!(
                conversation_id = %conversation_id,
                error = %ErrorChain(&*e),
                "Failed to init session, continuing without persistence"
            );
        }

        let approval_manager = Arc::new(ToolApprovalManager::new());

        if let Some(mode_str) = &config_extra.session_mode {
            let mode = parse_session_mode(mode_str);
            approval_manager.set_mode(mode);
            info!(
                conversation_id = %conversation_id,
                session_mode = mode_str,
                "Aionrs initial session mode applied"
            );
        }

        let confirmations = Arc::new(std::sync::RwLock::new(Vec::new()));
        let protocol_sink = BackendProtocolSink::new(runtime.event_sender(), confirmations.clone());
        engine.set_approval_manager(approval_manager.clone());
        engine.set_protocol_writer(Arc::new(protocol_sink));
        let slash_commands = engine
            .slash_command_list()
            .into_iter()
            .map(|(command, description)| SlashCommandItem {
                command,
                description,
                completion_behavior: None,
                empty_turn_tip_code: None,
                empty_turn_tip_params: None,
            })
            .collect();

        runtime.transition_to(ConversationStatus::Pending);

        Ok(Self {
            runtime,
            engine: Mutex::new(engine),
            slash_commands,
            mcp_managers: result.mcp_managers,
            approval_manager,
            confirmations,
            cancel_notify: Arc::new(Notify::new()),
            turn_finished_notify: Arc::new(Notify::new()),
        })
    }

    fn request_stop(&self, reason: Option<AgentKillReason>, operation: &'static str) -> bool {
        let was_running = self.runtime.status() == Some(ConversationStatus::Running);

        if let Ok(mut confs) = self.confirmations.write() {
            confs.clear();
        }

        if was_running {
            self.cancel_notify.notify_waiters();
        }

        info!(
            conversation_id = %self.runtime.conversation_id(),
            ?reason,
            was_running,
            operation,
            "Aionrs stop signal requested"
        );

        was_running
    }
}

#[async_trait::async_trait]
impl crate::agent_task::IAgentTask for AionrsAgentManager {
    fn agent_type(&self) -> AgentType {
        AgentType::Aionrs
    }

    fn conversation_id(&self) -> &str {
        self.runtime.conversation_id()
    }

    fn workspace(&self) -> &str {
        self.runtime.workspace()
    }

    fn status(&self) -> Option<ConversationStatus> {
        self.runtime.status()
    }

    fn last_activity_at(&self) -> TimestampMs {
        self.runtime.last_activity_at()
    }

    fn subscribe(&self) -> broadcast::Receiver<AgentStreamEvent> {
        self.runtime.subscribe()
    }

    async fn send_message(&self, data: SendMessageData) -> Result<(), AgentSendError> {
        let started_at = now_ms();
        info!(
            conversation_id = %self.runtime.conversation_id(),
            msg_id = %data.msg_id,
            turn_id = data.turn_id.as_deref().unwrap_or("none"),
            "Aionrs send_message started"
        );
        self.runtime.bump_activity();
        self.runtime.reset_for_new_turn(ConversationStatus::Running);

        let mut engine = self.engine.lock().await;

        let result = tokio::select! {
            res = engine.run(&data.content, &data.msg_id) => Some(res),
            _ = self.cancel_notify.notified() => {
                info!(
                    conversation_id = %self.runtime.conversation_id(),
                    "Aionrs engine.run() cancelled by stop signal"
                );
                engine.abort_current_turn("Tool execution canceled by user");
                None
            }
        };

        let elapsed_ms = now_ms() - started_at;
        self.runtime.bump_activity();

        let send_result = match result {
            Some(Ok(_)) => {
                info!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    "Aionrs engine.run() completed, emitting Finish"
                );
                self.runtime.emit_finish(None);
                Ok(())
            }
            Some(Err(e)) => {
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    error = %ErrorChain(&e),
                    "Aionrs engine.run() failed, emitting Error"
                );
                let send_error = aionrs_engine_error_to_send_error(&e);
                self.runtime.emit_error_data(send_error.stream_error().clone());
                Err(send_error)
            }
            None => {
                self.runtime.emit_finish(None);
                Ok(())
            }
        };
        self.turn_finished_notify.notify_waiters();
        send_result
    }

    async fn cancel(&self) -> Result<(), AgentError> {
        self.request_stop(None, "cancel");
        Ok(())
    }

    fn kill(&self, reason: Option<AgentKillReason>) -> Result<(), AgentError> {
        self.request_stop(reason, "kill");
        Ok(())
    }
}

impl AionrsAgentManager {
    pub fn kill_and_wait(
        &self,
        reason: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        let was_running = self.request_stop(reason, "kill");
        let turn_finished_notify = Arc::clone(&self.turn_finished_notify);
        let runtime = self.runtime.clone();
        let conversation_id = self.runtime.conversation_id().to_owned();

        Box::pin(async move {
            if was_running
                && tokio::time::timeout(Duration::from_secs(5), async {
                    while runtime.status() == Some(ConversationStatus::Running) {
                        turn_finished_notify.notified().await;
                    }
                })
                .await
                .is_err()
            {
                warn!(
                    conversation_id,
                    "Timed out waiting for aionrs turn to finish after kill"
                );
            }
        })
    }
}

/// Aionrs-specific operations reached through `AgentInstance::Aionrs(..)`
/// matches in the routes + services.
impl AionrsAgentManager {
    pub fn confirm(&self, _msg_id: &str, call_id: &str, data: Value, always_allow: bool) -> Result<(), AgentError> {
        if let Ok(mut confs) = self.confirmations.write() {
            confs.retain(|c| c.call_id != call_id);
        }

        let value = data.get("value").and_then(|v| v.as_str()).unwrap_or("cancel");

        let is_cancel = value == "cancel";

        debug!(
            conversation_id = %self.runtime.conversation_id(),
            call_id,
            value,
            always_allow,
            "Aionrs confirm"
        );

        if is_cancel {
            self.approval_manager.resolve(
                call_id,
                ToolApprovalResult::Denied {
                    reason: "User denied the tool request".into(),
                },
            );
        } else {
            let scope = if always_allow {
                aion_protocol::commands::ApprovalScope::Always
            } else {
                aion_protocol::commands::ApprovalScope::Once
            };
            self.approval_manager.approve(call_id, scope);
        }
        Ok(())
    }

    pub fn get_confirmations(&self) -> Vec<Confirmation> {
        self.confirmations.read().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn check_approval(&self, action: &str, _command_type: Option<&str>) -> bool {
        self.approval_manager.is_auto_approved(action)
    }

    pub async fn mode(&self) -> Result<AgentModeResponse, AgentError> {
        Ok(AgentModeResponse {
            mode: self.approval_manager.current_mode(),
            initialized: true,
        })
    }

    pub async fn set_mode(&self, mode: &str) -> Result<(), AgentError> {
        let prev = self.approval_manager.current_mode();
        self.approval_manager.set_mode(parse_session_mode(mode));
        info!(
            conversation_id = %self.runtime.conversation_id(),
            from = prev,
            to = mode,
            "Aionrs session mode switched"
        );
        Ok(())
    }

    pub async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AgentError> {
        Ok(self.slash_commands.clone())
    }
}

fn parse_session_mode(s: &str) -> SessionMode {
    match s {
        "auto_edit" => SessionMode::AutoEdit,
        "yolo" => SessionMode::Yolo,
        _ => SessionMode::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_task::IAgentTask;

    async fn assert_no_stop_signal(agent: &AionrsAgentManager) {
        let notified = agent.cancel_notify.notified();
        tokio::pin!(notified);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut notified)
                .await
                .is_err(),
            "idle stop must not leave a stale cancellation signal for the next turn"
        );
    }

    fn make_test_config() -> AionrsResolvedConfig {
        AionrsResolvedConfig {
            provider: "anthropic".into(),
            api_key: "sk-test-key".into(),
            model: "claude-sonnet-4-20250514".into(),
            base_url: None,
            system_prompt: None,
            max_tokens: 4096,
            max_turns: None,
            max_malformed_tool_call_turns: None,
            compat_overrides: Default::default(),
            session_directory: std::env::temp_dir().join("aionrs-test-sessions"),
            session_mode: None,
            extra_mcp_servers: std::collections::HashMap::new(),
            bedrock_config: None,
        }
    }

    #[tokio::test]
    async fn aionrs_agent_returns_correct_type() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        assert_eq!(agent.agent_type(), AgentType::Aionrs);
        assert_eq!(agent.workspace(), "/project");
        assert_eq!(agent.conversation_id(), "conv-1");
    }

    #[tokio::test]
    async fn aionrs_agent_initial_status_is_pending() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        assert_eq!(agent.status(), Some(ConversationStatus::Pending));
    }

    #[tokio::test]
    async fn aionrs_agent_subscribe_returns_receiver() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        let _rx = agent.subscribe();
    }

    #[tokio::test]
    async fn aionrs_agent_kill_succeeds() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        assert!(agent.kill(None).is_ok());
        // Idle kill only clears transient state; task-manager removal owns lifecycle cleanup.
        assert_eq!(agent.status(), Some(ConversationStatus::Pending));
    }

    #[tokio::test]
    async fn aionrs_agent_kill_with_reason_succeeds() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        assert!(agent.kill(Some(AgentKillReason::IdleTimeout)).is_ok());
    }

    #[tokio::test]
    async fn aionrs_agent_kill_running_turn_sends_stop_signal() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        agent.runtime.reset_for_new_turn(ConversationStatus::Running);

        let notified = agent.cancel_notify.notified();
        tokio::pin!(notified);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut notified)
                .await
                .is_err()
        );

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("kill should request stop");

        tokio::time::timeout(std::time::Duration::from_millis(50), &mut notified)
            .await
            .expect("running kill should wake in-flight turn");
    }

    #[tokio::test]
    async fn aionrs_agent_kill_and_wait_waits_for_running_turn_terminal() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        agent.runtime.reset_for_new_turn(ConversationStatus::Running);

        let wait = agent.kill_and_wait(Some(AgentKillReason::ConversationDeleted));
        tokio::pin!(wait);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut wait)
                .await
                .is_err(),
            "kill_and_wait must not return before a running turn reaches a terminal event"
        );

        agent.runtime.emit_finish(None);
        agent.turn_finished_notify.notify_waiters();

        tokio::time::timeout(std::time::Duration::from_millis(50), &mut wait)
            .await
            .expect("kill_and_wait should return after terminal notification");
    }

    #[tokio::test]
    async fn aionrs_agent_kill_idle_turn_does_not_leave_stale_stop_signal() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();

        agent
            .kill(Some(AgentKillReason::ConversationDeleted))
            .expect("idle kill should be harmless");

        assert_no_stop_signal(&agent).await;
    }

    #[tokio::test]
    async fn aionrs_agent_confirmations_initially_empty() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        assert!(agent.get_confirmations().is_empty());
    }

    #[tokio::test]
    async fn aionrs_agent_get_slash_commands_does_not_wait_for_engine_lock() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();

        let _engine_guard = agent.engine.lock().await;
        let commands = tokio::time::timeout(std::time::Duration::from_millis(50), agent.get_slash_commands())
            .await
            .expect("slash command metadata should not wait for an active engine run")
            .unwrap();

        assert!(!commands.is_empty());
    }

    #[tokio::test]
    async fn aionrs_agent_check_approval_returns_false_by_default() {
        let agent = AionrsAgentManager::new("conv-1".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        assert!(!agent.check_approval("any_action", None));
    }

    #[tokio::test]
    async fn stop_only_signals_in_flight_run() {
        let agent = AionrsAgentManager::new("conv-stop".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        let mut rx = agent.subscribe();

        agent.cancel().await.unwrap();

        assert_eq!(agent.status(), Some(ConversationStatus::Pending));
        assert!(matches!(rx.try_recv(), Err(broadcast::error::TryRecvError::Empty)));
        assert_no_stop_signal(&agent).await;
    }

    #[tokio::test]
    async fn runtime_can_emit_error_and_finish() {
        let agent = AionrsAgentManager::new("conv-err".into(), "/project".into(), make_test_config(), None)
            .await
            .unwrap();
        let mut rx = agent.subscribe();

        agent.runtime.emit_error("test error");
        // emit_error sets status to Finished, so emit_finish is a no-op here.
        // We emit directly for the Finish broadcast path test:
        agent
            .runtime
            .emit(AgentStreamEvent::Finish(crate::protocol::events::FinishEventData {
                session_id: None,
            }));

        match rx.try_recv().unwrap() {
            AgentStreamEvent::Error(data) => assert_eq!(data.message, "test error"),
            other => panic!("Expected Error, got {:?}", other),
        }
        match rx.try_recv().unwrap() {
            AgentStreamEvent::Finish(_) => {}
            other => panic!("Expected Finish, got {:?}", other),
        }
    }
}
