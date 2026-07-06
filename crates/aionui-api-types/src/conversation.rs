use aionui_common::{
    AgentType, ConversationSource, ConversationStatus, MessagePosition, MessageStatus, MessageType, PaginatedResult,
    ProviderWithModel, TimestampMs,
};
use serde::{Deserialize, Serialize};

use crate::acp::AcpConfigOptionDto;

/// Per-MCP snapshot status stored in `conversation.extra`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMcpStatusKind {
    Loaded,
    Failed,
    Unsupported,
}

/// A single MCP item shown in the conversation-scoped MCP list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMcpStatus {
    pub id: String,
    pub name: String,
    pub status: ConversationMcpStatusKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ── Request types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct AssistantConversationOverridesRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub skill_ids: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_builtin_skill_ids: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AssistantConversationRequest {
    pub id: String,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub conversation_overrides: Option<AssistantConversationOverridesRequest>,
}

/// Body for `POST /api/conversations`.
#[derive(Debug, Deserialize)]
pub struct CreateConversationRequest {
    #[serde(default)]
    pub r#type: Option<AgentType>,
    pub name: Option<String>,
    pub model: Option<ProviderWithModel>,
    pub assistant: Option<AssistantConversationRequest>,
    pub source: Option<ConversationSource>,
    pub channel_chat_id: Option<String>,
    pub extra: serde_json::Value,
}

/// Body for `PATCH /api/conversations/:id`.
///
/// All fields optional — only supplied fields are applied.
/// `extra` uses merge semantics (patch, not replace).
#[derive(Debug, Deserialize)]
pub struct UpdateConversationRequest {
    pub name: Option<String>,
    pub pinned: Option<bool>,
    pub model: Option<ProviderWithModel>,
    pub extra: Option<serde_json::Value>,
}

/// Body for `POST /api/conversations/clone`.
///
/// Despite the name, this endpoint no longer supports cloning from an
/// existing conversation — it's kept as a distinct route because multiple
/// call sites pass a pre-built `CreateConversationRequest` payload shape.
#[derive(Debug, Deserialize)]
pub struct CloneConversationRequest {
    pub conversation: CreateConversationRequest,
}

/// Body for `POST /api/conversations/:id/messages`.
///
/// `msg_id` is server-generated — clients must not provide one.
#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub inject_skills: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
}

/// Response for `POST /api/conversations/:id/messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub msg_id: String,
    pub turn_id: String,
    pub runtime: ConversationRuntimeSummary,
}

/// Body for `POST /api/conversations/:id/cancel`.
#[derive(Debug, Clone, Deserialize)]
pub struct CancelConversationRequest {
    pub turn_id: String,
}

