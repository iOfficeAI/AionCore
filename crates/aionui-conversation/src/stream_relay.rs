use std::sync::Arc;

use aionui_ai_agent::{
    AgentSendError, AgentStreamEvent,
    protocol::events::{
        ThinkingEventData,
        tool_call::{AcpToolCallSessionUpdateKind, AcpToolCallStatus, ToolCallStatus},
    },
};

use crate::response_middleware::{ICronService, MessageMiddleware, MiddlewareResult};
use aionui_api_types::{AgentErrorCode, ConversationRuntimeSummary, WebSocketMessage};
use aionui_common::{ErrorChain, normalize_keys_to_snake_case, now_ms};

use crate::runtime_state::ConversationRuntimeStateService;
use crate::service::ConversationService;
use aionui_db::models::MessageRow;
use aionui_db::{DbError, IConversationRepository};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, error, info, warn};

/// Number of text chunks to accumulate before flushing to the database.
const FLUSH_INTERVAL: u32 = 20;

fn is_not_found(err: &DbError) -> bool {
    matches!(err, DbError::NotFound(_))
}

fn is_foreign_key_constraint(err: &DbError) -> bool {
    err.to_string().contains("FOREIGN KEY constraint failed")
}

fn is_deleted_during_stream_persistence(err: &DbError) -> bool {
    is_not_found(err) || is_foreign_key_constraint(err)
}

fn log_persist_error(err: &DbError, message: &'static str) {
    if is_deleted_during_stream_persistence(err) {
        debug!(error = %ErrorChain(err), "{message}; conversation was likely deleted during stream finalization");
    } else {
        error!(error = %ErrorChain(err), "{message}");
    }
}

#[derive(Debug, Clone)]
struct TextSegmentState {
    id: String,
    buffer: String,
    created_at: i64,
    record_created: bool,
    flush_counter: u32,
}

#[derive(Debug, Clone)]
struct PersistedTextSegment {
    id: String,
}

#[derive(Debug, Clone)]
struct ThinkingSegmentState {
    id: String,
    buffer: String,
    started_at: i64,
}

/// Result returned after a relay turn has fully drained and finalized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayOutcome {
    pub system_responses: Vec<String>,
    pub terminal: RelayTerminal,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RelayTerminal {
    #[default]
    Finish,
    Error {
        code: Option<AgentErrorCode>,
        retryable: Option<bool>,
    },
    ChannelClosed,
}

impl RelayTerminal {
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }

    pub fn code(&self) -> Option<AgentErrorCode> {
        match self {
            Self::Error { code, .. } => *code,
            Self::Finish | Self::ChannelClosed => None,
        }
    }

    pub fn retryable(&self) -> Option<bool> {
        match self {
            Self::Error { retryable, .. } => *retryable,
            Self::Finish | Self::ChannelClosed => None,
        }
    }
}

/// Relays agent stream events to WebSocket and persists messages.
///
/// This struct is created for each `send_message` call and runs as a
/// background tokio task until the agent finishes or errors out.
pub struct StreamRelay {
    conversation_id: String,
    msg_id: String,
    user_id: String,
    repo: Arc<dyn IConversationRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    cron_service: Option<Arc<dyn ICronService>>,
    runtime_state: Option<Arc<ConversationRuntimeStateService>>,
    complete_turn: bool,
}

