//! Side-conversation fork primitive (`POST /api/conversations/:id/side`).

use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    CreateConversationRequest, CreateSideConversationRequest, CreateSideConversationResponse, SendMessageRequest,
    UpdateConversationRequest,
};
use aionui_common::{AgentType, AppError, now_ms};
use tracing::warn;
use aionui_db::SortOrder;
use serde_json::{Value, json};
use tracing::info;

use crate::service::ConversationService;

const SIDE_TRANSCRIPT_LIMIT: u32 = 40;

impl ConversationService {
    /// Fork a multi-turn side conversation from a parent row.
    #[tracing::instrument(skip_all, fields(parent_id = %parent_id))]
    pub async fn create_side_conversation(
        &self,
        user_id: &str,
        parent_id: &str,
        req: CreateSideConversationRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(CreateSideConversationResponse, bool), AppError> {
        let parent = self
            .conversation_repo()
            .get(parent_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| AppError::NotFound(format!("Conversation {parent_id} not found")))?;

        let mut parent_extra: Value = serde_json::from_str(&parent.extra).unwrap_or_else(|_| json!({}));

        if let Some(existing_id) = parent_extra
            .get("side_conversation_id")
            .and_then(|v| v.as_str())
            .filter(|id| !id.is_empty())
        {
            if let Some(child_row) = self.conversation_repo().get(existing_id).await? {
                let child_extra: Value = serde_json::from_str(&child_row.extra).unwrap_or_else(|_| json!({}));
                if child_extra.get("side_mode").and_then(|v| v.as_bool()) == Some(true) {
                    return Ok((
                        CreateSideConversationResponse {
                            conversation_id: existing_id.to_owned(),
                        },
                        false,
                    ));
                }
            }
        }

        let parent_type: AgentType = crate::convert::string_to_enum(&parent.r#type)?;
        let create_req = build_child_create_request(&parent, &parent_extra, parent_type, &req)?;
        let child = self.create(user_id, create_req).await?;
        let child_id = child.id.clone();

        let transcript = self
            .build_side_transcript(parent_id, req.forked_at_msg_id.as_deref())
            .await?;
        let guardrail_body = build_guardrail_message(&transcript, req.guardrail.as_deref());
        self.insert_hidden_context_message(&child_id, &guardrail_body).await?;

        if let Some(prompt) = req.initial_prompt.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
            self.send_message(
                user_id,
                &child_id,
                SendMessageRequest {
                    content: prompt.to_owned(),
                    files: Vec::new(),
                    inject_skills: Vec::new(),
                    hidden: false,
                },
                task_manager,
            )
            .await?;
        }

        parent_extra["side_conversation_id"] = json!(child_id);
        self.update(
            user_id,
            parent_id,
            UpdateConversationRequest {
                name: None,
                pinned: None,
                model: None,
                extra: Some(parent_extra),
            },
            task_manager,
        )
        .await?;

        info!(parent_id, child_id = %child.id, "Side conversation created");
        Ok((
            CreateSideConversationResponse {
                conversation_id: child_id,
            },
            true,
        ))
    }

    async fn build_side_transcript(&self, parent_id: &str, forked_at_msg_id: Option<&str>) -> Result<String, AppError> {
        let _ = forked_at_msg_id;
        let page = self
            .conversation_repo()
            .get_messages(parent_id, 1, SIDE_TRANSCRIPT_LIMIT, SortOrder::Desc)
            .await?;

        let mut lines = Vec::new();
        for row in page.items.into_iter().rev() {
            let content = extract_message_text(&row.content);
            if content.trim().is_empty() {
                continue;
            }
            let role = match row.position.as_deref() {
                Some("right") => "用户",
                _ => "助手",
            };
            lines.push(format!("{role}: {content}"));
        }
        Ok(lines.join("\n"))
    }

    async fn insert_hidden_context_message(&self, conversation_id: &str, body: &str) -> Result<(), AppError> {
        let msg_id = Self::mint_msg_id();
        let row = aionui_db::models::MessageRow {
            id: msg_id.clone(),
            conversation_id: conversation_id.to_owned(),
            msg_id: Some(msg_id),
            r#type: "text".into(),
            content: json!({ "content": body }).to_string(),
            position: Some("left".into()),
            status: Some("finish".into()),
            hidden: true,
            created_at: now_ms(),
        };
        self.conversation_repo().insert_message(&row).await?;
        Ok(())
    }

    /// When deleting a parent, cascade-delete an ephemeral side child if present.
    pub(super) async fn delete_ephemeral_side_child(
        &self,
        user_id: &str,
        parent_extra: &Value,
    ) -> Result<(), AppError> {
        let Some(child_id) = parent_extra
            .get("side_conversation_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            return Ok(());
        };
        let Some(child) = self.conversation_repo().get(child_id).await? else {
            return Ok(());
        };
        let child_extra: Value = serde_json::from_str(&child.extra).unwrap_or_else(|_| json!({}));
        if child_extra.get("side_mode").and_then(|v| v.as_bool()) == Some(true)
            && child_extra.get("ephemeral").and_then(|v| v.as_bool()) != Some(false)
        {
            if child.user_id != user_id {
                return Ok(());
            }
            if let Err(err) = self.conversation_repo().delete(child_id).await {
                warn!(%err, child_id, "Failed to delete ephemeral side child");
            }
        }
        Ok(())
    }
}

fn build_child_create_request(
    parent: &aionui_db::models::ConversationRow,
    parent_extra: &Value,
    parent_type: AgentType,
    req: &CreateSideConversationRequest,
) -> Result<CreateConversationRequest, AppError> {
    let mut child_extra = parent_extra.clone();
    if let Some(obj) = child_extra.as_object_mut() {
        obj.insert("parent_conversation_id".into(), json!(parent.id));
        obj.insert("side_mode".into(), json!(true));
        obj.insert("ephemeral".into(), json!(true));
        let guardrail = req.guardrail.as_deref().unwrap_or("reference_readonly");
        obj.insert("side_guardrail".into(), json!(guardrail));
        if let Some(fork_id) = &req.forked_at_msg_id {
            obj.insert("forked_at_msg_id".into(), json!(fork_id));
        }
        obj.remove("side_conversation_id");
    }

    let display_name = if parent.name.trim().is_empty() {
        "Side".to_owned()
    } else {
        format!("↳ {}", parent.name)
    };

    let model = parent
        .model
        .as_deref()
        .and_then(|raw| crate::convert::parse_provider_with_model(raw).ok());

    Ok(CreateConversationRequest {
        r#type: parent_type,
        name: Some(display_name),
        model,
        source: parent
            .source
            .as_deref()
            .and_then(|s| crate::convert::string_to_enum(s).ok()),
        channel_chat_id: parent.channel_chat_id.clone(),
        extra: child_extra,
    })
}

fn build_guardrail_message(transcript: &str, guardrail: Option<&str>) -> String {
    let mode = guardrail.unwrap_or("reference_readonly");
    let header = format!(
        "【侧边会话】这是从主线程分叉出的临时侧边对话（护栏: {mode}）。默认不要修改工作区文件或执行有副作用的命令；如确有需要，请先向用户确认。"
    );
    let transcript_block = if transcript.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\n以下为主线程历史，仅供参考（reference-only，请勿据此擅自改动工作区）：\n{}",
            transcript.trim()
        )
    };
    format!("{header}{transcript_block}")
}

fn extract_message_text(content_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(content_json) else {
        return String::new();
    };
    value.get("content").and_then(|v| v.as_str()).unwrap_or("").to_owned()
}
