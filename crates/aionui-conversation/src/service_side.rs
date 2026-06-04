//! Side-conversation fork primitive (`POST /api/conversations/:id/side`).
//!
//! v0.2: multi-tab sides, dual fork paths (`agent_fork` | `text_snapshot`).

use std::sync::Arc;

use aionui_ai_agent::IWorkerTaskManager;
use aionui_api_types::{
    ConversationResponse, CreateConversationRequest, CreateSideConversationRequest, CreateSideConversationResponse,
    SendMessageRequest, SideForkMode,
};
use aionui_common::{AgentType, TimestampMs, now_ms};
use aionui_db::{MessagePageCursor, MessagePageDirection, MessagePageParams};
use serde_json::{Value, json};
use tracing::{info, warn};

use crate::ConversationError;
use crate::service::ConversationService;

/// Safety cap when building a one-time parent transcript snapshot (path B).
const PARENT_SNAPSHOT_PAGE_SIZE: u32 = 100;

impl ConversationService {
    /// Restore side children for a parent row.
    #[tracing::instrument(skip_all, fields(parent_id = %parent_id))]
    pub async fn list_side_conversations(
        &self,
        user_id: &str,
        parent_id: &str,
    ) -> Result<Vec<ConversationResponse>, ConversationError> {
        self.conversation_repo()
            .get(parent_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: parent_id.to_owned(),
            })?;