impl StreamRelay {
    pub fn new(
        conversation_id: String,
        msg_id: String,
        user_id: String,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        cron_service: Option<Arc<dyn ICronService>>,
    ) -> Self {
        Self {
            conversation_id,
            msg_id,
            user_id,
            repo,
            broadcaster,
            cron_service,
            runtime_state: None,
            complete_turn: true,
        }
    }

    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = Some(runtime_state);
        self
    }

    pub fn with_turn_completion(mut self, enabled: bool) -> Self {
        self.complete_turn = enabled;
        self
    }

    /// Run the relay loop. Consumes `self` and runs until the agent stream ends.
    #[tracing::instrument(
        skip_all,
        fields(
            conversation_id = %self.conversation_id,
            msg_id = %self.msg_id,
        )
    )]
    pub async fn consume(self, rx: broadcast::Receiver<AgentStreamEvent>) -> RelayOutcome {
        self.consume_inner(rx, None).await
    }

    /// Run the relay loop while also accepting a typed send failure from the
    /// task that called `IAgentTask::send_message`.
    #[tracing::instrument(
        skip_all,
        fields(
            conversation_id = %self.conversation_id,
            msg_id = %self.msg_id,
        )
    )]
    pub async fn consume_with_send_error(
        self,
        rx: broadcast::Receiver<AgentStreamEvent>,
        send_error_rx: oneshot::Receiver<AgentSendError>,
    ) -> RelayOutcome {
        self.consume_inner(rx, Some(send_error_rx)).await
    }

    async fn consume_inner(
        self,
        mut rx: broadcast::Receiver<AgentStreamEvent>,
        mut send_error_rx: Option<oneshot::Receiver<AgentSendError>>,
    ) -> RelayOutcome {
        let started_at = now_ms();
        info!("StreamRelay started");

        let mut full_text_buffer = String::new();
        let mut text_segments: Vec<PersistedTextSegment> = Vec::new();
        let mut active_text: Option<TextSegmentState> = None;
        let mut active_thinking: Option<ThinkingSegmentState> = None;
        let mut used_primary_segment_msg_id = false;
        let mut first_agent_event_logged = false;
        let mut first_visible_output_logged = false;
        let mut send_error_done = send_error_rx.is_none();

        loop {
            let recv_result = if send_error_done {
                rx.recv().await
            } else {
                tokio::select! {
                    recv = rx.recv() => recv,
                    send_error = send_error_rx.as_mut().expect("send_error_rx exists while pending") => {
                        send_error_done = true;
                        match send_error {
                            Ok(send_error) => {
                                warn!(
                                    code = ?send_error.code(),
                                    ownership = ?send_error.ownership(),
                                    "Injecting stream error for failed agent send"
                                );
                                Ok(AgentStreamEvent::Error(send_error.into_stream_error()))
                            }
                            Err(_) => continue,
                        }
                    }
                }
            };

            match recv_result {
                Ok(event) => {
                    let deleting = self.is_deleting();
                    if deleting && !matches!(event, AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_)) {
                        debug!(
                            event_type = Self::event_kind(&event),
                            "Skipping non-terminal stream event because conversation is deleting"
                        );
                        continue;
                    }

                    if !first_agent_event_logged {
                        first_agent_event_logged = true;
                        info!(
                            event_type = Self::event_kind(&event),
                            elapsed_ms = now_ms().saturating_sub(started_at),
                            "StreamRelay received first agent event"
                        );
                    }

                    match &event {
                        AgentStreamEvent::Thinking(data) => {
                            if data.status.as_deref() == Some("done") {
                                self.complete_active_thinking(&mut active_thinking).await;
                                continue;
                            }

                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            if !first_visible_output_logged && !data.content.is_empty() {
                                first_visible_output_logged = true;
                                info!(
                                    event_type = "Thinking",
                                    elapsed_ms = now_ms().saturating_sub(started_at),
                                    "StreamRelay received first visible output"
                                );
                            }

                            let segment = active_thinking.get_or_insert_with(|| ThinkingSegmentState {
                                id: Self::mint_segment_msg_id(&mut used_primary_segment_msg_id, &self.msg_id),
                                buffer: String::new(),
                                started_at: now_ms(),
                            });
                            segment.buffer.push_str(&data.content);
                            self.forward_to_websocket_with_msg_id(&segment.id, &event);
                        }
                        AgentStreamEvent::Text(data) => {
                            self.complete_active_thinking(&mut active_thinking).await;
                            if !first_visible_output_logged && !data.content.is_empty() {
                                first_visible_output_logged = true;
                                info!(
                                    event_type = "Text",
                                    elapsed_ms = now_ms().saturating_sub(started_at),
                                    "StreamRelay received first visible output"
                                );
                            }

                            let segment = active_text.get_or_insert_with(|| TextSegmentState {
                                id: Self::mint_segment_msg_id(&mut used_primary_segment_msg_id, &self.msg_id),
                                buffer: String::new(),
                                created_at: now_ms(),
                                record_created: false,
                                flush_counter: 0,
                            });
                            self.forward_to_websocket_with_msg_id(&segment.id, &event);
                            segment.buffer.push_str(&data.content);
                            full_text_buffer.push_str(&data.content);
                            segment.flush_counter += 1;
                            if segment.flush_counter >= FLUSH_INTERVAL {
                                self.flush_text_segment(segment).await;
                                segment.flush_counter = 0;
                            }
                        }
                        AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_) => {
                            let elapsed_ms = now_ms() - started_at;
                            let event_type = if matches!(event, AgentStreamEvent::Finish(_)) {
                                "Finish"
                            } else {
                                "Error"
                            };
                            let terminal = Self::terminal_from_event(&event);
                            match &terminal {
                                RelayTerminal::Error { code, retryable } => {
                                    info!(
                                        event_type,
                                        elapsed_ms,
                                        text_len = full_text_buffer.len(),
                                        error_code = ?code,
                                        retryable = ?retryable,
                                        "StreamRelay received terminal event"
                                    );
                                }
                                RelayTerminal::Finish | RelayTerminal::ChannelClosed => {
                                    info!(
                                        event_type,
                                        elapsed_ms,
                                        text_len = full_text_buffer.len(),
                                        "StreamRelay received terminal event"
                                    );
                                }
                            }

                            if deleting {
                                debug!("Skipping terminal DB finalization because conversation is deleting");
                            } else {
                                self.complete_active_thinking(&mut active_thinking).await;
                                self.close_active_text_segment(
                                    &mut active_text,
                                    &mut text_segments,
                                    if matches!(event, AgentStreamEvent::Error(_)) {
                                        "error"
                                    } else {
                                        "finish"
                                    },
                                )
                                .await;
                            }
                            self.forward_to_websocket(&event);
                            let outcome = if deleting {
                                RelayOutcome {
                                    system_responses: Vec::new(),
                                    terminal,
                                }
                            } else {
                                self.finalize(&full_text_buffer, &text_segments, &event, terminal).await
                            };
                            if self.complete_turn && !deleting {
                                Self::complete_conversation(&self.repo, &self.broadcaster, &self.conversation_id).await;
                            }
                            break outcome;
                        }
                        AgentStreamEvent::ToolCall(data) => {
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.persist_tool_call(data).await;
                        }
                        AgentStreamEvent::AcpToolCall(data) => {
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.persist_acp_tool_call(data).await;
                        }
                        AgentStreamEvent::ToolGroup(entries) => {
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.persist_tool_group(entries).await;
                        }
                        _ => {
                            self.forward_to_websocket(&event);
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => {
                    let elapsed_ms = now_ms() - started_at;
                    warn!(
                        elapsed_ms,
                        text_len = full_text_buffer.len(),
                        "StreamRelay channel closed without terminal event"
                    );

                    let deleting = self.is_deleting();
                    if deleting {
                        debug!("Skipping channel-closed DB finalization because conversation is deleting");
                    } else {
                        self.complete_active_thinking(&mut active_thinking).await;
                        self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                            .await;
                    }
                    // Channel closed without finish/error — still finalize
                    let outcome = if deleting {
                        RelayOutcome {
                            system_responses: Vec::new(),
                            terminal: RelayTerminal::ChannelClosed,
                        }
                    } else {
                        self.finalize(
                            &full_text_buffer,
                            &text_segments,
                            &AgentStreamEvent::Finish(aionui_ai_agent::protocol::events::FinishEventData::default()),
                            RelayTerminal::ChannelClosed,
                        )
                        .await
                    };
                    if self.complete_turn && !deleting {
                        Self::complete_conversation(&self.repo, &self.broadcaster, &self.conversation_id).await;
                    }
                    break outcome;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!(lagged = n, "Stream relay lagged, some events dropped");
                }
            }
        }
    }

    fn is_deleting(&self) -> bool {
        self.runtime_state
            .as_ref()
            .is_some_and(|state| state.is_deleting(&self.conversation_id))
    }

    fn event_kind(event: &AgentStreamEvent) -> &'static str {
        match event {
            AgentStreamEvent::Start(_) => "Start",
            AgentStreamEvent::Text(_) => "Text",
            AgentStreamEvent::Tips(_) => "Tips",
            AgentStreamEvent::Thinking(_) => "Thinking",
            AgentStreamEvent::ToolCall(_) => "ToolCall",
            AgentStreamEvent::AcpToolCall(_) => "AcpToolCall",
            AgentStreamEvent::ToolGroup(_) => "ToolGroup",
            AgentStreamEvent::AgentStatus(_) => "AgentStatus",
            AgentStreamEvent::Plan(_) => "Plan",
            AgentStreamEvent::Permission(_) => "Permission",
            AgentStreamEvent::AcpPermission(_) => "AcpPermission",
            AgentStreamEvent::SkillSuggest(_) => "SkillSuggest",
            AgentStreamEvent::CronTrigger(_) => "CronTrigger",
            AgentStreamEvent::AcpModelInfo(_) => "AcpModelInfo",
            AgentStreamEvent::AcpModeInfo(_) => "AcpModeInfo",
            AgentStreamEvent::AcpConfigOption(_) => "AcpConfigOption",
            AgentStreamEvent::AcpSessionInfo(_) => "AcpSessionInfo",
            AgentStreamEvent::AcpContextUsage(_) => "AcpContextUsage",
            AgentStreamEvent::AcpPromptHookWarning(_) => "AcpPromptHookWarning",
            AgentStreamEvent::SlashCommandsUpdated(_) => "SlashCommandsUpdated",
            AgentStreamEvent::AvailableCommands(_) => "AvailableCommands",
            AgentStreamEvent::Finish(_) => "Finish",
            AgentStreamEvent::Error(_) => "Error",
            AgentStreamEvent::System(_) => "System",
            AgentStreamEvent::RequestTrace(_) => "RequestTrace",
            AgentStreamEvent::SessionAssigned(_) => "SessionAssigned",
        }
    }

    fn terminal_from_event(event: &AgentStreamEvent) -> RelayTerminal {
        match event {
            AgentStreamEvent::Error(data) => RelayTerminal::Error {
                code: data.code,
                retryable: data.retryable,
            },
            AgentStreamEvent::Finish(_) => RelayTerminal::Finish,
            _ => RelayTerminal::ChannelClosed,
        }
    }

    fn mint_segment_msg_id(used_primary: &mut bool, primary_msg_id: &str) -> String {
        if !*used_primary {
            *used_primary = true;
            primary_msg_id.to_owned()
        } else {
            ConversationService::mint_msg_id()
        }
    }

    /// Forward an agent event to connected WebSocket clients.
    #[tracing::instrument(skip_all)]
    fn forward_to_websocket(&self, event: &AgentStreamEvent) {
        self.forward_to_websocket_with_msg_id(&self.msg_id, event);
    }

    #[tracing::instrument(skip_all)]
    fn forward_to_websocket_with_msg_id(&self, msg_id: &str, event: &AgentStreamEvent) {
        let mut event_data = match serde_json::to_value(event) {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %ErrorChain(&e), "Failed to serialize agent event for WebSocket");
                return;
            }
        };
        // Nested ACP SDK payloads serialise as camelCase on their own;
        // force every object key down the tree to snake_case so the
        // wire contract stays uniform.
        normalize_keys_to_snake_case(&mut event_data);

        let payload = json!({
            "conversation_id": self.conversation_id,
            "msg_id": msg_id,
            "type": event_data.get("type").cloned().unwrap_or(json!("unknown")),
            "data": event_data.get("data").cloned().unwrap_or(json!({})),
            "hidden": false,
        });

        self.broadcast_stream_payload(payload);
    }

    /// Flush an active text segment to the database (create or update).
    #[tracing::instrument(skip_all)]
    async fn flush_text_segment(&self, segment: &mut TextSegmentState) {
        if segment.buffer.is_empty() {
            return;
        }

        let content = json!({ "content": segment.buffer }).to_string();

        if segment.record_created {
            let update = aionui_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some("work".into())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&segment.id, &update).await {
                log_persist_error(&e, "Failed to update streaming text segment");
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some("work".into()),
                hidden: false,
                created_at: segment.created_at,
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                log_persist_error(&e, "Failed to create streaming text segment");
            }
            segment.record_created = true;
        }
    }

    #[tracing::instrument(skip_all)]
    async fn finalize_text_segment(&self, segment: TextSegmentState, status: &str) -> Option<PersistedTextSegment> {
        if segment.buffer.is_empty() {
            return None;
        }

        let content = json!({ "content": segment.buffer }).to_string();
        if segment.record_created {
            let update = aionui_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: Some(false),
            };
            if let Err(e) = self.repo.update_message(&segment.id, &update).await {
                log_persist_error(&e, "Failed to finalize text segment");
                return None;
            }
        } else {
            let row = MessageRow {
                id: segment.id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(segment.id.clone()),
                r#type: "text".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: segment.created_at,
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                log_persist_error(&e, "Failed to create finalized text segment");
                return None;
            }
        }

        Some(PersistedTextSegment { id: segment.id })
    }

    /// Finalize assistant text on stream end and apply middleware rewrites.
    #[tracing::instrument(skip_all)]
    async fn finalize(
        &self,
        text: &str,
        text_segments: &[PersistedTextSegment],
        event: &AgentStreamEvent,
        terminal: RelayTerminal,
    ) -> RelayOutcome {
        let mut outcome = RelayOutcome {
            system_responses: Vec::new(),
            terminal,
        };
        let status = match event {
            AgentStreamEvent::Error(_) => "error",
            _ => "finish",
        };

        if !text.is_empty() {
            let processed = self.process_final_text(text).await;
            let final_text = processed.message.trim().to_owned();
            let hidden = final_text.is_empty();

            if let Some(primary_segment) = text_segments.first() {
                if processed.message != text || hidden {
                    let content = json!({ "content": final_text }).to_string();
                    let update = aionui_db::MessageRowUpdate {
                        content: Some(content),
                        status: Some(Some(status.to_owned())),
                        hidden: Some(hidden),
                    };
                    if let Err(e) = self.repo.update_message(&primary_segment.id, &update).await {
                        log_persist_error(&e, "Failed to rewrite finalized text segment");
                    }
                    self.send_final_text_override(&primary_segment.id, &processed.message, hidden);

                    for segment in text_segments.iter().skip(1) {
                        let hide_update = aionui_db::MessageRowUpdate {
                            content: None,
                            status: Some(Some(status.to_owned())),
                            hidden: Some(true),
                        };
                        if let Err(e) = self.repo.update_message(&segment.id, &hide_update).await {
                            log_persist_error(&e, "Failed to hide superseded text segment");
                        }
                        self.send_final_text_override(&segment.id, "", true);
                    }
                } else {
                    for segment in text_segments {
                        let status_update = aionui_db::MessageRowUpdate {
                            content: None,
                            status: Some(Some(status.to_owned())),
                            hidden: Some(false),
                        };
                        if let Err(e) = self.repo.update_message(&segment.id, &status_update).await {
                            log_persist_error(&e, "Failed to finalize text segment status");
                        }
                    }
                }
            } else if !hidden {
                let row = MessageRow {
                    id: self.msg_id.clone(),
                    conversation_id: self.conversation_id.clone(),
                    msg_id: Some(self.msg_id.clone()),
                    r#type: "text".into(),
                    content: json!({ "content": final_text }).to_string(),
                    position: Some("left".into()),
                    status: Some(status.to_owned()),
                    hidden: false,
                    created_at: now_ms(),
                };
                if let Err(e) = self.repo.insert_message(&row).await {
                    log_persist_error(&e, "Failed to create final fallback message");
                }
            }

            self.send_system_responses(&processed.system_responses);
            outcome.system_responses = processed.system_responses;
        } else if let AgentStreamEvent::Error(data) = event {
            // No text accumulated but got an error — store error as tips message
            let content = json!({ "content": &data.message, "type": "error", "error": &data }).to_string();
            let row = MessageRow {
                id: ConversationService::mint_msg_id(),
                conversation_id: self.conversation_id.clone(),
                msg_id: None,
                r#type: "tips".into(),
                content,
                position: Some("left".into()),
                status: Some("error".into()),
                hidden: false,
                created_at: now_ms(),
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                log_persist_error(&e, "Failed to store error message");
            }
        }

        outcome
    }

    #[tracing::instrument(skip_all)]
    async fn complete_active_thinking(&self, active_thinking: &mut Option<ThinkingSegmentState>) {
        let Some(segment) = active_thinking.take() else {
            return;
        };
        let duration_ms = (now_ms() - segment.started_at).max(0);
        self.send_thinking_done(&segment.id, duration_ms as u64);
        if segment.buffer.is_empty() {
            return;
        }
        let content = json!({
            "content": segment.buffer,
            "status": "done",
            "duration_ms": duration_ms,
        })
        .to_string();
        let row = MessageRow {
            id: segment.id.clone(),
            conversation_id: self.conversation_id.clone(),
            msg_id: Some(segment.id),
            r#type: "thinking".into(),
            content,
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: false,
            created_at: segment.started_at,
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            log_persist_error(&e, "Failed to persist thinking message");
        }
    }

    #[tracing::instrument(skip_all)]
    async fn close_active_text_segment(
        &self,
        active_text: &mut Option<TextSegmentState>,
        text_segments: &mut Vec<PersistedTextSegment>,
        status: &str,
    ) {
        let Some(text_segment) = active_text.take() else {
            return;
        };
        if let Some(segment) = self.finalize_text_segment(text_segment, status).await {
            text_segments.push(segment);
        }
    }

    /// Persist a Gemini-style tool_call event.
    #[tracing::instrument(skip_all)]
    async fn persist_tool_call(&self, data: &aionui_ai_agent::protocol::events::tool_call::ToolCallEventData) {
        if data.call_id.trim().is_empty() {
            warn!(
                tool = %data.name,
                status = ?data.status,
                "Skipping tool_call persistence because call_id is empty"
            );
            return;
        }

        let status = match data.status {
            ToolCallStatus::Running => "work",
            ToolCallStatus::Completed => "finish",
            ToolCallStatus::Error => "error",
        };
        let content = serde_json::to_string(data).unwrap_or_default();

        let existing = self
            .repo
            .get_message_by_msg_id(&self.conversation_id, &data.call_id, "tool_call")
            .await
            .unwrap_or(None);

        if let Some(existing_row) = existing {
            let merged_content = Self::merge_json_content(&existing_row.content, &content);
            let update = aionui_db::MessageRowUpdate {
                content: Some(merged_content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&data.call_id, &update).await {
                error!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to update tool_call message"
                );
            } else {
                debug!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    "Updated tool_call message"
                );
            }
        } else {
            let row = MessageRow {
                id: data.call_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(data.call_id.clone()),
                r#type: "tool_call".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                error!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    error = %ErrorChain(&e),
                    "Failed to persist tool_call message"
                );
            } else {
                debug!(
                    call_id = %data.call_id,
                    tool = %data.name,
                    status,
                    "Persisted tool_call message"
                );
            }
        }
    }

    /// Persist an ACP (Claude CLI) tool call event.
    /// First event (ToolCall) inserts; subsequent events (ToolCallUpdate) update.
    #[tracing::instrument(skip_all)]
    async fn persist_acp_tool_call(&self, data: &aionui_ai_agent::protocol::events::tool_call::AcpToolCallEventData) {
        let tool_call_id = &data.update.tool_call_id;
        let status = match data.update.status {
            Some(AcpToolCallStatus::Pending) | None => "work",
            Some(AcpToolCallStatus::InProgress) => "work",
            Some(AcpToolCallStatus::Completed) => "finish",
            Some(AcpToolCallStatus::Failed) => "error",
        };

        let mut value = serde_json::to_value(data).unwrap_or_default();
        normalize_keys_to_snake_case(&mut value);
        let content = value.to_string();

        match data.update.session_update {
            AcpToolCallSessionUpdateKind::ToolCall => {
                let row = MessageRow {
                    id: tool_call_id.clone(),
                    conversation_id: self.conversation_id.clone(),
                    msg_id: Some(tool_call_id.clone()),
                    r#type: "acp_tool_call".into(),
                    content,
                    position: Some("left".into()),
                    status: Some(status.to_owned()),
                    hidden: false,
                    created_at: now_ms(),
                };
                if let Err(e) = self.repo.insert_message(&row).await {
                    error!(error = %ErrorChain(&e), "Failed to persist acp_tool_call message");
                }
            }
            AcpToolCallSessionUpdateKind::ToolCallUpdate => {
                let merged_content = self.merge_acp_tool_call_content(tool_call_id, &value).await;
                let update = aionui_db::MessageRowUpdate {
                    content: Some(merged_content),
                    status: Some(Some(status.to_owned())),
                    hidden: None,
                };
                if let Err(e) = self.repo.update_message(tool_call_id, &update).await {
                    error!(error = %ErrorChain(&e), "Failed to update acp_tool_call message");
                }
            }
        }
    }

    /// Merge two JSON content strings: overlays non-null fields from `new_json`
    /// onto `existing_json`, preserving fields only present in the original.
    fn merge_json_content(existing_json: &str, new_json: &str) -> String {
        let mut base: serde_json::Value = serde_json::from_str(existing_json).unwrap_or_default();
        let new_value: serde_json::Value = serde_json::from_str(new_json).unwrap_or_default();
        if let (Some(base_obj), Some(new_obj)) = (base.as_object_mut(), new_value.as_object()) {
            for (key, val) in new_obj {
                if !val.is_null() {
                    base_obj.insert(key.clone(), val.clone());
                }
            }
        }
        base.to_string()
    }

    /// Merge an AcpToolCall update into the existing DB record.
    /// Reads the stored content, overlays non-null fields from the update,
    /// preserving fields like `raw_input` that the update event omits.
    async fn merge_acp_tool_call_content(&self, tool_call_id: &str, update_value: &serde_json::Value) -> String {
        let existing = self
            .repo
            .get_message_by_msg_id(&self.conversation_id, tool_call_id, "acp_tool_call")
            .await
            .ok()
            .flatten();

        let Some(existing_row) = existing else {
            return update_value.to_string();
        };

        let mut base: serde_json::Value = serde_json::from_str(&existing_row.content).unwrap_or_default();
        if let (Some(base_update), Some(new_update)) = (
            base.get_mut("update").and_then(|v| v.as_object_mut()),
            update_value.get("update").and_then(|v| v.as_object()),
        ) {
            for (key, val) in new_update {
                if !val.is_null() {
                    base_update.insert(key.clone(), val.clone());
                }
            }
        }
        base.to_string()
    }

    /// Persist a tool_group event (array of tool summaries).
    #[tracing::instrument(skip_all)]
    async fn persist_tool_group(&self, entries: &[aionui_ai_agent::protocol::events::tool_call::ToolGroupEntry]) {
        let all_done = entries
            .iter()
            .all(|e| matches!(e.status, ToolCallStatus::Completed | ToolCallStatus::Error));
        let status = if all_done { "finish" } else { "work" };
        let content = serde_json::to_string(entries).unwrap_or_default();

        let group_id = entries
            .first()
            .map(|e| e.call_id.clone())
            .unwrap_or_else(ConversationService::mint_msg_id);

        let existing = self
            .repo
            .get_message_by_msg_id(&self.conversation_id, &group_id, "tool_group")
            .await
            .unwrap_or(None);

        if existing.is_some() {
            let update = aionui_db::MessageRowUpdate {
                content: Some(content),
                status: Some(Some(status.to_owned())),
                hidden: None,
            };
            if let Err(e) = self.repo.update_message(&group_id, &update).await {
                error!(error = %ErrorChain(&e), "Failed to update tool_group message");
            }
        } else {
            let row = MessageRow {
                id: group_id.clone(),
                conversation_id: self.conversation_id.clone(),
                msg_id: Some(group_id),
                r#type: "tool_group".into(),
                content,
                position: Some("left".into()),
                status: Some(status.to_owned()),
                hidden: false,
                created_at: now_ms(),
            };
            if let Err(e) = self.repo.insert_message(&row).await {
                error!(error = %ErrorChain(&e), "Failed to persist tool_group message");
            }
        }
    }

    /// Send a `thinking` event with `status: "done"` to close the thinking UI.
    fn send_thinking_done(&self, msg_id: &str, duration: u64) {
        let thinking_done = AgentStreamEvent::Thinking(ThinkingEventData {
            content: String::new(),
            subject: None,
            duration: Some(duration),
            status: Some("done".into()),
        });
        self.forward_to_websocket_with_msg_id(msg_id, &thinking_done);
    }

    async fn process_final_text(&self, text: &str) -> MiddlewareResult {
        let middleware = MessageMiddleware::new(
            self.cron_service
                .as_ref()
                .map(|service| Box::new(SharedCronService(Arc::clone(service))) as Box<dyn ICronService>),
        );

        middleware.process(text, &self.user_id, &self.conversation_id).await
    }

    fn send_final_text_override(&self, msg_id: &str, text: &str, hidden: bool) {
        self.broadcast_stream_payload(json!({
            "conversation_id": self.conversation_id,
            "msg_id": msg_id,
            "type": "content",
            "data": { "content": text },
            "hidden": hidden,
            "replace": true,
        }));
    }

    fn send_system_responses(&self, responses: &[String]) {
        for response in responses {
            self.broadcast_stream_payload(json!({
                "conversation_id": self.conversation_id,
                "msg_id": ConversationService::mint_msg_id(),
                "type": "system",
                "data": response,
                "hidden": true,
            }));
        }
    }

    fn broadcast_stream_payload(&self, payload: serde_json::Value) {
        let msg = WebSocketMessage::new("message.stream", payload);
        self.broadcaster.broadcast(msg);
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn complete_conversation(
        repo: &Arc<dyn IConversationRepository>,
        broadcaster: &Arc<dyn EventBroadcaster>,
        conversation_id: &str,
    ) {
        Self::complete_conversation_with_runtime(repo, broadcaster, conversation_id, None).await;
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %conversation_id))]
    pub async fn complete_conversation_with_runtime(
        repo: &Arc<dyn IConversationRepository>,
        broadcaster: &Arc<dyn EventBroadcaster>,
        conversation_id: &str,
        runtime: Option<ConversationRuntimeSummary>,
    ) {
        let update = aionui_db::ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(e) = repo.update(conversation_id, &update).await {
            log_persist_error(&e, "Failed to update conversation status");
        }

        let payload = json!({
            "conversation_id": conversation_id,
            "session_id": conversation_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
        });
        let msg = WebSocketMessage::new("turn.completed", payload);
        broadcaster.broadcast(msg);

        debug!(conversation_id, status = "finished", "Turn completed");
    }
}

