use std::sync::Arc;

use aionui_ai_agent::{AgentStreamEvent, IWorkerTaskManager};
use aionui_api_types::{CreateConversationRequest, SendMessageRequest};
use aionui_common::{AgentType, ConversationSource};
use aionui_conversation::ConversationService;
use aionui_db::models::AssistantSessionRow;
use tokio::sync::broadcast;
use tracing::{debug, info, warn};

use crate::channel_settings::{ChannelSettingsService, resolved_model_to_provider};
use crate::constants::{STREAM_THROTTLE_INTERVAL, TOOL_CONFIRM_TIMEOUT};
use crate::error::ChannelError;
use crate::types::{ActionButton, OutgoingMessageType, PluginType, UnifiedOutgoingMessage};

const DEPRECATED_AGENT_TYPE_MESSAGE: &str = "This agent type is no longer supported for new conversations.";

/// Bridges channel messages to the conversation + AI agent layer.
///
/// Responsibilities:
/// - Creating conversations for channel sessions
/// - Sending user messages to the AI agent
/// - Receiving stream events and converting them to outgoing messages
/// - Throttling editMessage calls for streaming responses
/// - Handling tool confirmation with timeout
pub struct ChannelMessageService {
    conversation_svc: Arc<ConversationService>,
    task_manager: Arc<dyn IWorkerTaskManager>,
    settings: Arc<ChannelSettingsService>,
    owner_user_id: String,
}

impl ChannelMessageService {
    pub fn new(
        conversation_svc: Arc<ConversationService>,
        task_manager: Arc<dyn IWorkerTaskManager>,
        settings: Arc<ChannelSettingsService>,
        owner_user_id: String,
    ) -> Self {
        Self {
            conversation_svc,
            task_manager,
            settings,
            owner_user_id,
        }
    }

    /// Sends a text message from a channel user to the AI agent.
    ///
    /// 1. Ensures the session has a backing conversation (creates one if needed)
    /// 2. Warms up the backing agent task so stream subscription is available
    /// 3. Sends the message via ConversationService
    /// 4. Returns the conversation_id and stream receiver for relay
    ///
    /// The caller is responsible for subscribing to stream events and
    /// relaying them to the IM platform.
    pub async fn send_to_agent(
        &self,
        session: &AssistantSessionRow,
        text: &str,
        platform: PluginType,
    ) -> Result<SendResult, ChannelError> {
        // Ensure conversation exists
        let conversation_id = match &session.conversation_id {
            Some(cid) => cid.clone(),
            None => self.create_conversation_for_session(session, platform).await?,
        };

        // Send message through ConversationService. `msg_id` is now
        // server-generated inside the service; channel plugins that need to
        // correlate the user message back to the conversation should use
        // `conversation_id` + stream events instead of a client-provided id.
        let req = SendMessageRequest {
            content: text.to_owned(),
            files: vec![],
            inject_skills: vec![],
            hidden: false,
        };

        let user_id = &self.owner_user_id;
        // Channel relays need a stream subscription before the agent starts
        // emitting. `ConversationService::send_message` returns immediately
        // and builds cold agents in the background, so warm the conversation
        // explicitly for channel traffic.
        self.conversation_svc
            .warmup(user_id, &conversation_id, &self.task_manager)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        let stream_rx = self
            .task_manager
            .get_task(&conversation_id)
            .map(|handle| handle.subscribe())
            .ok_or_else(|| {
                ChannelError::MessageSendFailed(format!(
                    "Agent task missing after warmup for conversation {conversation_id}"
                ))
            })?;

        self.conversation_svc
            .send_message(user_id, &conversation_id, req, &self.task_manager)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        info!(
            conversation_id = %conversation_id,
            session_id = %session.id,
            has_stream = true,
            "message sent to agent"
        );

        Ok(SendResult {
            conversation_id,
            stream_rx: Some(stream_rx),
        })
    }

