use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::engine::AgentEngine;
use aion_agent::output::OutputSink;
use aion_agent::session::Session;
use aion_config::compat::ProviderCompat;
use aion_config::config::{CliArgs, Config, McpServerConfig, ProviderType};
use aion_mcp::manager::McpManager;
use aion_protocol::commands::{ApprovalScope, SessionMode};
use aion_protocol::{ToolApprovalManager, ToolApprovalResult};
use aionui_api_types::{
    AcpConfigOptionDto, AcpConfigSelectOptionDto, AgentModeResponse, ConfigOptionConfirmation,
    GetConfigOptionsResponse, SetConfigOptionResponse, SlashCommandItem,
};
use aionui_common::{AgentKillReason, AgentType, Confirmation, ConversationStatus, ErrorChain, TimestampMs, now_ms};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};
use tokio::time::timeout;
use tracing::{debug, error, info, warn};

use crate::agent_runtime::AgentRuntime;
use crate::agent_task::IAgentTask;
use crate::capability::backend_output_sink::BackendOutputSink;
use crate::capability::backend_protocol_sink::BackendProtocolSink;
use crate::capability::image_input::resolve_image_input_capability;
use crate::dev_prompt_dump::{AgentFinalInputDump, dump_agent_final_input};
use crate::error::AgentError;
use crate::protocol::events::AgentStreamEvent;
use crate::protocol::send_error::AgentSendError;
use crate::types::{AionrsResolvedConfig, SendMessageData};

use super::content::build_content_blocks;
use super::error::{aionrs_engine_error_to_send_error, aionrs_runtime_error_summary};

fn resolve_aionui_config(cli_args: &CliArgs) -> Result<Config, AgentError> {
    let mut config =
        Config::resolve(cli_args).map_err(|e| AgentError::internal(format!("Config resolve failed: {e}")))?;

    // AionUi owns the embedded runtime policy. Standalone aionrs max-token
    // settings must not leak in from global or workspace config files.
    config.max_tokens = None;
    let default_transport = match config.provider {
        ProviderType::Anthropic | ProviderType::Vertex => ProviderCompat::anthropic_defaults().transport,
        ProviderType::OpenAI => ProviderCompat::openai_defaults().transport,
        ProviderType::Bedrock => ProviderCompat::bedrock_defaults().transport,
    };
    config.compat.transport.default_max_tokens = default_transport.default_max_tokens;
    config.compat.transport.model_max_tokens = default_transport.model_max_tokens;

    Ok(config)
}

#[derive(Clone, Debug)]
struct AionrsFinalInputDumpContext {
    dump_dir: PathBuf,
    provider: String,
    model: String,
    base_url: Option<String>,
    system_prompt: Option<String>,
    session_mode: Option<String>,
    skills: Vec<String>,
    mcp_servers: HashMap<String, McpServerConfig>,
    runtime_env: Vec<(String, String)>,
}