        let children = self.conversation_repo().list_side_children(user_id, parent_id).await?;
        let mut responses = Vec::with_capacity(children.len());
        for child in children {
            match self.get(user_id, &child.id).await {
                Ok(resp) => responses.push(resp),
                Err(err) => warn!(%err, child_id = %child.id, "Failed to restore side child"),
            }
        }
        Ok(responses)
    }

    /// Fork a new side conversation from a parent row (always creates a new child).
    #[tracing::instrument(skip_all, fields(parent_id = %parent_id))]
    pub async fn create_side_conversation(
        &self,
        user_id: &str,
        parent_id: &str,
        req: CreateSideConversationRequest,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<CreateSideConversationResponse, ConversationError> {
        let parent = self
            .conversation_repo()
            .get(parent_id)
            .await?
            .filter(|r| r.user_id == user_id)
            .ok_or_else(|| ConversationError::NotFound {
                id: parent_id.to_owned(),
            })?;

        let parent_extra: Value = serde_json::from_str(&parent.extra).unwrap_or_else(|_| json!({}));
        let parent_type: AgentType = crate::convert::string_to_enum(&parent.r#type)?;

        if !is_side_supported_parent_type(parent_type) {
            return Err(ConversationError::BadRequest {
                reason: "Side conversation is not supported for this agent type".into(),
            });
        }

        let (fork_mode, fork_parent_session_id) = self
            .resolve_fork_strategy(user_id, parent_type, &parent, &parent_extra, task_manager)
            .await?;

        let bootstrap = match fork_mode {
            SideForkMode::AgentFork => build_side_fork_boundary_message(&parent, &parent_extra, &req),
            SideForkMode::TextSnapshot => {
                let transcript = self.build_parent_reference_transcript(parent_id).await?;
                build_side_snapshot_bootstrap_message(&parent, &parent_extra, &req, &transcript)
            }
        };
        let create_req = build_child_create_request(
            &parent,
            &parent_extra,
            parent_type,
            &req,
            fork_mode,
            fork_parent_session_id.as_deref(),
            &bootstrap,
        )?;
        let child = self.create(user_id, create_req).await?;
        let child_id = child.id.clone();

        self.insert_hidden_context_message(&child_id, &bootstrap, now_ms())
            .await?;

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

        info!(parent_id, child_id = %child.id, ?fork_mode, "Side conversation created");
        Ok(CreateSideConversationResponse {
            conversation_id: child_id,
            created: true,
            fork_mode,
        })
    }

    /// Prefix agent input with parent snapshot — **only** for legacy rows without `fork_mode`.
    pub(super) async fn enrich_side_agent_content(
        &self,
        child_extra: &Value,
        user_content: &str,
    ) -> Result<String, ConversationError> {
        if !is_side_conversation_extra(child_extra) {
            return Ok(user_content.to_owned());
        }
        // v0.2 rows carry `fork_mode`; snapshot is one-time at create — no per-turn enrich.
        if child_extra.get("fork_mode").is_some() {
            return Ok(user_content.to_owned());
        }

        let Some(parent_id) = child_extra
            .get("parent_conversation_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        else {
            return Ok(user_content.to_owned());
        };

        let transcript = self.build_parent_reference_transcript(parent_id).await?;
        let workspace = child_extra
            .get("workspace")
            .and_then(|v| v.as_str())
            .unwrap_or("(shared with parent)");

        let reference = if transcript.trim().is_empty() {
            format!(
                "[主线程参考 · 只读 · 本回合自动刷新]\n\
                 主会话 ID: {parent_id}\n\
                 工作区: {workspace}\n\
                 （主会话暂无可引用的文本消息；请结合工作区与侧边栏左侧主线程 UI 判断进展。）"
            )
        } else {
            format!(
                "[主线程参考 · 只读 · 本回合自动刷新]\n\
                 主会话 ID: {parent_id}\n\
                 工作区: {workspace}\n\n\
                 {transcript}"
            )
        };

        Ok(format!("{reference}\n\n---\n\n{user_content}"))
    }

    async fn build_parent_reference_transcript(&self, parent_id: &str) -> Result<String, ConversationError> {
        let mut direction = MessagePageDirection::InitialLatest;
        let mut batches = Vec::new();
        loop {
            let batch = self
                .conversation_repo()
                .list_messages_page(
                    parent_id,
                    &MessagePageParams {
                        limit: PARENT_SNAPSHOT_PAGE_SIZE,
                        direction,
                    },
                )
                .await?;
            let has_more_before = batch.has_more_before;
            let before_cursor = batch.items.first().map(MessagePageCursor::from);
            if !batch.items.is_empty() {
                batches.push(batch.items);
            }
            if !has_more_before {
                break;
            }
            let Some(cursor) = before_cursor else {
                break;
            };
            direction = MessagePageDirection::Before { cursor };
        }

        let mut lines = Vec::new();
        for batch in batches.into_iter().rev() {
            for row in batch {
                if row.hidden {
                    continue;
                }
                if !is_reference_snapshot_message_type(&row.r#type) {
                    continue;
                }
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
        }
        Ok(lines.join("\n"))
    }

    async fn insert_hidden_context_message(
        &self,
        conversation_id: &str,
        body: &str,
        created_at: TimestampMs,
    ) -> Result<(), ConversationError> {
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
            created_at,
        };
        self.conversation_repo().insert_message(&row).await?;
        Ok(())
    }

    /// When deleting a parent, cascade-delete ephemeral side children.
    pub(super) async fn delete_ephemeral_side_children(
        &self,
        user_id: &str,
        parent_id: &str,
    ) -> Result<(), ConversationError> {
        let children = self.conversation_repo().list_side_children(user_id, parent_id).await?;
        for child in children {
            let child_extra: Value = serde_json::from_str(&child.extra).unwrap_or_else(|_| json!({}));
            if child_extra.get("side_mode").and_then(|v| v.as_bool()) == Some(true)
                && child_extra.get("ephemeral").and_then(|v| v.as_bool()) != Some(false)
                && child.user_id == user_id
                && let Err(err) = self.conversation_repo().delete(&child.id).await
            {
                warn!(%err, child_id = %child.id, "Failed to delete ephemeral side child");
            }
        }
        Ok(())
    }

    async fn resolve_fork_strategy(
        &self,
        user_id: &str,
        parent_type: AgentType,
        parent: &aionui_db::models::ConversationRow,
        parent_extra: &Value,
        task_manager: &Arc<dyn IWorkerTaskManager>,
    ) -> Result<(SideForkMode, Option<String>), ConversationError> {
        match parent_type {
            AgentType::Aionrs => Ok((SideForkMode::TextSnapshot, None)),
            AgentType::Acp => {
                let backend = parent_extra.get("backend").and_then(|v| v.as_str()).unwrap_or("");
                // Snapshot backends never call session/fork — skip warming the parent CLI.
                if !acp_backend_has_spec_session_fork(backend) {
                    return Ok((SideForkMode::TextSnapshot, None));
                }
                let mut opts = self.build_task_options(parent).await?;
                let parent_id = parent.id.as_str();
                self.apply_conversation_runtime_context(&mut opts, user_id, parent_id);
                self.ensure_workspace_skill_links(parent, &opts).await;
                let instance = task_manager.get_or_build_task(parent_id, opts).await?;
                match instance.acp_ensure_warm_session_id().await {
                    Ok(parent_sid) => Ok((SideForkMode::AgentFork, Some(parent_sid))),
                    Err(err) => Err(ConversationError::BadRequest {
                        reason: format!("Side session fork requires a ready parent ACP session: {err}"),
                    }),
                }
            }
            _ => Err(ConversationError::BadRequest {
                reason: "Side conversation is not supported for this agent type".into(),
            }),
        }
    }
}

pub(super) fn is_side_conversation_extra(extra: &Value) -> bool {
    extra.get("side_mode").and_then(|v| v.as_bool()) == Some(true)
}

fn is_side_supported_parent_type(parent_type: AgentType) -> bool {
    matches!(parent_type, AgentType::Acp | AgentType::Aionrs)
}

fn is_reference_snapshot_message_type(message_type: &str) -> bool {
    message_type == "text"
}

/// ACP backends audited in side-conversation spec §3.5 as implementing `session/fork`.
/// For these, path A is the product default whenever the parent session is warm —
/// not gated on a flaky `sessionCapabilities.fork` field in every adapter build.
fn acp_backend_has_spec_session_fork(backend: &str) -> bool {
    matches!(backend, "claude" | "opencode" | "vibe")
}

#[cfg(test)]
mod fork_policy_tests {
    use super::{acp_backend_has_spec_session_fork, is_reference_snapshot_message_type};

    #[test]
    fn fork_backends_match_spec_section_3_5() {
        assert!(acp_backend_has_spec_session_fork("claude"));
        assert!(acp_backend_has_spec_session_fork("opencode"));
        assert!(acp_backend_has_spec_session_fork("vibe"));
        assert!(!acp_backend_has_spec_session_fork("codex"));
        assert!(!acp_backend_has_spec_session_fork("gemini"));
    }

    #[test]
    fn reference_snapshot_only_uses_visible_text_messages() {
        assert!(is_reference_snapshot_message_type("text"));
        assert!(!is_reference_snapshot_message_type("thinking"));
        assert!(!is_reference_snapshot_message_type("tool"));
    }
}

fn build_child_create_request(
    parent: &aionui_db::models::ConversationRow,
    parent_extra: &Value,
    parent_type: AgentType,
    req: &CreateSideConversationRequest,
    fork_mode: SideForkMode,
    fork_parent_session_id: Option<&str>,
    side_context: &str,
) -> Result<CreateConversationRequest, ConversationError> {
    let child_extra = sanitize_child_extra(
        parent_extra,
        &parent.id,
        parent_type,
        req,
        fork_mode,
        fork_parent_session_id,
        side_context,
    );

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
        r#type: Some(parent_type),
        name: Some(display_name),
        model,
        assistant: None,
        source: parent
            .source
            .as_deref()
            .and_then(|s| crate::convert::string_to_enum(s).ok()),
        channel_chat_id: parent.channel_chat_id.clone(),
        extra: child_extra,
    })
}