    /// Creates a new conversation for a channel session.
    ///
    /// Sets `source` to the appropriate platform and `channel_chat_id`
    /// for per-chat isolation.
    async fn create_conversation_for_session(
        &self,
        session: &AssistantSessionRow,
        platform: PluginType,
    ) -> Result<String, ChannelError> {
        let source = platform_to_source(platform);
        let agent_type = parse_agent_type(&session.agent_type)?;

        let agent_config = self.settings.get_agent_config(platform).await?;
        let model_config = self.settings.get_model_config(platform).await?;
        let model = resolved_model_to_provider(model_config.as_ref());
        let mut extra = Self::build_channel_extra(agent_config.backend.as_deref());
        let name = channel_conversation_name(
            platform,
            &session.agent_type,
            agent_config.backend.as_deref(),
            session.chat_id.as_deref(),
        );

        // Top-level `model` is only accepted for aionrs; other types pass via `extra`.
        let top_level_model = if agent_type == AgentType::Aionrs {
            Some(model)
        } else {
            extra["model"] = serde_json::to_value(&model).unwrap_or_default();
            None
        };

        let req = CreateConversationRequest {
            r#type: agent_type,
            name: Some(name),
            model: top_level_model,
            assistant: None,
            source: Some(source),
            channel_chat_id: session.chat_id.clone(),
            extra,
        };

        let response = self
            .conversation_svc
            .create(&self.owner_user_id, req)
            .await
            .map_err(|e| ChannelError::MessageSendFailed(e.to_string()))?;

        debug!(
            conversation_id = %response.id,
            session_id = %session.id,
            "conversation created for channel session"
        );

        Ok(response.id)
    }

    /// Processes a stream event from the AI agent and converts it to
    /// an optional outgoing message for the IM platform.
    ///
    /// Returns `None` for events that don't need to be sent to the user
    /// (e.g., internal status updates, thinking traces).
    pub fn process_stream_event(event: &AgentStreamEvent) -> Option<StreamAction> {
        match event {
            AgentStreamEvent::Text(data) => Some(StreamAction::AppendText(data.content.clone())),
            AgentStreamEvent::Finish(_) => Some(StreamAction::Finish),
            AgentStreamEvent::Error(data) => Some(StreamAction::Error(data.message.clone())),
            AgentStreamEvent::Thinking(data) => Some(StreamAction::Thinking(data.content.clone())),
            AgentStreamEvent::ToolCall(data) => Some(StreamAction::ToolCall {
                name: data.name.clone(),
                status: format!("{:?}", data.status),
            }),
            // Events that don't produce user-facing messages
            AgentStreamEvent::Start(_)
            | AgentStreamEvent::Tips(_)
            | AgentStreamEvent::ToolGroup(_)
            | AgentStreamEvent::AgentStatus(_)
            | AgentStreamEvent::Plan(_)
            | AgentStreamEvent::Permission(_)
            | AgentStreamEvent::AcpPermission(_)
            | AgentStreamEvent::AcpToolCall(_)
            | AgentStreamEvent::AvailableCommands(_)
            | AgentStreamEvent::SkillSuggest(_)
            | AgentStreamEvent::CronTrigger(_)
            | AgentStreamEvent::AcpModelInfo(_)
            | AgentStreamEvent::AcpModeInfo(_)
            | AgentStreamEvent::AcpConfigOption(_)
            | AgentStreamEvent::AcpSessionInfo(_)
            | AgentStreamEvent::AcpContextUsage(_)
            | AgentStreamEvent::AcpPromptHookWarning(_)
            | AgentStreamEvent::System(_)
            | AgentStreamEvent::RequestTrace(_)
            | AgentStreamEvent::SlashCommandsUpdated(_)
            | AgentStreamEvent::SessionAssigned(_) => None,
        }
    }