fn build_aionrs_final_input_dump_value(
    conversation_id: &str,
    workspace: &str,
    context: &AionrsFinalInputDumpContext,
    data: &SendMessageData,
) -> Value {
    serde_json::json!({
        "kind": "aionrs-final-input",
        "backend": "aionrs",
        "conversation_id": conversation_id,
        "session_id": "none",
        "msg_id": data.msg_id,
        "turn_id": data.turn_id.as_deref().unwrap_or("none"),
        "input": {
            "system_prompt": context.system_prompt.as_deref(),
            "user_content": &data.content,
        },
        "resolved_context": {
            "provider": &context.provider,
            "model": &context.model,
            "base_url": context.base_url.as_deref(),
            "workspace": {
                "path": workspace,
            },
            "session_mode": context.session_mode.as_deref(),
            "skills": &context.skills,
            "mcp_servers": serde_json::to_value(&context.mcp_servers).unwrap_or(Value::Null),
            "runtime_env": &context.runtime_env,
        },
    })
}

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
    confirmations: Arc<RwLock<Vec<Confirmation>>>,
    final_input_dump: Option<AionrsFinalInputDumpContext>,
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
        let runtime_env = config_extra.runtime_env.clone();
        let image_input_override = config_extra.compat_overrides.image_input;
        let image_input_capability = image_input_override.unwrap_or_else(|| {
            resolve_image_input_capability(
                &config_extra.provider,
                config_extra.base_url.as_deref(),
                &config_extra.model,
            )
        });
        info!(
            conversation_id = %conversation_id,
            provider = %config_extra.provider,
            model = %config_extra.model,
            image_input_capability = ?image_input_capability,
            image_input_source = if image_input_override.is_some() { "provider_settings" } else { "catalog" },
            "Resolved image input capability for Aionrs model"
        );
        let final_input_dump = config_extra
            .prompt_dump_dir
            .clone()
            .map(|dump_dir| AionrsFinalInputDumpContext {
                dump_dir,
                provider: config_extra.provider.clone(),
                model: config_extra.model.clone(),
                base_url: config_extra.base_url.clone(),
                system_prompt: config_extra.system_prompt.clone(),
                session_mode: config_extra.session_mode.clone(),
                skills: config_extra.skills.clone(),
                mcp_servers: config_extra.extra_mcp_servers.clone(),
                runtime_env: config_extra.runtime_env.clone(),
            });

        let cli_args = CliArgs {
            provider: Some(config_extra.provider.clone()),
            api_key: Some(config_extra.api_key.clone()),
            base_url: config_extra.base_url.clone(),
            model: Some(config_extra.model.clone()),
            max_tokens: None,
            max_turns: config_extra.max_turns,
            max_tool_call_malformed_turns: config_extra.max_tool_call_malformed_turns,
            max_tool_call_failure_turns: config_extra.max_tool_call_failure_turns,
            system_prompt: config_extra.system_prompt.clone(),
            profile: None,
            auto_approve: config_extra.session_mode.as_deref() == Some("yolo"),
            thinking: None,
            thinking_budget: None,
            project_dir: Some(PathBuf::from(&workspace)),
        };

        let mut config = resolve_aionui_config(&cli_args)?;

        // Backend-specific overrides
        config.bedrock = config_extra.bedrock_config;
        config.session.enabled = true;
        config.session.directory = config_extra.session_directory.to_string_lossy().into_owned();
        config.compat.image_input = Some(image_input_capability);

        if let Some(mode) = config_extra.compat_overrides.openai_api_mode {
            config.compat.transport.openai_api_mode = Some(mode);
        }
        if let Some(field) = config_extra.compat_overrides.max_tokens_field {
            config.compat.transport.max_tokens_field = Some(field);
        }
        if let Some(path) = config_extra.compat_overrides.api_path {
            config.compat.transport.api_path = Some(path);
        }

        if !config_extra.extra_mcp_servers.is_empty() {
            config.mcp.servers.extend(config_extra.extra_mcp_servers.clone());
        }

        let is_resume = resume_session.is_some();
        let provider_label = config.provider_label.clone();

        let mut bootstrap = AgentBootstrap::new(config, &workspace, sink).runtime_env(runtime_env);
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

        let confirmations = Arc::new(RwLock::new(Vec::new()));
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
            final_input_dump,
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

    fn dump_aionrs_final_input(&self, data: &SendMessageData) {
        let Some(context) = self.final_input_dump.as_ref() else {
            return;
        };

        let value = build_aionrs_final_input_dump_value(
            self.runtime.conversation_id(),
            self.runtime.workspace(),
            context,
            data,
        );
        let input = value.get("input").cloned().unwrap_or(Value::Null);
        let resolved_context = value.get("resolved_context").cloned().unwrap_or(Value::Null);

        match dump_agent_final_input(
            &context.dump_dir,
            AgentFinalInputDump {
                kind: "aionrs-final-input",
                backend: "aionrs",
                conversation_id: self.runtime.conversation_id(),
                session_id: None,
                msg_id: Some(data.msg_id.as_str()),
                turn_id: data.turn_id.as_deref(),
                input,
                resolved_context,
            },
        ) {
            Ok(path) => {
                debug!(
                    conversation_id = %self.runtime.conversation_id(),
                    msg_id = %data.msg_id,
                    path = %path.display(),
                    "DEV agent final input dump written"
                );
            }
            Err(error) => {
                warn!(
                    conversation_id = %self.runtime.conversation_id(),
                    msg_id = %data.msg_id,
                    error = %error,
                    "DEV agent final input dump failed"
                );
            }
        }
    }
}

