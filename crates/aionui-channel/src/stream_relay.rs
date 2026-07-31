use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_ai_agent::AgentStreamEvent;
use async_trait::async_trait;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::error::ChannelError;
use crate::formatter::format_text_for_platform;
use crate::message_service::{ChannelMessageService, StreamAction};
use crate::types::{OutgoingMessageType, PluginType, UnifiedOutgoingMessage};

/// Configuration for a stream relay session.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub owner_user_id: String,
    pub platform: PluginType,
    pub plugin_id: String,
    pub chat_id: String,
    pub throttle_ms: u64,
}

/// Abstraction for sending/editing messages through a channel plugin.
///
/// Decouples ChannelStreamRelay from ChannelManager for testability.
#[async_trait]
pub trait ChannelSender: Send + Sync {
    async fn send_message(
        &self,
        owner_user_id: &str,
        plugin_id: &str,
        chat_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError>;

    async fn edit_message(
        &self,
        owner_user_id: &str,
        plugin_id: &str,
        chat_id: &str,
        message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError>;
}

/// Relays agent stream events to an IM platform.
///
/// Responsibilities:
/// - Send "Thinking..." placeholder on start
/// - Accumulate text, throttled editMessage every N ms
/// - Send final message with action buttons on Finish
/// - Send error message on Error
pub struct ChannelStreamRelay {
    config: RelayConfig,
    sender: Arc<dyn ChannelSender>,
}

impl ChannelStreamRelay {
    pub fn new(config: RelayConfig, sender: Arc<dyn ChannelSender>) -> Self {
        Self { config, sender }
    }

    /// Run the relay loop until the agent stream ends.
    pub async fn run(self, rx: broadcast::Receiver<AgentStreamEvent>) {
        if is_weixin_platform(self.config.platform) {
            self.run_weixin(rx).await;
        } else {
            self.run_editable(rx).await;
        }
    }

    /// WeChat-specific relay: no edit support, accumulate text then send once.
    async fn run_weixin(self, mut rx: broadcast::Receiver<AgentStreamEvent>) {
        let mut text_buffer = String::new();
        let mut has_content = false;

        loop {
            match rx.recv().await {
                Ok(event) => match ChannelMessageService::process_stream_event(&event) {
                    Some(StreamAction::AppendText(chunk)) => {
                        text_buffer.push_str(&chunk);
                        has_content = true;
                    }
                    Some(StreamAction::Thinking(_)) => {}
                    Some(StreamAction::ToolCall { .. }) if has_content && !text_buffer.trim().is_empty() => {
                        let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                        let flush_msg = ChannelMessageService::build_streaming_message(&formatted);
                        let _ = self
                            .sender
                            .send_message(
                                &self.config.owner_user_id,
                                &self.config.plugin_id,
                                &self.config.chat_id,
                                flush_msg,
                            )
                            .await;
                        text_buffer.clear();
                        has_content = false;
                    }
                    Some(StreamAction::ToolCall { .. }) => {}
                    Some(StreamAction::Finish) => {
                        if has_content && !text_buffer.trim().is_empty() {
                            let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                            let final_msg = ChannelMessageService::build_final_message(&formatted);
                            let _ = self
                                .sender
                                .send_message(
                                    &self.config.owner_user_id,
                                    &self.config.plugin_id,
                                    &self.config.chat_id,
                                    final_msg,
                                )
                                .await;
                        }
                        info!(
                            plugin_id = %self.config.plugin_id,
                            chat_id = %self.config.chat_id,
                            text_len = text_buffer.len(),
                            "channel stream relay finished (weixin)"
                        );
                        break;
                    }
                    Some(StreamAction::Error(msg)) => {
                        let error_msg = UnifiedOutgoingMessage {
                            message_type: OutgoingMessageType::Text,
                            text: Some(format!("\u{274c} {msg}")),
                            parse_mode: None,
                            buttons: None,
                            keyboard: None,
                            image_url: None,
                            file_url: None,
                            file_name: None,
                            media_actions: None,
                            reply_to_message_id: None,
                            silent: None,
                        };
                        let _ = self
                            .sender
                            .send_message(
                                &self.config.owner_user_id,
                                &self.config.plugin_id,
                                &self.config.chat_id,
                                error_msg,
                            )
                            .await;
                        break;
                    }
                    None => {}
                },
                Err(broadcast::error::RecvError::Closed) => {
                    if has_content && !text_buffer.trim().is_empty() {
                        let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                        let final_msg = ChannelMessageService::build_final_message(&formatted);
                        let _ = self
                            .sender
                            .send_message(
                                &self.config.owner_user_id,
                                &self.config.plugin_id,
                                &self.config.chat_id,
                                final_msg,
                            )
                            .await;
                    }
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "channel stream relay lagged (weixin)");
                }
            }
        }

        debug!(
            plugin_id = %self.config.plugin_id,
            chat_id = %self.config.chat_id,
            "channel stream relay exited (weixin)"
        );
    }

    /// Standard relay for platforms that support edit (Telegram, Lark, DingTalk).
    async fn run_editable(self, mut rx: broadcast::Receiver<AgentStreamEvent>) {
        let throttle = Duration::from_millis(self.config.throttle_ms);

        let thinking_msg = ChannelMessageService::build_thinking_message();
        let thinking_msg_id = match self
            .sender
            .send_message(
                &self.config.owner_user_id,
                &self.config.plugin_id,
                &self.config.chat_id,
                thinking_msg,
            )
            .await
        {
            Ok(id) => id,
            Err(e) => {
                error!(error = %e, "failed to send thinking message");
                return;
            }
        };

        let mut text_buffer = String::new();
        let mut last_edit = Instant::now() - throttle;
        let mut has_content = false;

        loop {
            match rx.recv().await {
                Ok(event) => match ChannelMessageService::process_stream_event(&event) {
                    Some(StreamAction::AppendText(chunk)) => {
                        text_buffer.push_str(&chunk);
                        has_content = true;
                        if last_edit.elapsed() >= throttle {
                            let formatted = format_text_for_platform(&text_buffer, self.config.platform);
                            let msg = ChannelMessageService::build_streaming_message(&formatted);
                            let _ = self
                                .sender
                                .edit_message(
                                    &self.config.owner_user_id,
                                    &self.config.plugin_id,
                                    &self.config.chat_id,
                                    &thinking_msg_id,
                                    msg,
                                )
                                .await;
                            last_edit = Instant::now();
                        }
                    }
                    Some(StreamAction::Thinking(_)) => {}
                    Some(StreamAction::ToolCall { name, .. }) => {
                        let msg = ChannelMessageService::build_streaming_message(&format!("\u{23f3} {name}..."));
                        let _ = self
                            .sender
                            .edit_message(
                                &self.config.owner_user_id,
                                &self.config.plugin_id,
                                &self.config.chat_id,
                                &thinking_msg_id,
                                msg,
                            )
                            .await;
                    }
                    Some(StreamAction::Finish) => {
                        self.send_final_edit(&text_buffer, has_content, &thinking_msg_id).await;
                        info!(
                            plugin_id = %self.config.plugin_id,
                            chat_id = %self.config.chat_id,
                            text_len = text_buffer.len(),
                            "channel stream relay finished"
                        );
                        break;
                    }
                    Some(StreamAction::Error(msg)) => {
                        let error_msg = UnifiedOutgoingMessage {
                            message_type: OutgoingMessageType::Text,
                            text: Some(format!("\u{274c} {msg}")),
                            parse_mode: None,
                            buttons: None,
                            keyboard: None,
                            image_url: None,
                            file_url: None,
                            file_name: None,
                            media_actions: None,
                            reply_to_message_id: None,
                            silent: None,
                        };
                        let _ = self
                            .sender
                            .edit_message(
                                &self.config.owner_user_id,
                                &self.config.plugin_id,
                                &self.config.chat_id,
                                &thinking_msg_id,
                                error_msg,
                            )
                            .await;
                        break;
                    }
                    None => {}
                },
                Err(broadcast::error::RecvError::Closed) => {
                    warn!("channel stream relay: broadcast closed without terminal event");
                    self.send_final_edit(&text_buffer, has_content, &thinking_msg_id).await;
                    break;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "channel stream relay lagged");
                }
            }
        }

        debug!(
            plugin_id = %self.config.plugin_id,
            chat_id = %self.config.chat_id,
            "channel stream relay exited"
        );
    }

