//! Agent-facing contract for the `aioncore session` CLI.
//!
//! Shape follows `team_tools.rs`: a descriptor registry is the single source of
//! truth, so `session capabilities`, the marker-block pointer, and the
//! auto-inject skill cannot drift from the wired CLI. Deliberately a separate
//! type family from the team one — reusing `TeamToolCliEnvelope` would leak
//! team naming into a feature that has nothing to do with teams.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SESSION_TOOLS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionToolName {
    SessionList,
    SessionSendMessage,
}

impl SessionToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionList => "session_list",
            Self::SessionSendMessage => "session_send_message",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "session_list" => Self::SessionList,
            "session_send_message" => Self::SessionSendMessage,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub cli_command: Vec<String>,
    pub when: String,
    pub input_summary: String,
}

#[derive(Debug, Clone)]
struct SessionToolSpec {
    name: SessionToolName,
    description: &'static str,
    input_schema: Value,
    cli_command: &'static [&'static str],
    when: &'static str,
    input_summary: &'static str,
}

fn tool_specs() -> Vec<SessionToolSpec> {
    vec![
        SessionToolSpec {
            name: SessionToolName::SessionList,
            description: "List the conversations this conversation may deliver a message to. \
                           Use it only when the user named a target in prose instead of \
                           selecting it with `@@`.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string", "description": "Optional name filter." },
                    "project_id": { "type": "string", "description": "Optional project scope." },
                    "limit": { "type": "integer", "description": "Page size (default 20, max 50)." },
                    "cursor": { "type": "string", "description": "Opaque cursor from a previous page." }
                },
                "required": [],
                "additionalProperties": false
            }),
            cli_command: &["list"],
            when: "The user described a target conversation in prose and you need its id.",
            input_summary: "optional q / project_id / limit / cursor",
        },
        SessionToolSpec {
            name: SessionToolName::SessionSendMessage,
            description: "Deliver a message to another one of this user's conversations. \
                           Semantically identical to the user opening that conversation and \
                           pressing send: the recipient starts a turn and decides for itself \
                           whether to reply. Only conversation ids are accepted — not names, \
                           and there is no broadcast.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "to": { "type": "string", "description": "Target conversation id. Ids only — never a name, never `*`." },
                    "message": { "type": "string", "description": "The message body the recipient will receive." }
                },
                "required": ["to", "message"],
                "additionalProperties": false
            }),
            cli_command: &["send-message"],
            when: "The user asked you to tell another conversation something, or you are replying to a delivery via its `reply_to`.",
            input_summary: "{ to, message }",
        },
    ]
}

pub fn session_tool_descriptors() -> Vec<SessionToolDescriptor> {
    tool_specs()
        .into_iter()
        .map(|spec| SessionToolDescriptor {
            name: spec.name.as_str().to_owned(),
            description: spec.description.to_owned(),
            input_schema: spec.input_schema,
            cli_command: spec.cli_command.iter().map(|part| (*part).to_owned()).collect(),
            when: spec.when.to_owned(),
            input_summary: spec.input_summary.to_owned(),
        })
        .collect()
}

pub fn session_tool_descriptor(name: &str) -> Option<SessionToolDescriptor> {
    session_tool_descriptors().into_iter().find(|d| d.name == name)
}

