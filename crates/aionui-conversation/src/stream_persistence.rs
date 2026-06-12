use std::sync::Arc;

use aionui_ai_agent::protocol::events::{
    ErrorEventData, TipType, TipsEventData,
    tool_call::{AcpToolCallSessionUpdateKind, AcpToolCallStatus, ToolCallStatus},
};
use aionui_api_types::{ConversationRuntimeSummary, WebSocketMessage};
use aionui_common::{ErrorChain, normalize_keys_to_snake_case, now_ms};
use aionui_db::models::MessageRow;
use aionui_db::{ConversationRowUpdate, DbError, IConversationRepository, MessageRowUpdate};
use aionui_realtime::EventBroadcaster;
use serde_json::json;
use tracing::{debug, error, warn};

use crate::runtime_completion::RuntimeCompletionPublisher;
use crate::runtime_persistence::{RuntimePersistenceCoordinator, RuntimeWriteKind};
use crate::service::ConversationService;

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
pub(crate) struct TextSegmentState {
    pub id: String,
    pub buffer: String,
    pub created_at: i64,
    pub record_created: bool,
    pub flush_counter: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct PersistedTextSegment {
    pub id: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ThinkingSegmentState {
    pub id: String,
    pub buffer: String,
    pub started_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FinalTextOverride {
    pub msg_id: String,
    pub text: String,
    pub hidden: bool,
}

#[derive(Clone)]
pub(crate) struct StreamPersistenceAdapter {
    conversation_id: String,
    msg_id: String,
    repo: Arc<dyn IConversationRepository>,
    persistence: Option<RuntimePersistenceCoordinator>,
}

impl StreamPersistenceAdapter {
    pub fn new(
        conversation_id: String,
        msg_id: String,
        repo: Arc<dyn IConversationRepository>,
        persistence: Option<RuntimePersistenceCoordinator>,
    ) -> Self {
        Self {
            conversation_id,
            msg_id,
            repo,
            persistence,
        }
    }

    pub fn with_persistence(mut self, persistence: RuntimePersistenceCoordinator) -> Self {
        self.persistence = Some(persistence);
        self
    }

    #[tracing::instrument(skip_all, fields(conversation_id = %self.conversation_id))]
    pub async fn complete_conversation(
        &self,
        broadcaster: &Arc<dyn EventBroadcaster>,
        turn_id: &str,
        runtime: Option<ConversationRuntimeSummary>,
    ) {
        if let Some(persistence) = &self.persistence {
            RuntimeCompletionPublisher::new(self.repo.clone(), broadcaster.clone(), persistence.clone())
                .publish(&self.conversation_id, turn_id, runtime)
                .await;
            return;
        }

        let update = ConversationRowUpdate {
            status: Some("finished".to_owned()),
            updated_at: Some(now_ms()),
            ..Default::default()
        };
        if let Err(e) = self.repo.update(&self.conversation_id, &update).await {
            log_persist_error(&e, "Failed to update conversation status");
        }

        let payload = json!({
            "conversation_id": self.conversation_id,
            "session_id": self.conversation_id,
            "turn_id": turn_id,
            "status": "finished",
            "canSendMessage": true,
            "runtime": runtime,
        });
        broadcaster.broadcast(WebSocketMessage::new("turn.completed", payload));

        debug!(conversation_id = %self.conversation_id, turn_id, status = "finished", "Turn completed");
    }

    fn allows_write(&self, kind: RuntimeWriteKind) -> bool {
        self.persistence
            .as_ref()
            .is_none_or(|persistence| persistence.allows(&self.conversation_id, kind))
    }

    #[tracing::instrument(skip_all)]
    pub async fn flush_text_segment(&self, segment: &mut TextSegmentState) {
        if !self.allows_write(RuntimeWriteKind::AssistantTextFlush) {
            return;
        }
        if segment.buffer.is_empty() {
            return;
        }

        let content = json!({ "content": segment.buffer }).to_string();

        if segment.record_created {
            let update = MessageRowUpdate {
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
    pub async fn finalize_text_segment(&self, segment: TextSegmentState, status: &str) -> Option<PersistedTextSegment> {
        if !self.allows_write(RuntimeWriteKind::AssistantTextFinalize) {
            return None;
        }
        if segment.buffer.is_empty() {
            return None;
        }

        let content = json!({ "content": segment.buffer }).to_string();
        if segment.record_created {
            let update = MessageRowUpdate {
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

    #[tracing::instrument(skip_all)]
    pub async fn persist_final_text(
        &self,
        text_segments: &[PersistedTextSegment],
        status: &str,
        final_text: &str,
        hidden: bool,
        rewrite_segments: bool,
    ) -> Vec<FinalTextOverride> {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return Vec::new();
        }

        let mut overrides = Vec::new();
        if let Some(primary_segment) = text_segments.first() {
            if rewrite_segments {
                let content = json!({ "content": final_text }).to_string();
                let update = MessageRowUpdate {
                    content: Some(content),
                    status: Some(Some(status.to_owned())),
                    hidden: Some(hidden),
                };
                if let Err(e) = self.repo.update_message(&primary_segment.id, &update).await {
                    log_persist_error(&e, "Failed to rewrite finalized text segment");
                }
                overrides.push(FinalTextOverride {
                    msg_id: primary_segment.id.clone(),
                    text: final_text.to_owned(),
                    hidden,
                });

                for segment in text_segments.iter().skip(1) {
                    let hide_update = MessageRowUpdate {
                        content: None,
                        status: Some(Some(status.to_owned())),
                        hidden: Some(true),
                    };
                    if let Err(e) = self.repo.update_message(&segment.id, &hide_update).await {
                        log_persist_error(&e, "Failed to hide superseded text segment");
                    }
                    overrides.push(FinalTextOverride {
                        msg_id: segment.id.clone(),
                        text: String::new(),
                        hidden: true,
                    });
                }
            } else {
                for segment in text_segments {
                    let status_update = MessageRowUpdate {
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

        overrides
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_error_tip(&self, data: &ErrorEventData) {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return;
        }

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

    #[tracing::instrument(skip_all)]
    pub async fn persist_tip(&self, data: &TipsEventData) {
        if !self.allows_write(RuntimeWriteKind::TerminalFinalize) {
            return;
        }

        let status = match data.tip_type {
            TipType::Error => "error",
            TipType::Success | TipType::Warning | TipType::Info => "finish",
        };
        let content = json!({
            "content": &data.content,
            "type": &data.tip_type,
            "code": &data.code,
            "params": &data.params,
        })
        .to_string();
        let row = MessageRow {
            id: ConversationService::mint_msg_id(),
            conversation_id: self.conversation_id.clone(),
            msg_id: None,
            r#type: "tips".into(),
            content,
            position: Some("left".into()),
            status: Some(status.into()),
            hidden: false,
            created_at: now_ms(),
        };
        if let Err(e) = self.repo.insert_message(&row).await {
            log_persist_error(&e, "Failed to store tip message");
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn persist_thinking_segment(&self, segment: ThinkingSegmentState, duration_ms: u64) {
        if segment.buffer.is_empty() {
            return;
        }
        if !self.allows_write(RuntimeWriteKind::AssistantThinkingFinalize) {
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

    /// Persist a Gemini-style tool_call event.
    #[tracing::instrument(skip_all)]
    pub async fn persist_tool_call(&self, data: &aionui_ai_agent::protocol::events::tool_call::ToolCallEventData) {
        if !self.allows_write(RuntimeWriteKind::ToolCallPersist) {
            return;
        }
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
            let update = MessageRowUpdate {
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
    #[tracing::instrument(skip_all)]
    pub async fn persist_acp_tool_call(
        &self,
        data: &aionui_ai_agent::protocol::events::tool_call::AcpToolCallEventData,
    ) {
        if !self.allows_write(RuntimeWriteKind::AcpToolCallPersist) {
            return;
        }
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
                let update = MessageRowUpdate {
                    content: Some(merged_content.clone()),
                    status: Some(Some(status.to_owned())),
                    hidden: None,
                };
                if let Err(e) = self.repo.update_message(tool_call_id, &update).await {
                    match e {
                        DbError::NotFound(_) => {
                            warn!(
                                conversation_id = %self.conversation_id,
                                tool_call_id = %tool_call_id,
                                "ACP tool call update arrived before initial tool call; inserting placeholder"
                            );
                            let row = MessageRow {
                                id: tool_call_id.clone(),
                                conversation_id: self.conversation_id.clone(),
                                msg_id: Some(tool_call_id.clone()),
                                r#type: "acp_tool_call".into(),
                                content: merged_content,
                                position: Some("left".into()),
                                status: Some(status.to_owned()),
                                hidden: false,
                                created_at: now_ms(),
                            };
                            if let Err(insert_err) = self.repo.insert_message(&row).await {
                                error!(
                                    error = %ErrorChain(&insert_err),
                                    "Failed to insert late acp_tool_call placeholder"
                                );
                            }
                        }
                        other => {
                            error!(error = %ErrorChain(&other), "Failed to update acp_tool_call message");
                        }
                    }
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
    pub async fn persist_tool_group(&self, entries: &[aionui_ai_agent::protocol::events::tool_call::ToolGroupEntry]) {
        if !self.allows_write(RuntimeWriteKind::ToolGroupPersist) {
            return;
        }
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
            let update = MessageRowUpdate {
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
}