/// Copy only fork-safe fields. Do not clone immutable post-create snapshots (`skills`, MCP_*).
fn sanitize_child_extra(
    parent_extra: &Value,
    parent_id: &str,
    parent_type: AgentType,
    req: &CreateSideConversationRequest,
    fork_mode: SideForkMode,
    fork_parent_session_id: Option<&str>,
    side_context: &str,
) -> Value {
    let mut obj = serde_json::Map::new();
    if let Some(parent) = parent_extra.as_object() {
        for key in [
            "workspace",
            "backend",
            "agent_name",
            "agent_id",
            "cli_path",
            "session_mode",
            "current_model_id",
            "preset_context",
            "system_prompt",
            "preset_rules",
            "max_tokens",
            "max_turns",
            "gateway",
            "remote_agent_id",
            "remoteAgentId",
        ] {
            if let Some(value) = parent.get(key) {
                obj.insert(key.to_owned(), value.clone());
            }
        }
        if let Some(skills) = parent.get("skills").and_then(|v| v.as_array()) {
            obj.insert("preset_enabled_skills".to_owned(), Value::Array(skills.clone()));
        }
    }

    obj.insert("parent_conversation_id".into(), json!(parent_id));
    obj.insert("side_mode".into(), json!(true));
    obj.insert("ephemeral".into(), json!(true));
    let guardrail = req.guardrail.as_deref().unwrap_or("reference_readonly");
    obj.insert("side_guardrail".into(), json!(guardrail));
    obj.insert(
        "fork_mode".into(),
        json!(match fork_mode {
            SideForkMode::AgentFork => "agent_fork",
            SideForkMode::TextSnapshot => "text_snapshot",
        }),
    );
    if let Some(parent_sid) = fork_parent_session_id.filter(|s| !s.is_empty()) {
        obj.insert("fork_parent_session_id".into(), json!(parent_sid));
    }
    if let Some(fork_id) = &req.forked_at_msg_id
        && !fork_id.is_empty()
    {
        obj.insert("forked_at_msg_id".into(), json!(fork_id));
    }

    merge_side_context_for_agent(&mut obj, parent_type, side_context);

    Value::Object(obj)
}