pub fn tool_name_for_session_cli_path(path: &[String]) -> Option<SessionToolName> {
    tool_specs()
        .into_iter()
        .find(|spec| spec.cli_command == path.iter().map(String::as_str).collect::<Vec<_>>())
        .map(|spec| spec.name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionToolErrorCode {
    TargetNotFound,
    TargetIsTeam,
    SenderIsTeam,
    TargetIsSelf,
    QueueFull,
    RateLimited,
    FeatureDisabled,
    RuntimeAuthFailed,
    SchemaValidationFailed,
    TransportUnavailable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionToolErrorPayload {
    pub code: SessionToolErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl SessionToolErrorPayload {
    pub fn new(code: SessionToolErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCliMeta {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCliEnvelope<T> {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SessionToolErrorPayload>,
    pub meta: SessionCliMeta,
}

impl<T> SessionCliEnvelope<T> {
    pub fn success(data: T, command: Option<String>) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: SessionCliMeta {
                schema_version: SESSION_TOOLS_SCHEMA_VERSION,
                command,
            },
        }
    }

    pub fn failure(error: SessionToolErrorPayload, command: Option<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error),
            meta: SessionCliMeta {
                schema_version: SESSION_TOOLS_SCHEMA_VERSION,
                command,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSendMessageRequest {
    pub to: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionDeliveryStatus {
    /// Turn claim taken, message persisted, prompt dispatched — or merged into
    /// the target's already-running turn.
    Delivered,
    /// Target busy and its backend does not support mid-turn delivery (or that
    /// turn is blocked on a confirmation card); queued in memory for the drainer.
    Queued,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSendMessageResponse {
    pub status: SessionDeliveryStatus,
    pub to: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionMentionableQuery {
    pub q: Option<String>,
    pub project_id: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
    /// The conversation the picker is open in — excluded from results. Only the
    /// user-facing `mentionable` route needs it; the runtime `targets` route
    /// takes the caller's conversation from its token-bound header instead.
    pub current_conversation_id: Option<String>,
    /// Restrict the result to this one conversation, to answer "is this id still
    /// deliverable?".
    ///
    /// The UI needs that when a user clicks a conversation chip on an OLD
    /// message: `@@` references are atomic, so a target that has since been
    /// deleted or joined a team would fail the entire message at send time,
    /// after the user has already written it. Answering through this route
    /// rather than a bespoke check means the answer comes from exactly the same
    /// filtering the picker uses, and it returns the CURRENT name — so a
    /// conversation an agent has renamed since is mentioned under its new name.
    ///
    /// Deliberately absent from the `session list` tool descriptor: agents
    /// address conversations by id already and have no use for it.
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMentionTarget {
    pub id: String,
    pub name: String,
    /// Project name, for the human-facing picker. Absent when unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub modified_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMentionableResponse {
    pub items: Vec<SessionMentionTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionRateGate {
    Outbound,
    Pair,
}

/// Payload of the `sessionMessage.rateLimited` WS event.
///
/// `user_id` is REQUIRED: `BroadcastEventBus` fans out to every connection, so
/// per-user filtering can only happen on the payload. Omitting it would
/// broadcast one user's conversation names to all connections. Carries no
/// message body (spec §10).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessageRateLimitedPayload {
    pub user_id: String,
    pub from_conversation_id: String,
    pub from_name: String,
    pub to_conversation_id: String,
    pub to_name: String,
    pub window_count: u32,
    pub gate: SessionRateGate,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_descriptor_has_a_cli_command_and_a_reachable_name() {
        let descriptors = session_tool_descriptors();
        assert_eq!(descriptors.len(), 2, "v1 exposes list + send-message only");
        for descriptor in &descriptors {
            assert!(!descriptor.cli_command.is_empty(), "{}", descriptor.name);
            assert!(
                session_tool_descriptor(&descriptor.name).is_some(),
                "{} must be findable by name",
                descriptor.name
            );
            assert_eq!(
                tool_name_for_session_cli_path(&descriptor.cli_command).map(SessionToolName::as_str),
                Some(descriptor.name.as_str()),
                "cli path must round-trip to the tool name for {}",
                descriptor.name
            );
        }
    }

    #[test]
    fn send_message_schema_accepts_only_to_and_message() {
        // Locks spec §6.1: no `files`, no broadcast, no name-based addressing.
        let descriptor = session_tool_descriptor("session_send_message").unwrap();
        let properties = descriptor.input_schema["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["message", "to"]);
        let required = descriptor.input_schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 2);
    }

    #[test]
    fn error_codes_serialize_as_the_snake_case_wire_values_tests_assert_on() {
        assert_eq!(
            serde_json::to_value(SessionToolErrorCode::TargetIsTeam).unwrap(),
            serde_json::json!("target_is_team")
        );
        assert_eq!(
            serde_json::to_value(SessionToolErrorCode::FeatureDisabled).unwrap(),
            serde_json::json!("feature_disabled")
        );
        assert_eq!(
            serde_json::to_value(SessionDeliveryStatus::Queued).unwrap(),
            serde_json::json!("queued")
        );
    }

    #[test]
    fn envelope_failure_carries_the_code_and_omits_data() {
        let envelope = SessionCliEnvelope::<serde_json::Value>::failure(
            SessionToolErrorPayload::new(SessionToolErrorCode::QueueFull, "queue is full"),
            Some("session send-message".to_owned()),
        );
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["success"], serde_json::json!(false));
        assert_eq!(json["error"]["code"], serde_json::json!("queue_full"));
        assert!(json.get("data").is_none(), "{json}");
        assert_eq!(json["meta"]["schema_version"], serde_json::json!(1));
    }

    #[test]
    fn every_error_code_has_a_distinct_wire_value() {
        // The CLI envelope, the routes, and every bad-path test assert on these
        // strings, so a duplicate would silently collapse two failure modes.
        let codes = [
            SessionToolErrorCode::TargetNotFound,
            SessionToolErrorCode::TargetIsTeam,
            SessionToolErrorCode::SenderIsTeam,
            SessionToolErrorCode::TargetIsSelf,
            SessionToolErrorCode::QueueFull,
            SessionToolErrorCode::RateLimited,
            SessionToolErrorCode::FeatureDisabled,
            SessionToolErrorCode::RuntimeAuthFailed,
            SessionToolErrorCode::SchemaValidationFailed,
            SessionToolErrorCode::TransportUnavailable,
        ];
        let mut wire: Vec<String> = codes
            .iter()
            .map(|code| serde_json::to_value(code).unwrap().as_str().unwrap().to_owned())
            .collect();
        wire.sort();
        let count = wire.len();
        wire.dedup();
        assert_eq!(wire.len(), count, "duplicate wire value in {wire:?}");
    }

    #[test]
    fn an_unwired_cli_path_does_not_resolve_to_a_tool() {
        assert!(tool_name_for_session_cli_path(&["capabilities".to_owned()]).is_none());
        assert!(tool_name_for_session_cli_path(&[]).is_none());
        assert!(session_tool_descriptor("session_broadcast").is_none());
    }
}