struct SharedCronService(Arc<dyn ICronService>);

#[async_trait::async_trait]
impl ICronService for SharedCronService {
    async fn create_job(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &crate::response_middleware::CronCreateParams,
    ) -> crate::response_middleware::CronCommandResult {
        self.0.create_job(user_id, conversation_id, params).await
    }

    async fn update_job(
        &self,
        user_id: &str,
        conversation_id: &str,
        params: &crate::response_middleware::CronUpdateParams,
    ) -> crate::response_middleware::CronCommandResult {
        self.0.update_job(user_id, conversation_id, params).await
    }

    async fn list_jobs(&self, user_id: &str, conversation_id: &str) -> crate::response_middleware::CronCommandResult {
        self.0.list_jobs(user_id, conversation_id).await
    }

    async fn delete_job(&self, user_id: &str, job_id: &str) -> crate::response_middleware::CronCommandResult {
        self.0.delete_job(user_id, job_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::protocol::events::{ErrorEventData, FinishEventData, TextEventData, ThinkingEventData};
    use aionui_db::DbError;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ── run() async tests ─────────────────────────────────────────

    #[tokio::test]
    async fn run_text_then_finish_persists_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // Send text events then finish
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Hello ".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "World".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(outcome.terminal, RelayTerminal::Finish);

        // Should have inserted a message with accumulated text
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let msg = &inserts[0];
        assert_eq!(msg.conversation_id, "conv-1");
        assert_eq!(msg.id, "asst-1");
        assert_eq!(msg.r#type, "text");
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "Hello World");
    }

    #[tokio::test]
    async fn run_text_tool_text_splits_text_segments() {
        use aionui_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Alpha".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "read_file".into(),
            args: json!({"path": "a.ts"}),
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData { content: "Beta".into() }))
            .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let text_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "text").collect();
        assert_eq!(text_msgs.len(), 2, "text should split across tool boundaries");
        assert_eq!(text_msgs[0].id, "asst-1");
        assert_ne!(text_msgs[0].id, text_msgs[1].id);

        let mut text_event_msg_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name == "message.stream" && (evt.data["type"] == "text" || evt.data["type"] == "content") {
                text_event_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(text_event_msg_ids.len(), 2);
        assert_eq!(text_event_msg_ids[0], "asst-1");
        assert_ne!(text_event_msg_ids[0], text_event_msg_ids[1]);
    }

    #[tokio::test]
    async fn run_error_with_no_text_stores_tips_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "Something went wrong",
            None,
        )))
        .unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: None,
                retryable: None
            }
        );

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let msg = &inserts[0];
        assert_eq!(msg.r#type, "tips");
        assert_eq!(msg.status.as_deref(), Some("error"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "Something went wrong");
        assert_eq!(content["type"], "error");
    }

    #[tokio::test]
    async fn run_send_error_injects_error_and_completes_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(AgentSendError::from_app_error(aionui_common::AppError::BadGateway(
                "provider returned 401 invalid api key".into(),
            )))
            .unwrap();

        let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: Some(aionui_api_types::AgentErrorCode::UserLlmProviderAuthFailed),
                retryable: Some(false)
            }
        );

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "tips");
        assert_eq!(inserts[0].status.as_deref(), Some("error"));
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "The model provider rejected the request");
        assert_eq!(content["type"], "error");
        assert_eq!(content["error"]["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
        assert_eq!(content["error"]["ownership"], "user_llm_provider");
        assert_eq!(content["error"]["retryable"], false);
        assert_eq!(content["error"]["feedback_recommended"], false);
        assert_eq!(content["error"]["detail"], "provider returned 401 invalid api key");
        assert_eq!(content["error"]["resolution"]["kind"], "check_provider_credentials");
        assert_eq!(content["error"]["resolution"]["target"], "provider_settings");

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }

        let error_event = ws_events
            .iter()
            .find(|evt| evt.name == "message.stream" && evt.data["type"] == "error")
            .expect("send error should be forwarded as message.stream error");
        assert_eq!(error_event.data["data"]["code"], "USER_LLM_PROVIDER_AUTH_FAILED");
        assert_eq!(error_event.data["data"]["ownership"], "user_llm_provider");
        assert!(ws_events.iter().any(|evt| evt.name == "turn.completed"));
    }

    #[tokio::test]
    async fn run_send_error_keeps_existing_stream_error_when_it_arrives_first() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let send_error = AgentSendError::from_app_error(aionui_common::AppError::BadGateway(
            "provider returned 401 invalid api key".into(),
        ));
        tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
            "stream already emitted",
            None,
        )))
        .unwrap();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        let delayed_send_error = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = send_error_tx.send(send_error);
        });

        let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
        delayed_send_error.await.unwrap();
        assert!(outcome.system_responses.is_empty());

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "tips");
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "stream already emitted");
        assert_eq!(content["type"], "error");
    }

    #[tokio::test]
    async fn run_send_error_uses_send_error_when_it_arrives_first() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(AgentSendError::from_app_error(aionui_common::AppError::BadGateway(
                "provider returned 401 invalid api key".into(),
            )))
            .unwrap();
        let delayed_stream_error = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = tx.send(AgentStreamEvent::Error(ErrorEventData::legacy(
                "stream already emitted",
                None,
            )));
        });

        let outcome = relay.consume_with_send_error(rx, send_error_rx).await;
        delayed_stream_error.await.unwrap();
        assert!(outcome.system_responses.is_empty());
        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: Some(aionui_api_types::AgentErrorCode::UserLlmProviderAuthFailed),
                retryable: Some(false)
            }
        );

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        assert_eq!(inserts[0].r#type, "tips");
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "The model provider rejected the request");
        assert_eq!(content["type"], "error");
        assert_eq!(content["error"]["resolution"]["kind"], "check_provider_credentials");
        assert_eq!(content["error"]["resolution"]["target"], "provider_settings");
    }

    #[tokio::test]
    async fn run_thinking_tool_thinking_splits_thinking_segments() {
        use aionui_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Plan A".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "read_file".into(),
            args: json!({"path": "a.ts"}),
            status: ToolCallStatus::Running,
            description: None,
            input: None,
            output: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Plan B".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let thinking_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "thinking").collect();
        assert_eq!(thinking_msgs.len(), 2, "thinking should split across tool boundaries");
        assert_eq!(thinking_msgs[0].msg_id.as_deref(), Some("asst-1"));
        assert_ne!(thinking_msgs[0].msg_id, thinking_msgs[1].msg_id);

        let mut done_msg_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name == "message.stream" && evt.data["type"] == "thinking" && evt.data["data"]["status"] == "done" {
                done_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }
        assert_eq!(done_msg_ids.len(), 2);
        assert_eq!(done_msg_ids[0], "asst-1");
        assert_ne!(done_msg_ids[0], done_msg_ids[1]);
    }

    #[tokio::test]
    async fn run_thinking_then_text_uses_distinct_segment_ids() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Thinking(ThinkingEventData {
            content: "Plan first".into(),
            subject: None,
            duration: None,
            status: Some("thinking".into()),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Final answer".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let thinking_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "thinking").collect();
        let text_msgs: Vec<_> = inserts.iter().filter(|msg| msg.r#type == "text").collect();

        assert_eq!(thinking_msgs.len(), 1);
        assert_eq!(text_msgs.len(), 1);
        assert_eq!(thinking_msgs[0].id, "asst-1");
        assert_ne!(thinking_msgs[0].id, text_msgs[0].id);

        let mut text_msg_ids = Vec::new();
        let mut thinking_done_ids = Vec::new();
        while let Ok(evt) = ws_rx.try_recv() {
            if evt.name != "message.stream" {
                continue;
            }
            if evt.data["type"] == "text" || evt.data["type"] == "content" {
                text_msg_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
            if evt.data["type"] == "thinking" && evt.data["data"]["status"] == "done" {
                thinking_done_ids.push(evt.data["msg_id"].as_str().unwrap_or_default().to_owned());
            }
        }

        assert_eq!(thinking_done_ids, vec!["asst-1".to_string()]);
        assert_eq!(text_msg_ids.len(), 1);
        assert_ne!(text_msg_ids[0], "asst-1");
    }

    #[tokio::test]
    async fn run_channel_closed_finalizes() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // Send text then drop sender (channel closes without Finish)
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial".into(),
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(outcome.terminal, RelayTerminal::ChannelClosed);

        // Should still persist the partial text
        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let content: serde_json::Value = serde_json::from_str(&inserts[0].content).unwrap();
        assert_eq!(content["content"], "partial");
    }

    #[tokio::test]
    async fn run_broadcasts_turn_completed() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        // Subscribe to the bus before relay runs
        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());

        // Collect WebSocket events
        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }

        // Should have turn.completed event
        let turn_event = ws_events.iter().find(|e| e.name == "turn.completed");
        assert!(turn_event.is_some());
        let data = &turn_event.unwrap().data;
        assert_eq!(data["conversation_id"], "conv-1");
        assert_eq!(data["session_id"], "conv-1");
        assert_eq!(data["status"], "finished");
        assert_eq!(data["canSendMessage"], true);
    }

    #[tokio::test]
    async fn run_finalizes_with_cleaned_replacement_event() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            Some(Arc::new(MockCronService)),
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "Hello [CRON_LIST]".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert_eq!(outcome.system_responses, vec!["[System: listed]".to_string()]);

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let updates = repo.take_updates();
        let final_update = updates
            .iter()
            .find(|(id, update)| id == "asst-1" && update.content.is_some())
            .expect("expected cleaned final text update");
        let content: serde_json::Value = serde_json::from_str(final_update.1.content.as_deref().unwrap()).unwrap();
        assert_eq!(content["content"].as_str().map(str::trim), Some("Hello"));

        let mut ws_events = vec![];
        while let Ok(evt) = ws_rx.try_recv() {
            ws_events.push(evt);
        }

        let replacement = ws_events
            .iter()
            .find(|evt| evt.name == "message.stream" && evt.data["type"] == "content" && evt.data["replace"] == true);
        assert!(replacement.is_some());
        assert_eq!(
            replacement.unwrap().data["data"]["content"].as_str().map(str::trim),
            Some("Hello")
        );
    }

    // ── Tool persistence tests ────────────────────────────────────

    #[tokio::test]
    async fn run_tool_call_persists_message() {
        use aionui_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        // First event: Running with input but no output
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "a cat"}),
            status: ToolCallStatus::Running,
            input: Some(json!({"prompt": "a cat", "size": "1024x1024"})),
            output: None,
            description: Some("Generate image".into()),
        }))
        .unwrap();
        // Second event: Completed with output but no input
        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "tc-001".into(),
            name: "image_gen".into(),
            args: json!({"prompt": "a cat"}),
            status: ToolCallStatus::Completed,
            input: None,
            output: Some("image.png".into()),
            description: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let tool_msg = inserts.iter().find(|m| m.r#type == "tool_call");
        assert!(tool_msg.is_some());
        let msg = tool_msg.unwrap();
        assert_eq!(msg.id, "tc-001");
        assert_eq!(msg.status.as_deref(), Some("work"));

        let updates = repo.take_updates();
        let tool_update = updates.iter().find(|(id, _)| id == "tc-001");
        assert!(tool_update.is_some());
        let (_, upd) = tool_update.unwrap();
        assert_eq!(upd.status, Some(Some("finish".to_owned())));

        // Verify merge: input from first event preserved, output from second event added
        let merged: serde_json::Value = serde_json::from_str(upd.content.as_deref().unwrap()).unwrap();
        assert_eq!(merged["name"], "image_gen");
        assert_eq!(merged["status"], "completed");
        assert!(
            merged.get("input").is_some() && !merged["input"].is_null(),
            "input must be preserved after merge"
        );
        assert_eq!(merged["input"]["prompt"], "a cat");
        assert_eq!(merged["output"], "image.png");
        assert_eq!(merged["description"], "Generate image");
    }

    #[tokio::test]
    async fn run_acp_tool_call_inserts_then_updates() {
        use aionui_ai_agent::protocol::events::tool_call::{
            AcpToolCallEventData, AcpToolCallSessionUpdateKind, AcpToolCallStatus, AcpToolCallUpdateData,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "sess-1".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCall,
                tool_call_id: "atc-001".into(),
                status: Some(AcpToolCallStatus::InProgress),
                title: Some("Bash".into()),
                kind: None,
                raw_input: Some(json!({"command": "mv /tmp/a /tmp/b", "description": "Move file"})),
                raw_output: None,
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();

        tx.send(AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
            session_id: "sess-1".into(),
            update: AcpToolCallUpdateData {
                session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                tool_call_id: "atc-001".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: None,
                kind: None,
                raw_input: None,
                raw_output: Some(json!("Exit code: 0\nSTDOUT:\nSTDERR:")),
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();

        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let acp_msg = inserts.iter().find(|m| m.r#type == "acp_tool_call");
        assert!(acp_msg.is_some());
        let msg = acp_msg.unwrap();
        assert_eq!(msg.id, "atc-001");
        assert_eq!(msg.status.as_deref(), Some("work"));

        let updates = repo.take_updates();
        let acp_update = updates.iter().find(|(id, _)| id == "atc-001");
        assert!(acp_update.is_some());
        let (_, upd) = acp_update.unwrap();
        assert_eq!(upd.status, Some(Some("finish".to_owned())));

        // Verify merge: raw_input from ToolCall is preserved, raw_output from ToolCallUpdate is added
        let merged: serde_json::Value = serde_json::from_str(upd.content.as_deref().unwrap()).unwrap();
        let update_obj = merged.get("update").unwrap();
        assert!(
            update_obj.get("raw_input").is_some(),
            "raw_input must be preserved after merge"
        );
        assert_eq!(
            update_obj
                .get("raw_input")
                .unwrap()
                .get("command")
                .unwrap()
                .as_str()
                .unwrap(),
            "mv /tmp/a /tmp/b"
        );
        assert!(
            update_obj.get("raw_output").is_some(),
            "raw_output must be present after merge"
        );
    }

    #[tokio::test]
    async fn run_tool_group_persists_message() {
        use aionui_ai_agent::protocol::events::tool_call::{ToolCallStatus, ToolGroupEntry};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::ToolGroup(vec![
            ToolGroupEntry {
                call_id: "tg-001".into(),
                name: "search".into(),
                status: ToolCallStatus::Completed,
                description: Some("Web search".into()),
            },
            ToolGroupEntry {
                call_id: "tg-002".into(),
                name: "read_file".into(),
                status: ToolCallStatus::Completed,
                description: None,
            },
        ]))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let inserts = repo.take_inserts();
        let group_msg = inserts.iter().find(|m| m.r#type == "tool_group");
        assert!(group_msg.is_some());
        let msg = group_msg.unwrap();
        assert_eq!(msg.id, "tg-001");
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert!(content.is_array());
        assert_eq!(content.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn close_active_text_segment_treats_not_found_as_deleted_conversation() {
        let repo = Arc::new(RecordingRepo::new());
        repo.set_not_found(true);
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));

        let relay = StreamRelay::new(
            "deleted-conv".into(),
            "assistant-msg".into(),
            "user-1".into(),
            repo,
            bus,
            None,
        );
        let mut active_text = Some(TextSegmentState {
            id: "missing-segment".into(),
            buffer: "partial answer".into(),
            created_at: now_ms(),
            record_created: true,
            flush_counter: 0,
        });
        let mut text_segments = Vec::new();

        relay
            .close_active_text_segment(&mut active_text, &mut text_segments, "finish")
            .await;

        assert!(active_text.is_none());
        assert!(
            text_segments.is_empty(),
            "missing DB rows should not be treated as persisted text segments"
        );
    }

    #[tokio::test]
    async fn complete_conversation_ignores_not_found_after_delete() {
        let repo = Arc::new(RecordingRepo::new());
        repo.set_not_found(true);
        let repo: Arc<dyn IConversationRepository> = repo;
        let bus: Arc<dyn EventBroadcaster> = Arc::new(aionui_realtime::BroadcastEventBus::new(64));

        StreamRelay::complete_conversation_with_runtime(&repo, &bus, "deleted-conv", None).await;
    }

    #[tokio::test]
    async fn run_skips_db_finalization_when_conversation_is_deleting() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let runtime_state = Arc::new(ConversationRuntimeStateService::default());
        runtime_state.mark_deleting("deleted-conv");
        let (tx, _) = broadcast::channel(16);

        let relay = StreamRelay::new(
            "deleted-conv".into(),
            "assistant-msg".into(),
            "user-1".into(),
            repo.clone(),
            bus,
            None,
        )
        .with_runtime_state(runtime_state);
        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial answer".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;

        assert_eq!(outcome.terminal, RelayTerminal::Finish);
        assert!(repo.take_inserts().is_empty());
        assert!(repo.take_updates().is_empty());
    }

    #[tokio::test]
    async fn finalize_treats_foreign_key_failure_as_deleted_conversation() {
        let repo = Arc::new(RecordingRepo::new());
        repo.set_foreign_key_failure(true);
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let relay = StreamRelay::new(
            "deleted-conv".into(),
            "assistant-msg".into(),
            "user-1".into(),
            repo.clone(),
            bus,
            None,
        );

        let outcome = relay
            .finalize(
                "partial answer",
                &[],
                &AgentStreamEvent::Finish(FinishEventData::default()),
                RelayTerminal::Finish,
            )
            .await;

        assert_eq!(outcome.terminal, RelayTerminal::Finish);
        assert!(
            repo.take_inserts().is_empty(),
            "failed fallback writes must not be recorded as persisted messages"
        );
    }

    #[tokio::test]
    async fn complete_active_thinking_treats_foreign_key_failure_as_deleted_conversation() {
        let repo = Arc::new(RecordingRepo::new());
        repo.set_foreign_key_failure(true);
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let relay = StreamRelay::new(
            "deleted-conv".into(),
            "assistant-msg".into(),
            "user-1".into(),
            repo.clone(),
            bus,
            None,
        );
        let mut active_thinking = Some(ThinkingSegmentState {
            id: "thinking-1".into(),
            buffer: "working".into(),
            started_at: now_ms(),
        });

        relay.complete_active_thinking(&mut active_thinking).await;

        assert!(active_thinking.is_none());
        assert!(
            repo.take_inserts().is_empty(),
            "failed thinking writes must not be recorded as persisted messages"
        );
    }

    // ── Helpers ──────────────────────────────────────────────────

    struct MockCronService;

    #[async_trait::async_trait]
    impl ICronService for MockCronService {
        async fn create_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _params: &crate::response_middleware::CronCreateParams,
        ) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "created".into(),
            }
        }

        async fn update_job(
            &self,
            _user_id: &str,
            _conversation_id: &str,
            _params: &crate::response_middleware::CronUpdateParams,
        ) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "updated".into(),
            }
        }

        async fn list_jobs(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "listed".into(),
            }
        }

        async fn delete_job(&self, _user_id: &str, _job_id: &str) -> crate::response_middleware::CronCommandResult {
            crate::response_middleware::CronCommandResult {
                success: true,
                message: "deleted".into(),
            }
        }
    }

    /// Recording repo that captures insert/update calls for assertions.
    struct RecordingRepo {
        inserts: Mutex<Vec<MessageRow>>,
        updates: Mutex<Vec<(String, aionui_db::MessageRowUpdate)>>,
        not_found: AtomicBool,
        foreign_key_failure: AtomicBool,
    }

    impl RecordingRepo {
        fn new() -> Self {
            Self {
                inserts: Mutex::new(vec![]),
                updates: Mutex::new(vec![]),
                not_found: AtomicBool::new(false),
                foreign_key_failure: AtomicBool::new(false),
            }
        }

        fn set_not_found(&self, value: bool) {
            self.not_found.store(value, Ordering::Release);
        }

        fn set_foreign_key_failure(&self, value: bool) {
            self.foreign_key_failure.store(value, Ordering::Release);
        }

        fn take_inserts(&self) -> Vec<MessageRow> {
            std::mem::take(&mut self.inserts.lock().unwrap())
        }

        #[allow(dead_code)]
        fn take_updates(&self) -> Vec<(String, aionui_db::MessageRowUpdate)> {
            std::mem::take(&mut self.updates.lock().unwrap())
        }
    }

    #[async_trait::async_trait]
    impl IConversationRepository for RecordingRepo {
        async fn get(&self, _id: &str) -> Result<Option<aionui_db::models::ConversationRow>, DbError> {
            Ok(None)
        }
        async fn create(&self, _row: &aionui_db::models::ConversationRow) -> Result<(), DbError> {
            Ok(())
        }
        async fn update(&self, _id: &str, _updates: &aionui_db::ConversationRowUpdate) -> Result<(), DbError> {
            if self.not_found.load(Ordering::Acquire) {
                return Err(DbError::NotFound("Conversation deleted-conv not found".into()));
            }
            Ok(())
        }
        async fn delete(&self, _id: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn list_paginated(
            &self,
            _user_id: &str,
            _filters: &aionui_db::ConversationFilters,
        ) -> Result<aionui_common::PaginatedResult<aionui_db::models::ConversationRow>, DbError> {
            Ok(aionui_common::PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
        async fn find_by_source_and_chat(
            &self,
            _user_id: &str,
            _source: &str,
            _chat_id: &str,
            _agent_type: &str,
        ) -> Result<Option<aionui_db::models::ConversationRow>, DbError> {
            Ok(None)
        }
        async fn list_by_cron_job(
            &self,
            _user_id: &str,
            _cron_job_id: &str,
        ) -> Result<Vec<aionui_db::models::ConversationRow>, DbError> {
            Ok(vec![])
        }
        async fn list_associated(
            &self,
            _user_id: &str,
            _conversation_id: &str,
        ) -> Result<Vec<aionui_db::models::ConversationRow>, DbError> {
            Ok(vec![])
        }
        async fn get_messages(
            &self,
            _conv_id: &str,
            _page: u32,
            _page_size: u32,
            _order: aionui_db::SortOrder,
        ) -> Result<aionui_common::PaginatedResult<MessageRow>, DbError> {
            Ok(aionui_common::PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
        async fn insert_message(&self, row: &MessageRow) -> Result<(), DbError> {
            if self.not_found.load(Ordering::Acquire) {
                return Err(DbError::NotFound(format!("Message '{}'", row.id)));
            }
            if self.foreign_key_failure.load(Ordering::Acquire) {
                return Err(DbError::Init("FOREIGN KEY constraint failed".into()));
            }
            self.inserts.lock().unwrap().push(row.clone());
            Ok(())
        }
        async fn update_message(&self, id: &str, updates: &aionui_db::MessageRowUpdate) -> Result<(), DbError> {
            if self.not_found.load(Ordering::Acquire) {
                return Err(DbError::NotFound(format!("Message '{id}' not found")));
            }
            self.updates.lock().unwrap().push((id.to_owned(), updates.clone()));
            Ok(())
        }
        async fn delete_messages_by_conversation(&self, _conv_id: &str) -> Result<(), DbError> {
            Ok(())
        }
        async fn get_message_by_msg_id(
            &self,
            _conv_id: &str,
            msg_id: &str,
            msg_type: &str,
        ) -> Result<Option<MessageRow>, DbError> {
            let inserts = self.inserts.lock().unwrap();
            Ok(inserts
                .iter()
                .find(|m| m.msg_id.as_deref() == Some(msg_id) && m.r#type == msg_type)
                .cloned())
        }
        async fn search_messages(
            &self,
            _user_id: &str,
            _keyword: &str,
            _page: u32,
            _page_size: u32,
        ) -> Result<aionui_common::PaginatedResult<aionui_db::MessageSearchRow>, DbError> {
            Ok(aionui_common::PaginatedResult {
                items: vec![],
                total: 0,
                has_more: false,
            })
        }
    }
}