fn merge_side_context_for_agent(obj: &mut serde_json::Map<String, Value>, parent_type: AgentType, side_context: &str) {
    let context = side_context.trim();
    if context.is_empty() {
        return;
    }
    let key = match parent_type {
        AgentType::Acp => "preset_context",
        AgentType::Aionrs => "system_prompt",
        _ => return,
    };
    let merged = match obj
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(existing) => format!("{existing}\n\n{context}"),
        None => context.to_owned(),
    };
    obj.insert(key.to_owned(), json!(merged));
}

fn build_side_fork_boundary_message(
    parent: &aionui_db::models::ConversationRow,
    parent_extra: &Value,
    req: &CreateSideConversationRequest,
) -> String {
    let parent_id = parent.id.as_str();
    let title = if parent.name.trim().is_empty() {
        "(untitled)"
    } else {
        parent.name.trim()
    };
    let status = parent.status.as_deref().unwrap_or("unknown");
    let mode = req.guardrail.as_deref().unwrap_or("reference_readonly");
    let workspace = parent_extra
        .get("workspace")
        .and_then(|v| v.as_str())
        .unwrap_or("(same as parent)");
    let fork_note = req
        .forked_at_msg_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|id| format!("\n分叉锚点消息: {id}"))
        .unwrap_or_default();

    format!(
        "【侧边会话 · agent fork】你从主会话 {parent_id} 通过 ACP session/fork 分叉。\n\
         主会话标题: {title}\n\
         主会话状态: {status}\n\
         主会话仍在左侧继续；本侧边栏只展示侧边自己的对话。\n\
         工作区: {workspace}{fork_note}\n\
         护栏: {mode} — 默认只读参考主线程，不要擅自改工作区或执行有副作用命令。\n\
         用户在侧边里说“进度”“刚才”“主线”“现在做到哪了”时，默认是在问分叉时继承到的父主会话。\n\
         分叉之后主线程的新 turn **不会** 自动同步到本 tab；需要更新认知请新开 tab。\n\
         不要要求用户再说明“主会话进度”，除非问题确实无法从继承上下文回答。"
    )
}