    /// Builds the "thinking" placeholder message sent immediately after
    /// receiving a user message, before the AI starts streaming.
    pub fn build_thinking_message() -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some("\u{23f3} Thinking...".into()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Builds the final message after streaming completes, including
    /// action buttons for the user.
    pub fn build_final_message(text: &str) -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Buttons,
            text: Some(text.to_owned()),
            parse_mode: None,
            buttons: Some(vec![vec![
                ActionButton {
                    label: "\u{1f504} Regenerate".into(),
                    action: "chat.regenerate".into(),
                    params: None,
                },
                ActionButton {
                    label: "\u{25b6}\u{fe0f} Continue".into(),
                    action: "chat.continue".into(),
                    params: None,
                },
                ActionButton {
                    label: "\u{2795} New Session".into(),
                    action: "session.new".into(),
                    params: None,
                },
            ]]),
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Builds an intermediate streaming message (for editMessage calls).
    pub fn build_streaming_message(text: &str) -> UnifiedOutgoingMessage {
        UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(text.to_owned()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        }
    }

    /// Returns the stream throttle interval for editMessage calls.
    pub fn throttle_interval() -> std::time::Duration {
        STREAM_THROTTLE_INTERVAL
    }

    /// Returns the tool confirmation timeout duration.
    pub fn confirm_timeout() -> std::time::Duration {
        TOOL_CONFIRM_TIMEOUT
    }

    /// Build the `extra` JSON for channel conversations.
    ///
    /// Sets `session_mode` to `"yolo"` so the agent auto-approves tool calls —
    /// channel users have no interactive UI for confirmations.
    pub fn build_channel_extra(backend: Option<&str>) -> serde_json::Value {
        let mut extra = serde_json::json!({
            "session_mode": "yolo",
        });
        if let Some(b) = backend {
            extra["backend"] = serde_json::Value::String(b.to_owned());
        }
        extra
    }
}

/// Result of sending a message to the agent.
#[derive(Debug)]
pub struct SendResult {
    pub conversation_id: String,
    /// Agent event stream for the ChannelStreamRelay.
    /// `None` when the agent task could not be found after sending
    /// (should not happen in normal flow).
    pub stream_rx: Option<broadcast::Receiver<AgentStreamEvent>>,
}

/// Actions derived from agent stream events.
#[derive(Debug, Clone)]
pub enum StreamAction {
    /// Append text content to the current response.
    AppendText(String),
    /// Streaming finished.
    Finish,
    /// An error occurred.
    Error(String),
    /// Agent is thinking/reasoning.
    Thinking(String),
    /// Tool call status update.
    ToolCall { name: String, status: String },
}

/// Maps a PluginType to the corresponding ConversationSource.
fn platform_to_source(platform: PluginType) -> ConversationSource {
    match platform {
        PluginType::Telegram => ConversationSource::Telegram,
        PluginType::Lark => ConversationSource::Lark,
        PluginType::Dingtalk => ConversationSource::Dingtalk,
        PluginType::Weixin => ConversationSource::Weixin,
        // Reserved variants default to Aionui
        PluginType::Slack | PluginType::Discord => ConversationSource::Aionui,
    }
}

/// Parses a top-level agent_type string to an AgentType enum.
///
/// Falls back to `AgentType::Acp` for unknown values.
fn parse_agent_type(s: &str) -> Result<AgentType, ChannelError> {
    let agent_type = match s {
        "acp" => AgentType::Acp,
        "gemini" => AgentType::Gemini,
        "openclaw-gateway" => AgentType::OpenclawGateway,
        "nanobot" => AgentType::Nanobot,
        "remote" => AgentType::Remote,
        "aionrs" => AgentType::Aionrs,
        _ => {
            warn!(agent_type = %s, "unknown agent type, defaulting to Acp");
            AgentType::Acp
        }
    };

    if agent_type.is_deprecated_runtime() {
        return Err(ChannelError::InvalidConfig(DEPRECATED_AGENT_TYPE_MESSAGE.into()));
    }

    Ok(agent_type)
}

fn channel_conversation_name(
    platform: PluginType,
    agent_type: &str,
    backend: Option<&str>,
    chat_id: Option<&str>,
) -> String {
    let short = match platform {
        PluginType::Telegram => "tg",
        PluginType::Lark => "lark",
        PluginType::Dingtalk => "ding",
        PluginType::Weixin => "wx",
        PluginType::Slack => "slack",
        PluginType::Discord => "discord",
    };

    let mut parts = vec![short.to_owned()];
    if !agent_type.is_empty() {
        parts.push(agent_type.to_owned());
    }
    if agent_type == "acp"
        && let Some(b) = backend
    {
        parts.push(b.to_owned());
    }
    if let Some(cid) = chat_id {
        let end = cid.len().min(8);
        parts.push(cid[..end].to_owned());
    }
    parts.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::protocol::events::{
        ErrorEventData, FinishEventData, StartEventData, TextEventData, ThinkingEventData, ToolCallEventData,
        ToolCallStatus,
    };
    use aionui_common::ProviderWithModel;

    // ── platform_to_source ─────────────────────────────────────────────

    #[test]
    fn platform_to_source_telegram() {
        assert_eq!(platform_to_source(PluginType::Telegram), ConversationSource::Telegram);
    }

    #[test]
    fn platform_to_source_lark() {
        assert_eq!(platform_to_source(PluginType::Lark), ConversationSource::Lark);
    }

    #[test]
    fn platform_to_source_dingtalk() {
        assert_eq!(platform_to_source(PluginType::Dingtalk), ConversationSource::Dingtalk);
    }

    #[test]
    fn platform_to_source_weixin() {
        assert_eq!(platform_to_source(PluginType::Weixin), ConversationSource::Weixin);
    }

    #[test]
    fn platform_to_source_reserved_defaults_to_aionui() {
        assert_eq!(platform_to_source(PluginType::Slack), ConversationSource::Aionui);
        assert_eq!(platform_to_source(PluginType::Discord), ConversationSource::Aionui);
    }

    // ── parse_agent_type ───────────────────────────────────────────────

    #[test]
    fn parse_known_agent_types() {
        assert_eq!(parse_agent_type("acp").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("aionrs").unwrap(), AgentType::Aionrs);
    }

    #[test]
    fn parse_agent_type_rejects_deprecated_channel_runtime_types() {
        for raw in ["openclaw-gateway", "nanobot", "remote", "gemini"] {
            let err = parse_agent_type(raw).unwrap_err();
            assert!(matches!(err, ChannelError::InvalidConfig(_)));
        }
    }

    #[test]
    fn parse_unknown_agent_type_defaults_to_acp() {
        assert_eq!(parse_agent_type("unknown").unwrap(), AgentType::Acp);
        assert_eq!(parse_agent_type("").unwrap(), AgentType::Acp);
    }

    // ── process_stream_event ───────────────────────────────────────────

    #[test]
    fn text_event_produces_append() {
        let event = AgentStreamEvent::Text(TextEventData {
            content: "Hello".into(),
        });
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::AppendText(text)) => assert_eq!(text, "Hello"),
            _ => panic!("Expected AppendText"),
        }
    }

    #[test]
    fn finish_event_produces_finish() {
        let event = AgentStreamEvent::Finish(FinishEventData { session_id: None });
        let action = ChannelMessageService::process_stream_event(&event);
        assert!(matches!(action, Some(StreamAction::Finish)));
    }

    #[test]
    fn error_event_produces_error() {
        let event = AgentStreamEvent::Error(ErrorEventData::legacy("timeout", None));
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::Error(msg)) => assert_eq!(msg, "timeout"),
            _ => panic!("Expected Error"),
        }
    }

    #[test]
    fn thinking_event_produces_thinking() {
        let event = AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Analyzing...".into(),
            subject: None,
            duration: None,
            status: None,
        });
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::Thinking(text)) => assert_eq!(text, "Analyzing..."),
            _ => panic!("Expected Thinking"),
        }
    }

    #[test]
    fn tool_call_event_produces_tool_call() {
        let event = AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "c1".into(),
            name: "read_file".into(),
            args: serde_json::Value::Null,
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        });
        let action = ChannelMessageService::process_stream_event(&event);
        match action {
            Some(StreamAction::ToolCall { name, status }) => {
                assert_eq!(name, "read_file");
                assert_eq!(status, "Running");
            }
            _ => panic!("Expected ToolCall"),
        }
    }

    #[test]
    fn start_event_produces_none() {
        let event = AgentStreamEvent::Start(StartEventData { session_id: None });
        assert!(ChannelMessageService::process_stream_event(&event).is_none());
    }

    // ── build_thinking_message ─────────────────────────────────────────

    #[test]
    fn thinking_message_has_text() {
        let msg = ChannelMessageService::build_thinking_message();
        assert_eq!(msg.message_type, OutgoingMessageType::Text);
        let text = msg.text.unwrap();
        assert!(text.contains("Thinking"));
    }

    // ── build_final_message ────────────────────────────────────────────

    #[test]
    fn final_message_has_buttons() {
        let msg = ChannelMessageService::build_final_message("Response text");
        assert_eq!(msg.message_type, OutgoingMessageType::Buttons);
        assert_eq!(msg.text.as_deref(), Some("Response text"));
        let buttons = msg.buttons.unwrap();
        assert!(!buttons.is_empty());
        assert!(buttons[0].len() >= 2);
    }

    // ── build_streaming_message ────────────────────────────────────────

    #[test]
    fn streaming_message_is_plain_text() {
        let msg = ChannelMessageService::build_streaming_message("partial...");
        assert_eq!(msg.message_type, OutgoingMessageType::Text);
        assert_eq!(msg.text.as_deref(), Some("partial..."));
        assert!(msg.buttons.is_none());
    }

    // ── throttle & timeout constants ───────────────────────────────────

    #[test]
    fn throttle_interval_is_500ms() {
        assert_eq!(
            ChannelMessageService::throttle_interval(),
            std::time::Duration::from_millis(500)
        );
    }

    #[test]
    fn confirm_timeout_is_15s() {
        assert_eq!(
            ChannelMessageService::confirm_timeout(),
            std::time::Duration::from_secs(15)
        );
    }

    // ── build_channel_extra ───────────────────────────────────────────

    #[test]
    fn yolo_extra_contains_session_mode() {
        let extra = ChannelMessageService::build_channel_extra(None);
        assert_eq!(extra["session_mode"], "yolo");
        assert!(extra.get("backend").is_none());
    }

    #[test]
    fn yolo_extra_with_backend() {
        let extra = ChannelMessageService::build_channel_extra(Some("claude"));
        assert_eq!(extra["session_mode"], "yolo");
        assert_eq!(extra["backend"], "claude");
    }

    // ── model placement by agent_type (regression: non-aionrs must not
    //    use top-level model) ──────────────────────────────────────────

    #[test]
    fn acp_model_goes_into_extra_not_top_level() {
        let agent_type = AgentType::Acp;
        let model = ProviderWithModel {
            provider_id: "prov1".into(),
            model: "claude-sonnet".into(),
            use_model: Some("global.anthropic.claude-sonnet-4-6".into()),
        };
        let mut extra = ChannelMessageService::build_channel_extra(Some("codex"));

        let top_level_model = if agent_type == AgentType::Aionrs {
            Some(model.clone())
        } else {
            extra["model"] = serde_json::to_value(&model).unwrap();
            None
        };

        assert!(top_level_model.is_none(), "acp must not have top-level model");
        assert_eq!(extra["model"]["provider_id"], "prov1");
        assert_eq!(extra["model"]["use_model"], "global.anthropic.claude-sonnet-4-6");
    }

    #[test]
    fn aionrs_model_stays_at_top_level() {
        let agent_type = AgentType::Aionrs;
        let model = ProviderWithModel {
            provider_id: "prov2".into(),
            model: "gpt-4o".into(),
            use_model: None,
        };
        let mut extra = ChannelMessageService::build_channel_extra(None);

        let top_level_model = if agent_type == AgentType::Aionrs {
            Some(model.clone())
        } else {
            extra["model"] = serde_json::to_value(&model).unwrap();
            None
        };

        assert!(top_level_model.is_some(), "aionrs must use top-level model");
        assert!(extra.get("model").is_none() || extra["model"].is_null());
    }

    // ── channel_conversation_name ─────────────────────────────────────

    #[test]
    fn conv_name_telegram_acp_with_backend() {
        let name = channel_conversation_name(PluginType::Telegram, "acp", Some("claude"), Some("70880480"));
        assert_eq!(name, "tg-acp-claude-70880480");
    }

    #[test]
    fn conv_name_telegram_aionrs() {
        let name = channel_conversation_name(PluginType::Telegram, "aionrs", None, Some("70880480"));
        assert_eq!(name, "tg-aionrs-70880480");
    }

    #[test]
    fn conv_name_lark_acp_no_backend() {
        let name = channel_conversation_name(PluginType::Lark, "acp", None, Some("abcdef12"));
        assert_eq!(name, "lark-acp-abcdef12");
    }

    #[test]
    fn conv_name_dingtalk_truncates_long_chat_id() {
        let name = channel_conversation_name(PluginType::Dingtalk, "acp", Some("vertex"), Some("123456789abcdef"));
        assert_eq!(name, "ding-acp-vertex-12345678");
    }

    #[test]
    fn conv_name_weixin_no_chat_id() {
        let name = channel_conversation_name(PluginType::Weixin, "acp", Some("gemini"), None);
        assert_eq!(name, "wx-acp-gemini");
    }

    #[test]
    fn conv_name_non_acp_ignores_backend() {
        let name = channel_conversation_name(PluginType::Telegram, "aionrs", Some("claude"), Some("70880480"));
        assert_eq!(name, "tg-aionrs-70880480");
    }
}
