use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const TEAM_TOOLS_SCHEMA_VERSION: u32 = 1;

pub const TEAM_SPAWN_AGENT_DESCRIPTION: &str = r#"Create a new teammate agent to join the team.

Use this only when one of the following is true:
- The user explicitly approved the proposed teammate lineup in a previous message
- The user explicitly instructed you to create a specific teammate immediately

Before calling this tool in the normal planning flow:
- Start with one short sentence explaining why additional teammates would help
- Tell the user which teammate(s) you recommend
- Present the proposal as a table with: name, responsibility, and recommended assistant
- Include each teammate's responsibility and recommended assistant
- Ask whether to create them as proposed or change any names, responsibilities, or assistant choices
- In that approval question, remind the user that they can later ask you to replace or adjust any teammate if the lineup is not working well
- Do NOT call this tool in that same turn; wait for explicit approval in a later user message

When calling this tool, always provide assistant_id from the available assistants catalog.
Do not provide a model. The new teammate uses the selected assistant's configured/default model; users can adjust models from the UI model selector.

The new agent will be created and added to the team. You can then assign tasks and send messages to it."#;

pub const TEAM_DESCRIBE_ASSISTANT_DESCRIPTION: &str = "Get detailed information about an assistant before spawning it as a teammate.\n\n\
Returns the assistant's full description, enabled skills, and example tasks so you can\n\
judge whether it fits the user's request. Use this when two or more assistants look\n\
relevant from the one-line catalog in your system prompt.\n\n\
Use team_list_assistants to find candidate assistant_id values.\n\
After confirming a match, call team_spawn_agent with the same assistant_id.";

pub const TEAM_LIST_ASSISTANTS_DESCRIPTION: &str = "List the assistants available for team spawning. Returns the real assistant catalog with \
real assistant_id values, names, backends, descriptions, and skills.\n\nUse this before \
team_spawn_agent when you need the exact assistant_id for a teammate. Do NOT guess from backend \
names like claude/codex/gemini — only use assistant_id values returned here.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamToolPermission {
    AnyTeamAgent,
    LeadOnly,
}

