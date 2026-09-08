//! Agent-facing contract for the `aioncore conversation` CLI.
//!
//! Shape follows `session_tools.rs`: a descriptor registry is the single source
//! of truth, so `conversation capabilities` and the auto-inject skill cannot
//! drift from the wired CLI. Deliberately a separate type family from the
//! session one — the two command families are independent surfaces.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CONVERSATION_TOOLS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationToolName {
    ConversationCreate,
}

impl ConversationToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ConversationCreate => "conversation_create",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "conversation_create" => Self::ConversationCreate,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub cli_command: Vec<String>,
    pub when: String,
    pub input_summary: String,
}

#[derive(Debug, Clone)]
struct ConversationToolSpec {
    name: ConversationToolName,
    description: &'static str,
    input_schema: Value,
    cli_command: &'static [&'static str],
    when: &'static str,
    input_summary: &'static str,
}

fn tool_specs() -> Vec<ConversationToolSpec> {
    vec![ConversationToolSpec {
        name: ConversationToolName::ConversationCreate,
        description: "Create a new conversation for this user. By default it inherits this \
                       conversation's working directory and assistant; pass `workspace` or \
                       `assistant_id` to choose another. Creating does not send a message, \
                       does not open the new conversation, and does not switch the user's \
                       current conversation.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Short name describing the task, in the user's language. Required; must not be blank." },
                "workspace": { "type": "string", "description": "Absolute path of an existing directory. Omit to reuse this conversation's workspace." },
                "assistant_id": { "type": "string", "description": "Id of an enabled assistant. Omit to reuse this conversation's assistant." }
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        cli_command: &["create"],
        when: "The user asked you to open, start, or spin up a new conversation.",
        input_summary: "{ name, workspace?, assistant_id? }",
    }]
}

pub fn conversation_tool_descriptors() -> Vec<ConversationToolDescriptor> {
    tool_specs()
        .into_iter()
        .map(|spec| ConversationToolDescriptor {
            name: spec.name.as_str().to_owned(),
            description: spec.description.to_owned(),
            input_schema: spec.input_schema,
            cli_command: spec.cli_command.iter().map(|part| (*part).to_owned()).collect(),
            when: spec.when.to_owned(),
            input_summary: spec.input_summary.to_owned(),
        })
        .collect()
}

pub fn conversation_tool_descriptor(name: &str) -> Option<ConversationToolDescriptor> {
    conversation_tool_descriptors().into_iter().find(|d| d.name == name)
}