/// Response for `POST /api/conversations/:id/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelConversationResponse {
    pub runtime: ConversationRuntimeSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRuntimeStateKind {
    Idle,
    Starting,
    Running,
    Cancelling,
    WaitingConfirmation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRuntimeSummary {
    pub state: ConversationRuntimeStateKind,
    pub can_send_message: bool,
    pub has_task: bool,
    pub task_status: Option<ConversationStatus>,
    pub is_processing: bool,
    pub pending_confirmations: usize,
    pub turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureConversationRuntimeResponse {
    pub recovered: bool,
    pub config_options: Vec<AcpConfigOptionDto>,
    pub runtime: ConversationRuntimeSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationAssistantIdentityResponse {
    pub id: String,
    pub source: String,
    pub name: String,
    pub avatar: String,
    pub backend: String,
}

// ── Query types ────────────────────────────────────────────────────

/// Query parameters for `GET /api/conversations`.
#[derive(Debug, Default, Deserialize)]
pub struct ListConversationsQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub source: Option<String>,
    pub cron_job_id: Option<String>,
    pub pinned: Option<bool>,
}

/// Query parameters for `GET /api/conversations/:id/messages`.
#[derive(Debug, Default, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<u32>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub anchor_message_id: Option<String>,
    pub content_mode: Option<String>,
}

/// Body for `PATCH /api/conversations/:id/artifacts/:artifact_id`.
#[derive(Debug, Deserialize)]
pub struct UpdateConversationArtifactRequest {
    pub status: ConversationArtifactStatus,
}

/// Query parameters for `GET /api/messages/search`.
#[derive(Debug, Deserialize)]
pub struct SearchMessagesQuery {
    pub keyword: String,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

// ── Response types ─────────────────────────────────────────────────

/// Full conversation object returned in API responses.
///
/// `model` is the canonical top-level field **only for `AgentType::Aionrs`**.
/// For every other agent type, `model` is always `None` here and the client
/// should read agent-specific model/mode fields out of `extra` (e.g. ACP uses
/// `extra.current_model_id` / `extra.current_mode_id`). See
/// `docs/superpowers/specs/2026-05-12-conversation-type-aware-model-design.md`.
///
/// `Option<T>` fields use `skip_serializing_if = "Option::is_none"` so the
/// serialized JSON omits the key entirely when the value is absent. This
/// keeps the wire shape tight and matches what the frontend mapper already
/// tolerates (`'model' in r` guard handles missing keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationResponse {
    pub id: String,
    pub name: String,
    pub r#type: AgentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ProviderWithModel>,
    pub status: ConversationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ConversationRuntimeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ConversationSource>,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<TimestampMs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<ConversationAssistantIdentityResponse>,
    pub created_at: TimestampMs,
    pub modified_at: TimestampMs,
    pub extra: serde_json::Value,
}

/// Paginated list of conversations.
pub type ConversationListResponse = PaginatedResult<ConversationResponse>;

/// Single message object returned in API responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    pub conversation_id: String,
    pub msg_id: Option<String>,
    pub r#type: MessageType,
    pub content: serde_json::Value,
    pub position: Option<MessagePosition>,
    pub status: Option<MessageStatus>,
    pub hidden: bool,
    pub created_at: TimestampMs,
}

/// Cursor-paginated list of messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageListResponse {
    pub items: Vec<MessageResponse>,
    pub oldest_cursor: Option<String>,
    pub newest_cursor: Option<String>,
    pub has_more_before: bool,
    pub has_more_after: bool,
}

/// Response for `GET /api/conversations/active-count`.
#[derive(Debug, Serialize)]
pub struct ActiveCountResponse {
    pub count: usize,
}

/// Artifact kind discriminant for conversation-bound UI artifacts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationArtifactKind {
    CronTrigger,
    SkillSuggest,
}

/// Durable artifact state exposed to the client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationArtifactStatus {
    Active,
    Pending,
    Dismissed,
    Saved,
}

/// Artifact object returned by conversation artifact APIs and websocket events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationArtifactResponse {
    pub id: String,
    pub conversation_id: String,
    pub cron_job_id: Option<String>,
    pub kind: ConversationArtifactKind,
    pub status: ConversationArtifactStatus,
    pub payload: serde_json::Value,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// List of conversation artifacts for a single conversation.
pub type ConversationArtifactListResponse = Vec<ConversationArtifactResponse>;

/// A single item from cross-conversation message search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSearchItem {
    pub message_id: String,
    pub message_type: String,
    pub message_created_at: TimestampMs,
    pub preview_text: String,
    pub conversation: ConversationResponse,
}

