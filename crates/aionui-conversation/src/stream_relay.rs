use std::sync::Arc;

use aionui_ai_agent::protocol::events::{ErrorEventData, TipType};
use aionui_ai_agent::{AgentSendError, AgentStreamEvent, protocol::events::ThinkingEventData};

use crate::response_middleware::{ICronService, ISkillLoadService, MessageMiddleware, MiddlewareResult};
use crate::skill_resolver::{LoadedAgentSkill, SkillResolver};
use aionui_api_types::{AgentErrorCode, WebSocketMessage};
use aionui_common::{ErrorChain, normalize_keys_to_snake_case, now_ms};

use crate::runtime_persistence::RuntimePersistenceCoordinator;
use crate::runtime_state::ConversationRuntimeStateService;
use crate::service::ConversationService;
use crate::stream_persistence::{
    PersistedTextSegment, StreamPersistenceAdapter, TextSegmentState, ThinkingSegmentState,
};
use aionui_db::IConversationRepository;
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tokio::sync::broadcast::error::TryRecvError;
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, info, warn};

/// Number of text chunks to accumulate before flushing to the database.
const FLUSH_INTERVAL: u32 = 20;

/// Conservative summary of what happened during one agent send attempt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnAttemptSummary {
    pub saw_visible_output: bool,
    pub saw_tool_or_side_effect: bool,
    pub persisted_assistant_output: bool,
    pub terminal_error: Option<ErrorEventData>,
    pub terminal_error_deferred: bool,
}

impl TurnAttemptSummary {
    pub fn safe_to_auto_replay(&self) -> bool {
        !self.saw_visible_output && !self.saw_tool_or_side_effect && !self.persisted_assistant_output
    }

    pub fn merge(&mut self, other: &TurnAttemptSummary) {
        self.saw_visible_output |= other.saw_visible_output;
        self.saw_tool_or_side_effect |= other.saw_tool_or_side_effect;
        self.persisted_assistant_output |= other.persisted_assistant_output;
        if other.terminal_error.is_some() {
            self.terminal_error = other.terminal_error.clone();
        }
        self.terminal_error_deferred |= other.terminal_error_deferred;
    }
}

/// Result returned after a relay turn has fully drained and finalized.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelayOutcome {
    pub system_responses: Vec<String>,
    pub terminal: RelayTerminal,
    pub attempt: TurnAttemptSummary,
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
    turn_id: String,
    user_id: String,
    broadcaster: Arc<dyn EventBroadcaster>,
    cron_service: Option<Arc<dyn ICronService>>,
    skill_resolver: Option<Arc<dyn SkillResolver>>,
    allowed_skill_names: Vec<String>,
    runtime_state: Option<Arc<ConversationRuntimeStateService>>,
    persistence: Option<RuntimePersistenceCoordinator>,
    adapter: StreamPersistenceAdapter,
    complete_turn: bool,
    defer_clean_terminal_errors: bool,
}

impl StreamRelay {
    pub fn new(
        conversation_id: String,
        msg_id: String,
        turn_id: String,
        user_id: String,
        repo: Arc<dyn IConversationRepository>,
        broadcaster: Arc<dyn EventBroadcaster>,
        cron_service: Option<Arc<dyn ICronService>>,
    ) -> Self {
        let adapter = StreamPersistenceAdapter::new(conversation_id.clone(), msg_id.clone(), repo, None);
        Self {
            conversation_id,
            msg_id,
            turn_id,
            user_id,
            broadcaster,
            cron_service,
            skill_resolver: None,
            allowed_skill_names: Vec::new(),
            runtime_state: None,
            persistence: None,
            adapter,
            complete_turn: true,
            defer_clean_terminal_errors: false,
        }
    }

    pub fn with_runtime_state(mut self, runtime_state: Arc<ConversationRuntimeStateService>) -> Self {
        self.runtime_state = Some(runtime_state);
        self
    }