    async fn send_final_edit(&self, text_buffer: &str, has_content: bool, msg_id: &str) {
        if has_content {
            let formatted = format_text_for_platform(text_buffer, self.config.platform);
            let final_msg = ChannelMessageService::build_final_message(&formatted);
            let _ = self
                .sender
                .edit_message(
                    &self.config.owner_user_id,
                    &self.config.plugin_id,
                    &self.config.chat_id,
                    msg_id,
                    final_msg,
                )
                .await;
        }
    }
}

/// WeChat / WeCom channels cannot edit messages in place — each reply must be
/// a new send. The relay uses this predicate to flush pending assistant text
/// before rendering silent/non-text events (tool calls etc.) to avoid either
/// overwriting it with a tool-status indicator or deferring it until Finish.
fn is_weixin_platform(platform: PluginType) -> bool {
    matches!(platform, PluginType::Weixin)
}

// ── Test helpers (pub so integration tests can use them) ─────────

/// Records send/edit calls for test assertions.
pub struct MessageRecorder {
    sends: std::sync::Mutex<Vec<UnifiedOutgoingMessage>>,
    edits: std::sync::Mutex<Vec<UnifiedOutgoingMessage>>,
}

impl MessageRecorder {
    pub fn new() -> Self {
        Self {
            sends: std::sync::Mutex::new(Vec::new()),
            edits: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn take_sends(&self) -> Vec<UnifiedOutgoingMessage> {
        std::mem::take(&mut self.sends.lock().unwrap())
    }

    pub fn take_edits(&self) -> Vec<UnifiedOutgoingMessage> {
        std::mem::take(&mut self.edits.lock().unwrap())
    }
}

impl Default for MessageRecorder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelSender for MessageRecorder {
    async fn send_message(
        &self,
        _owner_user_id: &str,
        _plugin_id: &str,
        _chat_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<String, ChannelError> {
        self.sends.lock().unwrap().push(message);
        Ok("msg-1".into())
    }

    async fn edit_message(
        &self,
        _owner_user_id: &str,
        _plugin_id: &str,
        _chat_id: &str,
        _message_id: &str,
        message: UnifiedOutgoingMessage,
    ) -> Result<(), ChannelError> {
        self.edits.lock().unwrap().push(message);
        Ok(())
    }
}