impl TeamToolPermission {
    pub fn is_lead_only(self) -> bool {
        self == Self::LeadOnly
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamToolRole {
    Lead,
    Teammate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamToolTransport {
    Mcp,
    CliAssumed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamToolName {
    TeamMembers,
    TeamSendMessage,
    TeamTaskCreate,
    TeamTaskUpdate,
    TeamTaskList,
    TeamListAssistants,
    TeamDescribeAssistant,
    TeamSpawnAgent,
    TeamRenameAgent,
    TeamShutdownAgent,
}

impl TeamToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TeamMembers => "team_members",
            Self::TeamSendMessage => "team_send_message",
            Self::TeamTaskCreate => "team_task_create",
            Self::TeamTaskUpdate => "team_task_update",
            Self::TeamTaskList => "team_task_list",
            Self::TeamListAssistants => "team_list_assistants",
            Self::TeamDescribeAssistant => "team_describe_assistant",
            Self::TeamSpawnAgent => "team_spawn_agent",
            Self::TeamRenameAgent => "team_rename_agent",
            Self::TeamShutdownAgent => "team_shutdown_agent",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "team_members" => Self::TeamMembers,
            "team_send_message" => Self::TeamSendMessage,
            "team_task_create" => Self::TeamTaskCreate,
            "team_task_update" => Self::TeamTaskUpdate,
            "team_task_list" => Self::TeamTaskList,
            "team_list_assistants" => Self::TeamListAssistants,
            "team_describe_assistant" => Self::TeamDescribeAssistant,
            "team_spawn_agent" => Self::TeamSpawnAgent,
            "team_rename_agent" => Self::TeamRenameAgent,
            "team_shutdown_agent" => Self::TeamShutdownAgent,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamToolDescriptor {
    pub name: String,
    pub permission: TeamToolPermission,
    pub description: String,
    pub input_schema: Value,
    pub cli_command: Vec<String>,
    pub when: String,
    pub input_summary: String,
}

impl TeamToolDescriptor {
    pub fn lead_only(&self) -> bool {
        self.permission.is_lead_only()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamToolCall {
    pub tool: TeamToolName,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TeamToolErrorCode {
    UnknownTool,
    SchemaValidationFailed,
    PermissionDenied,
    TeamNotFound,
    ConversationNotFound,
    AgentNotFound,
    NotInTeam,
    TransportUnavailable,
    RuntimeContextMissing,
    RuntimeAuthFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TeamToolErrorPayload {
    pub code: TeamToolErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl TeamToolErrorPayload {
    pub fn new(code: TeamToolErrorCode, message: impl Into<String>) -> Self {
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
pub struct TeamToolCliMeta {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamToolCliEnvelope<T> {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TeamToolErrorPayload>,
    pub meta: TeamToolCliMeta,
}

impl<T> TeamToolCliEnvelope<T> {
    pub fn success(data: T, command: Option<String>) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
            meta: TeamToolCliMeta {
                schema_version: TEAM_TOOLS_SCHEMA_VERSION,
                command,
            },
        }
    }

    pub fn failure(error: TeamToolErrorPayload, command: Option<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(error),
            meta: TeamToolCliMeta {
                schema_version: TEAM_TOOLS_SCHEMA_VERSION,
                command,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamToolContextResponse {
    pub in_team: bool,
    pub conversation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<TeamToolRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<TeamToolTransport>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamToolRuntimeCallRequest {
    pub tool: TeamToolName,
    #[serde(default)]
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamToolRuntimeCallResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<TeamToolErrorPayload>,
}

pub fn team_tool_descriptors() -> Vec<TeamToolDescriptor> {
    tool_specs()
        .into_iter()
        .map(|spec| TeamToolDescriptor {
            name: spec.name.as_str().to_owned(),
            permission: spec.permission,
            description: spec.description.to_owned(),
            input_schema: spec.input_schema,
            cli_command: spec.cli_command.iter().map(|part| (*part).to_owned()).collect(),
            when: spec.when.to_owned(),
            input_summary: spec.input_summary.to_owned(),
        })
        .collect()
}

pub fn team_tool_descriptors_for_role(role: TeamToolRole) -> Vec<TeamToolDescriptor> {
    let is_lead = role == TeamToolRole::Lead;
    team_tool_descriptors()
        .into_iter()
        .filter(|descriptor| is_lead || !descriptor.permission.is_lead_only())
        .collect()
}

pub fn team_tool_descriptor(name: &str) -> Option<TeamToolDescriptor> {
    team_tool_descriptors()
        .into_iter()
        .find(|descriptor| descriptor.name == name)
}

pub fn cli_command_for_tool(name: &str) -> Option<&'static [&'static str]> {
    tool_specs()
        .into_iter()
        .find(|spec| spec.name.as_str() == name)
        .map(|spec| spec.cli_command)
}

pub fn tool_name_for_cli_path(path: &[String]) -> Option<TeamToolName> {
    tool_specs()
        .into_iter()
        .find(|spec| spec.cli_command == path.iter().map(String::as_str).collect::<Vec<_>>())
        .map(|spec| spec.name)
}

#[derive(Debug, Clone)]
struct TeamToolSpec {
    name: TeamToolName,
    permission: TeamToolPermission,
    description: &'static str,
    input_schema: Value,
    cli_command: &'static [&'static str],
    when: &'static str,
    input_summary: &'static str,
}

fn tool_specs() -> Vec<TeamToolSpec> {
    vec![
        TeamToolSpec {
            name: TeamToolName::TeamMembers,
            permission: TeamToolPermission::AnyTeamAgent,
            description: "List all team members with their roles and current status.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
            cli_command: &["members"],
            when: "Check roster/status",
            input_summary: "{}",
        },
        TeamToolSpec {
            name: TeamToolName::TeamSendMessage,
            permission: TeamToolPermission::AnyTeamAgent,
            description: "Send a message to a teammate or broadcast to all (to=\"*\").",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "to": { "type": "string", "description": "Target agent slot_id or \"*\" for broadcast" },
                    "message": { "type": "string", "description": "Message content" }
                },
                "required": ["to", "message"]
            }),
            cli_command: &["send-message"],
            when: "Send teammate message",
            input_summary: "to, message",
        },
        TeamToolSpec {
            name: TeamToolName::TeamTaskCreate,
            permission: TeamToolPermission::AnyTeamAgent,
            description: "Create a new task on the team task board.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "subject": { "type": "string", "description": "Task subject" },
                    "description": { "type": "string", "description": "Task description" },
                    "owner": { "type": "string", "description": "Owning agent slotId" },
                    "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Task IDs this task depends on" }
                },
                "required": ["subject"]
            }),
            cli_command: &["task", "create"],
            when: "Create task",
            input_summary: "subject, optional owner/deps",
        },
        TeamToolSpec {
            name: TeamToolName::TeamTaskUpdate,
            permission: TeamToolPermission::AnyTeamAgent,
            description: "Update an existing task on the team task board.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "task_id": { "type": "string", "description": "Task ID to update" },
                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed", "deleted"], "description": "New status" },
                    "description": { "type": "string", "description": "New description" },
                    "owner": { "type": "string", "description": "New owning agent slotId" },
                    "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "New dependency list" }
                },
                "required": ["task_id"]
            }),
            cli_command: &["task", "update"],
            when: "Update task",
            input_summary: "task_id, optional status/owner/deps",
        },
        TeamToolSpec {
            name: TeamToolName::TeamTaskList,
            permission: TeamToolPermission::AnyTeamAgent,
            description: "List tasks on the team task board. Pass {} for the full board, or use owner/status/include_deleted/limit for filtered views.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "owner": {
                        "type": "string",
                        "description": "Return only tasks owned by this exact agent slot_id"
                    },
                    "status": {
                        "description": "Return only tasks with these statuses. Accepts one status string or an array of status strings.",
                        "anyOf": [
                            {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "deleted"]
                            },
                            {
                                "type": "array",
                                "minItems": 1,
                                "items": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "deleted"]
                                }
                            }
                        ]
                    },
                    "include_deleted": {
                        "type": "boolean",
                        "description": "When status is omitted, include deleted tasks. Defaults to true so {} returns the full board."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Maximum number of returned tasks. Values above 200 are clamped to 200."
                    }
                }
            }),
            cli_command: &["task", "list"],
            when: "List tasks",
            input_summary: "owner, status, include_deleted, limit",
        },
        TeamToolSpec {
            name: TeamToolName::TeamListAssistants,
            permission: TeamToolPermission::AnyTeamAgent,
            description: TEAM_LIST_ASSISTANTS_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
            cli_command: &["list-assistants"],
            when: "List spawn assistants",
            input_summary: "{}",
        },
        TeamToolSpec {
            name: TeamToolName::TeamDescribeAssistant,
            permission: TeamToolPermission::AnyTeamAgent,
            description: TEAM_DESCRIBE_ASSISTANT_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "assistant_id": { "type": "string", "description": "The assistant ID from the available assistants catalog (e.g., \"word-creator\")." },
                    "locale": { "type": "string", "description": "Locale like \"zh-CN\" or \"en-US\". Defaults to the user's current UI language when omitted." }
                },
                "required": ["assistant_id"]
            }),
            cli_command: &["describe-assistant"],
            when: "Inspect assistant",
            input_summary: "assistant_id, optional locale",
        },
        TeamToolSpec {
            name: TeamToolName::TeamSpawnAgent,
            permission: TeamToolPermission::LeadOnly,
            description: TEAM_SPAWN_AGENT_DESCRIPTION,
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": { "type": "string", "description": "Agent display name" },
                    "assistant_id": { "type": "string", "description": "Assistant ID to spawn. Call team_list_assistants when you need candidates; the runtime backend is derived from this assistant." }
                },
                "required": ["name", "assistant_id"]
            }),
            cli_command: &["spawn-agent"],
            when: "Spawn teammate",
            input_summary: "name, assistant_id",
        },
        TeamToolSpec {
            name: TeamToolName::TeamRenameAgent,
            permission: TeamToolPermission::LeadOnly,
            description: "Rename a team member. Lead only.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "slot_id": { "type": "string", "description": "Agent slot_id to rename" },
                    "new_name": { "type": "string", "description": "New display name" }
                },
                "required": ["slot_id", "new_name"]
            }),
            cli_command: &["rename-agent"],
            when: "Rename teammate",
            input_summary: "slot_id, new_name",
        },
        TeamToolSpec {
            name: TeamToolName::TeamShutdownAgent,
            permission: TeamToolPermission::LeadOnly,
            description: "Initiate shutdown of a teammate. Lead only. Sends a shutdown_request to the target agent.",
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "slot_id": { "type": "string", "description": "Agent slot_id to shut down" },
                    "reason": { "type": "string", "description": "Reason for shutdown" }
                },
                "required": ["slot_id"]
            }),
            cli_command: &["shutdown-agent"],
            when: "Shut down teammate",
            input_summary: "slot_id, optional reason",
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn descriptor_count_and_names_are_unique() {
        let descriptors = team_tool_descriptors();
        assert_eq!(descriptors.len(), 10);
        let names = descriptors
            .iter()
            .map(|descriptor| descriptor.name.as_str())
            .collect::<HashSet<_>>();
        assert_eq!(names.len(), descriptors.len());
    }

    #[test]
    fn descriptors_have_required_prompt_and_schema_fields() {
        for descriptor in team_tool_descriptors() {
            assert!(!descriptor.name.is_empty());
            assert!(!descriptor.description.is_empty());
            assert!(!descriptor.when.is_empty());
            assert!(!descriptor.input_summary.is_empty());
            assert!(!descriptor.cli_command.is_empty());
            assert_eq!(descriptor.input_schema["type"], "object");
        }
    }

    #[test]
    fn cli_command_mapping_matches_spec() {
        let cases = [
            ("team_members", vec!["members"]),
            ("team_send_message", vec!["send-message"]),
            ("team_task_create", vec!["task", "create"]),
            ("team_task_update", vec!["task", "update"]),
            ("team_task_list", vec!["task", "list"]),
            ("team_list_assistants", vec!["list-assistants"]),
            ("team_describe_assistant", vec!["describe-assistant"]),
            ("team_spawn_agent", vec!["spawn-agent"]),
            ("team_rename_agent", vec!["rename-agent"]),
            ("team_shutdown_agent", vec!["shutdown-agent"]),
        ];
        for (tool, path) in cases {
            assert_eq!(cli_command_for_tool(tool), Some(path.as_slice()));
            let owned = path.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(tool_name_for_cli_path(&owned).map(TeamToolName::as_str), Some(tool));
        }
    }

    #[test]
    fn teammate_role_hides_lead_only_tools() {
        let names = team_tool_descriptors_for_role(TeamToolRole::Teammate)
            .into_iter()
            .map(|descriptor| descriptor.name)
            .collect::<Vec<_>>();
        assert!(!names.contains(&"team_spawn_agent".to_owned()));
        assert!(!names.contains(&"team_rename_agent".to_owned()));
        assert!(!names.contains(&"team_shutdown_agent".to_owned()));
        assert!(names.contains(&"team_send_message".to_owned()));
    }

    #[test]
    fn spawn_schema_is_assistant_first_and_excludes_legacy_fields() {
        let descriptor = team_tool_descriptor("team_spawn_agent").expect("spawn descriptor");
        assert_eq!(descriptor.permission, TeamToolPermission::LeadOnly);
        let props = descriptor.input_schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert!(props.contains_key("assistant_id"));
        assert!(!props.contains_key("model"));
        assert!(!props.contains_key("backend"));
        assert!(!props.contains_key("agent_type"));
        assert!(!props.contains_key("role"));
        let required = descriptor.input_schema["required"].as_array().unwrap();
        assert!(required.contains(&json!("name")));
        assert!(required.contains(&json!("assistant_id")));
    }
}