#[async_trait::async_trait]
impl IAgentTask for AionrsAgentManager {
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
        self.dump_aionrs_final_input(&data);

        // Keep attachment paths in the provider-independent history. Images
        // are loaded on demand by aionrs's ViewImage tool.
        debug!(
            attachment_count = data.files.len(),
            "Building structured Aionrs content blocks"
        );
        let content_blocks = build_content_blocks(&data.content, &data.files);
        debug!(
            block_count = content_blocks.len(),
            "Built structured Aionrs content blocks"
        );

        let mut engine = self.engine.lock().await;

        let result = tokio::select! {
            res = engine.run_with_blocks(content_blocks, &data.msg_id) => Some(res),
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
                let summary = aionrs_runtime_error_summary(&e);
                error!(
                    conversation_id = %self.runtime.conversation_id(),
                    elapsed_ms,
                    error = %ErrorChain(&e),
                    "Aionrs engine.run() failed, emitting Error"
                );
                error!(
                    target: "aionui_feedback_diagnostics",
                    diagnostic_event = "feedback.runtime.aionrs_error",
                    conversation_id = %self.runtime.conversation_id(),
                    msg_id = %data.msg_id,
                    turn_id = data.turn_id.as_deref().unwrap_or("none"),
                    elapsed_ms,
                    error_kind = summary.kind,
                    provider_error_class = summary.provider_error_class,
                    http_status = summary.http_status,
                    failure_count = summary.failure_count,
                    failure_limit = summary.failure_limit,
                    "feedback.runtime.aionrs_error"
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
    pub fn kill_and_wait(&self, reason: Option<AgentKillReason>) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let was_running = self.request_stop(reason, "kill");
        let turn_finished_notify = Arc::clone(&self.turn_finished_notify);
        let runtime = self.runtime.clone();
        let conversation_id = self.runtime.conversation_id().to_owned();

        Box::pin(async move {
            if was_running
                && timeout(Duration::from_secs(5), async {
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
                ApprovalScope::Always
            } else {
                ApprovalScope::Once
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

    pub async fn config_options(&self) -> Result<GetConfigOptionsResponse, AgentError> {
        Ok(GetConfigOptionsResponse {
            config_options: vec![aionrs_mode_config_option(self.approval_manager.current_mode())],
        })
    }

    pub async fn set_config_option(&self, option_id: &str, value: &str) -> Result<SetConfigOptionResponse, AgentError> {
        let option_id = option_id.trim();
        let value = value.trim();

        if option_id != AIONRS_MODE_OPTION_ID {
            return Err(AgentError::bad_request(format!(
                "Config option '{option_id}' is not available"
            )));
        }
        if !is_aionrs_session_mode(value) {
            return Err(AgentError::bad_request(format!(
                "Value '{value}' is not selectable for config option '{option_id}'"
            )));
        }

        self.set_mode(value).await?;
        Ok(SetConfigOptionResponse {
            confirmation: ConfigOptionConfirmation::Observed,
            config_options: Some(self.config_options().await?.config_options),
        })
    }

    pub async fn get_slash_commands(&self) -> Result<Vec<SlashCommandItem>, AgentError> {
        Ok(self.slash_commands.clone())
    }
}

const AIONRS_MODE_OPTION_ID: &str = "mode";

fn is_aionrs_session_mode(s: &str) -> bool {
    matches!(s, "default" | "auto_edit" | "yolo")
}

fn aionrs_mode_config_option(current_value: String) -> AcpConfigOptionDto {
    AcpConfigOptionDto {
        id: AIONRS_MODE_OPTION_ID.to_owned(),
        name: Some("Mode".to_owned()),
        label: None,
        description: None,
        category: Some("mode".to_owned()),
        option_type: "select".to_owned(),
        current_value: Some(current_value),
        options: vec![
            aionrs_mode_select_option("default", "Default"),
            aionrs_mode_select_option("auto_edit", "Auto Edit"),
            aionrs_mode_select_option("yolo", "YOLO"),
        ],
    }
}

fn aionrs_mode_select_option(value: &str, name: &str) -> AcpConfigSelectOptionDto {
    AcpConfigSelectOptionDto {
        value: value.to_owned(),
        name: Some(name.to_owned()),
        label: None,
        description: None,
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
#[path = "agent_test.rs"]
mod agent_test;