    pub fn with_skill_resolver(mut self, skill_resolver: Arc<dyn SkillResolver>) -> Self {
        self.skill_resolver = Some(skill_resolver);
        self
    }

    pub fn with_allowed_skill_names(mut self, skill_names: Vec<String>) -> Self {
        self.allowed_skill_names = skill_names;
        self
    }

    pub fn with_persistence(mut self, persistence: RuntimePersistenceCoordinator) -> Self {
        self.persistence = Some(persistence.clone());
        self.adapter = self.adapter.with_persistence(persistence);
        self
    }

    pub fn with_turn_completion(mut self, enabled: bool) -> Self {
        self.complete_turn = enabled;
        self
    }

    pub fn with_defer_clean_terminal_errors(mut self, enabled: bool) -> Self {
        self.defer_clean_terminal_errors = enabled;
        self
    }

    /// Run the relay loop. Consumes `self` and runs until the agent stream ends.
    #[tracing::instrument(
        skip_all,
        fields(
            conversation_id = %self.conversation_id,
            msg_id = %self.msg_id,
            turn_id = %self.turn_id,
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
            turn_id = %self.turn_id,
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
        info!(
            conversation_id = %self.conversation_id,
            turn_id = %self.turn_id,
            msg_id = %self.msg_id,
            "StreamRelay started"
        );

        let mut full_text_buffer = String::new();
        let mut text_segments: Vec<PersistedTextSegment> = Vec::new();
        let mut active_text: Option<TextSegmentState> = None;
        let mut active_thinking: Option<ThinkingSegmentState> = None;
        let mut used_primary_segment_msg_id = false;
        let mut first_agent_event_logged = false;
        let mut first_visible_output_logged = false;
        let mut send_error_done = send_error_rx.is_none();
        let mut pending_send_error: Option<AgentSendError> = None;
        let mut attempt = TurnAttemptSummary::default();

        loop {
            let recv_result = if send_error_done {
                if let Some(send_error) = pending_send_error.take() {
                    match rx.try_recv() {
                        Ok(event) => {
                            if !matches!(event, AgentStreamEvent::Finish(_) | AgentStreamEvent::Error(_)) {
                                pending_send_error = Some(send_error);
                            } else {
                                debug!("Runtime terminal event won race with fallback send error");
                            }
                            Ok(event)
                        }
                        Err(TryRecvError::Empty) | Err(TryRecvError::Closed) => {
                            warn!(
                                code = ?send_error.code(),
                                ownership = ?send_error.ownership(),
                                "Injecting stream error for failed agent send"
                            );
                            Ok(AgentStreamEvent::Error(send_error.into_stream_error()))
                        }
                        Err(TryRecvError::Lagged(n)) => Err(broadcast::error::RecvError::Lagged(n)),
                    }
                } else {
                    rx.recv().await
                }
            } else {
                tokio::select! {
                    recv = rx.recv() => recv,
                    send_error = send_error_rx.as_mut().expect("send_error_rx exists while pending") => {
                        send_error_done = true;
                        match send_error {
                            Ok(send_error) => {
                                pending_send_error = Some(send_error);
                                continue;
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
                            if !data.content.is_empty() {
                                attempt.saw_visible_output = true;
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
                            if !data.content.is_empty() {
                                attempt.saw_visible_output = true;
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
                                self.adapter.flush_text_segment(segment).await;
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

                            let defer_clean_error = self.defer_clean_terminal_errors
                                && matches!(
                                    terminal,
                                    RelayTerminal::Error {
                                        retryable: Some(true),
                                        ..
                                    }
                                )
                                && terminal.code() != Some(AgentErrorCode::UserLlmProviderModelNotFound)
                                && attempt.safe_to_auto_replay();

                            if defer_clean_error {
                                if let AgentStreamEvent::Error(data) = &event {
                                    attempt.terminal_error = Some(data.clone());
                                    attempt.terminal_error_deferred = true;
                                }
                                info!(
                                    event_type,
                                    elapsed_ms, "StreamRelay deferred clean terminal error for possible auto replay"
                                );
                                break RelayOutcome {
                                    system_responses: Vec::new(),
                                    terminal,
                                    attempt,
                                };
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
                            if let AgentStreamEvent::Error(data) = &event {
                                attempt.terminal_error = Some(data.clone());
                            }
                            let mut outcome = if deleting {
                                RelayOutcome {
                                    system_responses: Vec::new(),
                                    terminal,
                                    attempt: attempt.clone(),
                                }
                            } else {
                                self.finalize(&full_text_buffer, &text_segments, &event, terminal).await
                            };
                            outcome.attempt = attempt.clone();
                            if !full_text_buffer.is_empty() {
                                outcome.attempt.persisted_assistant_output = true;
                            }
                            if self.complete_turn && !deleting {
                                self.adapter
                                    .complete_conversation(&self.broadcaster, &self.turn_id, None)
                                    .await;
                            }
                            break outcome;
                        }
                        AgentStreamEvent::ToolCall(data) => {
                            attempt.saw_tool_or_side_effect = true;
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.adapter.persist_tool_call(data).await;
                        }
                        AgentStreamEvent::AcpToolCall(data) => {
                            attempt.saw_tool_or_side_effect = true;
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.adapter.persist_acp_tool_call(data).await;
                        }
                        AgentStreamEvent::ToolGroup(entries) => {
                            attempt.saw_tool_or_side_effect = true;
                            self.complete_active_thinking(&mut active_thinking).await;
                            self.close_active_text_segment(&mut active_text, &mut text_segments, "finish")
                                .await;
                            self.forward_to_websocket(&event);
                            self.adapter.persist_tool_group(entries).await;
                        }
                        AgentStreamEvent::Tips(data) => {
                            if matches!(data.tip_type, TipType::Success | TipType::Warning | TipType::Info) {
                                attempt.saw_visible_output = true;
                            }
                            self.forward_to_websocket(&event);
                            if matches!(data.tip_type, TipType::Success | TipType::Warning | TipType::Info) {
                                self.adapter.persist_tip(data).await;
                            }
                        }
                        AgentStreamEvent::CronTrigger(_)
                        | AgentStreamEvent::Permission(_)
                        | AgentStreamEvent::AcpPermission(_) => {
                            attempt.saw_tool_or_side_effect = true;
                            self.forward_to_websocket(&event);
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
                    let mut outcome = if deleting {
                        RelayOutcome {
                            system_responses: Vec::new(),
                            terminal: RelayTerminal::ChannelClosed,
                            attempt: attempt.clone(),
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
                    outcome.attempt = attempt.clone();
                    if !full_text_buffer.is_empty() {
                        outcome.attempt.persisted_assistant_output = true;
                    }
                    if self.complete_turn && !deleting {
                        self.adapter
                            .complete_conversation(&self.broadcaster, &self.turn_id, None)
                            .await;
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
            "turn_id": self.turn_id,
            "type": event_data.get("type").cloned().unwrap_or(json!("unknown")),
            "data": event_data.get("data").cloned().unwrap_or(json!({})),
            "hidden": false,
        });

        self.broadcast_stream_payload(payload);
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
            attempt: TurnAttemptSummary::default(),
        };
        let status = match event {
            AgentStreamEvent::Error(_) => "error",
            _ => "finish",
        };

        if !text.is_empty() {
            let processed = self.process_final_text(text).await;
            let final_text = processed.message.trim().to_owned();
            let hidden = final_text.is_empty();

            let rewrite_segments = processed.message != text || hidden;
            let overrides = self
                .adapter
                .persist_final_text(text_segments, status, &final_text, hidden, rewrite_segments)
                .await;
            for override_event in overrides {
                self.send_final_text_override(&override_event.msg_id, &override_event.text, override_event.hidden);
            }

            self.send_system_responses(&processed.system_responses);
            outcome.system_responses = processed.system_responses;
        } else if let AgentStreamEvent::Error(data) = event {
            self.adapter.persist_error_tip(data).await;
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
        self.adapter.persist_thinking_segment(segment, duration_ms as u64).await;
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
        if let Some(segment) = self.adapter.finalize_text_segment(text_segment, status).await {
            text_segments.push(segment);
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
        let middleware = MessageMiddleware::new_with_skill_loader(
            self.cron_service
                .as_ref()
                .map(|service| Box::new(SharedCronService(Arc::clone(service))) as Box<dyn ICronService>),
            self.skill_resolver.as_ref().map(|resolver| {
                Box::new(SharedSkillResolver {
                    resolver: Arc::clone(resolver),
                    allowed_skill_names: self.allowed_skill_names.clone(),
                }) as Box<dyn ISkillLoadService>
            }),
        );

        middleware.process(text, &self.user_id, &self.conversation_id).await
    }

    fn send_final_text_override(&self, msg_id: &str, text: &str, hidden: bool) {
        self.broadcast_stream_payload(json!({
            "conversation_id": self.conversation_id,
            "msg_id": msg_id,
            "turn_id": self.turn_id,
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
                "turn_id": self.turn_id,
                "type": "system",
                "data": response,
                "hidden": true,
            }));
        }
    }

    fn broadcast_stream_payload(&self, mut payload: serde_json::Value) {
        if let Some(obj) = payload.as_object_mut() {
            obj.entry("turn_id")
                .or_insert_with(|| serde_json::Value::String(self.turn_id.clone()));
        }
        let msg = WebSocketMessage::new("message.stream", payload);
        self.broadcaster.broadcast(msg);
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

struct SharedSkillResolver {
    resolver: Arc<dyn SkillResolver>,
    allowed_skill_names: Vec<String>,
}

#[async_trait::async_trait]
impl ISkillLoadService for SharedSkillResolver {
    async fn load_skill_bodies(&self, names: &[String]) -> Vec<LoadedAgentSkill> {
        if self.allowed_skill_names.is_empty() {
            return Vec::new();
        }
        let filtered: Vec<String> = names
            .iter()
            .filter(|name| self.allowed_skill_names.iter().any(|allowed| allowed == *name))
            .cloned()
            .collect();
        self.resolver.load_skill_bodies(&filtered).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream_persistence::StreamPersistenceAdapter;
    use aionui_ai_agent::AgentError;
    use aionui_ai_agent::protocol::events::{ErrorEventData, FinishEventData, TextEventData, ThinkingEventData};
    use aionui_db::DbError;
    use aionui_db::models::MessageRow;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ── run() async tests ─────────────────────────────────────────

    #[derive(Default)]
    struct RecordingSkillResolverForRelay {
        requested: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl SkillResolver for RecordingSkillResolverForRelay {
        async fn auto_inject_names(&self) -> Vec<String> {
            Vec::new()
        }

        async fn resolve_skills(&self, _names: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
            Vec::new()
        }

        async fn load_skill_bodies(&self, names: &[String]) -> Vec<LoadedAgentSkill> {
            self.requested.lock().unwrap().extend(names.iter().cloned());
            names
                .iter()
                .map(|name| LoadedAgentSkill {
                    name: name.clone(),
                    body: format!("{name} body"),
                })
                .collect()
        }

        async fn link_workspace_skills(
            &self,
            _workspace: &std::path::Path,
            _rel_dirs: &[&str],
            _skills: &[aionui_extension::ResolvedAgentSkill],
        ) -> usize {
            0
        }
    }

    #[tokio::test]
    async fn shared_skill_resolver_filters_requests_to_allowed_skill_names() {
        let concrete = Arc::new(RecordingSkillResolverForRelay::default());
        let resolver: Arc<dyn SkillResolver> = concrete.clone();
        let loader = SharedSkillResolver {
            resolver,
            allowed_skill_names: vec!["cron".into()],
        };

        let loaded = loader.load_skill_bodies(&["cron".to_owned(), "pdf".to_owned()]).await;

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "cron");
        assert_eq!(concrete.requested.lock().unwrap().as_slice(), ["cron"]);
    }

    #[tokio::test]
    async fn run_text_then_finish_persists_message() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "turn-1".into(),
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
            "turn-1".into(),
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
            "turn-1".into(),
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
    async fn clean_retryable_error_can_be_deferred_for_replay() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, rx) = broadcast::channel(8);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "msg-1".into(),
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_turn_completion(false)
        .with_defer_clean_terminal_errors(true);

        tx.send(AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;

        assert_eq!(
            outcome.terminal,
            RelayTerminal::Error {
                code: Some(AgentErrorCode::UnknownUpstreamError),
                retryable: Some(true),
            }
        );
        assert!(outcome.attempt.safe_to_auto_replay());
        assert!(
            ws_rx.try_recv().is_err(),
            "clean first-attempt errors stay hidden before replay"
        );
        assert!(
            repo.take_inserts().is_empty(),
            "no assistant error tip is persisted before replay"
        );
    }

    #[tokio::test]
    async fn text_before_retryable_error_is_not_clean_for_replay() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let mut ws_rx = bus.subscribe();
        let (tx, rx) = broadcast::channel(8);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "msg-1".into(),
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_turn_completion(false)
        .with_defer_clean_terminal_errors(true);

        tx.send(AgentStreamEvent::Text(TextEventData {
            content: "partial".into(),
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;

        assert!(outcome.attempt.saw_visible_output);
        assert!(!outcome.attempt.safe_to_auto_replay());
        let mut saw_error = false;
        while let Ok(event) = ws_rx.try_recv() {
            saw_error |= event.data["type"] == "error";
        }
        assert!(saw_error, "unsafe errors are still broadcast");
    }

    #[tokio::test]
    async fn tool_call_before_retryable_error_is_not_clean_for_replay() {
        use aionui_ai_agent::protocol::events::tool_call::{ToolCallEventData, ToolCallStatus};

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, rx) = broadcast::channel(8);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "msg-1".into(),
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        )
        .with_turn_completion(false)
        .with_defer_clean_terminal_errors(true);

        tx.send(AgentStreamEvent::ToolCall(ToolCallEventData {
            call_id: "call-1".into(),
            name: "write_file".into(),
            args: serde_json::json!({}),
            status: ToolCallStatus::Running,
            input: None,
            output: None,
            description: None,
        }))
        .unwrap();
        tx.send(AgentStreamEvent::Error(ErrorEventData {
            message: "temporary provider failure".into(),
            code: Some(AgentErrorCode::UnknownUpstreamError),
            ownership: None,
            detail: None,
            workspace_path: None,
            retryable: Some(true),
            feedback_recommended: None,
            resolution: None,
        }))
        .unwrap();
        drop(tx);

        let outcome = relay.consume(rx).await;

        assert!(outcome.attempt.saw_tool_or_side_effect);
        assert!(!outcome.attempt.safe_to_auto_replay());
    }

    #[tokio::test]
    async fn run_warning_tip_with_finish_persists_warning_tip() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();

        tx.send(AgentStreamEvent::Tips(
            aionui_ai_agent::protocol::events::TipsEventData {
                content: String::new(),
                tip_type: aionui_ai_agent::protocol::events::TipType::Warning,
                code: Some("ACP_EMPTY_TURN".into()),
                params: None,
            },
        ))
        .unwrap();
        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        let outcome = relay.consume(rx).await;
        assert!(outcome.system_responses.is_empty());
        assert_eq!(outcome.terminal, RelayTerminal::Finish);

        let inserts = repo.take_inserts();
        assert_eq!(inserts.len(), 1);
        let msg = &inserts[0];
        assert_eq!(msg.r#type, "tips");
        assert_eq!(msg.status.as_deref(), Some("finish"));

        let content: serde_json::Value = serde_json::from_str(&msg.content).unwrap();
        assert_eq!(content["content"], "");
        assert_eq!(content["type"], "warning");
        assert_eq!(content["code"], "ACP_EMPTY_TURN");
    }

    #[tokio::test]
    async fn run_send_error_injects_error_and_completes_turn() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let mut ws_rx = bus.subscribe();
        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(AgentSendError::from_agent_error(AgentError::bad_gateway(
                "provider returned 401 invalid api key",
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
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let send_error =
            AgentSendError::from_agent_error(AgentError::bad_gateway("provider returned 401 invalid api key"));
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
            "turn-1".into(),
            "user-1".into(),
            repo.clone(),
            bus.clone(),
            None,
        );

        let rx = tx.subscribe();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        send_error_tx
            .send(AgentSendError::from_agent_error(AgentError::bad_gateway(
                "provider returned 401 invalid api key",
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
            "turn-1".into(),
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
            "turn-1".into(),
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
            "turn-1".into(),
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
            "turn-1".into(),
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
        assert_eq!(data["turn_id"], "turn-1");
        assert_eq!(data["status"], "finished");
        assert_eq!(data["canSendMessage"], true);

        let stream_event = ws_events
            .iter()
            .find(|e| e.name == "message.stream")
            .expect("finish should be forwarded as message.stream");
        assert_eq!(stream_event.data["turn_id"], "turn-1");
    }

    #[tokio::test]
    async fn run_finalizes_with_cleaned_replacement_event() {
        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);
        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "turn-1".into(),
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
            "turn-1".into(),
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
            "turn-1".into(),
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
    async fn run_acp_image_tool_call_update_persists_finish_without_base64() {
        use aionui_ai_agent::protocol::events::tool_call::{
            AcpToolCallEventData, AcpToolCallKind, AcpToolCallSessionUpdateKind, AcpToolCallStatus,
            AcpToolCallUpdateData,
        };

        let repo = Arc::new(RecordingRepo::new());
        let bus = Arc::new(aionui_realtime::BroadcastEventBus::new(64));
        let (tx, _) = broadcast::channel(64);

        let relay = StreamRelay::new(
            "conv-1".into(),
            "asst-1".into(),
            "turn-1".into(),
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
                tool_call_id: "ig_test_image".into(),
                status: Some(AcpToolCallStatus::InProgress),
                title: Some("Image generation".into()),
                kind: Some(AcpToolCallKind::Execute),
                raw_input: Some(json!({"prompt": "一只小猫"})),
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
                tool_call_id: "ig_test_image".into(),
                status: Some(AcpToolCallStatus::Completed),
                title: None,
                kind: Some(AcpToolCallKind::Execute),
                raw_input: None,
                raw_output: Some(json!({
                    "saved_path": "/Users/test/.codex/generated_images/session/ig_test_image.png",
                    "image": {
                        "path": "/Users/test/.codex/generated_images/session/ig_test_image.png",
                        "mime_type": "image/png",
                        "source": "codex_image_generation"
                    },
                    "result_omitted": true,
                    "result_omitted_reason": "image_base64",
                    "result_bytes": 131_083
                })),
                content: None,
                locations: None,
            },
            meta: None,
        }))
        .unwrap();

        tx.send(AgentStreamEvent::Finish(FinishEventData::default())).unwrap();

        relay.consume(rx).await;

        let updates = repo.take_updates();
        let acp_update = updates.iter().find(|(id, _)| id == "ig_test_image");
        assert!(acp_update.is_some());
        let (_, upd) = acp_update.unwrap();
        assert_eq!(upd.status, Some(Some("finish".to_owned())));

        let content = upd.content.as_deref().unwrap();
        assert!(!content.contains("iVBORw0KGgo"));
        assert!(content.contains("result_omitted"));

        let merged: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(
            merged["update"]["raw_output"]["image"]["path"],
            "/Users/test/.codex/generated_images/session/ig_test_image.png"
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
            "turn-1".into(),
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
            "turn-1".into(),
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
        let adapter = StreamPersistenceAdapter::new("deleted-conv".into(), "msg-1".into(), repo, None);

        adapter.complete_conversation(&bus, "turn-1", None).await;
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
            "turn-1".into(),
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
            "turn-1".into(),
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
            "turn-1".into(),
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

        fn merged_row(existing: &MessageRow, incoming: &MessageRow) -> MessageRow {
            let preserve_terminal_status = matches!(existing.status.as_deref(), Some("finish" | "error"))
                && incoming.status.as_deref() == Some("work");
            let mut content = Self::merge_json_content(&existing.content, &incoming.content);
            if preserve_terminal_status {
                content = Self::preserve_json_status(&content, &existing.content, &existing.r#type);
            }

            let mut merged = existing.clone();
            merged.content = content;
            merged.status = if preserve_terminal_status {
                existing.status.clone()
            } else {
                incoming.status.clone()
            };
            merged.hidden = incoming.hidden;
            merged
        }

        fn merge_json_content(existing_json: &str, incoming_json: &str) -> String {
            let mut existing: serde_json::Value = serde_json::from_str(existing_json).unwrap_or_default();
            let incoming: serde_json::Value = serde_json::from_str(incoming_json).unwrap_or_default();
            Self::merge_json_value(&mut existing, incoming);
            existing.to_string()
        }

        fn merge_json_value(existing: &mut serde_json::Value, incoming: serde_json::Value) {
            match (existing, incoming) {
                (serde_json::Value::Object(existing_obj), serde_json::Value::Object(incoming_obj)) => {
                    for (key, value) in incoming_obj {
                        if !value.is_null() {
                            if let Some(existing_value) = existing_obj.get_mut(&key) {
                                Self::merge_json_value(existing_value, value);
                            } else {
                                existing_obj.insert(key, value);
                            }
                        }
                    }
                }
                (existing_value, incoming_value) => {
                    if !incoming_value.is_null() {
                        *existing_value = incoming_value;
                    }
                }
            }
        }

        fn preserve_json_status(merged_json: &str, existing_json: &str, msg_type: &str) -> String {
            let mut merged: serde_json::Value = serde_json::from_str(merged_json).unwrap_or_default();
            let existing: serde_json::Value = serde_json::from_str(existing_json).unwrap_or_default();
            let status = if msg_type == "acp_tool_call" {
                existing.pointer("/update/status").cloned()
            } else {
                existing.get("status").cloned()
            };

            if let Some(status) = status {
                if msg_type == "acp_tool_call" {
                    if let Some(update) = merged.get_mut("update").and_then(|value| value.as_object_mut()) {
                        update.insert("status".into(), status);
                    }
                } else if let Some(object) = merged.as_object_mut() {
                    object.insert("status".into(), status);
                }
            }

            merged.to_string()
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
        async fn list_messages_page(
            &self,
            _conv_id: &str,
            _params: &aionui_db::MessagePageParams,
        ) -> Result<aionui_db::MessagePageResult, DbError> {
            Ok(aionui_db::MessagePageResult {
                items: vec![],
                has_more_before: false,
                has_more_after: false,
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
        async fn upsert_message(&self, row: &MessageRow) -> Result<(), DbError> {
            if self.not_found.load(Ordering::Acquire) {
                return Err(DbError::NotFound(format!("Message '{}'", row.id)));
            }
            if self.foreign_key_failure.load(Ordering::Acquire) {
                return Err(DbError::Init("FOREIGN KEY constraint failed".into()));
            }

            let mut inserts = self.inserts.lock().unwrap();
            if let Some(existing) = inserts.iter().find(|message| message.id == row.id) {
                let merged = Self::merged_row(existing, row);
                self.updates.lock().unwrap().push((
                    row.id.clone(),
                    aionui_db::MessageRowUpdate {
                        content: Some(merged.content),
                        status: Some(merged.status),
                        hidden: Some(merged.hidden),
                    },
                ));
            } else {
                inserts.push(row.clone());
            }
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