fn build_side_snapshot_bootstrap_message(
    parent: &aionui_db::models::ConversationRow,
    parent_extra: &Value,
    req: &CreateSideConversationRequest,
    transcript: &str,
) -> String {
    let parent_id = parent.id.as_str();
    let title = if parent.name.trim().is_empty() {
        "(untitled)"
    } else {
        parent.name.trim()
    };
    let status = parent.status.as_deref().unwrap_or("unknown");
    let mode = req.guardrail.as_deref().unwrap_or("reference_readonly");
    let workspace = parent_extra
        .get("workspace")
        .and_then(|v| v.as_str())
        .unwrap_or("(same as parent)");
    let fork_note = req
        .forked_at_msg_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|id| format!("\n分叉锚点消息: {id}"))
        .unwrap_or_default();

    let snapshot_block = if transcript.trim().is_empty() {
        "（主会话暂无可引用的文本消息；请结合工作区与左侧主线程 UI 判断进展。）".to_owned()
    } else {
        format!("[主线程快照 · 只读 · 创建时固定]\n{transcript}")
    };

    format!(
        "【侧边会话 · 摘要模式】你从主会话 {parent_id} 分叉（text snapshot）。\n\
         主会话标题: {title}\n\
         主会话状态: {status}\n\
         主会话仍在左侧继续；本侧边栏只展示侧边自己的对话。\n\
         工作区: {workspace}{fork_note}\n\
         护栏: {mode} — 默认只读参考主线程，不要擅自改工作区或执行有副作用命令。\n\
         用户在侧边里说“进度”“刚才”“主线”“现在做到哪了”时，默认是在问下方主线程快照。\n\
         以下为主线程在创建本 tab 时的快照（之后主线新 turn 不会自动写入）：\n\n\
         {snapshot_block}"
    )
}

fn extract_message_text(content_json: &str) -> String {
    let Ok(value) = serde_json::from_str::<Value>(content_json) else {
        return String::new();
    };
    value.get("content").and_then(|v| v.as_str()).unwrap_or("").to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_child_extra_drops_immutable_snapshots() {
        let parent_extra = json!({
            "workspace": "/w",
            "backend": "codex",
            "skills": ["a"],
            "mcp_server_ids": ["m1"],
            "mcp_statuses": [],
            "side_conversation_id": "old-child"
        });
        let req = CreateSideConversationRequest {
            guardrail: None,
            initial_prompt: None,
            forked_at_msg_id: Some("msg-1".into()),
        };
        let child = sanitize_child_extra(
            &parent_extra,
            "parent-1",
            AgentType::Acp,
            &req,
            SideForkMode::TextSnapshot,
            None,
            "side context",
        );
        let obj = child.as_object().unwrap();
        assert_eq!(obj.get("workspace").unwrap(), "/w");
        assert_eq!(obj.get("preset_enabled_skills").unwrap(), &json!(["a"]));
        assert!(obj.get("skills").is_none());
        assert!(obj.get("mcp_server_ids").is_none());
        assert!(obj.get("side_conversation_id").is_none());
        assert_eq!(obj.get("parent_conversation_id").unwrap(), "parent-1");
        assert_eq!(obj.get("fork_mode").unwrap(), "text_snapshot");
        assert_eq!(obj.get("preset_context").unwrap(), "side context");
    }

    #[test]
    fn sanitize_child_extra_merges_side_context_into_existing_agent_context() {
        let parent_extra = json!({
            "system_prompt": "base system",
        });
        let req = CreateSideConversationRequest {
            guardrail: None,
            initial_prompt: None,
            forked_at_msg_id: None,
        };
        let child = sanitize_child_extra(
            &parent_extra,
            "parent-1",
            AgentType::Aionrs,
            &req,
            SideForkMode::TextSnapshot,
            None,
            "side context",
        );
        let obj = child.as_object().unwrap();
        assert_eq!(obj.get("system_prompt").unwrap(), "base system\n\nside context");
    }
}