/// Paginated search results for messages.
pub type MessageSearchResponse = PaginatedResult<MessageSearchItem>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── CreateConversationRequest ───────────────────────────────────

    #[test]
    fn deserialize_create_request_full() {
        let raw = json!({
            "type": "acp",
            "name": "Code Review",
            "model": { "provider_id": "p1", "model": "claude-sonnet-4-20250514" },
            "assistant": {
                "id": "assistant-1",
                "locale": "zh-CN",
                "conversation_overrides": {
                    "model": "opus-4.1",
                    "permission": "yolo",
                    "thought_level": "high",
                    "skill_ids": ["skill-a"],
                    "disabled_builtin_skill_ids": ["builtin-a"],
                    "mcp_ids": ["mcp-a"]
                }
            },
            "source": "aionui",
            "channel_chat_id": "user:123",
            "extra": { "workspace": "/project" }
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, Some(AgentType::Acp));
        assert_eq!(req.name.as_deref(), Some("Code Review"));
        assert_eq!(req.model.unwrap().model, "claude-sonnet-4-20250514");
        assert_eq!(
            req.assistant,
            Some(AssistantConversationRequest {
                id: "assistant-1".into(),
                locale: Some("zh-CN".into()),
                conversation_overrides: Some(AssistantConversationOverridesRequest {
                    model: Some("opus-4.1".into()),
                    permission: Some("yolo".into()),
                    thought_level: Some("high".into()),
                    skill_ids: Some(vec!["skill-a".into()]),
                    disabled_builtin_skill_ids: Some(vec!["builtin-a".into()]),
                    mcp_ids: Some(vec!["mcp-a".into()]),
                }),
            })
        );
        assert_eq!(req.source, Some(ConversationSource::Aionui));
        assert_eq!(req.channel_chat_id.as_deref(), Some("user:123"));
        assert_eq!(req.extra["workspace"], "/project");
    }

    #[test]
    fn deserialize_create_request_minimal() {
        let raw = json!({
            "type": "acp",
            "model": { "provider_id": "p1", "model": "m1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, Some(AgentType::Acp));
        assert!(req.name.is_none());
        assert!(req.assistant.is_none());
        assert!(req.source.is_none());
        assert!(req.channel_chat_id.is_none());
    }

    #[test]
    fn deserialize_create_request_without_model() {
        let raw = json!({
            "type": "acp",
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, Some(AgentType::Acp));
        assert!(req.model.is_none());
    }

    #[test]
    fn deserialize_create_request_missing_type_without_assistant() {
        let raw = json!({
            "model": { "provider_id": "p1", "model": "m1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.r#type.is_none());
    }

    #[test]
    fn deserialize_create_request_missing_type_with_assistant() {
        let raw = json!({
            "assistant": { "id": "assistant-1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.r#type.is_none());
        assert_eq!(req.assistant.unwrap().id, "assistant-1");
    }

    #[test]
    fn deserialize_create_request_missing_extra() {
        let raw = json!({
            "type": "acp",
            "model": { "provider_id": "p1", "model": "m1" }
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_create_request_invalid_type() {
        let raw = json!({
            "type": "invalid_type",
            "model": { "provider_id": "p1", "model": "m1" },
            "extra": {}
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    // ── UpdateConversationRequest ───────────────────────────────────

    #[test]
    fn deserialize_update_request_partial() {
        let raw = json!({ "name": "New Name" });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("New Name"));
        assert!(req.pinned.is_none());
        assert!(req.model.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn deserialize_update_request_all_fields() {
        let raw = json!({
            "name": "Updated",
            "pinned": true,
            "model": { "provider_id": "p2", "model": "new-model" },
            "extra": { "workspace": "/new" }
        });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("Updated"));
        assert_eq!(req.pinned, Some(true));
        assert!(req.model.is_some());
        assert_eq!(req.extra.as_ref().unwrap()["workspace"], "/new");
    }

    #[test]
    fn deserialize_update_request_empty() {
        let raw = json!({});
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.name.is_none());
        assert!(req.pinned.is_none());
        assert!(req.model.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn deserialize_update_artifact_request() {
        let raw = json!({ "status": "dismissed" });
        let req: UpdateConversationArtifactRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.status, ConversationArtifactStatus::Dismissed);
    }

    // ── CloneConversationRequest ────────────────────────────────────

    #[test]
    fn deserialize_clone_request() {
        let raw = json!({
            "conversation": {
                "type": "acp",
                "model": { "provider_id": "p1", "model": "m1" },
                "extra": {}
            }
        });
        let req: CloneConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.conversation.r#type, Some(AgentType::Acp));
    }

    // ── ListConversationsQuery ──────────────────────────────────────

    #[test]
    fn deserialize_list_query_full() {
        let raw = json!({
            "cursor": "conv_last",
            "limit": 10,
            "source": "telegram",
            "cron_job_id": "cron_1",
            "pinned": true
        });
        let q: ListConversationsQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.cursor.as_deref(), Some("conv_last"));
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.source.as_deref(), Some("telegram"));
        assert_eq!(q.cron_job_id.as_deref(), Some("cron_1"));
        assert_eq!(q.pinned, Some(true));
    }

    #[test]
    fn deserialize_list_query_empty() {
        let raw = json!({});
        let q: ListConversationsQuery = serde_json::from_value(raw).unwrap();
        assert!(q.cursor.is_none());
        assert!(q.limit.is_none());
        assert!(q.source.is_none());
        assert!(q.cron_job_id.is_none());
        assert!(q.pinned.is_none());
    }

    // ── ListMessagesQuery ───────────────────────────────────────────

    #[test]
    fn deserialize_messages_query_defaults() {
        let raw = json!({});
        let q: ListMessagesQuery = serde_json::from_value(raw).unwrap();
        assert!(q.limit.is_none());
        assert!(q.before.is_none());
        assert!(q.after.is_none());
        assert!(q.anchor_message_id.is_none());
        assert!(q.content_mode.is_none());
    }

    #[test]
    fn deserialize_messages_query_with_values() {
        let raw = json!({
            "limit": 200,
            "before": "v1.abc",
            "content_mode": "compact"
        });
        let q: ListMessagesQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.limit, Some(200));
        assert_eq!(q.before.as_deref(), Some("v1.abc"));
        assert!(q.after.is_none());
        assert!(q.anchor_message_id.is_none());
        assert_eq!(q.content_mode.as_deref(), Some("compact"));
    }

    // ── SearchMessagesQuery ─────────────────────────────────────────

    #[test]
    fn deserialize_search_query() {
        let raw = json!({ "keyword": "rust", "page": 1, "page_size": 20 });
        let q: SearchMessagesQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.keyword, "rust");
        assert_eq!(q.page, Some(1));
        assert_eq!(q.page_size, Some(20));
    }

    #[test]
    fn deserialize_search_query_missing_keyword() {
        let raw = json!({ "page": 1 });
        assert!(serde_json::from_value::<SearchMessagesQuery>(raw).is_err());
    }

    // ── ConversationResponse ────────────────────────────────────────

    #[test]
    fn serialize_conversation_response_snake_case() {
        let resp = ConversationResponse {
            id: "conv_1".into(),
            name: "Test".into(),
            r#type: AgentType::Acp,
            model: Some(ProviderWithModel {
                provider_id: "p1".into(),
                model: "m1".into(),
                use_model: None,
            }),
            status: ConversationStatus::Pending,
            runtime: None,
            source: Some(ConversationSource::Aionui),
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            assistant: None,
            created_at: 1712345678000,
            modified_at: 1712345678000,
            extra: json!({ "workspace": "/project" }),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "conv_1");
        assert_eq!(json["type"], "acp");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["source"], "aionui");
        assert_eq!(json["created_at"], 1712345678000_i64);
        assert_eq!(json["modified_at"], 1712345678000_i64);
        assert_eq!(json["extra"]["workspace"], "/project");
        // Verify snake_case keys
        assert!(json.get("channelChatId").is_none());
        assert!(json.get("createdAt").is_none());
        assert!(json.get("pinnedAt").is_none());
        // Null-valued Option fields must be omitted from JSON.
        assert!(json.get("pinned_at").is_none(), "pinned_at None should be omitted");
        assert!(
            json.get("channel_chat_id").is_none(),
            "channel_chat_id None should be omitted"
        );
    }

    #[test]
    fn serialize_conversation_response_omits_none_keys() {
        let resp = ConversationResponse {
            id: "conv_none".into(),
            name: "Test".into(),
            r#type: AgentType::Acp,
            model: None,
            status: ConversationStatus::Pending,
            runtime: None,
            source: None,
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            assistant: None,
            created_at: 1,
            modified_at: 1,
            extra: json!({}),
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("model").is_none(), "model None should be omitted");
        assert!(json.get("source").is_none(), "source None should be omitted");
        assert!(json.get("pinned_at").is_none(), "pinned_at None should be omitted");
        assert!(
            json.get("channel_chat_id").is_none(),
            "channel_chat_id None should be omitted"
        );
        // Non-optional fields still present.
        assert_eq!(json["id"], "conv_none");
        assert_eq!(json["type"], "acp");
        assert_eq!(json["pinned"], false);
    }

    #[test]
    fn conversation_response_roundtrip() {
        let resp = ConversationResponse {
            id: "conv_2".into(),
            name: "Round".into(),
            r#type: AgentType::Acp,
            model: None,
            status: ConversationStatus::Running,
            runtime: None,
            source: None,
            pinned: true,
            pinned_at: Some(1712345678000),
            channel_chat_id: Some("group:42".into()),
            assistant: None,
            created_at: 1000,
            modified_at: 2000,
            extra: json!({}),
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: ConversationResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, resp.id);
        assert!(deserialized.pinned);
        assert_eq!(deserialized.pinned_at, Some(1712345678000));
        assert_eq!(deserialized.channel_chat_id.as_deref(), Some("group:42"));
    }

    // ── MessageResponse ─────────────────────────────────────────────

    #[test]
    fn serialize_message_response_snake_case() {
        let resp = MessageResponse {
            id: "msg_1".into(),
            conversation_id: "conv_1".into(),
            msg_id: Some("client_1".into()),
            r#type: MessageType::Text,
            content: json!({ "content": "Hello" }),
            position: Some(MessagePosition::Right),
            status: Some(MessageStatus::Finish),
            hidden: false,
            created_at: 1712345678000,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "msg_1");
        assert_eq!(json["conversation_id"], "conv_1");
        assert_eq!(json["msg_id"], "client_1");
        assert_eq!(json["type"], "text");
        assert_eq!(json["position"], "right");
        assert_eq!(json["status"], "finish");
        assert_eq!(json["hidden"], false);
        assert_eq!(json["created_at"], 1712345678000_i64);
        // Verify no camelCase leaks
        assert!(json.get("conversationId").is_none());
        assert!(json.get("msgId").is_none());
        assert!(json.get("createdAt").is_none());
    }

    #[test]
    fn message_response_roundtrip() {
        let resp = MessageResponse {
            id: "msg_2".into(),
            conversation_id: "conv_2".into(),
            msg_id: None,
            r#type: MessageType::ToolCall,
            content: json!({ "callId": "c1", "name": "bash" }),
            position: Some(MessagePosition::Left),
            status: None,
            hidden: true,
            created_at: 5000,
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: MessageResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.r#type, MessageType::ToolCall);
        assert!(deserialized.hidden);
        assert!(deserialized.msg_id.is_none());
        assert!(deserialized.status.is_none());
    }

    // ── MessageSearchItem ───────────────────────────────────────────

    #[test]
    fn serialize_search_item_snake_case() {
        let item = MessageSearchItem {
            message_id: "msg_1".into(),
            message_type: "text".into(),
            message_created_at: 1712345678000,
            preview_text: "matched snippet".into(),
            conversation: ConversationResponse {
                id: "conv_1".into(),
                name: "Code Review".into(),
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Finished,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                assistant: None,
                created_at: 1712345678000,
                modified_at: 1712345678000,
                extra: json!({}),
            },
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["message_id"], "msg_1");
        assert_eq!(json["message_type"], "text");
        assert_eq!(json["message_created_at"], 1712345678000_i64);
        assert_eq!(json["preview_text"], "matched snippet");
        assert_eq!(json["conversation"]["id"], "conv_1");
        assert_eq!(json["conversation"]["name"], "Code Review");
        // Verify no camelCase leaks
        assert!(json.get("messageId").is_none());
        assert!(json.get("messageType").is_none());
        assert!(json.get("previewText").is_none());
    }

    #[test]
    fn search_item_roundtrip() {
        let item = MessageSearchItem {
            message_id: "msg_x".into(),
            message_type: "tips".into(),
            message_created_at: 9000,
            preview_text: "some content preview".into(),
            conversation: ConversationResponse {
                id: "conv_x".into(),
                name: "Search Test".into(),
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Finished,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                assistant: None,
                created_at: 9000,
                modified_at: 9000,
                extra: json!({}),
            },
        };
        let serialized = serde_json::to_string(&item).unwrap();
        let deserialized: MessageSearchItem = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.message_id, "msg_x");
        assert_eq!(deserialized.message_type, "tips");
        assert_eq!(deserialized.preview_text, "some content preview");
        assert_eq!(deserialized.conversation.name, "Search Test");
    }

    // ── SendMessageRequest ──────────────────────────────────────────

    #[test]
    fn deserialize_send_message_full() {
        let raw = json!({
            "content": "Review this code",
            "files": ["/tmp/a.rs"],
            "inject_skills": ["security-review"],
            "hidden": true
        });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Review this code");
        assert_eq!(req.files, vec!["/tmp/a.rs"]);
        assert_eq!(req.inject_skills, vec!["security-review"]);
        assert!(req.hidden);
    }

    #[test]
    fn deserialize_send_message_minimal() {
        let raw = json!({ "content": "Hi" });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Hi");
        assert!(req.files.is_empty());
        assert!(req.inject_skills.is_empty());
        assert!(!req.hidden);
    }

    #[test]
    fn deserialize_send_message_missing_content() {
        let raw = json!({});
        assert!(serde_json::from_value::<SendMessageRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_send_message_ignores_client_msg_id() {
        // Clients may still send msg_id from stale builds — it must be ignored.
        let raw = json!({ "content": "Hi", "msg_id": "client-supplied" });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Hi");
    }

    // ── Paginated type aliases ──────────────────────────────────────

    #[test]
    fn conversation_list_response_serialization() {
        let list: ConversationListResponse = PaginatedResult {
            items: vec![ConversationResponse {
                id: "conv_1".into(),
                name: "Test".into(),
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Pending,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                assistant: None,
                created_at: 1000,
                modified_at: 1000,
                extra: json!({}),
            }],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        assert_eq!(json["total"], 1);
        assert_eq!(json["has_more"], false);
    }

    #[test]
    fn serialize_message_list_response_cursor_shape() {
        let resp = MessageListResponse {
            items: vec![],
            oldest_cursor: None,
            newest_cursor: None,
            has_more_before: false,
            has_more_after: false,
        };
        let raw = serde_json::to_value(resp).unwrap();
        assert_eq!(raw["items"], json!([]));
        assert!(raw["oldest_cursor"].is_null());
        assert!(raw["newest_cursor"].is_null());
        assert_eq!(raw["has_more_before"], false);
        assert_eq!(raw["has_more_after"], false);
        assert!(raw.get("total").is_none());
        assert!(raw.get("has_more").is_none());
    }

    #[test]
    fn message_search_response_serialization() {
        let resp: MessageSearchResponse = PaginatedResult {
            items: vec![MessageSearchItem {
                message_id: "m1".into(),
                message_type: "text".into(),
                message_created_at: 5000,
                preview_text: "matched".into(),
                conversation: ConversationResponse {
                    id: "c1".into(),
                    name: "Conv".into(),
                    r#type: AgentType::Acp,
                    model: None,
                    status: ConversationStatus::Finished,
                    runtime: None,
                    source: None,
                    pinned: false,
                    pinned_at: None,
                    channel_chat_id: None,
                    assistant: None,
                    created_at: 5000,
                    modified_at: 5000,
                    extra: json!({}),
                },
            }],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["items"][0]["message_id"], "m1");
        assert_eq!(json["items"][0]["conversation"]["id"], "c1");
        assert_eq!(json["items"][0]["preview_text"], "matched");
        assert_eq!(json["total"], 1);
    }

    #[test]
    fn serialize_conversation_artifact_response() {
        let artifact = ConversationArtifactResponse {
            id: "conv_1:skill_suggest:cron_1".into(),
            conversation_id: "conv_1".into(),
            cron_job_id: Some("cron_1".into()),
            kind: ConversationArtifactKind::SkillSuggest,
            status: ConversationArtifactStatus::Active,
            payload: json!({
                "cron_job_id": "cron_1",
                "name": "daily-report",
                "description": "Daily report",
                "skillContent": "---\nname: daily-report\n---\nUse it.",
            }),
            created_at: 1000,
            updated_at: 2000,
        };

        let raw = serde_json::to_value(&artifact).unwrap();
        assert_eq!(raw["kind"], "skill_suggest");
        assert_eq!(raw["status"], "active");
        assert_eq!(raw["payload"]["name"], "daily-report");
    }
}