pub fn tool_name_for_conversation_cli_path(path: &[String]) -> Option<ConversationToolName> {
    tool_specs()
        .into_iter()
        .find(|spec| spec.cli_command == path.iter().map(String::as_str).collect::<Vec<_>>())
        .map(|spec| spec.name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationToolErrorCode {
    CallerIsTeam,
    WorkspaceNotAbsolute,
    WorkspaceUnavailable,
    AssistantNotFound,
    AssistantDisabled,
    AssistantModelUnresolved,
    RuntimeAuthFailed,
    SchemaValidationFailed,
    TransportUnavailable,
}

impl ConversationToolErrorCode {
    /// The wire value, for structured log fields. Kept in lock-step with the
    /// serde rename by a unit test.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CallerIsTeam => "caller_is_team",
            Self::WorkspaceNotAbsolute => "workspace_not_absolute",
            Self::WorkspaceUnavailable => "workspace_unavailable",
            Self::AssistantNotFound => "assistant_not_found",
            Self::AssistantDisabled => "assistant_disabled",
            Self::AssistantModelUnresolved => "assistant_model_unresolved",
            Self::RuntimeAuthFailed => "runtime_auth_failed",
            Self::SchemaValidationFailed => "schema_validation_failed",
            Self::TransportUnavailable => "transport_unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationToolErrorPayload {
    pub code: ConversationToolErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl ConversationToolErrorPayload {
    pub fn new(code: ConversationToolErrorCode, message: impl Into<String>) -> Self {
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
pub struct ConversationCliMeta {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationCliEnvelope<T> {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ConversationToolErrorPayload>,
    pub meta: ConversationCliMeta,
}

impl<T> ConversationCliEnvelope<T> {
    pub fn success(data: T, command: Option<String>) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: ConversationCliMeta {
                schema_version: CONVERSATION_TOOLS_SCHEMA_VERSION,
                command,
            },
        }
    }

    pub fn failure(error: ConversationToolErrorPayload, command: Option<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error),
            meta: ConversationCliMeta {
                schema_version: CONVERSATION_TOOLS_SCHEMA_VERSION,
                command,
            },
        }
    }
}

/// Body of `POST /api/runtime/conversations/create`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationCreateRequest {
    pub name: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub assistant_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationCreateAssistant {
    pub id: String,
    pub name: String,
    pub backend: String,
}

/// `data` of a successful `conversation create`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationCreateResponse {
    pub id: String,
    pub name: String,
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<ConversationCreateAssistant>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_exactly_create_and_it_round_trips_through_its_cli_path() {
        let descriptors = conversation_tool_descriptors();
        assert_eq!(descriptors.len(), 1, "v1 exposes create only");
        for descriptor in &descriptors {
            assert!(!descriptor.cli_command.is_empty(), "{}", descriptor.name);
            assert!(conversation_tool_descriptor(&descriptor.name).is_some());
            assert_eq!(
                tool_name_for_conversation_cli_path(&descriptor.cli_command).map(ConversationToolName::as_str),
                Some(descriptor.name.as_str()),
                "cli path must round-trip to the tool name for {}",
                descriptor.name
            );
        }
        assert_eq!(
            ConversationToolName::parse("conversation_create"),
            Some(ConversationToolName::ConversationCreate)
        );
        assert_eq!(ConversationToolName::parse("conversation_delete"), None);
    }

    #[test]
    fn create_schema_accepts_only_name_workspace_and_assistant_id_with_name_required() {
        // additionalProperties=false, `name` is the only required field.
        let descriptor = conversation_tool_descriptor("conversation_create").unwrap();
        let properties = descriptor.input_schema["properties"].as_object().unwrap();
        let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["assistant_id", "name", "workspace"]);
        assert_eq!(descriptor.input_schema["required"], serde_json::json!(["name"]));
        assert_eq!(
            descriptor.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(descriptor.cli_command, vec!["create".to_owned()]);
    }

    #[test]
    fn every_error_code_has_a_distinct_snake_case_wire_value_matching_as_str() {
        let codes = [
            ConversationToolErrorCode::CallerIsTeam,
            ConversationToolErrorCode::WorkspaceNotAbsolute,
            ConversationToolErrorCode::WorkspaceUnavailable,
            ConversationToolErrorCode::AssistantNotFound,
            ConversationToolErrorCode::AssistantDisabled,
            ConversationToolErrorCode::AssistantModelUnresolved,
            ConversationToolErrorCode::RuntimeAuthFailed,
            ConversationToolErrorCode::SchemaValidationFailed,
            ConversationToolErrorCode::TransportUnavailable,
        ];
        let mut wire: Vec<String> = codes
            .iter()
            .map(|code| {
                let value = serde_json::to_value(code).unwrap().as_str().unwrap().to_owned();
                assert_eq!(value, code.as_str(), "as_str must equal the serde wire value");
                assert!(
                    value.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                    "{value} is not snake_case"
                );
                value
            })
            .collect();
        wire.sort();
        let count = wire.len();
        wire.dedup();
        assert_eq!(wire.len(), count, "duplicate wire value in {wire:?}");
        assert_eq!(ConversationToolErrorCode::CallerIsTeam.as_str(), "caller_is_team");
    }

    #[test]
    fn envelope_failure_carries_the_code_and_omits_data() {
        let envelope = ConversationCliEnvelope::<serde_json::Value>::failure(
            ConversationToolErrorPayload::new(ConversationToolErrorCode::CallerIsTeam, "caller is a team conversation"),
            Some("conversation create".to_owned()),
        );
        let json = serde_json::to_value(&envelope).unwrap();
        assert_eq!(json["success"], serde_json::json!(false));
        assert_eq!(json["error"]["code"], serde_json::json!("caller_is_team"));
        assert!(json.get("data").is_none(), "{json}");
        assert_eq!(json["meta"]["schema_version"], serde_json::json!(1));
        assert_eq!(json["meta"]["command"], serde_json::json!("conversation create"));
    }

    #[test]
    fn create_request_rejects_unknown_fields_and_requires_name() {
        // Server-side second line of defence behind the CLI's descriptor check.
        assert!(serde_json::from_str::<ConversationCreateRequest>(r#"{"name":"x","files":[]}"#).is_err());
        assert!(serde_json::from_str::<ConversationCreateRequest>(r#"{"workspace":"/tmp"}"#).is_err());
        let ok: ConversationCreateRequest = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert_eq!(ok.name, "x");
        assert!(ok.workspace.is_none() && ok.assistant_id.is_none());
    }

    #[test]
    fn an_unwired_cli_path_does_not_resolve_to_a_tool() {
        assert!(tool_name_for_conversation_cli_path(&["capabilities".to_owned()]).is_none());
        assert!(tool_name_for_conversation_cli_path(&[]).is_none());
        assert!(conversation_tool_descriptor("conversation_delete").is_none());
    }
}
